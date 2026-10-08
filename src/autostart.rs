//! Login autostart via the OS mechanism (Run registry key on Windows,
//! LaunchAgent on macOS).

use anyhow::{Context, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder};

/// Passed when Cronch is launched at login so it starts hidden in the tray.
pub const MINIMIZED_ARG: &str = "--minimized";

fn builder() -> Result<AutoLaunch> {
    let exe = std::env::current_exe().context("resolve current exe path")?;
    let exe = exe.to_string_lossy().to_string();
    let mut b = AutoLaunchBuilder::new();
    b.set_app_name(crate::config::APP_NAME);
    b.set_app_path(&exe);
    b.set_args(&[MINIMIZED_ARG]);
    #[cfg(target_os = "macos")]
    {
        b.set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent);
    }
    b.build().context("build auto-launch config")
}

pub fn enable() -> Result<()> {
    builder()?.enable().context("enable login autostart")
}

pub fn disable() -> Result<()> {
    builder()?.disable().context("disable login autostart")
}

pub fn is_enabled() -> bool {
    builder()
        .and_then(|a| a.is_enabled().map_err(Into::into))
        .unwrap_or(false)
}
