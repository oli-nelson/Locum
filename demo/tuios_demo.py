#!/usr/bin/env python3
"""Interactive tuios TML demo: an OpenAI client talking to locum, served by OpenCode.

Usage (from inside a tuios pane):
    python3 demo/tuios_demo.py [--port 11435] [--model opencode/openai/gpt-6.1-sol#medium]

The script starts `locum serve` in its own tuios pane (so the server log is visible), opens the
TML dashboard as a pane, and drives real HTTP requests when you press Send, patching the surface
live with each SSE chunk. Standard library only.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
TML = os.path.join(HERE, "locum_demo.tml")
SESSION = os.environ.get("TUIOS_SESSION")


def tuios(*args, check=True, capture=True):
    cmd = ["tuios", *args]
    r = subprocess.run(cmd, text=True, capture_output=capture)
    if check and r.returncode != 0:
        raise SystemExit(f"{' '.join(cmd)} failed:\n{r.stderr or r.stdout}")
    return r.stdout.strip() if capture else ""


def find_locum():
    for p in (os.path.join(ROOT, "target", "release", "locum"), os.path.join(ROOT, "target", "debug", "locum")):
        if os.path.exists(p):
            return p
    return shutil.which("locum")


def healthy(base):
    try:
        with urllib.request.urlopen(base.replace("/v1", "") + "/healthz", timeout=1) as r:
            return r.status == 200
    except Exception:
        return False


class Surface:
    """Owns the TML surface and batches patches so we stay well under the rate limit."""

    def __init__(self, proc):
        self.proc = proc
        self.id = None
        self.pending = {}
        self.lock = threading.Lock()
        self.flusher = threading.Thread(target=self._flush_loop, daemon=True)
        self.flusher.start()

    def set(self, **kv):
        with self.lock:
            self.pending.update(kv)

    def _flush_loop(self):
        while True:
            time.sleep(0.08)
            self.flush()

    def flush(self):
        with self.lock:
            if not self.pending or not self.id:
                return
            ops = [{"op": "replace", "path": f"/{k}", "value": v} for k, v in self.pending.items()]
            self.pending = {}
        subprocess.run(["tuios", "tml", "patch", self.id, json.dumps(ops)], capture_output=True, text=True)


def sse_frames(resp):
    buf = b""
    for chunk in iter(lambda: resp.read(1), b""):
        buf += chunk
        while b"\n\n" in buf:
            frame, buf = buf.split(b"\n\n", 1)
            for line in frame.decode("utf-8", "replace").splitlines():
                if line.startswith("data:"):
                    yield line[5:].strip()


def run_request(surface, base, model, prompt, stream):
    payload = {
        "model": model,
        "stream": stream,
        "messages": [{"role": "user", "content": prompt}],
    }
    if stream:
        payload["stream_options"] = {"include_usage": True}
    body = json.dumps(payload, indent=2)
    surface.set(
        status="sending", status_state="working", request_json=body, response="", frames="",
        ttfb_ms="–", duration_ms="–", chunks=0, prompt_tokens="–", completion_tokens="–",
        http="–", harness="–", resolved="–", request_id="–",
    )
    surface.flush()
    req = urllib.request.Request(
        base + "/chat/completions",
        data=body.encode(),
        headers={"content-type": "application/json", "authorization": "Bearer locum-demo"},
    )
    t0 = time.monotonic()
    text, frames, chunks, ttfb = "", [], 0, None
    try:
        with urllib.request.urlopen(req, timeout=300) as resp:
            h = resp.headers
            surface.set(
                http=f"{resp.status} {resp.reason}",
                harness=h.get("x-locum-harness", "–"),
                resolved=h.get("x-locum-resolved-model", "–"),
                request_id=h.get("x-request-id", "–"),
                status="streaming" if stream else "waiting",
            )
            if stream:
                for data in sse_frames(resp):
                    chunks += 1
                    frames.append(data if len(data) < 160 else data[:157] + "…")
                    frames = frames[-12:]
                    if data == "[DONE]":
                        break
                    ev = json.loads(data)
                    if "error" in ev:
                        text += f"\n\n**error:** {ev['error'].get('message')}"
                    for ch in ev.get("choices", []):
                        delta = ch.get("delta", {}).get("content")
                        if delta:
                            if ttfb is None:
                                ttfb = int((time.monotonic() - t0) * 1000)
                                surface.set(ttfb_ms=ttfb)
                            text += delta
                    if ev.get("usage"):
                        surface.set(
                            prompt_tokens=ev["usage"].get("prompt_tokens", "–"),
                            completion_tokens=ev["usage"].get("completion_tokens", "–"),
                        )
                    surface.set(response=text or "…", frames="\n".join(frames), chunks=chunks)
            else:
                raw = resp.read().decode()
                ttfb = int((time.monotonic() - t0) * 1000)
                ev = json.loads(raw)
                text = ev["choices"][0]["message"]["content"]
                frames = [json.dumps(ev, indent=2)[:1200]]
                surface.set(
                    ttfb_ms=ttfb,
                    prompt_tokens=ev["usage"].get("prompt_tokens", "–"),
                    completion_tokens=ev["usage"].get("completion_tokens", "–"),
                    chunks=1,
                    response=text,
                    frames=frames[0],
                )
        surface.set(status="done", status_state="done", duration_ms=int((time.monotonic() - t0) * 1000))
    except urllib.error.HTTPError as e:
        err = e.read().decode(errors="replace")
        try:
            msg = json.loads(err)["error"]["message"]
        except Exception:
            msg = err[:300]
        surface.set(status=f"HTTP {e.code}", status_state="errored", http=f"{e.code}", response=f"**error:** {msg}", frames=err[:800])
    except Exception as e:  # noqa: BLE001
        surface.set(status="failed", status_state="errored", response=f"**error:** {e}")
    surface.flush()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=11435)
    ap.add_argument("--model", default="opencode/openai/gpt-6.1-sol#medium")
    ap.add_argument("--no-server", action="store_true", help="assume locum is already running")
    args = ap.parse_args()

    if not SESSION:
        raise SystemExit("run this from inside a tuios pane")
    base = f"http://127.0.0.1:{args.port}/v1"

    server_pane = None
    if not args.no_server and not healthy(base):
        locum = find_locum()
        if not locum:
            raise SystemExit("locum binary not found; run `cargo build --release` first")
        out = tuios(
            "new-window", "-s", SESSION, "locum-serve", "--no-focus", "--keep-open", "--",
            "env", "LOCUM_LOG=info", locum, "serve", "--bind", f"127.0.0.1:{args.port}",
        )
        server_pane = out.split()[0] if out else None
        deadline = time.time() + 20
        while time.time() < deadline and not healthy(base):
            time.sleep(0.2)

    with open(TML) as f:
        doc = f.read().replace("http://127.0.0.1:11435/v1", base).replace(
            "opencode/openai/gpt-6.1-sol#medium", args.model
        )
    tml_path = os.path.join(HERE, ".locum_demo.rendered.tml")
    with open(tml_path, "w") as f:
        f.write(doc)

    proc = subprocess.Popen(
        ["tuios", "tml", "show", tml_path, "--pane", "--events"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1,
    )
    surface = Surface(proc)
    ok = healthy(base)
    surface.set(
        server="locum up" if ok else "locum unreachable",
        server_state="done" if ok else "errored",
    )

    log = open(os.path.join(HERE, ".events.log"), "a")
    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue
        log.write(line + "\n")
        log.flush()
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        kind = ev.get("type") or ev.get("event")
        if kind in ("tml_opened", "tml-opened") or (surface.id is None and ev.get("surface")):
            surface.id = ev.get("surface") or ev.get("id") or ev.get("surface_id")
            surface.flush()
            continue
        tml = ev.get("tml") or {}
        name = tml.get("name") or ev.get("name")
        value = tml.get("value", ev.get("value"))
        if name in ("send", "send_nostream"):
            prompt = value if isinstance(value, str) and value.strip() else "Say hello."
            threading.Thread(
                target=run_request, args=(surface, base, args.model, prompt, name == "send"), daemon=True
            ).start()
        elif name == "clear":
            surface.set(response="", frames="", status="idle", status_state="idle", chunks=0,
                        ttfb_ms="–", duration_ms="–", request_json="{ }")
        elif name == "close":
            break

    proc.wait(timeout=5)
    if server_pane:
        subprocess.run(["tuios", "send-keys", "-s", SESSION, "-w", server_pane, "ctrl+c"], capture_output=True)


if __name__ == "__main__":
    main()
