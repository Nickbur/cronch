#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod controller;
mod envres;
mod executor;
mod icon;
mod model;
mod scheduler;
mod shell;
mod storage;
mod tray;

slint::include_modules!();

use anyhow::Result;
use slint::{ComponentHandle, TimerMode};
use std::sync::Arc;
use std::time::Duration;
use tray_icon::menu::MenuEvent;
use tray_icon::TrayIconEvent;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Only one Cronch daemon at a time.
    let instance = single_instance::SingleInstance::new(config::SINGLE_INSTANCE_ID)?;
    if !instance.is_single() {
        log::warn!("another instance of Cronch is already running; exiting");
        return Ok(());
    }

    let dirs = config::project_dirs()?;
    let db = config::db_path(&dirs)?;
    let store = storage::Store::open(&db)?;
    let catalog = Arc::new(shell::ShellCatalog::detect());
    let base_env = Arc::new(envres::BaseEnv::resolve());
    log::info!(
        "detected shells: {:?}",
        catalog.shells.iter().map(|s| &s.key).collect::<Vec<_>>()
    );

    // First run: enable login autostart by default (disclosed in the UI banner).
    let first_run = store.setting_get("first_run_seen")?.is_none();
    if first_run {
        if let Err(e) = autostart::enable() {
            log::warn!("could not enable autostart on first run: {e}");
        }
        let _ = store.setting_set("autostart", "1");
        let _ = store.setting_set("history_retention_days", "30");
    }

    // Scheduler engine on its own thread with a tokio runtime.
    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    {
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
            })?;
    }
    let engine = handle_rx.recv().expect("receive engine handle");
    engine.startup_load();

    // UI.
    let ui = MainWindow::new()?;
    ui.set_show_first_run(first_run);
    let _refresh_timer = controller::setup(&ui, store.clone(), engine.clone(), catalog.clone());

    // Close button hides to tray instead of quitting.
    ui.window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);

    // Tray icon + menu, polled on the main thread.
    let tray = tray::Tray::build()?;
    let tray_timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let engine = engine.clone();
        tray_timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                let id = ev.id();
                if id == &tray.id_add {
                    if let Some(ui) = weak.upgrade() {
                        let _ = ui.show();
                        ui.invoke_request_add();
                    }
                } else if id == &tray.id_open {
                    if let Some(ui) = weak.upgrade() {
                        let _ = ui.show();
                        ui.set_active_view(0);
                    }
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
                if let TrayIconEvent::Click { .. } = ev {
                    if let Some(ui) = weak.upgrade() {
                        let _ = ui.show();
                        ui.set_active_view(0);
                    }
                }
            }
            tray.set_paused(engine.is_paused());
        });
    }

    // Start hidden when launched at login (unless it's the very first run).
    let minimized = std::env::args().any(|a| a == autostart::MINIMIZED_ARG);
    if !(minimized && !first_run) {
        ui.show()?;
    }

    slint::run_event_loop()?;
    engine.shutdown();
    Ok(())
}
