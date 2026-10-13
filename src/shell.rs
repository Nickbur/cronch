//! Per-machine shell auto-detection and invocation templates.
//!
//! Each detected shell carries an `arg_template` whose `{cmd}` placeholder is
//! replaced by the rule's command **as a single argument** at execution time.

use crate::envres::BaseEnv;
use std::ffi::OsStr;
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

    /// Detect installed shells, searching the PATH of the resolved login
    /// environment (the one jobs run with) rather than this process's own,
    /// which can be minimal when Cronch was started at login.
    pub fn detect(base_env: &BaseEnv) -> ShellCatalog {
        let path = base_env.get("PATH").map(OsStr::new);
        ShellCatalog {
            shells: detect_platform(path),
        }
    }
}

/// True if `path` is a regular file we may execute. On Unix this checks the
/// executable bit; on Windows the extension (PATHEXT) is what matters, and the
/// callers pass explicit `.exe` names.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Search `path_var` (a PATH-style list) for an executable, returning its
/// absolute path.
fn which(path_var: Option<&OsStr>, program: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var?) {
        let candidate = dir.join(program);
        if is_executable(&candidate) {
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

/// The Git for Windows install that `exe` (a `git.exe` or `bash.exe` found on
/// PATH) belongs to, if any. Git puts `<root>\cmd`, `<root>\bin`,
/// `<root>\usr\bin` or `<root>\mingw64\bin` on PATH, so the root is one or two
/// levels above the binary's folder; it is recognised by its own `git.exe`
/// and `bash.exe`. `C:\Windows\System32\bash.exe` (the WSL launcher) has no
/// such root.
#[cfg(any(windows, test))]
fn git_install_root(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .skip(2)
        .take(2)
        .find(|root| {
            root.join("cmd").join("git.exe").is_file()
                && root.join("bin").join("bash.exe").is_file()
        })
        .map(Path::to_path_buf)
}

/// Git Bash: a standard install, else the Git install that `git.exe` or
/// `bash.exe` on PATH belongs to. A `bash.exe` from anywhere else is never
/// taken, so WSL is not offered as Git Bash.
#[cfg(windows)]
fn find_git_bash(path: Option<&OsStr>) -> Option<PathBuf> {
    first_existing(&[
        "C:\\Program Files\\Git\\bin\\bash.exe",
        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
    ])
    .or_else(|| {
        ["git.exe", "bash.exe"]
            .into_iter()
            .filter_map(|program| which(path, program))
            .find_map(|exe| git_install_root(&exe))
            .map(|root| root.join("bin").join("bash.exe"))
    })
}

#[cfg(windows)]
fn detect_platform(path: Option<&OsStr>) -> Vec<ShellInfo> {
    let mut out = Vec::new();
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());

    // Command Prompt. `/S` makes cmd strip exactly the outer quotes the
    // executor wraps the command in, so the command runs verbatim.
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
            arg_template: "/S /C {cmd}".into(),
        });
    }

    // Windows PowerShell (5.x)
    if let Some(p) = first_existing(&[&format!(
        "{sysroot}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
    )])
    .or_else(|| which(path, "powershell.exe"))
    {
        out.push(ShellInfo {
            key: "powershell".into(),
            label: "Windows PowerShell".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-NoProfile -Command {cmd}".into(),
        });
    }

    // PowerShell 7+
    if let Some(p) = which(path, "pwsh.exe") {
        out.push(ShellInfo {
            key: "pwsh".into(),
            label: "PowerShell 7+".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-NoProfile -Command {cmd}".into(),
        });
    }

    // Git Bash
    if let Some(p) = find_git_bash(path) {
        out.push(ShellInfo {
            key: "gitbash".into(),
            label: "Git Bash".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-c {cmd}".into(),
        });
    }

    // WSL
    if let Some(p) = which(path, "wsl.exe") {
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
fn detect_platform(path: Option<&OsStr>) -> Vec<ShellInfo> {
    let mut out = Vec::new();

    let candidates = [
        ("zsh", "Zsh", "/bin/zsh"),
        ("bash", "Bash", "/bin/bash"),
        ("sh", "sh", "/bin/sh"),
    ];
    for (key, label, default_path) in candidates {
        let resolved = first_existing(&[default_path]).or_else(|| which(path, key));
        if let Some(p) = resolved {
            out.push(ShellInfo {
                key: key.into(),
                label: label.into(),
                path: p.to_string_lossy().into_owned(),
                arg_template: "-c {cmd}".into(),
            });
        }
    }

    if let Some(p) = which(path, "fish") {
        out.push(ShellInfo {
            key: "fish".into(),
            label: "fish".into(),
            path: p.to_string_lossy().into_owned(),
            arg_template: "-c {cmd}".into(),
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create an empty file (and its folders) at `root/parts…`.
    fn touch(root: &Path, parts: &[&str]) -> PathBuf {
        let path = parts
            .iter()
            .fold(root.to_path_buf(), |p, part| p.join(part));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn git_bash_only_comes_from_a_git_install() {
        let dir = std::env::temp_dir().join(format!("cronch-shell-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let git = dir.join("Git");
        let on_path = [
            touch(&git, &["cmd", "git.exe"]),
            touch(&git, &["bin", "bash.exe"]),
            touch(&git, &["usr", "bin", "bash.exe"]),
        ];
        let wsl_bash = touch(&dir, &["Windows", "System32", "bash.exe"]);

        let roots = on_path.map(|exe| git_install_root(&exe));
        let wsl_root = git_install_root(&wsl_bash);
        let _ = std::fs::remove_dir_all(&dir);
        for root in roots {
            assert_eq!(root.as_deref(), Some(git.as_path()));
        }
        assert_eq!(wsl_root, None, "WSL's bash.exe is not Git Bash");
    }
}
