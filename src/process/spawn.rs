use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::harness::HarnessError;

/// Directories GUI-launched or minimal-PATH processes commonly miss.
fn well_known_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".local/bin"));
        v.push(home.join(".cargo/bin"));
        v.push(home.join(".npm-global/bin"));
        v.push(home.join(".bun/bin"));
        v.push(home.join(".volta/bin"));
        v.push(home.join("bin"));
    }
    v.push(PathBuf::from("/opt/homebrew/bin"));
    v.push(PathBuf::from("/usr/local/bin"));
    v.push(PathBuf::from("/usr/bin"));
    v.push(PathBuf::from("/bin"));
    v
}

/// Resolve a harness executable from an explicit override, `PATH`, or well-known directories.
pub fn resolve_executable(name: &str, explicit: Option<&Path>) -> Result<PathBuf, HarnessError> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        if let Ok(found) = which::which(p) {
            return Ok(found);
        }
        return Err(HarnessError::unavailable(format!(
            "configured executable {} not found",
            p.display()
        )));
    }
    if let Ok(found) = which::which(name) {
        return Ok(found);
    }
    for dir in well_known_dirs() {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(HarnessError::unavailable(format!(
        "`{name}` not found on PATH or in well-known locations"
    )))
}

/// Typed argv/env builder. No shell is ever involved.
pub struct CommandBuilder {
    program: PathBuf,
    args: Vec<OsString>,
    envs: Vec<(OsString, Option<OsString>)>,
    cwd: Option<PathBuf>,
    stdin: bool,
}

const PROVIDER_KEYS: &[&str] = &["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY"];

impl CommandBuilder {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: vec![
                (OsString::from("NO_COLOR"), Some(OsString::from("1"))),
                (OsString::from("TERM"), Some(OsString::from("dumb"))),
                (OsString::from("CI"), Some(OsString::from("1"))),
            ],
            cwd: None,
            stdin: true,
        }
    }

    pub fn arg(mut self, a: impl Into<OsString>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: impl Into<OsString>, v: impl Into<OsString>) -> Self {
        self.envs.push((k.into(), Some(v.into())));
        self
    }

    pub fn env_remove(mut self, k: impl Into<OsString>) -> Self {
        self.envs.push((k.into(), None));
        self
    }

    pub fn strip_provider_keys(mut self, strip: bool) -> Self {
        if strip {
            for k in PROVIDER_KEYS {
                self.envs.push((OsString::from(k), None));
            }
        }
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn no_stdin(mut self) -> Self {
        self.stdin = false;
        self
    }

    pub fn argv_for_log(&self) -> String {
        let mut s = self.program.display().to_string();
        for a in &self.args {
            s.push(' ');
            s.push_str(&a.to_string_lossy());
        }
        s
    }

    pub fn build(self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        for (k, v) in &self.envs {
            match v {
                Some(v) => {
                    cmd.env(k, v);
                }
                None => {
                    cmd.env_remove(k);
                }
            }
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(if self.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        {
            // Own process group so the whole tree can be signalled.
            cmd.process_group(0);
        }
        cmd
    }
}
