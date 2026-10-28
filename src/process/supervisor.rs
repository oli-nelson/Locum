use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr};
use tokio::task::JoinHandle;

use crate::harness::HarnessError;

/// Bounded ring buffer that keeps the tail of a child's stderr for diagnostics.
#[derive(Clone, Default)]
pub struct StderrTail {
    inner: Arc<Mutex<VecDeque<u8>>>,
    cap: usize,
}

impl StderrTail {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap.min(8192)))),
            cap,
        }
    }

    pub fn push(&self, chunk: &[u8]) {
        let mut buf = self.inner.lock();
        for &b in chunk {
            if buf.len() >= self.cap {
                buf.pop_front();
            }
            buf.push_back(b);
        }
    }

    pub fn snapshot(&self) -> String {
        let buf = self.inner.lock();
        let (a, b) = buf.as_slices();
        let mut v = Vec::with_capacity(buf.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        String::from_utf8_lossy(&v).into_owned()
    }

    pub fn spawn_drain(&self, stderr: ChildStderr) -> JoinHandle<()> {
        let tail = self.clone();
        tokio::spawn(async move {
            let mut rd = BufReader::new(stderr);
            let mut buf = [0u8; 4096];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => tail.push(&buf[..n]),
                }
            }
        })
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Owns a child process and guarantees the whole process group is killed on drop.
pub struct ChildGuard {
    child: Option<Child>,
    pid: Option<u32>,
    pub stderr: StderrTail,
    stderr_task: Option<JoinHandle<()>>,
    name: &'static str,
}

impl ChildGuard {
    pub fn new(mut child: Child, name: &'static str, stderr_cap: usize) -> Self {
        let tail = StderrTail::new(stderr_cap);
        let stderr_task = child.stderr.take().map(|e| tail.spawn_drain(e));
        Self {
            pid: child.id(),
            child: Some(child),
            stderr: tail,
            stderr_task,
            name,
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child taken")
    }

    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// Wait for exit with a timeout; returns the exit code if it exited.
    pub async fn wait_timeout(&mut self, dur: Duration) -> Option<i32> {
        let child = self.child.as_mut()?;
        match tokio::time::timeout(dur, child.wait()).await {
            Ok(Ok(status)) => status.code().or(Some(-1)),
            _ => None,
        }
    }

    pub fn try_wait(&mut self) -> Option<i32> {
        self.child
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten())
            .map(|s| s.code().unwrap_or(-1))
    }

    fn signal_group(&self, sig: Signal) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            let sig = match sig {
                Signal::Term => libc::SIGTERM,
                Signal::Kill => libc::SIGKILL,
            };
            unsafe {
                libc::killpg(pid as i32, sig);
            }
        }
        #[cfg(not(unix))]
        let _ = sig;
    }

    /// SIGTERM the group, then SIGKILL after the grace period.
    pub async fn terminate(&mut self, grace: Duration) {
        if self.child.is_none() {
            return;
        }
        if self.try_wait().is_some() {
            return;
        }
        self.signal_group(Signal::Term);
        if self.wait_timeout(grace).await.is_none() {
            self.signal_group(Signal::Kill);
            if let Some(c) = self.child.as_mut() {
                let _ = c.kill().await;
                let _ = tokio::time::timeout(Duration::from_secs(2), c.wait()).await;
            }
        }
        tracing::debug!(harness = self.name, pid = ?self.pid, "child terminated");
    }

    pub fn kill_now(&mut self) {
        self.signal_group(Signal::Kill);
        if let Some(c) = self.child.as_mut() {
            let _ = c.start_kill();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                self.signal_group(Signal::Kill);
                let _ = child.start_kill();
                // Reap asynchronously so we never leave a zombie behind.
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
        if let Some(t) = self.stderr_task.take() {
            t.abort();
        }
    }
}

/// Line-framed reader with a hard per-line byte cap.
pub struct LineReader<R> {
    rd: BufReader<R>,
    buf: BytesMut,
    max: usize,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub fn new(rd: R, max_line: usize) -> Self {
        Self {
            rd: BufReader::with_capacity(64 * 1024, rd),
            buf: BytesMut::with_capacity(8 * 1024),
            max: max_line,
        }
    }

    /// Returns `Ok(None)` on EOF.
    pub async fn next_line(&mut self) -> Result<Option<String>, HarnessError> {
        self.buf.clear();
        loop {
            let available = self
                .rd
                .fill_buf()
                .await
                .map_err(|e| HarnessError::failed(format!("read: {e}")))?;
            if available.is_empty() {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let s = String::from_utf8_lossy(&self.buf).into_owned();
                self.buf.clear();
                return Ok(Some(s));
            }
            if let Some(pos) = memchr(b'\n', available) {
                self.buf.extend_from_slice(&available[..pos]);
                self.rd.consume(pos + 1);
                if self.buf.last() == Some(&b'\r') {
                    self.buf.truncate(self.buf.len() - 1);
                }
                let s = String::from_utf8_lossy(&self.buf).into_owned();
                return Ok(Some(s));
            }
            let n = available.len();
            self.buf.extend_from_slice(available);
            self.rd.consume(n);
            if self.buf.len() > self.max {
                return Err(HarnessError::failed(format!(
                    "harness emitted a line longer than {} bytes",
                    self.max
                )));
            }
        }
    }
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}
