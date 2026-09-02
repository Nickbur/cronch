//! Command execution: builds a process from a rule's shell + command and
//! captures its output.

use crate::config;
use crate::envres::BaseEnv;
use crate::model::{Rule, ShellKind};
use crate::shell::ShellCatalog;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct Outcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    fn error(msg: String) -> Outcome {
        Outcome {
            exit_code: None,
            success: false,
            stdout: String::new(),
            stderr: format!("cronch: {msg}"),
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

    // Do not flash a console window when running console programs.
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    Ok(cmd)
}

const MAX_CAPTURE: usize = 256 * 1024; // cap captured stream size

fn clamp(mut s: String) -> String {
    if s.len() > MAX_CAPTURE {
        s.truncate(MAX_CAPTURE);
        s.push_str("\n… [output truncated]");
    }
    s
}

pub async fn execute(rule: &Rule, catalog: &ShellCatalog, base_env: &BaseEnv) -> Outcome {
    let mut cmd = match build_command(rule, catalog, base_env) {
        Ok(c) => c,
        Err(e) => return Outcome::error(e),
    };

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::error(format!("failed to start process: {e}")),
    };

    match child.wait_with_output().await {
        Ok(out) => Outcome {
            exit_code: out.status.code(),
            success: out.status.success(),
            stdout: clamp(String::from_utf8_lossy(&out.stdout).into_owned()),
            stderr: clamp(String::from_utf8_lossy(&out.stderr).into_owned()),
        },
        Err(e) => Outcome::error(format!("process error: {e}")),
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
}
