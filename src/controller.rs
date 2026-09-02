//! Glue between the Slint UI and the core (store + engine).

use crate::autostart;
use crate::model::{EnvVar, OverlapPolicy, Rule, Schedule, ShellKind};
use crate::scheduler::EngineHandle;
use crate::shell::ShellCatalog;
use crate::storage::Store;
use crate::{MainWindow, RuleRow, RunRow};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

struct ShellOptions {
    labels: Vec<SharedString>,
    keys: Vec<String>,
    custom_index: i32,
    direct_index: i32,
}

fn shell_options(catalog: &ShellCatalog) -> ShellOptions {
    let mut labels: Vec<SharedString> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for s in &catalog.shells {
        labels.push(s.label.clone().into());
        keys.push(s.key.clone());
    }
    let custom_index = labels.len() as i32;
    labels.push("Custom…".into());
    let direct_index = labels.len() as i32;
    labels.push("Direct (no shell)".into());
    ShellOptions {
        labels,
        keys,
        custom_index,
        direct_index,
    }
}

fn fmt_dt(dt: Option<DateTime<Utc>>, fmt: &str) -> String {
    match dt {
        Some(d) => d.with_timezone(&Local).format(fmt).to_string(),
        None => "—".into(),
    }
}

fn build_rule_rows(store: &Store, catalog: &ShellCatalog) -> Vec<RuleRow> {
    store
        .list_rules()
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let shell_missing =
                matches!(&r.shell, ShellKind::Detected { key } if catalog.get(key).is_none());
            RuleRow {
                id: r.id.to_string().into(),
                name: r.name.clone().into(),
                schedule: r.schedule.describe().into(),
                shell: r.shell.summary().into(),
                status: r.last_status.as_str().into(),
                enabled: r.enabled,
                last_run: fmt_dt(r.last_run, "%m-%d %H:%M").into(),
                next_run: if r.enabled {
                    fmt_dt(r.next_run, "%m-%d %H:%M").into()
                } else {
                    SharedString::from("—")
                },
                shell_missing,
            }
        })
        .collect()
}

fn build_run_rows(store: &Store, rule_id: Uuid) -> Vec<RunRow> {
    store
        .list_runs(rule_id, 200)
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let status = if r.finished_at.is_none() {
                "Running"
            } else if r.success {
                "Success"
            } else {
                "Failed"
            };
            RunRow {
                id: r.id.to_string().into(),
                started: r.started_at.with_timezone(&Local).format("%m-%d %H:%M:%S").to_string().into(),
                finished: fmt_dt(r.finished_at, "%H:%M:%S").into(),
                status: status.into(),
                exit_code: r.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "—".into()).into(),
                trigger: r.trigger.clone().into(),
                stdout: r.stdout.clone().into(),
                stderr: r.stderr.clone().into(),
                success: r.success,
            }
        })
        .collect()
}

fn parse_env(text: &str) -> Vec<EnvVar> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            Some(EnvVar {
                key: k.trim().to_string(),
                value: v.to_string(),
            })
        })
        .collect()
}

fn env_to_text(env: &[EnvVar]) -> String {
    env.iter()
        .map(|e| format!("{}={}", e.key, e.value))
        .collect::<Vec<_>>()
        .join("\n")
}

fn secs_to_value_unit(secs: i64) -> (i32, i32) {
    if secs % 86400 == 0 {
        ((secs / 86400) as i32, 3)
    } else if secs % 3600 == 0 {
        ((secs / 3600) as i32, 2)
    } else if secs % 60 == 0 {
        ((secs / 60) as i32, 1)
    } else {
        (secs as i32, 0)
    }
}

fn build_schedule(ui: &MainWindow) -> Result<Schedule, String> {
    match ui.get_edit_schedule_mode() {
        0 => {
            let v = ui.get_edit_interval_value() as i64;
            let mult = match ui.get_edit_interval_unit() {
                1 => 60,
                2 => 3600,
                3 => 86400,
                _ => 1,
            };
            let seconds = v * mult;
            if seconds < 1 {
                Err("Interval must be at least 1 second".into())
            } else {
                Ok(Schedule::Interval { seconds })
            }
        }
        1 => {
            let s = Schedule::Cron {
                expr: ui.get_edit_cron().to_string(),
            };
            s.validate()?;
            Ok(s)
        }
        2 => {
            let raw = ui.get_edit_once().to_string();
            let naive = NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%d %H:%M")
                .map_err(|_| "Once time must be in the form YYYY-MM-DD HH:MM".to_string())?;
            let local = Local
                .from_local_datetime(&naive)
                .single()
                .ok_or_else(|| "Ambiguous or invalid local time".to_string())?;
            Ok(Schedule::Once {
                at: local.with_timezone(&Utc),
            })
        }
        _ => Err("Unknown schedule mode".into()),
    }
}

fn build_shell(ui: &MainWindow, opts: &ShellOptions) -> Result<ShellKind, String> {
    let idx = ui.get_edit_shell_index();
    if idx == opts.custom_index {
        Ok(ShellKind::Custom {
            path: ui.get_edit_custom_path().to_string(),
            arg_template: ui.get_edit_custom_arg().to_string(),
        })
    } else if idx == opts.direct_index {
        Ok(ShellKind::Direct)
    } else {
        opts.keys
            .get(idx as usize)
            .map(|k| ShellKind::Detected { key: k.clone() })
            .ok_or_else(|| "Please choose a shell".to_string())
    }
}

fn refresh_rules(ui: &MainWindow, store: &Store, catalog: &ShellCatalog, engine: &EngineHandle) {
    let rows = build_rule_rows(store, catalog);
    ui.set_rules(ModelRc::new(VecModel::from(rows)));
    ui.set_paused(engine.is_paused());
}

/// Wire up all callbacks + data. Returns the refresh timer (keep it alive).
pub fn setup(
    ui: &MainWindow,
    store: Store,
    engine: EngineHandle,
    catalog: Arc<ShellCatalog>,
) -> Timer {
    let opts = Rc::new(shell_options(&catalog));
    ui.set_shell_options(ModelRc::new(VecModel::from(opts.labels.clone())));
    ui.set_custom_shell_index(opts.custom_index);
    ui.set_direct_shell_index(opts.direct_index);

    let current_logs_rule: Rc<RefCell<Option<Uuid>>> = Rc::new(RefCell::new(None));

    refresh_rules(ui, &store, &catalog, &engine);

    // --- Add ---
    {
        let weak = ui.as_weak();
        ui.on_request_add(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_edit_id("".into());
            ui.set_edit_title("New rule".into());
            ui.set_edit_name("".into());
            ui.set_edit_enabled(true);
            ui.set_edit_command("".into());
            ui.set_edit_shell_index(0);
            ui.set_edit_custom_path("".into());
            ui.set_edit_custom_arg("-c {cmd}".into());
            ui.set_edit_schedule_mode(0);
            ui.set_edit_interval_value(5);
            ui.set_edit_interval_unit(1);
            ui.set_edit_cron("0 9 * * *".into());
            ui.set_edit_once("".into());
            ui.set_edit_catch_up(true);
            ui.set_edit_overlap(0);
            ui.set_edit_working_dir("".into());
            ui.set_edit_env("".into());
            ui.set_edit_error("".into());
            ui.set_active_view(1);
        });
    }

    // --- Edit ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let opts = opts.clone();
        ui.on_request_edit(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else { return };
            let Ok(Some(rule)) = store.get_rule(uuid) else { return };

            ui.set_edit_id(id);
            ui.set_edit_title("Edit rule".into());
            ui.set_edit_name(rule.name.clone().into());
            ui.set_edit_enabled(rule.enabled);
            ui.set_edit_command(rule.command.clone().into());
            ui.set_edit_custom_path("".into());
            ui.set_edit_custom_arg("-c {cmd}".into());
            match &rule.shell {
                ShellKind::Detected { key } => {
                    let idx = opts
                        .keys
                        .iter()
                        .position(|k| k == key)
                        .map(|p| p as i32)
                        .unwrap_or(opts.direct_index);
                    ui.set_edit_shell_index(idx);
                }
                ShellKind::Custom { path, arg_template } => {
                    ui.set_edit_shell_index(opts.custom_index);
                    ui.set_edit_custom_path(path.clone().into());
                    ui.set_edit_custom_arg(arg_template.clone().into());
                }
                ShellKind::Direct => ui.set_edit_shell_index(opts.direct_index),
            }
            match &rule.schedule {
                Schedule::Interval { seconds } => {
                    ui.set_edit_schedule_mode(0);
                    let (v, u) = secs_to_value_unit(*seconds);
                    ui.set_edit_interval_value(v);
                    ui.set_edit_interval_unit(u);
                }
                Schedule::Cron { expr } => {
                    ui.set_edit_schedule_mode(1);
                    ui.set_edit_cron(expr.clone().into());
                }
                Schedule::Once { at } => {
                    ui.set_edit_schedule_mode(2);
                    ui.set_edit_once(
                        at.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string().into(),
                    );
                }
            }
            ui.set_edit_catch_up(rule.catch_up);
            ui.set_edit_overlap(match rule.overlap {
                OverlapPolicy::Skip => 0,
                OverlapPolicy::Queue => 1,
                OverlapPolicy::Parallel => 2,
            });
            ui.set_edit_working_dir(rule.working_dir.clone().unwrap_or_default().into());
            ui.set_edit_env(env_to_text(&rule.env).into());
            ui.set_edit_error("".into());
            ui.set_active_view(1);
        });
    }

    // --- Save rule ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let engine = engine.clone();
        let catalog = catalog.clone();
        let opts = opts.clone();
        ui.on_save_rule(move || {
            let Some(ui) = weak.upgrade() else { return };
            let shell = match build_shell(&ui, &opts) {
                Ok(s) => s,
                Err(e) => {
                    ui.set_edit_error(e.into());
                    return;
                }
            };
            let schedule = match build_schedule(&ui) {
                Ok(s) => s,
                Err(e) => {
                    ui.set_edit_error(e.into());
                    return;
                }
            };

            let id_str = ui.get_edit_id().to_string();
            let mut rule = if id_str.is_empty() {
                Rule::new(
                    String::new(),
                    String::new(),
                    ShellKind::Direct,
                    Schedule::Interval { seconds: 1 },
                )
            } else {
                match Uuid::parse_str(&id_str)
                    .ok()
                    .and_then(|u| store.get_rule(u).ok().flatten())
                {
                    Some(r) => r,
                    None => Rule::new(
                        String::new(),
                        String::new(),
                        ShellKind::Direct,
                        Schedule::Interval { seconds: 1 },
                    ),
                }
            };

            rule.name = ui.get_edit_name().to_string();
            rule.enabled = ui.get_edit_enabled();
            rule.command = ui.get_edit_command().to_string();
            rule.shell = shell;
            rule.schedule = schedule;
            rule.catch_up = ui.get_edit_catch_up();
            rule.overlap = match ui.get_edit_overlap() {
                1 => OverlapPolicy::Queue,
                2 => OverlapPolicy::Parallel,
                _ => OverlapPolicy::Skip,
            };
            let wd = ui.get_edit_working_dir().to_string();
            rule.working_dir = if wd.trim().is_empty() { None } else { Some(wd) };
            rule.env = parse_env(&ui.get_edit_env());

            if let Err(e) = rule.validate() {
                ui.set_edit_error(e.into());
                return;
            }
            if let Err(e) = store.upsert_rule(&rule) {
                ui.set_edit_error(format!("Save failed: {e}").into());
                return;
            }
            engine.reload_rule(rule.id);
            ui.set_edit_error("".into());
            ui.set_active_view(0);
            refresh_rules(&ui, &store, &catalog, &engine);
        });
    }

    // --- Cancel ---
    {
        let weak = ui.as_weak();
        ui.on_cancel_edit(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_edit_error("".into());
                ui.set_active_view(0);
            }
        });
    }

    // --- Delete ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let engine = engine.clone();
        let catalog = catalog.clone();
        ui.on_request_delete(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else { return };
            let confirm = rfd::MessageDialog::new()
                .set_title("Delete rule")
                .set_description("Delete this rule and its run history? This cannot be undone.")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            if confirm != rfd::MessageDialogResult::Yes {
                return;
            }
            let _ = store.delete_rule(uuid);
            engine.remove_rule(uuid);
            refresh_rules(&ui, &store, &catalog, &engine);
        });
    }

    // --- Run now ---
    {
        let engine = engine.clone();
        ui.on_request_run_now(move |id| {
            if let Ok(uuid) = Uuid::parse_str(id.as_str()) {
                engine.run_now(uuid);
            }
        });
    }

    // --- Toggle enabled ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let engine = engine.clone();
        let catalog = catalog.clone();
        ui.on_request_toggle_enabled(move |id, enabled| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else { return };
            let _ = store.set_enabled(uuid, enabled);
            engine.reload_rule(uuid);
            refresh_rules(&ui, &store, &catalog, &engine);
        });
    }

    // --- Open logs ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let current = current_logs_rule.clone();
        ui.on_request_open_logs(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else { return };
            *current.borrow_mut() = Some(uuid);
            let name = store
                .get_rule(uuid)
                .ok()
                .flatten()
                .map(|r| r.name)
                .unwrap_or_default();
            ui.set_logs_rule_name(name.into());
            ui.set_selected_run_index(0);
            ui.set_runs(ModelRc::new(VecModel::from(build_run_rows(&store, uuid))));
            ui.set_active_view(2);
        });
    }

    // --- Settings open ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        ui.on_request_settings(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_setting_autostart(autostart::is_enabled());
            let days = store
                .setting_get("history_retention_days")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(30);
            ui.set_setting_retention_days(days);
            ui.set_settings_status("".into());
            ui.set_active_view(3);
        });
    }

    // --- Save settings ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        ui.on_save_settings(move || {
            let Some(ui) = weak.upgrade() else { return };
            let want = ui.get_setting_autostart();
            let msg = if want {
                match autostart::enable() {
                    Ok(_) => "Saved. Launch at login is on.",
                    Err(_) => "Saved, but enabling autostart failed.",
                }
            } else {
                let _ = autostart::disable();
                "Saved. Launch at login is off."
            };
            let _ = store.setting_set("autostart", if want { "1" } else { "0" });
            let days = ui.get_setting_retention_days();
            let _ = store.setting_set("history_retention_days", &days.to_string());
            let _ = store.prune_history(days as i64);
            ui.set_settings_status(msg.into());
        });
    }

    // --- Export ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        ui.on_export_rules(move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("cronch-rules.json")
                .add_filter("JSON", &["json"])
                .save_file()
            {
                match store.list_rules() {
                    Ok(rules) => match serde_json::to_vec_pretty(&rules) {
                        Ok(bytes) => {
                            if std::fs::write(&path, bytes).is_ok() {
                                ui.set_settings_status(format!("Exported {} rules.", rules.len()).into());
                            } else {
                                ui.set_settings_status("Export failed to write file.".into());
                            }
                        }
                        Err(e) => ui.set_settings_status(format!("Export failed: {e}").into()),
                    },
                    Err(e) => ui.set_settings_status(format!("Export failed: {e}").into()),
                }
            }
        });
    }

    // --- Import ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let engine = engine.clone();
        let catalog = catalog.clone();
        ui.on_import_rules(move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(path) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {
                match std::fs::read(&path) {
                    Ok(bytes) => match serde_json::from_slice::<Vec<Rule>>(&bytes) {
                        Ok(rules) => {
                            for r in &rules {
                                let _ = store.upsert_rule(r);
                            }
                            engine.reload_all();
                            refresh_rules(&ui, &store, &catalog, &engine);
                            ui.set_settings_status(format!("Imported {} rules.", rules.len()).into());
                        }
                        Err(e) => ui.set_settings_status(format!("Import failed: {e}").into()),
                    },
                    Err(e) => ui.set_settings_status(format!("Import failed: {e}").into()),
                }
            }
        });
    }

    // --- Pause / Resume from the header ---
    {
        let weak = ui.as_weak();
        let engine = engine.clone();
        ui.on_pause_all(move || {
            engine.pause_all();
            if let Some(ui) = weak.upgrade() {
                ui.set_paused(true);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let engine = engine.clone();
        ui.on_resume_all(move || {
            engine.resume_all();
            if let Some(ui) = weak.upgrade() {
                ui.set_paused(false);
            }
        });
    }

    // --- First-run dismiss ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        ui.on_dismiss_first_run(move || {
            let _ = store.setting_set("first_run_seen", "1");
            if let Some(ui) = weak.upgrade() {
                ui.set_show_first_run(false);
            }
        });
    }

    // --- Periodic refresh (live statuses + logs) ---
    let timer = Timer::default();
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let engine = engine.clone();
        let catalog = catalog.clone();
        let current = current_logs_rule.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(1000), move || {
            let Some(ui) = weak.upgrade() else { return };
            let view = ui.get_active_view();
            if view == 0 {
                refresh_rules(&ui, &store, &catalog, &engine);
            } else {
                // keep the paused indicator fresh everywhere
                ui.set_paused(engine.is_paused());
            }
            if view == 2 {
                if let Some(uuid) = *current.borrow() {
                    ui.set_runs(ModelRc::new(VecModel::from(build_run_rows(&store, uuid))));
                }
            }
        });
    }
    timer
}
