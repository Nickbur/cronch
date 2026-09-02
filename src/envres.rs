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

    pub fn resolve() -> BaseEnv {
        BaseEnv {
            vars: resolve_platform(),
        }
    }
}

#[cfg(windows)]
fn resolve_platform() -> Vec<(String, String)> {
    // On Windows a process launched at login already carries the full user
    // environment (registry-based), so the process environment is correct.
    std::env::vars().collect()
}

#[cfg(not(windows))]
fn resolve_platform() -> Vec<(String, String)> {
    use std::collections::HashMap;

    // Start from the current process environment.
    let mut map: HashMap<String, String> = std::env::vars().collect();

    // Ask the user's login shell to print its environment, which fixes the
    // minimal-PATH trap for GUI/LaunchAgent-started processes.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    if let Ok(output) = std::process::Command::new(&shell)
        .arg("-l")
        .arg("-c")
        .arg("env")
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }

    map.into_iter().collect()
}
