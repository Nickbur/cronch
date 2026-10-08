//! Filesystem locations and app identity.
//!
//! Debug builds run as a separate "Cronch-Dev" app — their own data folder,
//! database, log, login item and single-instance lock — so developing never
//! touches an installed Cronch.

use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};
use std::path::{Path, PathBuf};

#[cfg(not(debug_assertions))]
pub const APP_NAME: &str = "Cronch";
#[cfg(debug_assertions)]
pub const APP_NAME: &str = "Cronch-Dev";

pub const QUALIFIER: &str = "net";
pub const ORGANIZATION: &str = "burakov";

/// Stable id used for the single-instance guard (see [`single_instance_id`]).
#[cfg(not(debug_assertions))]
const SINGLE_INSTANCE_ID: &str = "net.burakov.cronch.instance";
#[cfg(debug_assertions)]
const SINGLE_INSTANCE_ID: &str = "net.burakov.cronch-dev.instance";

/// Whether the first run turns on launch-at-login. Release builds only: a dev
/// build must never register itself as a login item on its own.
pub const AUTOSTART_ON_FIRST_RUN: bool = !cfg!(debug_assertions);

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APP_NAME)
        .context("cannot resolve platform project directories")
}

/// The per-user data folder (database, log), created if missing.
pub fn data_dir(dirs: &ProjectDirs) -> Result<PathBuf> {
    let dir = dirs.data_dir();
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create data dir {}", dir.display()))?;
    Ok(dir.to_path_buf())
}

/// The single-instance guard's name. On macOS the guard locks a *file* at this
/// path, so it must be absolute: a bare name would land in the current
/// directory, which is the read-only `/` when Cronch is started at login or
/// from Finder. Elsewhere it names a mutex (Windows) or an abstract socket
/// (Linux), where a plain id is required.
pub fn single_instance_id(data_dir: &Path) -> String {
    if cfg!(target_os = "macos") {
        data_dir
            .join("cronch.instance")
            .to_string_lossy()
            .into_owned()
    } else {
        SINGLE_INSTANCE_ID.to_string()
    }
}

pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cronch.db")
}

/// Log file written by release builds (debug builds log to the console).
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cronch.log")
}

/// Flag file a second launch creates to ask the running instance to show its
/// window.
pub fn show_request_path(data_dir: &Path) -> PathBuf {
    data_dir.join("show-window.request")
}

/// The current user's home directory (fallback-safe).
pub fn home_dir() -> PathBuf {
    if let Some(base) = BaseDirs::new() {
        return base.home_dir().to_path_buf();
    }
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_id_never_depends_on_the_working_directory() {
        let data_dir = std::env::temp_dir().join("cronch-test-data");
        let id = single_instance_id(&data_dir);
        if cfg!(target_os = "macos") {
            assert!(
                Path::new(&id).is_absolute(),
                "the lock file must not land in the current directory: {id}"
            );
        } else {
            assert!(
                !id.contains(['/', '\\']),
                "mutex/socket names must be plain ids: {id}"
            );
        }
    }
}
