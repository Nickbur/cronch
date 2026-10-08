//! Login-environment resolution.
//!
//! A tray daemon started at login can inherit a minimal environment — most
//! painfully on macOS, where a LaunchAgent gets a bare `PATH` and commands like
//! `node`/`python`/`git` fail even though they work in a terminal. We resolve
//! the user's real login environment once at startup and hand it to every job.

use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct BaseEnv {
    vars: Vec<(String, String)>,
}

impl BaseEnv {
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.vars.iter().map(|(k, v)| (k, v))
    }

    pub fn resolve() -> BaseEnv {
        BaseEnv {
            vars: resolve_platform(),
        }
    }
}

fn lossy_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
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

    // Start from the current process environment (lossy: a non-UTF-8 value
    // must not panic the app).
    let mut map: HashMap<String, String> = lossy_env().into_iter().collect();

    // Ask the user's login shell to print its environment, which fixes the
    // minimal-PATH trap for GUI/LaunchAgent-started processes. Run it on a
    // helper thread with a timeout so a hung shell init cannot freeze startup.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    for (k, v) in query_login_env(&shell) {
        map.insert(k, v);
    }

    map.into_iter().collect()
}

/// Run `$SHELL -l -c env` on a helper thread, bounded by a timeout so a slow
/// or hanging login shell cannot block app startup.
#[cfg(not(windows))]
fn query_login_env(shell: &str) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;

    let mut out = HashMap::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let shell = shell.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(
            std::process::Command::new(&shell)
                .arg("-l")
                .arg("-c")
                .arg("env")
                .output(),
        );
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    out.insert(k.to_string(), v.to_string());
                }
            }
        }
        _ => {}
    }
    out
}
