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
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::protocol::{Envelope, MAX_FRAME_BYTES, ProtocolError};

/// Environment variables the helper is allowed to see. Anything else is
/// cleared before launch.
const ALLOWED_ENV: [&str; 3] = ["PATH", "LANG", "LC_ALL"];

/// Windows-only OS variables a Node.js runtime needs and that carry no
/// credentials: where Windows lives (`SystemRoot`/`windir`), how to resolve an
/// executable (`PATHEXT`), the command interpreter (`COMSPEC`), and basic CPU
/// topology. The helper still gets no `USERPROFILE`/`APPDATA`, so user config
/// stays out of its environment.
#[cfg(windows)]
const WINDOWS_OS_ENV: [&str; 6] = [
    "SystemRoot",
    "windir",
    "PATHEXT",
    "COMSPEC",
    "PROCESSOR_ARCHITECTURE",
    "NUMBER_OF_PROCESSORS",
];

/// Minimum Node.js version required by the bundled Pi helper and its SDK
/// (`standalone/pi-helper/package.json` -> `engines.node`).
pub const REQUIRED_NODE_VERSION: (u64, u64, u64) = (22, 19, 0);
pub const REQUIRED_NODE_VERSION_STR: &str = "22.19.0";

/// Why the configured helper runtime cannot run the helper.
///
/// These messages are shown to the user, so they name the executable and the
/// action that fixes the problem instead of dumping a Node crash.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RuntimeCheckError {
    #[error(
        "the helper runtime `{executable}` was not found. warpi runs the standalone backend on \
         Node.js >= {required}: it prefers the Node runtime bundled next to the executable and \
         otherwise resolves `{executable}` on PATH. Install an official Node.js {required}+ build \
         (nodejs.org), reinstall warpi to restore its bundled runtime, or set `helper_executable` \
         in the standalone config to an existing node binary"
    )]
    NotFound {
        executable: String,
        required: &'static str,
    },
    #[error(
        "the helper runtime `{executable}` is Node.js {found}, but this build needs Node.js >= \
         {required}. Install an official Node.js {required} (LTS) or newer, reinstall warpi to \
         restore its bundled runtime, or set `helper_executable` in the standalone config"
    )]
    Version {
        executable: String,
        found: String,
        required: &'static str,
    },
    #[error(
        "the helper runtime `{path}` failed to start (exit code {code:?}). warpi needs a working \
         Node.js >= {required}; this is a Node/runtime failure, not a warpi failure. Install an \
         official Node.js LTS build from nodejs.org, reinstall warpi to restore its bundled \
         runtime, or set `helper_executable` in the standalone config to a known-good node \
         binary. Runtime stderr:\n{stderr}"
    )]
    Unusable {
        path: String,
        code: Option<i32>,
        required: &'static str,
        stderr: String,
    },
}

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

/// Apply the environment a helper child is allowed to see: a strict allowlist,
/// the fork-private home/scratch directories, and (on Windows) the OS variables
/// a Node runtime needs to start. Never credentials.
fn apply_sandbox_env(command: &mut Command, config: &HelperLaunchConfig) {
    command.env_clear();
    for (key, value) in &config.extra_env {
        command.env(key, value);
    }
    for key in ALLOWED_ENV {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    #[cfg(windows)]
    for key in WINDOWS_OS_ENV {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    command
        .env("HOME", &config.data_dir)
        .env("TMPDIR", &config.scratch_dir)
        // Node reads TEMP/TMP on Windows, not TMPDIR, so point both at the
        // fork-private scratch dir there too.
        .env("TEMP", &config.scratch_dir)
        .env("TMP", &config.scratch_dir)
        .env("WARPI_PI_SCRATCH_DIR", &config.scratch_dir)
        .env("PI_OFFLINE", "1")
        .env("NO_COLOR", "1");
}

/// Resolve the helper executable the way `CreateProcess`/`execvp` will: an
/// explicit path is used as-is, a bare name is searched on `PATH` (honouring
/// `PATHEXT` on Windows). Returns the first existing candidate.
pub fn resolve_helper_executable(executable: &Path) -> Option<PathBuf> {
    resolve_helper_executable_with_path(executable, std::env::var_os("PATH").as_deref())
}

/// [`resolve_helper_executable`] with an injected `PATH`, so tests do not
/// mutate the process environment.
pub fn resolve_helper_executable_with_path(
    executable: &Path,
    path_env: Option<&OsStr>,
) -> Option<PathBuf> {
    if executable.as_os_str().is_empty() {
        return None;
    }
    let explicit = executable.is_absolute() || executable.components().count() > 1;
    if explicit {
        return executable.is_file().then(|| executable.to_path_buf());
    }
    let Some(path_env) = path_env else {
        return executable.is_file().then(|| executable.to_path_buf());
    };
    for dir in std::env::split_paths(path_env) {
        let candidate = dir.join(executable);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        if executable.extension().is_none() {
            let pathext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            for ext in pathext.split(';').filter(|ext| !ext.is_empty()) {
                let candidate = dir.join(format!(
                    "{}{}",
                    executable.to_string_lossy(),
                    ext.to_ascii_lowercase()
                ));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Parse the first three dot-separated numeric components of a version string,
/// tolerating a leading `v` and a pre-release suffix.
fn parse_node_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches('v');
    let core = text.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Verify the configured helper runtime can actually start Node and run the
/// required version, before the bridge spawns the real helper.
///
/// The probe runs a one-line script rather than `--version`: `node --version`
/// returns before Node initializes OpenSSL, so it succeeds even on a runtime
/// whose startup self-check (`CHECK(ncrypto::CSPRNG(nullptr, 0))`) aborts for
/// every real invocation. A one-line script exercises that same startup path.
pub async fn check_helper_runtime(config: &HelperLaunchConfig) -> Result<(), RuntimeCheckError> {
    // The probe runs with the same working directory as the helper; create the
    // fork-private dirs first, since `HelperProcess::spawn` (which normally
    // does that) has not run yet.
    for dir in [&config.data_dir, &config.scratch_dir] {
        std::fs::create_dir_all(dir).map_err(|error| RuntimeCheckError::Unusable {
            path: config.executable.display().to_string(),
            code: None,
            required: REQUIRED_NODE_VERSION_STR,
            stderr: format!("cannot create {}: {error}", dir.display()),
        })?;
    }
    let Some(path) = resolve_helper_executable(&config.executable) else {
        return Err(RuntimeCheckError::NotFound {
            executable: config.executable.display().to_string(),
            required: REQUIRED_NODE_VERSION_STR,
        });
    };
    let path_string = path.display().to_string();
    let mut command = Command::new(&path);
    command
        .args(["-e", "process.stdout.write(process.versions.node)"])
        .current_dir(&config.working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_sandbox_env(&mut command, config);
    let child = command
        .spawn()
        .map_err(|error| RuntimeCheckError::Unusable {
            path: path_string.clone(),
            code: None,
            required: REQUIRED_NODE_VERSION_STR,
            stderr: error.to_string(),
        })?;
    let output = match tokio::time::timeout(RUNTIME_PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(RuntimeCheckError::Unusable {
                path: path_string,
                code: None,
                required: REQUIRED_NODE_VERSION_STR,
                stderr: error.to_string(),
            });
        }
        Err(_) => {
            return Err(RuntimeCheckError::Unusable {
                path: path_string,
                code: None,
                required: REQUIRED_NODE_VERSION_STR,
                stderr: format!(
                    "the runtime did not answer within {}s",
                    RUNTIME_PROBE_TIMEOUT.as_secs()
                ),
            });
        }
    };
    if !output.status.success() {
        return Err(RuntimeCheckError::Unusable {
            path: path_string,
            code: output.status.code(),
            required: REQUIRED_NODE_VERSION_STR,
            stderr: bounded_probe_stderr(&output.stderr),
        });
    }
    let version = String::from_utf8_lossy(&output.stdout);
    match parse_node_version(&version) {
        Some(found) if found < REQUIRED_NODE_VERSION => Err(RuntimeCheckError::Version {
            executable: path_string,
            found: version.trim().to_string(),
            required: REQUIRED_NODE_VERSION_STR,
        }),
        // A successful start with an unrecognized version string is accepted:
        // the point of this probe is liveness, and the helper reports its exact
        // Node version in the `hello` handshake.
        _ => Ok(()),
    }
}

const RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Keep the tail of a failed probe's stderr for the user-visible error. It is
/// Node's own startup output, not helper traffic, so it does not need the
/// prompt-scrubbing the live stderr tail gets.
fn bounded_probe_stderr(stderr: &[u8]) -> String {
    const MAX_PROBE_STDERR_BYTES: usize = 4096;
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.len() <= MAX_PROBE_STDERR_BYTES {
        return text.to_string();
    }
    let mut end = MAX_PROBE_STDERR_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        apply_sandbox_env(&mut command, &config);
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
/// 1. explicit `WARPI_PI_HELPER_ENTRY` override (development/testing only);
/// 2. `standalone/pi-helper/dist/main.js` next to the executable;
/// 3. repository-relative path for `cargo run` development builds.
pub fn default_helper_entry() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("WARPI_PI_HELPER_ENTRY")
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

/// Path of the bundled Node runtime inside `<exe_dir>/standalone/node`, per
/// platform. Windows ships `node.exe` directly; Unix distributions ship the
/// upstream layout at `bin/node`.
#[cfg(windows)]
const BUNDLED_NODE_RELATIVE: &[&str] = &["node.exe"];
#[cfg(not(windows))]
const BUNDLED_NODE_RELATIVE: &[&str] = &["bin/node", "node"];

/// First existing Node executable inside a bundled runtime directory.
fn bundled_runtime_in(node_dir: &Path) -> Option<PathBuf> {
    BUNDLED_NODE_RELATIVE
        .iter()
        .map(|relative| node_dir.join(relative))
        .find(|candidate| candidate.is_file())
}

/// Locate the Node runtime the installer placed next to the executable.
///
/// Resolution order for the helper executable (documented in
/// `standalone/ARCHITECTURE.md`):
/// 1. an explicit `helper_executable` in the standalone config (the caller
///    applies this before falling back here);
/// 2. the runtime bundled by the installer at
///    `<exe_dir>/standalone/node/node.exe` on Windows (or
///    `<exe_dir>/standalone/node/bin/node` on Unix), searching a few ancestor
///    directories so `cargo run` development builds resolve the checkout copy;
/// 3. the bare name `node`, resolved on `PATH` by the OS (the caller's final
///    fallback).
///
/// Returns `None` when no bundled runtime is present, leaving the decision to
/// the caller so a development tree can still use the system Node.
pub fn default_helper_executable() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent().map(Path::to_path_buf);
    for _ in 0..4 {
        let Some(candidate_dir) = dir else { break };
        if let Some(node) = bundled_runtime_in(&candidate_dir.join("standalone").join("node")) {
            return Some(node);
        }
        dir = candidate_dir.parent().map(Path::to_path_buf);
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
        unsafe { std::env::set_var("WARPI_TEST_SECRET", "leak-me") };
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
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                break;
            }
        }
        unsafe { std::env::remove_var("WARPI_TEST_SECRET") };
        assert!(
            !keys.iter().any(|key| key == "WARPI_TEST_SECRET"),
            "keys: {keys:?}"
        );
        assert!(
            keys.contains(&"HOME".to_string()),
            "HOME must be set to the private dir"
        );
        for key in ["TMPDIR", "TEMP", "TMP"] {
            assert!(
                keys.contains(&key.to_string()),
                "{key} must point the helper's temp files at the private scratch dir"
            );
        }
        helper.shutdown().await;
    }

    fn probe_config(executable: PathBuf, dir: &Path) -> HelperLaunchConfig {
        HelperLaunchConfig {
            executable,
            args: vec!["helper.mjs".to_string()],
            working_dir: dir.to_path_buf(),
            data_dir: dir.to_path_buf(),
            scratch_dir: dir.join("scratch"),
            extra_env: Vec::new(),
            stderr_tail_lines: 10,
            shutdown_timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn parse_node_version_accepts_v_prefix_and_suffix() {
        assert_eq!(parse_node_version("v22.19.0"), Some((22, 19, 0)));
        assert_eq!(parse_node_version("22.19.0"), Some((22, 19, 0)));
        assert_eq!(
            parse_node_version("v23.1.0-nightly20250101"),
            Some((23, 1, 0))
        );
        assert_eq!(parse_node_version("not-a-version"), None);
        assert_eq!(parse_node_version("v22"), Some((22, 0, 0)));
    }

    #[test]
    fn resolve_helper_executable_uses_explicit_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("node");
        std::fs::write(&file, b"").expect("write");
        let resolved = resolve_helper_executable_with_path(&file, None);
        assert_eq!(resolved.as_deref(), Some(file.as_path()));
    }

    #[test]
    fn resolve_helper_executable_reports_missing_explicit_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("node");
        assert_eq!(resolve_helper_executable_with_path(&missing, None), None);
    }

    #[test]
    fn resolve_helper_executable_searches_injected_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let node = bin.join("node");
        std::fs::write(&node, b"").expect("write");
        let path_env = std::env::join_paths([bin.as_path()]).expect("join paths");
        let resolved =
            resolve_helper_executable_with_path(Path::new("node"), Some(path_env.as_os_str()));
        assert_eq!(resolved.as_deref(), Some(node.as_path()));
    }

    #[test]
    fn bundled_runtime_is_found_in_its_install_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node_dir = dir.path().join("standalone").join("node");
        let expected = node_dir.join(BUNDLED_NODE_RELATIVE[0]);
        std::fs::create_dir_all(expected.parent().expect("parent")).expect("mkdir");
        std::fs::write(&expected, b"").expect("write");
        assert_eq!(bundled_runtime_in(&node_dir), Some(expected));
    }

    #[test]
    fn bundled_runtime_is_absent_without_an_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node_dir = dir.path().join("standalone").join("node");
        std::fs::create_dir_all(&node_dir).expect("mkdir");
        assert_eq!(bundled_runtime_in(&node_dir), None);
    }

    #[tokio::test]
    async fn runtime_check_reports_missing_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-node");
        let error = check_helper_runtime(&probe_config(missing, dir.path()))
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeCheckError::NotFound { .. }),
            "{error}"
        );
    }

    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_check_accepts_a_supported_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = dir.path().join("node");
        write_script(&runtime, "#!/bin/sh\nprintf '22.19.0'\n");
        check_helper_runtime(&probe_config(runtime, dir.path()))
            .await
            .expect("supported node passes");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_check_reports_an_unsupported_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = dir.path().join("node");
        write_script(&runtime, "#!/bin/sh\nprintf 'v18.20.4'\n");
        let error = check_helper_runtime(&probe_config(runtime, dir.path()))
            .await
            .unwrap_err();
        match error {
            RuntimeCheckError::Version {
                found, required, ..
            } => {
                assert_eq!(found, "v18.20.4");
                assert_eq!(required, REQUIRED_NODE_VERSION_STR);
            }
            other => panic!("expected Version, got {other}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_check_surfaces_a_crashing_runtime_with_its_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = dir.path().join("node");
        write_script(
            &runtime,
            "#!/bin/sh\necho '  #  Assertion failed: ncrypto::CSPRNG(nullptr, 0)' >&2\nexit 134\n",
        );
        let error = check_helper_runtime(&probe_config(runtime, dir.path()))
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("ncrypto::CSPRNG"),
            "crash stderr must be surfaced: {message}"
        );
        assert!(
            message.contains("official Node.js LTS"),
            "message must be actionable: {message}"
        );
    }
}
