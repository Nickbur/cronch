//! Filesystem locations and app identity.

use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};
use std::path::PathBuf;

pub const APP_NAME: &str = "Cronch";
pub const QUALIFIER: &str = "net";
pub const ORGANIZATION: &str = "burakov";
/// Stable id used for the single-instance guard.
pub const SINGLE_INSTANCE_ID: &str = "net.burakov.cronch.instance";

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APP_NAME)
        .context("cannot resolve platform project directories")
}

pub fn db_path(dirs: &ProjectDirs) -> Result<PathBuf> {
    let dir = dirs.data_dir();
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create data dir {}", dir.display()))?;
    Ok(dir.join("cronch.db"))
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
