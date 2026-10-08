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
    /// True when the run was cancelled because the app is shutting down.
    pub cancelled: bool,
}

impl Outcome {
    fn error(msg: String) -> Outcome {
        Outcome {
            exit_code: None,
            success: false,
            stdout: String::new(),
            stderr: format!("cronch: {msg}"),
            timed_out: false,
            cancelled: false,
        }
    }

    fn cancelled() -> Outcome {
        Outcome {
            exit_code: None,
            success: false,
            stdout: String::new(),
            stderr: "cronch: cancelled (app is shutting down)".to_string(),
            timed_out: false,
            cancelled: true,
        }
    }
}

/// Expand an argument template, replacing the `{cmd}` placeholder with
/// `command` as a single argument (the shell parses it — we never split the
/// user command). The template is word-split with shell-like quoting so
/// `-c {cmd}`, `-c "{cmd}"` and `--opt={cmd}` all behave as written.
fn render_template(template: &str, command: &str) -> Result<Vec<String>, String> {
    let words = crate::model::parse_arg_template(template)?;
    Ok(words
        .into_iter()
        .map(|w| w.replace(crate::model::CMD_PLACEHOLDER, command))
        .collect())
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
            Ok((path.clone(), render_template(arg_template, &rule.command)?))
        }
        ShellKind::Detected { key } => {
            let info = catalog
                .get(key)
                .ok_or_else(|| format!("shell '{key}' not found on this machine"))?;
            Ok((
                info.path.clone(),
                render_template(&info.arg_template, &rule.command)?,
            ))
        }
    }
}

fn build_command(
    rule: &Rule,
    catalog: &ShellCatalog,
    base_env: &BaseEnv,
) -> Result<Command, String> {
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
        // CREATE_NO_WINDOW so killing a job never flashes a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = tokio::process::Command::new("taskkill");
        cmd.arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .creation_flags(CREATE_NO_WINDOW);
        let _ = cmd.status().await;
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
    TimedOut {
        secs: i64,
        stdout: String,
        stderr: String,
    },
    Cancelled {
        stdout: String,
        stderr: String,
    },
}

/// Wait (bounded) for a killed job's drain task to hand back whatever it read,
/// so a double-forked survivor holding the pipes open cannot hang shutdown.
async fn reap(
    job: &mut tokio::task::JoinHandle<(std::io::Result<std::process::ExitStatus>, String, String)>,
) -> (String, String) {
    match tokio::time::timeout(tokio::time::Duration::from_secs(2), &mut *job).await {
        Ok(Ok((_status, o, e))) => (o, e),
        _ => (String::new(), String::new()),
    }
}

pub async fn execute(
    rule: &Rule,
    catalog: &ShellCatalog,
    base_env: &BaseEnv,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Outcome {
    // A shutdown broadcast may already have happened before this job
    // subscribed; in that case `changed()` would never fire, so detect it up
    // front and never start the process at all.
    if *shutdown.borrow() {
        return Outcome::cancelled();
    }

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
    // timeout/cancel branches can kill the group and then wait for this to
    // wind down.
    let mut job = tokio::spawn(async move {
        let wait_fut = child.wait();
        let so_fut = capture_opt(stdout);
        let se_fut = capture_opt(stderr);
        let (status, (o, _), (e, _)) = tokio::join!(wait_fut, so_fut, se_fut);
        (status, o, e)
    });

    let deadline = if timeout_secs > 0 {
        Some(tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs as u64))
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
                let (o, e) = reap(&mut job).await;
                RunResult::TimedOut { secs: timeout_secs, stdout: o, stderr: e }
            }
            _ = shutdown.changed() => {
                kill_group(pid).await;
                let (o, e) = reap(&mut job).await;
                RunResult::Cancelled { stdout: o, stderr: e }
            }
        },
        None => tokio::select! {
            r = &mut job => match r {
                Ok((status, o, e)) => RunResult::Done(status, o, e),
                Err(e) => RunResult::Done(Err(std::io::Error::other(e)), String::new(), String::new()),
            },
            _ = shutdown.changed() => {
                kill_group(pid).await;
                let (o, e) = reap(&mut job).await;
                RunResult::Cancelled { stdout: o, stderr: e }
            }
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
                cancelled: false,
            },
            Err(e) => Outcome::error(format!("process error: {e}")),
        },
        RunResult::TimedOut {
            secs,
            stdout,
            stderr,
        } => {
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
                cancelled: false,
            }
        }
        RunResult::Cancelled { stdout, stderr } => {
            let note = "cronch: cancelled (app is shutting down)";
            let stderr = if stderr.is_empty() {
                note.to_string()
            } else {
                format!("{stderr}\n{note}")
            };
            Outcome {
                exit_code: None,
                success: false,
                stdout,
                stderr,
                timed_out: false,
                cancelled: true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Schedule;

    /// Run a rule with a never-fired shutdown channel (the sender stays alive
    /// for the duration of the call, so the job is never cancelled).
    async fn run_rule(rule: &Rule) -> Outcome {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        execute(rule, &ShellCatalog::detect(), &BaseEnv::resolve(), rx).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_echo_captures_stdout() {
        let cmd = if cfg!(windows) {
            "cmd /C echo hello123"
        } else {
            "echo hello123"
        };
        let rule = Rule::new(
            "t".into(),
            cmd.into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 1 },
        );
        let out = run_rule(&rule).await;
        assert!(out.success, "stderr: {}", out.stderr);
        assert!(out.stdout.contains("hello123"), "stdout: {}", out.stdout);
    }

    #[test]
    fn template_keeps_command_as_single_arg() {
        let args = render_template("-NoProfile -Command {cmd}", "echo a b c").unwrap();
        assert_eq!(args, vec!["-NoProfile", "-Command", "echo a b c"]);
    }

    #[test]
    fn template_expands_placeholder_inside_quotes_and_words() {
        // Quotes are consumed by word-splitting; the command stays one argument.
        assert_eq!(
            render_template("-c \"{cmd}\"", "echo a b").unwrap(),
            vec!["-c", "echo a b"]
        );
        // The placeholder can be embedded in a larger word.
        assert_eq!(
            render_template("--eval={cmd}", "echo a").unwrap(),
            vec!["--eval=echo a"]
        );
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
        let out = run_rule(&rule).await;
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
        let out = run_rule(&rule).await;
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
        let out = run_rule(&rule).await;
        assert!(out.success, "stderr: {}", out.stderr);
        assert!(!out.timed_out);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_long_running_job() {
        let rule = Rule::new(
            "t".into(),
            "sh -c 'sleep 60'".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            execute(&rule, &ShellCatalog::detect(), &BaseEnv::resolve(), rx).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        tx.send(true).unwrap();
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("shutdown must not hang")
            .unwrap();
        assert!(!out.success);
        assert!(!out.timed_out);
        assert!(out.stderr.contains("cancelled"), "stderr: {}", out.stderr);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_signalled_before_subscribe_cancels_new_job() {
        // Reproduces the race: the receiver is created AFTER shutdown was
        // broadcast, so `changed()` would never fire. The up-front value check
        // must catch this and cancel immediately.
        let rule = Rule::new(
            "t".into(),
            "sh -c 'sleep 60'".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        let (tx, _rx) = tokio::sync::watch::channel(false);
        tx.send_replace(true);
        let late_rx = tx.subscribe();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            execute(&rule, &ShellCatalog::detect(), &BaseEnv::resolve(), late_rx),
        )
        .await
        .expect("an already-signalled shutdown must cancel immediately");
        assert!(out.cancelled, "outcome must be marked cancelled");
        assert!(!out.success);
        assert!(out.stderr.contains("cancelled"), "stderr: {}", out.stderr);
    }
}
