//! Command execution: builds a process from a rule's shell + command and
//! captures its output.

use crate::config;
use crate::envres::BaseEnv;
use crate::model::{Rule, ShellKind};
use crate::shell::ShellCatalog;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::Duration;

#[derive(Clone, Debug)]
pub struct Outcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    /// True when the run was killed by the rule's timeout.
    pub timed_out: bool,
    /// True when the run was cancelled (app shutting down or rule deleted).
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

    fn cancelled(reason: &str) -> Outcome {
        Outcome {
            exit_code: None,
            success: false,
            stdout: String::new(),
            stderr: format!("cronch: cancelled ({reason})"),
            timed_out: false,
            cancelled: true,
        }
    }
}

/// Signals that end a job early. Both are `watch` channels whose value flips
/// to `true`: `shutdown` is shared by every job, `cancel` belongs to this run.
pub struct StopSignals {
    pub shutdown: watch::Receiver<bool>,
    pub cancel: watch::Receiver<bool>,
}

const SHUTDOWN_REASON: &str = "app is shutting down";
const DELETED_REASON: &str = "rule was deleted";

impl StopSignals {
    /// A signal that was raised before this job subscribed never fires
    /// `changed()`, so check the current values up front.
    fn already_raised(&self) -> Option<&'static str> {
        if *self.shutdown.borrow() {
            Some(SHUTDOWN_REASON)
        } else if *self.cancel.borrow() {
            Some(DELETED_REASON)
        } else {
            None
        }
    }

    /// Resolves when either signal is raised (or its sender goes away).
    async fn raised(&mut self) -> &'static str {
        tokio::select! {
            _ = self.shutdown.changed() => SHUTDOWN_REASON,
            _ = self.cancel.changed() => DELETED_REASON,
        }
    }
}

/// One argument of the process to start.
#[derive(Debug, PartialEq)]
enum Arg {
    /// A word of the shell's argument template.
    Plain(String),
    /// The template word that carried `{cmd}`, with the command substituted.
    Command(String),
}

/// Expand an argument template, replacing the `{cmd}` placeholder with
/// `command` as a single argument (the shell parses it — we never split the
/// user command). The template is word-split with the platform's quoting rules
/// so `-c {cmd}`, `-c "{cmd}"` and `--opt={cmd}` all behave as written.
fn render_template(template: &str, command: &str) -> Result<Vec<Arg>, String> {
    let words = crate::model::parse_arg_template(template)?;
    Ok(words
        .into_iter()
        .map(|w| {
            if w.contains(crate::model::CMD_PLACEHOLDER) {
                Arg::Command(w.replace(crate::model::CMD_PLACEHOLDER, command))
            } else {
                Arg::Plain(w)
            }
        })
        .collect())
}

fn resolve_program_args(rule: &Rule, catalog: &ShellCatalog) -> Result<(String, Vec<Arg>), String> {
    match &rule.shell {
        ShellKind::Direct => {
            let parts = crate::model::split_command_line(&rule.command)
                .map_err(|e| format!("cannot parse command: {e}"))?;
            let mut it = parts.into_iter();
            let program = it.next().ok_or_else(|| "empty command".to_string())?;
            Ok((program, it.map(Arg::Plain).collect()))
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

/// True when `program` is the Windows command interpreter (`cmd` / `cmd.exe`,
/// optionally with a directory, either slash style).
#[cfg_attr(not(windows), allow(dead_code))]
fn is_cmd_exe(program: &str) -> bool {
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd")
}

fn build_command(
    rule: &Rule,
    catalog: &ShellCatalog,
    base_env: &BaseEnv,
) -> Result<Command, String> {
    let (program, args) = resolve_program_args(rule, catalog)?;

    let mut cmd = Command::new(&program);
    #[cfg(windows)]
    let raw_command = is_cmd_exe(&program);
    for arg in args {
        match arg {
            Arg::Plain(w) => {
                cmd.arg(w);
            }
            // cmd.exe does not parse its command line with the MSVC rules the
            // standard quoting follows (`"` would arrive as `\"`), so hand it
            // the command verbatim, wrapped in the outer quotes that `/S /C`
            // strips again.
            #[cfg(windows)]
            Arg::Command(w) if raw_command => {
                cmd.raw_arg(format!("\"{w}\""));
            }
            Arg::Command(w) => {
                cmd.arg(w);
            }
        }
    }

    // Working directory: rule override, else the user's home.
    let cwd = rule
        .working_dir
        .clone()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(config::home_dir);
    if !cwd.is_dir() {
        return Err(format!("working directory not found: {}", cwd.display()));
    }
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

/// How long output is still collected after the command itself has exited.
/// Whatever still holds the pipes open after that is a background process the
/// command started; the run is finished without waiting for it.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
const DETACHED_NOTE: &str = "cronch: the command exited but a process it started in the background still holds its output; later output is not recorded";

/// Output captured from one stream, shared so the run can take a snapshot
/// while a background process may still be holding the pipe open.
#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

type SharedCapture = Arc<Mutex<Capture>>;

/// Drain a stream until EOF, keeping at most `MAX_CAPTURE` bytes (further data
/// is still read and discarded so the writer never blocks on a full pipe).
async fn drain_stream<R: tokio::io::AsyncRead + Unpin>(mut reader: R, sink: SharedCapture) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut c = sink.lock().unwrap();
        let room = MAX_CAPTURE.saturating_sub(c.bytes.len());
        let take = n.min(room);
        c.bytes.extend_from_slice(&buf[..take]);
        if take < n {
            c.truncated = true;
        }
    }
}

/// The captured text so far (lossy UTF-8, marked when it hit the cap).
fn snapshot(capture: &SharedCapture) -> String {
    let c = capture.lock().unwrap();
    let mut s = String::from_utf8_lossy(&c.bytes).into_owned();
    // The lossy conversion may push a few bytes past the cap (replacement
    // chars); trim back to a UTF-8 boundary without panicking.
    if s.len() > MAX_CAPTURE {
        s.truncate(s.floor_char_boundary(MAX_CAPTURE));
    }
    if c.truncated {
        s.push_str(TRUNCATED_MARKER);
    }
    s
}

fn append_note(stderr: String, note: &str) -> String {
    if stderr.is_empty() {
        note.to_string()
    } else {
        format!("{stderr}\n{note}")
    }
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

/// How the process side of a run ended.
enum Ended {
    /// The command exited on its own.
    Exited {
        status: std::io::Result<std::process::ExitStatus>,
    },
    TimedOut {
        secs: i64,
    },
    Cancelled {
        reason: &'static str,
    },
}

type ExitHandle = tokio::task::JoinHandle<std::io::Result<std::process::ExitStatus>>;

/// Wait (bounded) for a killed process to be reaped.
async fn reap(exited: &mut ExitHandle) {
    let _ = tokio::time::timeout(Duration::from_secs(2), &mut *exited).await;
}

/// Give the drain tasks up to [`DRAIN_GRACE`] to reach the end of their
/// streams. Returns false when something (a background process the command
/// started) still holds a pipe open; its drain task then outlives the run.
async fn settle_output(
    out_task: Option<tokio::task::JoinHandle<()>>,
    err_task: Option<tokio::task::JoinHandle<()>>,
) -> bool {
    tokio::time::timeout(DRAIN_GRACE, async {
        if let Some(t) = out_task {
            let _ = t.await;
        }
        if let Some(t) = err_task {
            let _ = t.await;
        }
    })
    .await
    .is_ok()
}

pub async fn execute(
    rule: &Rule,
    catalog: &ShellCatalog,
    base_env: &BaseEnv,
    mut stop: StopSignals,
) -> Outcome {
    if let Some(reason) = stop.already_raised() {
        return Outcome::cancelled(reason);
    }

    let mut cmd = match build_command(rule, catalog, base_env) {
        Ok(c) => c,
        Err(e) => return Outcome::error(e),
    };

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::error(format!("failed to start process: {e}")),
    };
    let pid = child.id();

    // Drain both pipes concurrently in their own tasks (a full pipe would
    // otherwise block the child). If a background process keeps a pipe open,
    // its drain task simply outlives the run.
    let out_cap = SharedCapture::default();
    let err_cap = SharedCapture::default();
    let out_task = child
        .stdout
        .take()
        .map(|s| tokio::spawn(drain_stream(s, out_cap.clone())));
    let err_task = child
        .stderr
        .take()
        .map(|s| tokio::spawn(drain_stream(s, err_cap.clone())));

    // Owns the child (kill_on_drop) and resolves when the command itself
    // exits — independently of its output, so once it has exited a timeout
    // or cancel can no longer overrule the result.
    let mut exited: ExitHandle = tokio::spawn(async move { child.wait().await });

    // 0 = no limit; an out-of-range value also means "no deadline" rather
    // than a panic (rule validation keeps real values within 7 days).
    let deadline = u64::try_from(rule.timeout_secs)
        .ok()
        .filter(|s| *s > 0)
        .and_then(|s| tokio::time::Instant::now().checked_add(Duration::from_secs(s)));
    let timeout = async {
        match deadline {
            Some(dl) => tokio::time::sleep_until(dl).await,
            None => std::future::pending().await,
        }
    };

    let ended = tokio::select! {
        r = &mut exited => Ended::Exited {
            status: r.unwrap_or_else(|e| Err(std::io::Error::other(e))),
        },
        _ = timeout => {
            kill_group(pid).await;
            reap(&mut exited).await;
            Ended::TimedOut { secs: rule.timeout_secs }
        }
        reason = stop.raised() => {
            kill_group(pid).await;
            reap(&mut exited).await;
            Ended::Cancelled { reason }
        }
    };

    // Collect the output that is still in flight (after a kill the pipes
    // close right away), without waiting on a background process.
    let drained = settle_output(out_task, err_task).await;
    let stdout = snapshot(&out_cap);
    let stderr = snapshot(&err_cap);
    match ended {
        Ended::Exited { status } => match status {
            Ok(status) => Outcome {
                exit_code: status.code(),
                success: status.success(),
                stdout,
                stderr: if drained {
                    stderr
                } else {
                    append_note(stderr, DETACHED_NOTE)
                },
                timed_out: false,
                cancelled: false,
            },
            Err(e) => Outcome::error(format!("process error: {e}")),
        },
        Ended::TimedOut { secs } => Outcome {
            exit_code: None,
            success: false,
            stdout,
            stderr: append_note(stderr, &format!("cronch: killed after {secs}s (timeout)")),
            timed_out: true,
            cancelled: false,
        },
        Ended::Cancelled { reason } => Outcome {
            exit_code: None,
            success: false,
            stdout,
            stderr: append_note(stderr, &format!("cronch: cancelled ({reason})")),
            timed_out: false,
            cancelled: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Schedule;

    fn base_env() -> BaseEnv {
        BaseEnv::resolve()
    }

    /// Run a rule with stop signals that never fire (the senders stay alive
    /// for the duration of the call).
    async fn run_rule(rule: &Rule) -> Outcome {
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let (_cancel_tx, cancel) = watch::channel(false);
        let env = base_env();
        execute(
            rule,
            &ShellCatalog::detect(&env),
            &env,
            StopSignals { shutdown, cancel },
        )
        .await
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

    fn command_args(args: Vec<Arg>) -> Vec<(bool, String)> {
        args.into_iter()
            .map(|a| match a {
                Arg::Plain(w) => (false, w),
                Arg::Command(w) => (true, w),
            })
            .collect()
    }

    #[test]
    fn template_keeps_command_as_single_arg() {
        let args = render_template("-NoProfile -Command {cmd}", "echo a b c").unwrap();
        assert_eq!(
            command_args(args),
            vec![
                (false, "-NoProfile".into()),
                (false, "-Command".into()),
                (true, "echo a b c".into())
            ]
        );
    }

    #[test]
    fn template_expands_placeholder_inside_quotes_and_words() {
        // Quotes are consumed by word-splitting; the command stays one argument.
        assert_eq!(
            command_args(render_template("-c \"{cmd}\"", "echo a b").unwrap()),
            vec![(false, "-c".into()), (true, "echo a b".into())]
        );
        // The placeholder can be embedded in a larger word.
        assert_eq!(
            command_args(render_template("--eval={cmd}", "echo a").unwrap()),
            vec![(true, "--eval=echo a".into())]
        );
    }

    #[test]
    fn detects_the_windows_command_interpreter() {
        assert!(is_cmd_exe(r"C:\Windows\System32\cmd.exe"));
        assert!(is_cmd_exe("CMD.EXE"));
        assert!(is_cmd_exe("cmd"));
        assert!(is_cmd_exe("C:/Windows/System32/cmd.exe"));
        assert!(!is_cmd_exe(r"C:\Tools\mycmd.exe"));
        assert!(!is_cmd_exe("powershell.exe"));
    }

    #[test]
    fn missing_working_directory_is_reported_clearly() {
        let mut rule = Rule::new(
            "t".into(),
            "echo hi".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        rule.working_dir = Some("/definitely/not/a/dir".into());
        let env = base_env();
        let err = build_command(&rule, &ShellCatalog::detect(&env), &env)
            .expect_err("a missing working directory must be an error");
        assert!(err.contains("working directory not found"), "{err}");
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
    async fn background_process_does_not_hold_the_run() {
        let mut rule = Rule::new(
            "t".into(),
            "sh -c 'echo started; sleep 20 &'".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        rule.timeout_secs = 15;
        let start = std::time::Instant::now();
        let out = run_rule(&rule).await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "the run must finish once the command exits, took {:?}",
            start.elapsed()
        );
        assert!(out.success, "stderr: {}", out.stderr);
        assert!(!out.timed_out);
        assert!(out.stdout.contains("started"), "stdout: {}", out.stdout);
        assert!(out.stderr.contains("background"), "stderr: {}", out.stderr);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exit_just_before_the_timeout_is_not_overruled() {
        // The command succeeds at ~1s while a background child keeps the
        // output open; the 2s timeout lands inside the output grace period.
        let mut rule = Rule::new(
            "t".into(),
            "sh -c 'sleep 20 & sleep 1; echo done'".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        rule.timeout_secs = 2;
        let out = run_rule(&rule).await;
        assert!(!out.timed_out, "stderr: {}", out.stderr);
        assert!(out.success);
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("done"), "stdout: {}", out.stdout);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_kills_long_running_job() {
        let mut rule = Rule::new(
            "t".into(),
            "sh -c 'echo partial; sleep 60'".into(),
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
            out.stdout.contains("partial"),
            "output before the kill is kept"
        );
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
        let (tx, shutdown) = watch::channel(false);
        let (_cancel_tx, cancel) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let env = BaseEnv::resolve();
            execute(
                &rule,
                &ShellCatalog::detect(&env),
                &env,
                StopSignals { shutdown, cancel },
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        tx.send(true).unwrap();
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("shutdown must not hang")
            .unwrap();
        assert!(!out.success);
        assert!(!out.timed_out);
        assert!(out.cancelled);
        assert!(
            out.stderr.contains("shutting down"),
            "stderr: {}",
            out.stderr
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deleting_the_rule_cancels_its_run() {
        let rule = Rule::new(
            "t".into(),
            "sh -c 'sleep 60'".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let (cancel_tx, cancel) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let env = BaseEnv::resolve();
            execute(
                &rule,
                &ShellCatalog::detect(&env),
                &env,
                StopSignals { shutdown, cancel },
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel_tx.send(true).unwrap();
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("cancel must not hang")
            .unwrap();
        assert!(out.cancelled);
        assert!(
            out.stderr.contains("rule was deleted"),
            "stderr: {}",
            out.stderr
        );
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
        let (tx, _rx) = watch::channel(false);
        tx.send_replace(true);
        let late_rx = tx.subscribe();
        let (_cancel_tx, cancel) = watch::channel(false);
        let env = base_env();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            execute(
                &rule,
                &ShellCatalog::detect(&env),
                &env,
                StopSignals {
                    shutdown: late_rx,
                    cancel,
                },
            ),
        )
        .await
        .expect("an already-signalled shutdown must cancel immediately");
        assert!(out.cancelled, "outcome must be marked cancelled");
        assert!(!out.success);
        assert!(out.stderr.contains("cancelled"), "stderr: {}", out.stderr);
    }
}
