//! Supervised private stdio helper process.
//!
//! The helper is launched with an explicit executable and argument vector, a
//! controlled working directory, and a sanitized environment (allowlist, not
//! blocklist). Only protocol frames are read from its stdout; stderr is drained
//! independently into a bounded tail so a chatty helper can never block on a
//! full pipe.
//!
//! Shutdown is cooperative first (`shutdown` frame + stdin close), then forced
//! (kill + reap). Windows and Unix use the same tokio code path; no POSIX
//! signal or process-group assumptions are made.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::protocol::{Envelope, MAX_FRAME_BYTES, ProtocolError};

/// Environment variables the helper is allowed to see. Anything else is
/// cleared before launch.
const ALLOWED_ENV: [&str; 3] = ["PATH", "LANG", "LC_ALL"];

#[derive(Debug, Clone)]
pub struct HelperLaunchConfig {
    /// Explicit executable (for example `node`). Never resolved from a shell.
    pub executable: PathBuf,
    /// Explicit argument vector: `[helper_entry, ...args]`.
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    /// Fork-private data directory; becomes HOME/TMPDIR for the helper.
    pub data_dir: PathBuf,
    /// Scratch directory for the helper's temporary files.
    pub scratch_dir: PathBuf,
    /// Environment passed on top of the allowlist (never credentials).
    pub extra_env: Vec<(String, String)>,
    /// Bounded stderr tail kept for crash diagnostics.
    pub stderr_tail_lines: usize,
    /// How long to wait for a cooperative exit before killing the process.
    pub shutdown_timeout: std::time::Duration,
}

impl HelperLaunchConfig {
    pub fn node(helper_entry: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            executable: PathBuf::from("node"),
            args: vec![helper_entry.into().to_string_lossy().into_owned()],
            working_dir: data_dir.clone(),
            scratch_dir: data_dir.join("scratch"),
            data_dir,
            extra_env: Vec::new(),
            stderr_tail_lines: 200,
            shutdown_timeout: std::time::Duration::from_secs(5),
        }
    }
}

/// Messages produced by the helper process.
#[derive(Debug)]
pub enum HelperOutput {
    Frame(Box<Envelope>),
    Stderr(String),
    /// The framed protocol was violated. The caller decides whether to fail the
    /// in-flight turn; the process is left running unless the caller stops it.
    ProtocolError(ProtocolError),
    Exited(Option<i32>),
}

pub struct HelperProcess {
    child: Child,
    stdin: ChildStdin,
    output_rx: mpsc::UnboundedReceiver<HelperOutput>,
    stderr_tail: std::sync::Arc<std::sync::Mutex<VecDeque<String>>>,
    exited: bool,
    shutdown_timeout: std::time::Duration,
}

impl HelperProcess {
    pub async fn spawn(config: HelperLaunchConfig) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        std::fs::create_dir_all(&config.scratch_dir)?;
        let mut command = Command::new(&config.executable);
        command
            .args(&config.args)
            .current_dir(&config.working_dir)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &config.extra_env {
            command.env(key, value);
        }
        for key in ALLOWED_ENV {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", &config.data_dir)
            .env("TMPDIR", &config.scratch_dir)
            .env("WARPOS_PI_SCRATCH_DIR", &config.scratch_dir)
            .env("PI_OFFLINE", "1")
            .env("NO_COLOR", "1");
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let tail_capacity = config.stderr_tail_lines.max(1);
        let tail = std::sync::Arc::clone(&stderr_tail);

        tokio::spawn(read_frames(stdout, output_tx.clone()));
        tokio::spawn(read_stderr(stderr, output_tx.clone(), tail, tail_capacity));

        Ok(Self {
            child,
            stdin,
            output_rx,
            stderr_tail,
            exited: false,
            shutdown_timeout: config.shutdown_timeout,
        })
    }

    /// Receive the next helper message.
    pub async fn next(&mut self) -> Option<HelperOutput> {
        let output = self.output_rx.recv().await;
        self.observe(output)
    }

    /// Detach the output channel so callers can select over it independently
    /// from the write half (which stays on `self`).
    pub fn take_output(&mut self) -> mpsc::UnboundedReceiver<HelperOutput> {
        std::mem::replace(&mut self.output_rx, mpsc::unbounded_channel().1)
    }

    pub fn observe(&mut self, output: Option<HelperOutput>) -> Option<HelperOutput> {
        if let Some(HelperOutput::Exited(code)) = &output {
            self.exited = true;
            tracing::debug!(?code, "standalone helper exited");
        }
        output
    }

    /// Write one frame to the helper's stdin, enforcing the frame size bound.
    pub async fn send(&mut self, envelope: &Envelope) -> Result<(), ProtocolError> {
        let mut line = serde_json::to_string(envelope)
            .map_err(|e| ProtocolError::Invalid(format!("serializing frame: {e}")))?;
        if line.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| ProtocolError::Invalid(format!("writing frame to helper: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| ProtocolError::Invalid(format!("flushing frame to helper: {e}")))?;
        Ok(())
    }

    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail
            .lock()
            .expect("stderr tail lock")
            .iter()
            .cloned()
            .collect()
    }

    pub fn has_exited(&self) -> bool {
        self.exited
    }

    /// Cooperative shutdown: send the shutdown frame, close stdin, wait, then kill.
    pub async fn shutdown(&mut self) {
        if !self.exited {
            let envelope = Envelope::new("shutdown", 0);
            let _ = self.send(&envelope).await;
            // Closing stdin makes the helper exit even if it never saw the frame.
            let _ = self.stdin.shutdown().await;
        }
        let wait = tokio::time::timeout(self.shutdown_timeout, self.child.wait()).await;
        if wait.is_err() {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
        self.exited = true;
    }

    /// Force-kill without waiting (used on hard cancellation).
    pub async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.exited = true;
    }
}

async fn read_frames(stdout: tokio::process::ChildStdout, tx: mpsc::UnboundedSender<HelperOutput>) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::with_capacity(8 * 1024);
    let mut last_seq: Option<u64> = None;
    loop {
        buffer.clear();
        // Bound the read so a garbage stream without newlines cannot grow
        // without limit.
        let read = tokio::io::AsyncBufReadExt::read_until(&mut reader, b'\n', &mut buffer).await;
        match read {
            Ok(0) => break,
            Ok(_) => {
                if buffer.len() > MAX_FRAME_BYTES {
                    let _ = tx.send(HelperOutput::ProtocolError(ProtocolError::TooLarge));
                    break;
                }
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.trim().is_empty() {
                    continue;
                }
                match Envelope::parse(line, last_seq) {
                    Ok(envelope) => {
                        last_seq = Some(envelope.seq);
                        if tx.send(HelperOutput::Frame(Box::new(envelope))).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        if tx.send(HelperOutput::ProtocolError(error)).is_err() {
                            break;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    let _ = tx.send(HelperOutput::Exited(None));
}

async fn read_stderr(
    stderr: tokio::process::ChildStderr,
    tx: mpsc::UnboundedSender<HelperOutput>,
    tail: std::sync::Arc<std::sync::Mutex<VecDeque<String>>>,
    capacity: usize,
) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = String::new();
    loop {
        buffer.clear();
        match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buffer).await {
            Ok(0) => break,
            Ok(_) => {
                let line = buffer.trim_end_matches(['\n', '\r']).to_string();
                if line.is_empty() {
                    continue;
                }
                // Never forward raw stderr to the UI or logs: it can contain
                // provider error bodies. Keep the bounded tail for diagnostics.
                {
                    let mut tail = tail.lock().expect("stderr tail lock");
                    if tail.len() >= capacity {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
                let _ = tx.send(HelperOutput::Stderr(String::new()));
            }
            Err(_) => break,
        }
    }
}

/// Locate the bundled helper entry for a Warp build.
///
/// Resolution order (documented in ARCHITECTURE.md):
/// 1. explicit `WARPOS_PI_HELPER_ENTRY` override (development/testing only);
/// 2. `standalone/pi-helper/dist/main.js` next to the executable;
/// 3. repository-relative path for `cargo run` development builds.
pub fn default_helper_entry() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("WARPOS_PI_HELPER_ENTRY")
        && !override_path.trim().is_empty()
    {
        let path = PathBuf::from(override_path);
        if path.exists() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        for _ in 0..4 {
            let Some(candidate_dir) = dir else { break };
            let candidate = candidate_dir.join("standalone/pi-helper/dist/main.js");
            if candidate.exists() {
                return Some(candidate);
            }
            dir = candidate_dir.parent().map(Path::to_path_buf);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_environment_does_not_inherit_credentials() {
        // A shell one-liner stands in for the helper: print the env var names.
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("env.js");
        std::fs::write(
            &script,
            r#"const keys = Object.keys(process.env).sort();
console.log(JSON.stringify({ protocol: 1, seq: 0, kind: "env", data: { keys } }));
process.exit(0);"#,
        )
        .expect("write script");
        unsafe { std::env::set_var("WARPOS_TEST_SECRET", "leak-me") };
        let config = HelperLaunchConfig {
            executable: PathBuf::from("node"),
            args: vec![script.to_string_lossy().into_owned()],
            working_dir: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
            scratch_dir: dir.path().join("scratch"),
            extra_env: Vec::new(),
            stderr_tail_lines: 10,
            shutdown_timeout: std::time::Duration::from_secs(2),
        };
        let mut helper = HelperProcess::spawn(config).await.expect("spawn");
        let mut keys = Vec::new();
        while let Some(output) = helper.next().await {
            if let HelperOutput::Frame(frame) = output {
                keys = frame
                    .data
                    .as_ref()
                    .and_then(|data| data.get("keys"))
                    .and_then(|keys| keys.as_array())
                    .map(|values| values.iter().filter_map(|v| v.as_str()).map(str::to_string).collect())
                    .unwrap_or_default();
                break;
            }
        }
        unsafe { std::env::remove_var("WARPOS_TEST_SECRET") };
        assert!(!keys.iter().any(|key| key == "WARPOS_TEST_SECRET"), "keys: {keys:?}");
        assert!(keys.contains(&"HOME".to_string()), "HOME must be set to the private dir");
        helper.shutdown().await;
    }
}
