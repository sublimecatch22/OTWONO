//! Running a backend as a supervised child process.
//!
//! `docs/ai/AI-RUNTIME.md` §3: "Backends run out of process and are supervised. A backend
//! crash is a typed error, never a hang and never a daemon restart."
//!
//! Both halves of that sentence are load-bearing:
//!
//! * **Never a hang.** Every read has a deadline. An inference engine that wedges must not
//!   wedge `otwono-aid` with it, because a stuck AI daemon is a stuck control plane for
//!   everything that asks it a question.
//! * **Never a daemon restart.** A backend that dies is a typed error the caller sees, not
//!   a supervisor decision to bounce the parent. If loading a particular model reliably
//!   segfaults, the node must keep answering `ai.capabilities` and keep saying *which*
//!   model does it.
//!
//! # Why this exists before any engine does
//!
//! Crash, hang, and garbage-output paths are the ones that matter and the hardest to
//! provoke on demand once a real engine is in the loop, where every failure looks like a
//! bug in llama.cpp. Against a fake backend that misbehaves to order, they are ordinary
//! tests — see the ones at the bottom of this file, which run a shell script as the
//! "backend" and make it die, hang, and lie.
//!
//! # The protocol
//!
//! Newline-delimited JSON on stdin/stdout, the same shape as the Local Control Plane
//! (ADR-0003), so there is one framing to reason about rather than two. The first exchange
//! is a `hello`, which is how the supervisor learns it is talking to something that speaks
//! this protocol at all rather than, say, a shell error message.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long to wait for a backend to answer `hello`.
pub const DEFAULT_HELLO_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest line the supervisor will accept from a backend.
///
/// A backend is less trusted than the daemon running it: it is a large C++ program parsing
/// model files. An unbounded read would let a confused one exhaust the daemon's memory.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// What a backend says about itself when it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendHello {
    /// Protocol version the backend implements.
    pub protocol: u32,
    /// Engine name and version, for `ai.capabilities` and for bug reports.
    pub engine: String,
    pub version: String,
}

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// A running backend.
///
/// Dropping this kills the child. A supervisor that leaked processes would leave a node
/// slowly filling with orphaned inference engines holding gigabytes each.
pub struct BackendProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Lines the reader thread has produced.
    ///
    /// One long-lived thread, not one per read. A per-read thread has to be joined, and
    /// joining a thread blocked on a pipe that will never close is itself a hang — which
    /// is exactly what an earlier version of this file did: killing the wrapper left its
    /// grandchildren holding the pipe open, and the supervisor waited for ever inside the
    /// code meant to prevent waiting for ever.
    lines: mpsc::Receiver<Result<String, ReadFault>>,
    hello: BackendHello,
    label: String,
}

/// Why the reader thread stopped producing lines.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadFault {
    /// The backend closed stdout. It is exiting or has exited.
    Eof,
    /// A single line exceeded [`MAX_LINE_BYTES`].
    LineTooLong(String),
    Io(String),
}

impl std::fmt::Debug for BackendProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendProcess")
            .field("label", &self.label)
            .field("engine", &self.hello.engine)
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl BackendProcess {
    /// Spawn `command` and complete the `hello` exchange.
    ///
    /// Fails rather than returning a half-usable handle: a backend that cannot say hello
    /// is not a backend, and discovering that at first inference instead of at load time
    /// is how a user ends up with an unexplained timeout.
    pub fn spawn(
        label: impl Into<String>,
        command: &mut Command,
        timeout: Duration,
    ) -> Result<Self, BackendError> {
        let label = label.into();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so the whole backend subtree can be killed together.
            // Without this, terminating a wrapper script orphans the engine it started.
            .process_group(0)
            .spawn()
            .map_err(|e| BackendError::Spawn {
                label: label.clone(),
                reason: e.to_string(),
            })?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let mut process = BackendProcess {
            child,
            stdin: Some(stdin),
            lines: spawn_reader(stdout),
            hello: BackendHello {
                protocol: 0,
                engine: String::new(),
                version: String::new(),
            },
            label: label.clone(),
        };

        let line = process.read_line(timeout)?;
        let hello: BackendHello = serde_json::from_str(&line).map_err(|e| BackendError::Protocol {
            label: label.clone(),
            reason: format!("first line was not a hello: {e}"),
            saw: truncate(&line),
        })?;
        if hello.protocol != PROTOCOL_VERSION {
            return Err(BackendError::Protocol {
                label,
                reason: format!(
                    "backend speaks protocol {} and this build speaks {PROTOCOL_VERSION}",
                    hello.protocol
                ),
                saw: truncate(&line),
            });
        }
        process.hello = hello;
        Ok(process)
    }

    pub fn hello(&self) -> &BackendHello {
        &self.hello
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Send one request and read one response, within `timeout`.
    pub fn request(
        &mut self,
        value: &serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, BackendError> {
        let mut line = serde_json::to_string(value).map_err(|e| BackendError::Protocol {
            label: self.label.clone(),
            reason: format!("cannot serialise the request: {e}"),
            saw: String::new(),
        })?;
        line.push('\n');

        let stdin = self.stdin.as_mut().ok_or_else(|| BackendError::Closed {
            label: self.label.clone(),
        })?;
        // A backend that has already died turns this into a broken pipe, which is a
        // crash to report rather than an I/O error to bubble up raw.
        if stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.flush())
            .is_err()
        {
            return Err(self.crash_error());
        }

        let response = self.read_line(timeout)?;
        serde_json::from_str(&response).map_err(|e| BackendError::Protocol {
            label: self.label.clone(),
            reason: format!("response was not JSON: {e}"),
            saw: truncate(&response),
        })
    }

    /// Read one line, or fail: timeout, EOF, or an over-long line.
    fn read_line(&mut self, timeout: Duration) -> Result<String, BackendError> {
        match self.lines.recv_timeout(timeout) {
            Ok(Ok(line)) => Ok(line),
            // EOF means the backend closed stdout, which for a child that is exiting means
            // it died. Report the exit status and stderr, not "unexpected EOF".
            Ok(Err(ReadFault::Eof)) => Err(self.crash_error()),
            Ok(Err(ReadFault::LineTooLong(saw))) => {
                self.terminate();
                Err(BackendError::Protocol {
                    label: self.label.clone(),
                    reason: format!("sent a line over {MAX_LINE_BYTES} bytes"),
                    saw: truncate(&saw),
                })
            }
            Ok(Err(ReadFault::Io(e))) => Err(BackendError::Protocol {
                label: self.label.clone(),
                reason: format!("read failed: {e}"),
                saw: String::new(),
            }),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Kill the whole group and report the hang. The handle is dead after
                // this: leaving the child running would mean the next call reads the
                // answer to the *previous* request, which is worse than the timeout.
                self.terminate();
                Err(BackendError::Timeout {
                    label: self.label.clone(),
                    waited: timeout,
                })
            }
            // The reader thread is gone, so stdout is closed for good.
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.crash_error()),
        }
    }

    /// Build a crash error, reaping the child and capturing what it said on the way out.
    fn crash_error(&mut self) -> BackendError {
        let status = self.child.wait().ok();
        BackendError::Crashed {
            label: self.label.clone(),
            status: status.and_then(|s| s.code()),
            stderr: self.stderr_tail(),
        }
    }

    /// Whatever the backend wrote to stderr, truncated.
    ///
    /// Worth the effort: an engine that fails to load a model almost always says why on
    /// stderr, and discarding it turns a diagnosable problem into "exit code 1".
    fn stderr_tail(&mut self) -> String {
        let Some(mut err) = self.child.stderr.take() else {
            return String::new();
        };
        let mut buf = Vec::new();
        let _ = Read::by_ref(&mut err).take(8 * 1024).read_to_end(&mut buf);
        truncate(&String::from_utf8_lossy(&buf))
    }

    /// Ask the backend to exit, and make sure it does.
    pub fn shutdown(mut self) -> Result<(), BackendError> {
        self.terminate();
        Ok(())
    }

    fn terminate(&mut self) {
        // Closing stdin is the polite signal; killing is the guarantee. A backend that
        // ignores EOF must not survive its supervisor.
        drop(self.stdin.take());

        // Kill the process *group*, not just the child. A backend is realistically a
        // wrapper script around an engine, and killing only the wrapper leaves the engine
        // running — holding gigabytes and holding the stdout pipe open. The child was
        // spawned into its own group precisely so this is safe: the signal cannot reach
        // anything outside the subtree we started.
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::Kill);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for BackendProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// One long-lived thread turning the backend's stdout into a channel of lines.
///
/// It exits on EOF or error, so nothing ever has to join it — see the note on
/// [`BackendProcess::lines`].
fn spawn_reader(stdout: ChildStdout) -> mpsc::Receiver<Result<String, ReadFault>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            // Bounded: a backend is less trusted than its supervisor — a large C++ program
            // parsing untrusted model files — and an unbounded read_line would let a
            // confused one exhaust the daemon's memory.
            let read = {
                let limited = Read::by_ref(&mut reader).take(MAX_LINE_BYTES as u64 + 1);
                let mut limited = BufReader::new(limited);
                limited.read_line(&mut line)
            };
            let message = match read {
                Ok(0) => Err(ReadFault::Eof),
                Ok(n) if n > MAX_LINE_BYTES => Err(ReadFault::LineTooLong(line)),
                Ok(_) => Ok(line.trim_end().to_string()),
                Err(e) => Err(ReadFault::Io(e.to_string())),
            };
            let fatal = message.is_err();
            if tx.send(message).is_err() || fatal {
                return;
            }
        }
    });
    rx
}

fn truncate(s: &str) -> String {
    const MAX: usize = 400;
    let trimmed = s.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    trimmed.chars().take(MAX).collect::<String>() + "…"
}

#[derive(Debug)]
pub enum BackendError {
    Spawn {
        label: String,
        reason: String,
    },
    /// The backend exited. `status` is `None` when it was killed by a signal.
    Crashed {
        label: String,
        status: Option<i32>,
        stderr: String,
    },
    /// The backend did not answer in time and was killed.
    Timeout {
        label: String,
        waited: Duration,
    },
    /// The backend answered with something this build cannot understand.
    Protocol {
        label: String,
        reason: String,
        saw: String,
    },
    /// The handle has already been shut down.
    Closed {
        label: String,
    },
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Spawn { label, reason } => {
                write!(f, "cannot start the {label} backend: {reason}")
            }
            BackendError::Crashed {
                label,
                status,
                stderr,
            } => {
                match status {
                    Some(code) => write!(f, "the {label} backend exited with status {code}")?,
                    None => write!(f, "the {label} backend was killed by a signal")?,
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            BackendError::Timeout { label, waited } => write!(
                f,
                "the {label} backend did not respond within {}s and was stopped",
                waited.as_secs()
            ),
            BackendError::Protocol { label, reason, saw } => {
                write!(f, "the {label} backend broke the protocol: {reason}")?;
                if !saw.is_empty() {
                    write!(f, " (saw {saw:?})")?;
                }
                Ok(())
            }
            BackendError::Closed { label } => {
                write!(f, "the {label} backend has already been shut down")
            }
        }
    }
}

impl std::error::Error for BackendError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A fake backend, written as a shell script.
    ///
    /// Deliberately a separate process rather than an in-process double: the whole point
    /// of this module is what happens across a process boundary, and an in-process fake
    /// cannot segfault, cannot hang in a blocking read, and cannot write garbage to a pipe.
    fn fake(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    }

    const HELLO: &str = r#"printf '{"protocol":1,"engine":"fake","version":"0.1"}\n'"#;

    fn short() -> Duration {
        Duration::from_secs(3)
    }

    #[test]
    fn a_well_behaved_backend_says_hello_and_answers() {
        let script = format!(
            r#"{HELLO}
            while IFS= read -r line; do printf '{{"echo":true}}\n'; done"#
        );
        let mut b = BackendProcess::spawn("fake", &mut fake(&script), short()).unwrap();
        assert_eq!(b.hello().engine, "fake");
        let reply = b.request(&serde_json::json!({"ping": 1}), short()).unwrap();
        assert_eq!(reply["echo"], true);
        b.shutdown().unwrap();
    }

    #[test]
    fn a_backend_that_hangs_is_a_timeout_not_a_hung_daemon() {
        // The failure this module exists to prevent. A wedged engine must not wedge the
        // daemon that supervises it.
        let script = format!("{HELLO}\nsleep 600");
        let mut b = BackendProcess::spawn("hangs", &mut fake(&script), short()).unwrap();

        let started = Instant::now();
        let err = b
            .request(&serde_json::json!({"ping": 1}), Duration::from_millis(300))
            .unwrap_err();
        assert!(
            matches!(err, BackendError::Timeout { .. }),
            "expected a timeout, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must actually bound the wait, took {:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("did not respond"), "{err}");
    }

    #[test]
    fn a_backend_that_never_says_hello_fails_to_spawn_rather_than_lingering() {
        let mut b = BackendProcess::spawn("silent", &mut fake("sleep 600"), Duration::from_millis(300));
        assert!(matches!(b, Err(BackendError::Timeout { .. })), "{b:?}");
        b = Err(BackendError::Closed { label: "x".into() });
        let _ = b;
    }

    #[test]
    fn a_backend_that_dies_is_a_crash_with_its_exit_status_and_stderr() {
        // "exit code 1" alone is not diagnosable. The engine almost always says why on
        // stderr, and throwing that away is how a fixable problem becomes a mystery.
        let script = format!(
            r#"{HELLO}
            read -r line
            echo 'could not load model: bad magic' >&2
            exit 3"#
        );
        let mut b = BackendProcess::spawn("dies", &mut fake(&script), short()).unwrap();
        let err = b.request(&serde_json::json!({"ping": 1}), short()).unwrap_err();
        let BackendError::Crashed { status, stderr, .. } = &err else {
            panic!("expected Crashed, got {err:?}");
        };
        assert_eq!(*status, Some(3));
        assert!(stderr.contains("bad magic"), "{stderr:?}");
        assert!(err.to_string().contains("bad magic"), "{err}");
    }

    #[test]
    fn a_backend_killed_by_a_signal_is_reported_as_such() {
        let script = format!(
            r#"{HELLO}
            read -r line
            kill -9 $$"#
        );
        let mut b = BackendProcess::spawn("signalled", &mut fake(&script), short()).unwrap();
        let err = b.request(&serde_json::json!({"ping": 1}), short()).unwrap_err();
        assert!(
            matches!(err, BackendError::Crashed { status: None, .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("signal"), "{err}");
    }

    #[test]
    fn garbage_instead_of_a_hello_is_a_protocol_error_that_quotes_what_it_saw() {
        // A shell error message on stdout is the realistic version of this: a wrapper
        // script that failed before the engine started.
        let err = BackendProcess::spawn(
            "garbage",
            &mut fake("echo 'sh: llama-server: not found'"),
            short(),
        )
        .unwrap_err();
        let BackendError::Protocol { saw, .. } = &err else {
            panic!("expected Protocol, got {err:?}");
        };
        assert!(saw.contains("not found"), "{saw:?}");
    }

    #[test]
    fn a_backend_speaking_another_protocol_version_is_refused_at_startup() {
        let err = BackendProcess::spawn(
            "future",
            &mut fake(r#"printf '{"protocol":99,"engine":"x","version":"9"}\n'"#),
            short(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("protocol 99"), "{err}");
    }

    #[test]
    fn a_backend_that_floods_one_line_is_cut_off_rather_than_exhausting_memory() {
        // A backend is a large C++ program parsing untrusted model files. An unbounded
        // read would let a confused one take the daemon down with it.
        let script = format!(
            r#"{HELLO}
            read -r line
            yes 0123456789abcdef | tr -d '\n' | head -c {}
            printf '\n'"#,
            MAX_LINE_BYTES + 4096
        );
        let mut b = BackendProcess::spawn("flood", &mut fake(&script), short()).unwrap();
        let err = b.request(&serde_json::json!({"ping": 1}), short()).unwrap_err();
        assert!(
            matches!(err, BackendError::Protocol { .. }),
            "expected a protocol error, got {err:?}"
        );
        assert!(err.to_string().contains("over"), "{err}");
    }

    #[test]
    fn a_response_that_is_not_json_is_a_protocol_error_not_a_panic() {
        let script = format!(
            r#"{HELLO}
            read -r line
            printf 'not json at all\n'
            sleep 5"#
        );
        let mut b = BackendProcess::spawn("nonjson", &mut fake(&script), short()).unwrap();
        let err = b.request(&serde_json::json!({"ping": 1}), short()).unwrap_err();
        assert!(matches!(err, BackendError::Protocol { .. }), "{err:?}");
    }

    #[test]
    fn a_command_that_does_not_exist_is_a_spawn_error() {
        let err = BackendProcess::spawn(
            "missing",
            &mut Command::new("/nonexistent/otwono/llama-server"),
            short(),
        )
        .unwrap_err();
        assert!(matches!(err, BackendError::Spawn { .. }), "{err:?}");
        assert!(err.to_string().contains("cannot start"), "{err}");
    }

    /// Wait for a pid to stop running, rather than sleeping and hoping.
    ///
    /// A zombie counts as stopped. When the wrapper dies its children are reparented, and
    /// whether they are reaped promptly is up to whatever PID 1 is — inside a container,
    /// often nothing. The `/proc` entry can therefore linger long after the process has
    /// been killed, holding no memory and no pipe. Treating that as "still running" made
    /// this test flaky; the property under test is that the process is dead, not that
    /// somebody has collected its exit status.
    fn wait_gone(pid: u32) -> bool {
        for _ in 0..250 {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat) => {
                    let state = stat
                        .rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().next());
                    if state == Some("Z") {
                        return true;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// The pid of the `sleep` a fake backend started, read out of /proc.
    fn grandchild_of(parent: u32) -> Option<u32> {
        for _ in 0..100 {
            if let Ok(entries) = std::fs::read_dir("/proc") {
                for e in entries.flatten() {
                    let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                        continue;
                    };
                    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                        continue;
                    };
                    // ppid is field 4, after the comm field which may contain spaces.
                    let Some(after_comm) = stat.rsplit_once(')') else {
                        continue;
                    };
                    let ppid: Option<u32> = after_comm
                        .1
                        .split_whitespace()
                        .nth(1)
                        .and_then(|v| v.parse().ok());
                    if ppid == Some(parent) && pid != parent {
                        return Some(pid);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn dropping_the_handle_kills_the_whole_backend_subtree() {
        // A supervisor that leaked processes would leave a node filling with orphaned
        // engines holding gigabytes each. Killing only the direct child is not enough: a
        // backend is realistically a wrapper script around the engine, and the engine is
        // the grandchild. This test exists because an earlier version killed only the
        // wrapper, the `sleep` survived, and it held the stdout pipe open — which then
        // hung the supervisor's own reader.
        let script = format!("{HELLO}\nsleep 600");
        let b = BackendProcess::spawn("orphan", &mut fake(&script), short()).unwrap();
        let child = b.child.id();
        let grandchild = grandchild_of(child).expect("the fake backend must start a sleep");

        drop(b);

        assert!(wait_gone(child), "the wrapper {child} outlived its supervisor");
        assert!(
            wait_gone(grandchild),
            "the engine {grandchild} outlived its supervisor"
        );
    }

    #[test]
    fn a_timeout_does_not_leave_the_backend_running() {
        // The timeout is only meaningful if it also stops the thing that hung.
        let script = format!("{HELLO}\nsleep 600");
        let mut b = BackendProcess::spawn("hang-cleanup", &mut fake(&script), short()).unwrap();
        let child = b.child.id();
        let grandchild = grandchild_of(child).expect("the fake backend must start a sleep");

        let err = b
            .request(&serde_json::json!({"ping": 1}), Duration::from_millis(300))
            .unwrap_err();
        assert!(matches!(err, BackendError::Timeout { .. }), "{err:?}");
        assert!(wait_gone(child), "the wrapper survived the timeout");
        assert!(wait_gone(grandchild), "the engine survived the timeout");
    }

    #[test]
    fn several_requests_reuse_one_process() {
        // Model load is the expensive part; a supervisor that respawned per request would
        // make every answer cost a cold start.
        let script = format!(
            r#"{HELLO}
            n=0
            while IFS= read -r line; do
                n=$((n + 1))
                printf '{{"n":%d}}\n' "$n"
            done"#
        );
        let mut b = BackendProcess::spawn("reuse", &mut fake(&script), short()).unwrap();
        for expected in 1..=5 {
            let reply = b
                .request(&serde_json::json!({"ping": expected}), short())
                .unwrap();
            assert_eq!(reply["n"], expected);
        }
    }
}
