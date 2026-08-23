//! Starting, health-checking and stopping a `llama-server` process.
//!
//! This is the half of the adapter that owns a child process. It exists so that the
//! *daemon* never does: `otwono-aid` supervises one adapter (`otwono-ai::supervisor`), the
//! adapter supervises one engine, and each layer only has to understand the one below it.
//!
//! # The engine deliberately does *not* get its own process group
//!
//! The supervisor kills the adapter's whole process group, precisely because "a backend is
//! realistically a wrapper script around an engine, and killing only the wrapper leaves
//! the engine running". For that to reach `llama-server`, the engine has to stay *in* the
//! adapter's group — so it is spawned without `process_group(0)` and inherits it.
//!
//! Giving the engine its own group looks tidier and is exactly wrong: it puts the engine
//! outside the group the supervisor signals, and a SIGKILLed adapter then leaves a
//! `llama-server` holding the whole model in memory with nothing left to stop it. The
//! adapter's own `Drop` does not save you, because `Drop` does not run on SIGKILL. The
//! end-to-end test asserts the engine dies with the adapter for this reason.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::http;

/// `sun_path` is 108 bytes on Linux, including the terminator. A socket path that is one
/// byte too long fails at `bind` inside the engine with an error that does not name the
/// cause, so we check it ourselves and say what is wrong.
pub const MAX_SOCKET_PATH: usize = 107;

/// How much engine stderr to keep for diagnostics.
const STDERR_TAIL_LINES: usize = 40;

/// How long to wait for the stderr reader to reach EOF once the engine has exited.
///
/// Bounded rather than a plain join: the pipe stays open as long as *any* process holds
/// the write end, so an engine that left a child behind would otherwise hang the error
/// path — inside the code whose whole job is to explain a failure.
const STDERR_DRAIN_WAIT: Duration = Duration::from_secs(2);

/// How often to re-poll `/health` while the engine loads.
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long to let the engine exit on SIGTERM before resorting to SIGKILL: 20 × 50 ms.
const GRACEFUL_STOP_POLLS: u32 = 20;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Path to the `llama-server` binary.
    pub binary: PathBuf,
    /// Directory the engine's Unix socket is created in. Must already exist and should be
    /// mode 0700: the socket is the only thing standing between another local user and
    /// this node's inference engine.
    pub runtime_dir: PathBuf,
    /// How long to wait for the engine to answer `/health` with 200.
    pub startup_timeout: Duration,
    /// Extra arguments appended verbatim. The typed escape hatch ADR-0005 asks for: the
    /// abstraction cannot cover every engine flag and pretending otherwise would make it
    /// useless for tuning.
    pub extra_args: Vec<String>,
}

/// What to load, in terms the adapter's caller already computed.
///
/// `context_tokens` and `sequences` are not suggestions: admission control charged the
/// node's memory budget for exactly these numbers (`otwono-ai::admission`), so the engine
/// has to be started with them and not with its own defaults.
#[derive(Debug, Clone)]
pub struct LoadRequest {
    pub model_path: PathBuf,
    pub context_tokens: u32,
    pub sequences: u32,
    pub threads: Option<u32>,
    /// Layers to offload to an accelerator. `None` leaves the engine's default.
    pub gpu_layers: Option<u32>,
}

/// A running `llama-server` with a model loaded.
pub struct Engine {
    child: Child,
    socket: PathBuf,
    model_path: PathBuf,
    context_tokens: u32,
    sequences: u32,
    stderr: Arc<Mutex<Vec<String>>>,
    /// Set by the stderr reader when it reaches EOF.
    ///
    /// Needed because a dead child and a fully-read pipe are different events. An engine
    /// that fails to load says why and exits immediately, so `try_wait` reports the exit
    /// while its last words are still in the pipe — the diagnosis is lost precisely in the
    /// case it was collected for.
    stderr_done: Arc<AtomicBool>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("pid", &self.child.id())
            .field("model", &self.model_path)
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Spawn the engine and wait until it reports healthy.
    pub fn start(config: &EngineConfig, request: &LoadRequest) -> Result<Engine, EngineError> {
        if !config.binary.exists() {
            return Err(EngineError::MissingBinary {
                path: config.binary.clone(),
            });
        }
        if !request.model_path.exists() {
            return Err(EngineError::MissingModel {
                path: request.model_path.clone(),
            });
        }

        // One socket per process, so a stale file from a killed engine can never be
        // mistaken for a live one.
        let socket = config
            .runtime_dir
            .join(format!("llama-{}.sock", std::process::id()));
        let socket_len = socket.as_os_str().len();
        if socket_len > MAX_SOCKET_PATH {
            return Err(EngineError::SocketPathTooLong {
                path: socket,
                len: socket_len,
            });
        }
        // A leftover socket file makes bind fail; removing it is safe because the name
        // contains our own pid.
        let _ = std::fs::remove_file(&socket);

        let mut command = Command::new(&config.binary);
        command
            .arg("--model")
            .arg(&request.model_path)
            // `--host` ending in .sock is how llama-server is told to bind a Unix socket.
            .arg("--host")
            .arg(&socket)
            .arg("--ctx-size")
            .arg(request.context_tokens.to_string())
            .arg("--parallel")
            .arg(request.sequences.to_string())
            // No browser UI in a system service. It is a second attack surface for a
            // feature nothing here uses.
            .arg("--no-webui");
        if let Some(threads) = request.threads {
            command.arg("--threads").arg(threads.to_string());
        }
        if let Some(layers) = request.gpu_layers {
            command.arg("--n-gpu-layers").arg(layers.to_string());
        }
        command.args(&config.extra_args);

        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| EngineError::Spawn {
                binary: config.binary.clone(),
                reason: e.to_string(),
            })?;

        // Drain both streams. This is not optional: llama-server is chatty on stderr, and
        // a pipe nobody reads fills up and blocks the writer — the engine would hang
        // partway through loading and look like a timeout.
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_done = Arc::new(AtomicBool::new(true));
        if let Some(pipe) = child.stderr.take() {
            stderr_done.store(false, Ordering::Release);
            drain(pipe, Some(Arc::clone(&stderr)), Some(Arc::clone(&stderr_done)));
        }
        if let Some(pipe) = child.stdout.take() {
            drain(pipe, None, None);
        }

        let engine = Engine {
            child,
            socket,
            model_path: request.model_path.clone(),
            context_tokens: request.context_tokens,
            sequences: request.sequences,
            stderr,
            stderr_done,
        };
        engine.wait_until_healthy(config.startup_timeout)
    }

    fn wait_until_healthy(mut self, timeout: Duration) -> Result<Engine, EngineError> {
        let deadline = Instant::now() + timeout;
        loop {
            // Check the child first. An engine that died — bad model, no memory, missing
            // shared library — must be reported as a crash with its stderr, not as a
            // timeout after the full budget has elapsed.
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    return Err(EngineError::Died {
                        status: status.code(),
                        stderr: self.stderr_tail(),
                    })
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(EngineError::Spawn {
                        binary: self.model_path.clone(),
                        reason: format!("cannot poll the engine: {e}"),
                    })
                }
            }

            match http::request(&self.socket, "GET", "/health", None, Duration::from_secs(5)) {
                // 200 and only 200. While the model loads the engine answers 503 with a
                // JSON body, and a check that accepted "the socket connected" would call
                // that ready and then fail the first inference.
                Ok(r) if r.is_success() => return Ok(self),
                Ok(_) | Err(_) => {}
            }

            if Instant::now() >= deadline {
                let tail = self.stderr_tail();
                self.terminate();
                return Err(EngineError::StartupTimeout {
                    waited: timeout,
                    stderr: tail,
                });
            }
            std::thread::sleep(HEALTH_POLL_INTERVAL);
        }
    }

    /// Send a JSON request to one of the engine's endpoints.
    pub fn post(
        &mut self,
        path: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, EngineError> {
        let encoded = serde_json::to_vec(body).map_err(|e| EngineError::Protocol(e.to_string()))?;
        let response = match http::request(&self.socket, "POST", path, Some(&encoded), timeout) {
            Ok(r) => r,
            Err(e) => {
                // A dead engine looks like a connection failure. Say which it is: "engine
                // crashed, here is its stderr" is actionable and "connection refused" is
                // not.
                if let Ok(Some(status)) = self.child.try_wait() {
                    return Err(EngineError::Died {
                        status: status.code(),
                        stderr: self.stderr_tail(),
                    });
                }
                return Err(EngineError::Http(e.to_string()));
            }
        };

        let value: serde_json::Value = serde_json::from_slice(&response.body).map_err(|e| {
            EngineError::Protocol(format!(
                "engine returned {} with a body that is not JSON: {e}",
                response.status
            ))
        })?;
        if !response.is_success() {
            // The engine's own error text is the useful part; keep it rather than
            // flattening every failure into a status code.
            let message = value
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or_else(|| value.as_str().unwrap_or("no message"));
            return Err(EngineError::Refused {
                status: response.status,
                message: message.to_string(),
            });
        }
        Ok(value)
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    pub fn sequences(&self) -> u32 {
        self.sequences
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The last few lines the engine wrote to stderr.
    ///
    /// Worth keeping: an engine that fails to load a model says why here, and discarding
    /// it turns a diagnosable problem into "exit code 1".
    ///
    /// Waits, briefly, for the reader thread to reach EOF. Without that this races the
    /// very failure it documents — a model that will not load makes the engine exit within
    /// milliseconds, so `try_wait` sees the exit while the explanation is still in the
    /// pipe, and the caller gets an empty string. That is not theoretical: it passed
    /// locally and failed on a loaded CI runner.
    pub fn stderr_tail(&self) -> String {
        let deadline = Instant::now() + STDERR_DRAIN_WAIT;
        while !self.stderr_done.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.stderr
            .lock()
            .map(|lines| lines.join("\n"))
            .unwrap_or_default()
    }

    fn terminate(&mut self) {
        // SIGTERM first: llama-server closes its socket and frees the model on it, and a
        // straight SIGKILL leaves the socket file behind on every unload.
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::Term);
        }
        for _ in 0..GRACEFUL_STOP_POLLS {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Then insist. `kill` is a no-op on a process that has already exited.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Read a child pipe to EOF, optionally keeping the tail.
///
/// Always reads, even when the output is discarded, because an unread pipe eventually
/// blocks the writer.
fn drain<R: std::io::Read + Send + 'static>(
    pipe: R,
    keep: Option<Arc<Mutex<Vec<String>>>>,
    done: Option<Arc<AtomicBool>>,
) {
    std::thread::spawn(move || {
        // Set on every exit path, including a read error. A flag that is only set on the
        // happy path is a two-second stall on the unhappy one.
        let _guard = done.map(FinishedOnDrop);
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { return };
            if let Some(buffer) = &keep {
                let Ok(mut buffer) = buffer.lock() else { return };
                if buffer.len() == STDERR_TAIL_LINES {
                    buffer.remove(0);
                }
                buffer.push(line);
            }
        }
    });
}

/// Marks the drain as finished however the reader thread leaves, panic included.
struct FinishedOnDrop(Arc<AtomicBool>);

impl Drop for FinishedOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
pub enum EngineError {
    MissingBinary {
        path: PathBuf,
    },
    MissingModel {
        path: PathBuf,
    },
    SocketPathTooLong {
        path: PathBuf,
        len: usize,
    },
    Spawn {
        binary: PathBuf,
        reason: String,
    },
    /// The engine exited. `status` is `None` when a signal killed it.
    Died {
        status: Option<i32>,
        stderr: String,
    },
    StartupTimeout {
        waited: Duration,
        stderr: String,
    },
    /// The engine answered, with a refusal.
    Refused {
        status: u16,
        message: String,
    },
    Http(String),
    Protocol(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::MissingBinary { path } => {
                write!(f, "no llama.cpp engine at {}", path.display())
            }
            EngineError::MissingModel { path } => write!(f, "no model file at {}", path.display()),
            EngineError::SocketPathTooLong { path, len } => write!(
                f,
                "engine socket path is {len} bytes and the kernel allows {MAX_SOCKET_PATH}: {}",
                path.display()
            ),
            EngineError::Spawn { binary, reason } => {
                write!(f, "cannot start {}: {reason}", binary.display())
            }
            EngineError::Died { status, stderr } => {
                match status {
                    Some(code) => write!(f, "the engine exited with status {code}")?,
                    None => write!(f, "the engine was killed by a signal")?,
                }
                if !stderr.is_empty() {
                    write!(f, "; stderr: {stderr}")?;
                }
                Ok(())
            }
            EngineError::StartupTimeout { waited, stderr } => {
                write!(f, "the engine did not become healthy within {waited:?}")?;
                if !stderr.is_empty() {
                    write!(f, "; stderr: {stderr}")?;
                }
                Ok(())
            }
            EngineError::Refused { status, message } => {
                write!(f, "the engine refused the request ({status}): {message}")
            }
            EngineError::Http(e) => write!(f, "engine transport error: {e}"),
            EngineError::Protocol(e) => write!(f, "engine protocol error: {e}"),
        }
    }
}

impl std::error::Error for EngineError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path, binary: &str) -> EngineConfig {
        EngineConfig {
            binary: PathBuf::from(binary),
            runtime_dir: dir.to_path_buf(),
            startup_timeout: Duration::from_secs(2),
            extra_args: Vec::new(),
        }
    }

    fn load(model: &Path) -> LoadRequest {
        LoadRequest {
            model_path: model.to_path_buf(),
            context_tokens: 512,
            sequences: 1,
            threads: Some(1),
            gpu_layers: None,
        }
    }

    /// A temp dir that cleans up. Not worth a dependency for.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let path = std::env::temp_dir().join(format!("otwono-llama-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_engine_binary_is_named_not_guessed() {
        let dir = TempDir::new("nobin");
        let model = dir.0.join("m.gguf");
        std::fs::write(&model, b"x").unwrap();
        let err = Engine::start(&config(&dir.0, "/nonexistent/llama-server"), &load(&model)).unwrap_err();
        assert!(
            matches!(&err, EngineError::MissingBinary { path } if path.ends_with("llama-server")),
            "{err:?}"
        );
        // The message has to carry the path, or an operator cannot fix the install.
        assert!(err.to_string().contains("/nonexistent/llama-server"), "{err}");
    }

    #[test]
    fn a_missing_model_is_reported_before_anything_is_spawned() {
        let dir = TempDir::new("nomodel");
        let err = Engine::start(&config(&dir.0, "/bin/sh"), &load(&dir.0.join("absent.gguf"))).unwrap_err();
        assert!(matches!(err, EngineError::MissingModel { .. }), "{err:?}");
    }

    #[test]
    fn an_over_long_socket_path_is_refused_with_the_reason() {
        // sun_path is 108 bytes. Without this check the failure surfaces from inside the
        // engine as a bind error that does not mention path length.
        let dir = TempDir::new(&"d".repeat(120));
        let model = dir.0.join("m.gguf");
        std::fs::write(&model, b"x").unwrap();
        let err = Engine::start(&config(&dir.0, "/bin/sh"), &load(&model)).unwrap_err();
        assert!(matches!(err, EngineError::SocketPathTooLong { .. }), "{err:?}");
        assert!(err.to_string().contains("kernel allows"), "{err}");
    }

    #[test]
    fn an_engine_that_exits_immediately_is_a_crash_with_its_stderr() {
        // What a missing shared library or an unreadable model looks like.
        let dir = TempDir::new("dies");
        let model = dir.0.join("m.gguf");
        std::fs::write(&model, b"x").unwrap();
        let fake = dir.0.join("fake-server");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'error loading model: bad magic' >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let err = Engine::start(&config(&dir.0, fake.to_str().unwrap()), &load(&model)).unwrap_err();
        let EngineError::Died { status, stderr } = &err else {
            panic!("expected Died, got {err:?}");
        };
        assert_eq!(*status, Some(3));
        // The engine's own diagnosis is the whole value of capturing stderr.
        assert!(stderr.contains("bad magic"), "{stderr:?}");
    }

    #[test]
    fn a_fast_failing_engines_last_words_are_not_lost_to_the_race() {
        // The regression this exists for: the engine writes a lot and exits at once, so
        // `try_wait` reports the exit long before the reader has drained the pipe. The
        // earlier version returned an empty string here, and did it only under load --
        // it passed locally and failed on a CI runner.
        let dir = TempDir::new("race");
        let model = dir.0.join("m.gguf");
        std::fs::write(&model, b"x").unwrap();
        let fake = dir.0.join("fake-server");
        std::fs::write(
            &fake,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 200 ]; do echo \"line $i\" >&2; i=$((i+1)); done\n\
             echo 'error loading model: the last word' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let err = Engine::start(&config(&dir.0, fake.to_str().unwrap()), &load(&model)).unwrap_err();
        let EngineError::Died { stderr, .. } = &err else {
            panic!("expected Died, got {err:?}");
        };
        // The *last* line specifically: anything less means the tail was read early.
        assert!(stderr.contains("the last word"), "{stderr:?}");
    }

    #[test]
    fn an_engine_that_never_becomes_healthy_times_out_and_is_killed() {
        let dir = TempDir::new("hangs");
        let model = dir.0.join("m.gguf");
        std::fs::write(&model, b"x").unwrap();
        let fake = dir.0.join("fake-server");
        // Starts, says nothing useful, never binds a socket.
        std::fs::write(&fake, "#!/bin/sh\necho 'still loading' >&2\nsleep 600\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        let err = Engine::start(&config(&dir.0, fake.to_str().unwrap()), &load(&model)).unwrap_err();
        let EngineError::StartupTimeout { stderr, .. } = &err else {
            panic!("expected StartupTimeout, got {err:?}");
        };
        assert!(stderr.contains("still loading"), "{stderr:?}");
    }
}
