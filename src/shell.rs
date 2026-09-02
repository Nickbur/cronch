//! Per-machine shell auto-detection and invocation templates.
//!
//! Each detected shell carries an `arg_template` whose `{cmd}` placeholder is
//! replaced by the rule's command **as a single argument** at execution time.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ShellInfo {
    /// Stable key stored on rules (e.g. "powershell", "cmd", "zsh").
    pub key: String,
    /// Human label for the UI.
    pub label: String,
    /// Absolute path to the shell binary.
    pub path: String,
    /// Argument template containing the `{cmd}` placeholder.
    pub arg_template: String,
}

#[derive(Clone, Debug, Default)]
pub struct ShellCatalog {
    pub shells: Vec<ShellInfo>,
}

impl ShellCatalog {
    pub fn get(&self, key: &str) -> Option<&ShellInfo> {
        self.shells.iter().find(|s| s.key == key)
    }

    pub fn detect() -> ShellCatalog {
        ShellCatalog {
            shells: detect_platform(),
        }
    }
}

/// Search PATH for an executable, returning its absolute path.
fn which(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn first_existing(paths: &[&str]) -> Option<PathBuf> {
    paths
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .map(|p| p.to_path_buf())
}

#[cfg(windows)]
fn detect_platform() -> Vec<ShellInfo> {
    let mut out = Vec::new();
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());

    // Command Prompt
    let cmd = std::env::var("ComSpec")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| first_existing(&[&format!("{sysroot}\\System32\\cmd.exe")]));
    if let Some(p) = cmd {
        out.push(ShellInfo {
            key: "cmd".into(),
            label: "Command Prompt (cmd)".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "/C {cmd}".into(),
        });
    }

    // Windows PowerShell (5.x)
    if let Some(p) = first_existing(&[&format!(
        "{sysroot}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
    )])
    .or_else(|| which("powershell.exe"))
    {
        out.push(ShellInfo {
            key: "powershell".into(),
            label: "Windows PowerShell".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-NoProfile -Command {cmd}".into(),
        });
    }

    // PowerShell 7+
    if let Some(p) = which("pwsh.exe") {
        out.push(ShellInfo {
            key: "pwsh".into(),
            label: "PowerShell 7+".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-NoProfile -Command {cmd}".into(),
        });
    }

    // Git Bash
    if let Some(p) = first_existing(&[
        "C:\\Program Files\\Git\\bin\\bash.exe",
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
    ])
    .or_else(|| which("bash.exe"))
    {
        out.push(ShellInfo {
            key: "gitbash".into(),
            label: "Git Bash".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-c {cmd}".into(),
        });
    }

    // WSL
    if let Some(p) = which("wsl.exe") {
        out.push(ShellInfo {
            key: "wsl".into(),
            label: "WSL (bash)".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-e bash -c {cmd}".into(),
        });
    }

    out
}

#[cfg(not(windows))]
fn detect_platform() -> Vec<ShellInfo> {
    let mut out = Vec::new();

    let candidates = [
        ("zsh", "Zsh", "/bin/zsh"),
        ("bash", "Bash", "/bin/bash"),
        ("sh", "sh", "/bin/sh"),
    ];
    for (key, label, path) in candidates {
        let resolved = first_existing(&[path]).or_else(|| which(key));
        if let Some(p) = resolved {
            out.push(ShellInfo {
                key: key.into(),
                label: label.into(),
                path: p.to_string_lossy().into_owned(),
                arg_template: "-c {cmd}".into(),
            });
        }
    }

    if let Some(p) = which("fish") {
        out.push(ShellInfo {
            key: "fish".into(),
            label: "fish".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-c {cmd}".into(),
        });
    }

    out
}
