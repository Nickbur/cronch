//! Login-environment resolution.
//!
//! A tray daemon started at login can inherit a minimal environment — most
//! painfully on macOS, where a LaunchAgent gets a bare `PATH` and commands like
//! `node`/`python`/`git` fail even though they work in a terminal. We resolve
//! the user's real login environment once at startup and hand it to every job.

#[derive(Clone, Debug, Default)]
pub struct BaseEnv {
    vars: Vec<(String, String)>,
}

impl BaseEnv {
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.vars.iter().map(|(k, v)| (k, v))
    }

    /// Look up one variable (case-insensitively on Windows, where `Path` and
    /// `PATH` name the same variable).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| {
                if cfg!(windows) {
                    k.eq_ignore_ascii_case(key)
                } else {
                    k == key
                }
            })
            .map(|(_, v)| v.as_str())
    }

    pub fn resolve() -> BaseEnv {
        BaseEnv {
            vars: resolve_platform(),
        }
    }
}

fn lossy_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

#[cfg(windows)]
fn resolve_platform() -> Vec<(String, String)> {
    // On Windows a process launched at login already carries the full user
    // environment (registry-based), so the process environment is correct.
    lossy_env()
}

#[cfg(not(windows))]
fn resolve_platform() -> Vec<(String, String)> {
    use std::collections::HashMap;
    use std::time::Duration;

    // Start from the current process environment (lossy: a non-UTF-8 value
    // must not panic the app).
    let mut map: HashMap<String, String> = lossy_env().into_iter().collect();

    // Ask the user's shell to print its environment, which fixes the
    // minimal-PATH trap for GUI/LaunchAgent-started processes. An interactive
    // login shell reads the same startup files as a terminal (`.zshrc`,
    // `.bashrc`, where tools like nvm or pyenv add themselves to PATH); if its
    // startup files hang or fail, fall back to a plain login shell. Both runs
    // are bounded by a timeout so a stuck shell cannot freeze startup.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let resolved = query_shell_env(&shell, &["-l", "-i"], Duration::from_secs(5))
        .or_else(|| query_shell_env(&shell, &["-l"], Duration::from_secs(3)));
    match resolved {
        Some(vars) => map.extend(vars),
        None => log::warn!("could not read the login environment from {shell}"),
    }

    map.into_iter().collect()
}

/// Run `shell <flags> -c` with a script that prints the environment as
/// NUL-separated `KEY=VALUE` pairs between two markers. The markers let us skip
/// anything the startup files print, and NUL separation keeps multi-line
/// values intact. Returns `None` on failure or timeout (the shell and anything
/// it started are then killed so the helper thread can wind down).
#[cfg(not(windows))]
fn query_shell_env(
    shell: &str,
    flags: &[&str],
    timeout: std::time::Duration,
) -> Option<std::collections::HashMap<String, String>> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    let marker = format!("__CRONCH_ENV_{}__", std::process::id());
    // `env -0` keeps multi-line values intact; plain `env` is the fallback
    // where `-0` is not supported.
    let script = format!(
        "printf '%s' '{marker}'; command env -0 2>/dev/null || command env; printf '%s' '{marker}'"
    );
    let mut cmd = std::process::Command::new(shell);
    cmd.args(flags)
        .arg("-c")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // A new session: no controlling terminal, so an interactive shell started
    // from a terminal (e.g. `cargo run`) cannot get stopped trying to take it
    // over; and its own process group, so a timeout can kill the shell and
    // everything it started.
    // SAFETY: `setsid` is async-signal-safe, as `pre_exec` requires.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;
    let pid = child.id();
    let mut stdout = child.stdout.take()?;

    let (tx, rx) = std::sync::mpsc::channel();
    let reader_marker = marker.clone();
    std::thread::spawn(move || {
        // Read until the closing marker rather than EOF: a background process
        // started by the startup files may keep the pipe open.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if extract_env(&buf, &reader_marker).is_some() {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(buf);
    });

    let vars = rx
        .recv_timeout(timeout)
        .ok()
        .and_then(|buf| extract_env(&buf, &marker));
    if vars.is_none() {
        // SAFETY: plain libc call; the negative pid targets only the process
        // group this function created.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    // Reap the shell (finished or just killed) without blocking startup.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    vars
}

/// Parse the environment printed between two `marker`s: NUL-separated
/// (`env -0`), or line-separated from the plain `env` fallback.
/// `None` until both markers are present (or if nothing usable was printed).
#[cfg(not(windows))]
fn extract_env(buf: &[u8], marker: &str) -> Option<std::collections::HashMap<String, String>> {
    let text = String::from_utf8_lossy(buf);
    let start = text.find(marker)? + marker.len();
    let len = text[start..].find(marker)?;
    let section = &text[start..start + len];
    let separator = if section.contains('\0') { '\0' } else { '\n' };
    let vars: std::collections::HashMap<String, String> = section
        .split(separator)
        .filter_map(|entry| entry.split_once('='))
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    (!vars.is_empty()).then_some(vars)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn extract_env_skips_noise_and_keeps_multiline_values() {
        let out = b"Welcome!\nM__A=1\0MULTI=line1\nline2\0=bad\0M__ trailing";
        let vars = extract_env(out, "M__").expect("both markers present");
        assert_eq!(vars.get("A").map(String::as_str), Some("1"));
        assert_eq!(
            vars.get("MULTI").map(String::as_str),
            Some("line1\nline2"),
            "multi-line values must survive"
        );
        assert_eq!(vars.len(), 2, "entries without a key are dropped");
    }

    #[test]
    fn extract_env_reads_the_line_separated_fallback() {
        let vars = extract_env(b"M__A=1\nB=x=y\nM__", "M__").expect("both markers present");
        assert_eq!(vars.get("A").map(String::as_str), Some("1"));
        assert_eq!(vars.get("B").map(String::as_str), Some("x=y"));
    }

    #[test]
    fn extract_env_waits_for_closing_marker() {
        assert!(extract_env(b"M__A=1\0", "M__").is_none());
    }

    #[test]
    fn resolves_a_path_from_the_login_shell() {
        let env = BaseEnv::resolve();
        assert!(env.get("PATH").is_some_and(|p| !p.is_empty()));
    }
}
