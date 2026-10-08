#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod controller;
mod envres;
mod executor;
mod icon;
mod model;
mod reopen;
mod scheduler;
mod shell;
mod storage;
mod tray;

slint::include_modules!();

use anyhow::Result;
use slint::{ComponentHandle, TimerMode};
use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tray_icon::TrayIconEvent;
use tray_icon::menu::MenuEvent;

/// The log file is rotated (one previous file kept) once it passes this size.
const LOG_ROTATE_BYTES: u64 = 1024 * 1024;

/// Append-only log file that rotates itself to `<name>.1` once it passes
/// [`LOG_ROTATE_BYTES`], so even a long-running instance stays bounded.
struct RotatingLog {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

impl RotatingLog {
    fn open(path: PathBuf) -> std::io::Result<RotatingLog> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(RotatingLog {
            path,
            file: Some(file),
            written,
        })
    }

    fn rotate(&mut self) {
        // Close first: Windows cannot rename a file that is still open.
        self.file = None;
        let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        self.written = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written >= LOG_ROTATE_BYTES {
            self.rotate();
        }
        match &mut self.file {
            Some(f) => {
                let n = f.write(buf)?;
                self.written += n as u64;
                Ok(n)
            }
            // Logging must never take the app down: drop the line instead.
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Debug builds log to the console. Release builds have no console (Windows)
/// or a detached one (started at login), so they log to `cronch.log` in the
/// data folder, keeping the previous file as `cronch.log.1`.
fn init_logging(data_dir: &Path) {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if !cfg!(debug_assertions) {
        let path = config::log_path(data_dir);
        match RotatingLog::open(path.clone()) {
            Ok(log) => {
                builder.target(env_logger::Target::Pipe(Box::new(log)));
            }
            Err(e) => eprintln!("cronch: cannot open log file {}: {e}", path.display()),
        }
    }
    builder.init();
    // Release builds abort on panic; record why before the process dies.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default_hook(info);
    }));
}

/// Bring the main window up on the rule list.
fn show_window(weak: &slint::Weak<MainWindow>) {
    if let Some(ui) = weak.upgrade() {
        let _ = ui.show();
        ui.set_active_view(0);
    }
}

/// Bring the window up because Cronch was opened again: a hidden window opens
/// on the rule list; a minimized or visible one comes back as it was, so an
/// edit in progress is not lost.
fn reveal_window(weak: &slint::Weak<MainWindow>) {
    let Some(ui) = weak.upgrade() else { return };
    if ui.window().is_visible() {
        ui.window().set_minimized(false);
        let _ = ui.show();
    } else {
        show_window(weak);
    }
}

fn main() -> Result<()> {
    let dirs = config::project_dirs()?;
    let data_dir = config::data_dir(&dirs)?;
    let minimized = std::env::args().any(|a| a == autostart::MINIMIZED_ARG);
    let show_request = config::show_request_path(&data_dir);

    // Only one Cronch at a time. A second launch by the user asks the running
    // instance to show its window (a duplicate login launch stays silent).
    let instance = single_instance::SingleInstance::new(&config::single_instance_id(&data_dir))?;
    if !instance.is_single() {
        if !minimized {
            let _ = std::fs::write(&show_request, b"");
        }
        return Ok(());
    }
    // A request left from before this instance started is stale.
    let _ = std::fs::remove_file(&show_request);

    init_logging(&data_dir);

    let store = storage::Store::open(&config::db_path(&data_dir))?;
    // A previous quit/crash may have left runs open and rules stuck as
    // "Running"; nothing is running at startup, so close those out.
    if let Ok(n) = store.reconcile_orphaned_runs()
        && n > 0
    {
        log::info!("reconciled {n} orphaned run records");
    }
    let base_env = Arc::new(envres::BaseEnv::resolve());
    let catalog = Arc::new(shell::ShellCatalog::detect(&base_env));
    log::info!(
        "detected shells: {:?}",
        catalog.shells.iter().map(|s| &s.key).collect::<Vec<_>>()
    );

    // First run: enable login autostart by default (disclosed in the UI banner).
    // Mark the setup as done immediately so a later launch never re-enables
    // autostart against the user's wishes (e.g. after they turned it off).
    let first_run = store.setting_get("first_run_seen")?.is_none();
    if first_run {
        if config::AUTOSTART_ON_FIRST_RUN {
            if let Err(e) = autostart::enable() {
                log::warn!("could not enable autostart on first run: {e}");
            }
        } else {
            log::info!("dev build: launch at login is not turned on automatically");
        }
        let _ = store.setting_set("first_run_seen", "1");
        let _ = store.setting_set("history_retention_days", "30");
    }

    // Scheduler engine on its own thread with a tokio runtime.
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let engine_thread = {
        let store = store.clone();
        let catalog = catalog.clone();
        let base_env = base_env.clone();
        std::thread::Builder::new()
            .name("cronch-engine".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("build tokio runtime");
                rt.block_on(async move {
                    let (handle, fut) = scheduler::create(store, catalog, base_env);
                    let _ = handle_tx.send(handle);
                    fut.await;
                });
                // Bounded, unlike dropping the runtime: on Windows a pipe still
                // held by a job's background process keeps a blocking read
                // thread busy, and an unbounded wait would hang quitting.
                rt.shutdown_timeout(Duration::from_secs(2));
            })?
    };
    let engine = handle_rx.recv().expect("receive engine handle");
    engine.startup_load();

    // UI. The banner discloses the automatic launch-at-login, so it only
    // shows when that actually happened.
    let ui = MainWindow::new()?;
    ui.set_show_first_run(first_run && config::AUTOSTART_ON_FIRST_RUN);
    let _refresh_timer = controller::setup(&ui, store.clone(), engine.clone(), catalog.clone());

    // Closing the window hides Cronch to the tray. Without a tray icon there
    // would be no way back, so in that case closing quits instead.
    let tray_ready = Rc::new(Cell::new(false));
    {
        let tray_ready = tray_ready.clone();
        let engine = engine.clone();
        ui.window().on_close_requested(move || {
            if !tray_ready.get() {
                engine.shutdown();
                let _ = slint::quit_event_loop();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    // Tray icon + menu, polled on the main thread. The icon and the macOS
    // reopen handler are set up on the first tick: both need the event loop
    // to be running by then.
    let tray_timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let engine = engine.clone();
        let mut tray: Option<tray::Tray> = None;
        let mut started = false;
        tray_timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            if !started {
                started = true;
                reopen::install();
                match tray::Tray::build() {
                    Ok(t) => {
                        tray = Some(t);
                        tray_ready.set(true);
                    }
                    Err(e) => {
                        log::error!("cannot create the tray icon: {e}");
                        // The window is then the only way in; never leave it hidden.
                        if let Some(ui) = weak.upgrade() {
                            let _ = ui.show();
                        }
                    }
                }
            }
            // Another launch of Cronch asked for the window.
            if std::fs::remove_file(&show_request).is_ok() {
                log::info!("another launch asked for the window");
                reveal_window(&weak);
            }
            // macOS: the running app was opened again (Dock icon, Finder, …).
            if reopen::take_request() {
                log::info!("Cronch was opened again; showing the window");
                reveal_window(&weak);
            }
            let Some(tray) = &tray else { return };
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                let id = ev.id();
                if id == &tray.id_add {
                    if let Some(ui) = weak.upgrade() {
                        let _ = ui.show();
                        ui.invoke_request_add();
                    }
                } else if id == &tray.id_open {
                    show_window(&weak);
                } else if id == &tray.id_toggle {
                    if engine.is_paused() {
                        engine.resume_all();
                    } else {
                        engine.pause_all();
                    }
                } else if id == &tray.id_quit {
                    engine.shutdown();
                    let _ = slint::quit_event_loop();
                }
            }
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                // Only react to left-clicks; right-click opens the context menu.
                if let TrayIconEvent::Click { button, .. } = ev
                    && button == tray_icon::MouseButton::Left
                {
                    show_window(&weak);
                }
            }
            tray.set_paused(engine.is_paused());
        });
    }

    // Start hidden when launched at login (unless it's the very first run).
    if !minimized || first_run {
        ui.show()?;
    }

    // Keep running with the window hidden: only Quit (or closing the window
    // when there is no tray icon) ends the loop.
    slint::run_event_loop_until_quit()?;
    // Stop the engine and wait for it: it broadcasts a shutdown to in-flight
    // jobs (killing their process groups) and closes their run rows before the
    // process exits, so quitting leaves nothing running behind.
    engine.shutdown();
    let _ = engine_thread.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_file_rotates_while_running() {
        let dir = std::env::temp_dir().join(format!("cronch-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cronch.log");
        let mut log = RotatingLog::open(path.clone()).unwrap();
        let line = [b'x'; 1024];
        for _ in 0..(LOG_ROTATE_BYTES / 1024 + 10) {
            log.write_all(&line).unwrap();
        }
        log.flush().unwrap();
        let rotated = std::fs::metadata(path.with_extension("log.1"))
            .unwrap()
            .len();
        let current = std::fs::metadata(&path).unwrap().len();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(rotated >= LOG_ROTATE_BYTES, "the full file moved aside");
        assert!(
            current < LOG_ROTATE_BYTES,
            "writing continues in a fresh file"
        );
    }
}
