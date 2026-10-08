//! Command execution: builds a process from a rule's shell + command and
//! captures its output.

use crate::config;
use crate::envres::BaseEnv;
use crate::model::{Rule, ShellKind};
use crate::shell::ShellCatalog;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct Outcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    /// True when the run was killed by the rule's timeout.
    pub timed_out: bool,
}

impl Outcome {
    fn error(msg: String) -> Outcome {
        Outcome {
            exit_code: None,
            success: false,
            stdout: String::new(),
            stderr: format!("cronch: {msg}"),
            timed_out: false,
        }
    }
}

/// Expand an argument template, replacing the `{cmd}` token with `command` as a
/// single argument (the shell parses it — we never split the user command).
fn render_template(template: &str, command: &str) -> Vec<String> {
    template
        .split_whitespace()
        .map(|tok| {
            if tok == "{cmd}" {
                command.to_string()
            } else {
                tok.to_string()
            }
        })
        .collect()
}

fn resolve_program_args(
    rule: &Rule,
    catalog: &ShellCatalog,
) -> Result<(String, Vec<String>), String> {
    match &rule.shell {
        ShellKind::Direct => {
            let parts = shell_words::split(&rule.command)
                .map_err(|e| format!("cannot parse command: {e}"))?;
            let mut it = parts.into_iter();
            let program = it.next().ok_or_else(|| "empty command".to_string())?;
            Ok((program, it.collect()))
        }
        ShellKind::Custom { path, arg_template } => {
            Ok((path.clone(), render_template(arg_template, &rule.command)))
        }
        ShellKind::Detected { key } => {
            let info = catalog
                .get(key)
                .ok_or_else(|| format!("shell '{key}' not found on this machine"))?;
            Ok((info.path.clone(), render_template(&info.arg_template, &rule.command)))
        }
    }
}

fn build_command(rule: &Rule, catalog: &ShellCatalog, base_env: &BaseEnv) -> Result<Command, String> {
    let (program, args) = resolve_program_args(rule, catalog)?;

    let mut cmd = Command::new(&program);
    cmd.args(&args);

    // Working directory: rule override, else the user's home.
    let cwd = rule
        .working_dir
        .clone()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(config::home_dir);
    cmd.current_dir(&cwd);

    // Environment: full login env, then per-rule overrides.
    cmd.env_clear();
    for (k, v) in base_env.iter() {
        cmd.env(k, v);
    }
    for ev in &rule.env {
        cmd.env(&ev.key, &ev.value);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Kill the child if the job handle is dropped (e.g. app quit mid-run) so
    // orphaned processes are not left behind.
    cmd.kill_on_drop(true);

    // Run each job in its own process group so a timeout can kill the whole
    // tree (shell + grandchildren) and app-level signals never reach the job.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    // Do not flash a console window when running console programs.
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    Ok(cmd)
}

const MAX_CAPTURE: usize = 256 * 1024; // cap captured stream size
const READ_CHUNK: usize = 16 * 1024;
const TRUNCATED_MARKER: &str = "\n… [output truncated]";

/// Drain a stream into memory, keeping at most `MAX_CAPTURE` bytes (further
/// data is still read and discarded so the child never blocks on a full pipe).
/// Returns the captured text and whether the source produced more than the cap.
async fn capture_stream<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> (String, bool) {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut out: Vec<u8> = Vec::with_capacity(MAX_CAPTURE.min(READ_CHUNK));
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if out.len() < MAX_CAPTURE {
            let room = MAX_CAPTURE - out.len();
            let take = n.min(room);
            out.extend_from_slice(&buf[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    let mut s = String::from_utf8_lossy(&out).into_owned();
    // The lossy conversion may push a few bytes past the cap (replacement
    // chars); trim back to a UTF-8 boundary without panicking.
    if s.len() > MAX_CAPTURE {
        s.truncate(s.floor_char_boundary(MAX_CAPTURE));
    }
    if truncated {
        s.push_str(TRUNCATED_MARKER);
    }
    (s, truncated)
}

/// Kill the whole process group of a job.
#[cfg(unix)]
async fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // The child is its own group leader (process_group(0)), so a negative
        // pid targets every process in the group. SIGKILL cannot be caught.
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(windows)]
async fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // taskkill /T kills the process tree (Windows has no POSIX groups).
        let _ = tokio::process::Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .status()
            .await;
    }
}

/// Capture an optional stream (empty when the pipe was not set up).
async fn capture_opt<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>) -> (String, bool) {
    match stream {
        Some(s) => capture_stream(s).await,
        None => (String::new(), false),
    }
}

enum RunResult {
    Done(std::io::Result<std::process::ExitStatus>, String, String),
    TimedOut { secs: i64, stdout: String, stderr: String },
}

pub async fn execute(rule: &Rule, catalog: &ShellCatalog, base_env: &BaseEnv) -> Outcome {
    let mut cmd = match build_command(rule, catalog, base_env) {
        Ok(c) => c,
        Err(e) => return Outcome::error(e),
    };

    let timeout_secs = rule.timeout_secs;

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::error(format!("failed to start process: {e}")),
    };
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Wait for the process while draining both pipes concurrently (a full
    // pipe would otherwise block the child forever). Runs in a task so the
    // timeout branch can kill the group and then wait for this to wind down.
    let mut job = tokio::spawn(async move {
        let wait_fut = child.wait();
        let so_fut = capture_opt(stdout);
        let se_fut = capture_opt(stderr);
        let (status, (o, _), (e, _)) = tokio::join!(wait_fut, so_fut, se_fut);
        (status, o, e)
    });

    let deadline = if timeout_secs > 0 {
        Some(
            tokio::time::Instant::now()
                + tokio::time::Duration::from_secs(timeout_secs as u64),
        )
    } else {
        None
    };

    let result = match deadline {
        Some(dl) => tokio::select! {
            r = &mut job => match r {
                Ok((status, o, e)) => RunResult::Done(status, o, e),
                Err(e) => RunResult::Done(Err(std::io::Error::other(e)), String::new(), String::new()),
            },
            _ = tokio::time::sleep_until(dl) => {
                kill_group(pid).await;
                // Wait for the drained result; guard against a double-forked
                // survivor that keeps the pipes open forever.
                let (o, e) = match tokio::time::timeout(
                    tokio::time::Duration::from_secs(2),
                    &mut job,
                ).await {
                    Ok(Ok((_status, o, e))) => (o, e),
                    _ => (String::new(), String::new()),
                };
                RunResult::TimedOut { secs: timeout_secs, stdout: o, stderr: e }
            }
        },
        None => match job.await {
            Ok((status, o, e)) => RunResult::Done(status, o, e),
            Err(e) => RunResult::Done(Err(std::io::Error::other(e)), String::new(), String::new()),
        },
    };

    match result {
        RunResult::Done(status, stdout, stderr) => match status {
            Ok(status) => Outcome {
                exit_code: status.code(),
                success: status.success(),
                stdout,
                stderr,
                timed_out: false,
            },
            Err(e) => Outcome::error(format!("process error: {e}")),
        },
        RunResult::TimedOut { secs, stdout, stderr } => {
            let note = format!("cronch: killed after {secs}s (timeout)");
            let stderr = if stderr.is_empty() {
                note
            } else {
                format!("{stderr}\n{note}")
            };
            Outcome {
                exit_code: None,
                success: false,
                stdout,
                stderr,
                timed_out: true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Schedule;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_echo_captures_stdout() {
        let cmd = if cfg!(windows) { "cmd /C echo hello123" } else { "echo hello123" };
        let rule = Rule::new(
            "t".into(),
            cmd.into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 1 },
        );
        let cat = ShellCatalog::detect();
        let env = BaseEnv::resolve();
        let out = execute(&rule, &cat, &env).await;
        assert!(out.success, "stderr: {}", out.stderr);
        assert!(out.stdout.contains("hello123"), "stdout: {}", out.stdout);
    }

    #[test]
    fn template_keeps_command_as_single_arg() {
        let args = render_template("-NoProfile -Command {cmd}", "echo a b c");
        assert_eq!(args, vec!["-NoProfile", "-Command", "echo a b c"]);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn output_capture_is_bounded_and_marks_truncation() {
        let rule = Rule::new(
            "t".into(),
            "dd if=/dev/zero bs=1048576 count=1".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        let out = execute(&rule, &ShellCatalog::detect(), &BaseEnv::resolve()).await;
        assert!(out.success, "stderr: {}", out.stderr);
        assert!(
            out.stdout.len() <= MAX_CAPTURE + TRUNCATED_MARKER.len(),
            "capture must be bounded"
        );
        assert!(out.stdout.contains(TRUNCATED_MARKER));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_kills_long_running_job() {
        let mut rule = Rule::new(
            "t".into(),
            "sh -c 'sleep 60'".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        rule.timeout_secs = 1;
        let start = std::time::Instant::now();
        let out = execute(&rule, &ShellCatalog::detect(), &BaseEnv::resolve()).await;
        assert!(out.timed_out, "expected the run to be killed by timeout");
        assert!(!out.success);
        assert_eq!(out.exit_code, None);
        assert!(out.stderr.contains("timeout"), "stderr: {}", out.stderr);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "timeout must not hang"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_timeout_means_unbounded_run() {
        let mut rule = Rule::new(
            "t".into(),
            "echo ok".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        rule.timeout_secs = 0;
        let out = execute(&rule, &ShellCatalog::detect(), &BaseEnv::resolve()).await;
        assert!(out.success, "stderr: {}", out.stderr);
        assert!(!out.timed_out);
    }
}
