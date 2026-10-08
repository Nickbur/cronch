//! Glue between the Slint UI and the core (store + engine).

use crate::autostart;
use crate::model::{
    EnvVar, LastStatus, MAX_TIMEOUT_SECS, OverlapPolicy, Rule, Schedule, ShellKind,
};
use crate::scheduler::EngineHandle;
use crate::shell::ShellCatalog;
use crate::storage::Store;
use crate::{MainWindow, RuleRow, RunRow};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Date/time format of the editor's "Once" field (local time).
const ONCE_FORMAT: &str = "%Y-%m-%d %H:%M";

struct ShellOptions {
    labels: Vec<SharedString>,
    keys: Vec<String>,
    custom_index: i32,
    direct_index: i32,
}

/// The editor's shell choices: detected shells, then (when editing a rule
/// whose shell is not on this machine) that shell so saving keeps it, then
/// Custom and Direct.
fn shell_options(catalog: &ShellCatalog, missing: Option<&str>) -> ShellOptions {
    let mut labels: Vec<SharedString> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for s in &catalog.shells {
        labels.push(s.label.clone().into());
        keys.push(s.key.clone());
    }
    if let Some(key) = missing {
        labels.push(format!("{key} (not found on this machine)").into());
        keys.push(key.to_string());
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

fn apply_shell_options(ui: &MainWindow, opts: &ShellOptions) {
    ui.set_shell_options(ModelRc::new(VecModel::from(opts.labels.clone())));
    ui.set_custom_shell_index(opts.custom_index);
    ui.set_direct_shell_index(opts.direct_index);
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
            let status = match (r.finished_at, r.status) {
                (None, _) => "Running",
                (Some(_), Some(s)) => s.as_str(),
                // Runs recorded before outcomes were stored.
                (Some(_), None) if r.success => "Success",
                (Some(_), None) => "Failed",
            };
            RunRow {
                id: r.id.to_string().into(),
                started: r
                    .started_at
                    .with_timezone(&Local)
                    .format("%m-%d %H:%M:%S")
                    .to_string()
                    .into(),
                finished: fmt_dt(r.finished_at, "%H:%M:%S").into(),
                status: status.into(),
                exit_code: r
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "—".into())
                    .into(),
                trigger: r.trigger.clone().into(),
                stdout: r.stdout.clone().into(),
                stderr: r.stderr.clone().into(),
                success: r.success,
            }
        })
        .collect()
}

/// Content fingerprint of a run list, so the logs model is only rebuilt when
/// its rows actually change. Hashing every row (not just the newest) means a
/// `Parallel` run finishing out of order still triggers a redraw.
fn run_signature(rows: &[RunRow]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    rows.len().hash(&mut h);
    for r in rows {
        r.id.as_str().hash(&mut h);
        r.started.as_str().hash(&mut h);
        r.finished.as_str().hash(&mut h);
        r.status.as_str().hash(&mut h);
        r.exit_code.as_str().hash(&mut h);
        r.success.hash(&mut h);
    }
    h.finish()
}

/// Content fingerprint of the rule list (same rationale as [`run_signature`]).
fn rule_signature(rows: &[RuleRow]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    rows.len().hash(&mut h);
    for r in rows {
        r.id.as_str().hash(&mut h);
        r.name.as_str().hash(&mut h);
        r.schedule.as_str().hash(&mut h);
        r.shell.as_str().hash(&mut h);
        r.status.as_str().hash(&mut h);
        r.last_run.as_str().hash(&mut h);
        r.next_run.as_str().hash(&mut h);
        r.enabled.hash(&mut h);
        r.shell_missing.hash(&mut h);
    }
    h.finish()
}

/// Point the selection back at the selected run after the list was rebuilt
/// (new runs are inserted at the top, so a fixed index would drift onto a
/// different run). Falls back to the newest run if the selected one is gone.
fn reselect_run(ui: &MainWindow, model: &Rc<VecModel<RunRow>>, selected: &RefCell<Option<i64>>) {
    let wanted = selected.borrow().map(|id| id.to_string());
    let idx = wanted
        .and_then(|id| {
            (0..model.row_count()).find(|&i| model.row_data(i).is_some_and(|r| r.id == id))
        })
        .unwrap_or(0);
    *selected.borrow_mut() = model.row_data(idx).and_then(|r| r.id.parse().ok());
    ui.set_selected_run_index(idx as i32);
}

/// Load the captured output for the currently selected run into the model so
/// the log view shows it without pulling every row's bodies on each refresh.
fn fill_selected_run(store: &Store, ui: &MainWindow, model: &Rc<VecModel<RunRow>>) {
    let idx = ui.get_selected_run_index() as usize;
    let Some(mut row) = model.row_data(idx) else {
        return;
    };
    let Ok(id) = row.id.parse::<i64>() else {
        return;
    };
    let Ok(Some(full)) = store.get_run(id) else {
        return;
    };
    row.stdout = full.stdout.into();
    row.stderr = full.stderr.into();
    model.set_row_data(idx, row);
}

fn parse_env(text: &str) -> Vec<EnvVar> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let key = k.trim();
            if key.is_empty() {
                return None;
            }
            Some(EnvVar {
                key: key.to_string(),
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
            let naive = NaiveDateTime::parse_from_str(raw.trim(), ONCE_FORMAT)
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

fn refresh_rules(
    ui: &MainWindow,
    store: &Store,
    catalog: &ShellCatalog,
    engine: &EngineHandle,
    rules_model: &Rc<VecModel<RuleRow>>,
    rules_signature: &Rc<RefCell<Option<u64>>>,
) {
    let rows = build_rule_rows(store, catalog);
    let signature = rule_signature(&rows);
    // Only rebuild when the content changed, so a long list keeps its scroll
    // position and hover state instead of being replaced every second.
    if *rules_signature.borrow() != Some(signature) {
        *rules_signature.borrow_mut() = Some(signature);
        rules_model.set_vec(rows);
    }
    ui.set_paused(engine.is_paused());
}

/// Wire up all callbacks + data. Returns the refresh timer (keep it alive).
pub fn setup(
    ui: &MainWindow,
    store: Store,
    engine: EngineHandle,
    catalog: Arc<ShellCatalog>,
) -> Timer {
    // Rebuilt per editor session (see `shell_options`).
    let opts = Rc::new(RefCell::new(shell_options(&catalog, None)));
    apply_shell_options(ui, &opts.borrow());

    let current_logs_rule: Rc<RefCell<Option<Uuid>>> = Rc::new(RefCell::new(None));
    // The run whose output the logs view shows, tracked by id (see `reselect_run`).
    let selected_run: Rc<RefCell<Option<i64>>> = Rc::new(RefCell::new(None));
    // Content fingerprints of the last rendered lists, so a view is only
    // rebuilt when its rows actually change (avoids wiping scroll/selection and
    // the selected run's output every second).
    let rules_signature: Rc<RefCell<Option<u64>>> = Rc::new(RefCell::new(None));
    let runs_signature: Rc<RefCell<Option<u64>>> = Rc::new(RefCell::new(None));
    // When the header notice should disappear (set when the user clicks "Run now").
    let notice_until: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));

    // Persistent model instances: updates happen in place (set_vec), so the UI
    // keeps scroll/selection state instead of being re-bound every second.
    let rules_model = Rc::new(VecModel::<RuleRow>::default());
    let runs_model = Rc::new(VecModel::<RunRow>::default());
    ui.set_rules(ModelRc::new(rules_model.clone()));
    ui.set_runs(ModelRc::new(runs_model.clone()));

    refresh_rules(
        ui,
        &store,
        &catalog,
        &engine,
        &rules_model,
        &rules_signature,
    );

    // --- Add ---
    {
        let weak = ui.as_weak();
        let catalog = catalog.clone();
        let opts = opts.clone();
        ui.on_request_add(move || {
            let Some(ui) = weak.upgrade() else { return };
            *opts.borrow_mut() = shell_options(&catalog, None);
            apply_shell_options(&ui, &opts.borrow());
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
            ui.set_edit_timeout(crate::model::default_timeout_secs() as i32);
            ui.set_edit_error("".into());
            ui.set_active_view(1);
        });
    }

    // --- Edit ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let catalog = catalog.clone();
        let opts = opts.clone();
        ui.on_request_edit(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else {
                return;
            };
            let Ok(Some(rule)) = store.get_rule(uuid) else {
                return;
            };

            // A shell that is not installed here stays selectable, so saving
            // an unrelated change never switches the rule to another shell.
            let missing = match &rule.shell {
                ShellKind::Detected { key } if catalog.get(key).is_none() => Some(key.as_str()),
                _ => None,
            };
            *opts.borrow_mut() = shell_options(&catalog, missing);
            let opts = opts.borrow();
            apply_shell_options(&ui, &opts);

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
                        at.with_timezone(&Local)
                            .format(ONCE_FORMAT)
                            .to_string()
                            .into(),
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
            ui.set_edit_timeout(rule.timeout_secs.clamp(0, MAX_TIMEOUT_SECS) as i32);
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
        let rules_model = rules_model.clone();
        let rules_signature = rules_signature.clone();
        ui.on_save_rule(move || {
            let Some(ui) = weak.upgrade() else { return };
            let shell = match build_shell(&ui, &opts.borrow()) {
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

            let existing = Uuid::parse_str(ui.get_edit_id().as_str())
                .ok()
                .and_then(|u| store.get_rule(u).ok().flatten());

            // A one-time run must be in the future, unless it is the unchanged
            // time of an existing rule (e.g. renaming one that already ran);
            // that keeps its stored time exactly.
            let schedule = match (&schedule, existing.as_ref().map(|r| &r.schedule)) {
                (Schedule::Once { at }, Some(old @ Schedule::Once { at: old_at }))
                    if at.with_timezone(&Local).format(ONCE_FORMAT).to_string()
                        == old_at.with_timezone(&Local).format(ONCE_FORMAT).to_string() =>
                {
                    old.clone()
                }
                (Schedule::Once { at }, _) if *at <= Utc::now() => {
                    ui.set_edit_error("Once time must be in the future".into());
                    return;
                }
                _ => schedule,
            };

            let mut rule = existing.unwrap_or_else(|| {
                Rule::new(
                    String::new(),
                    String::new(),
                    ShellKind::Direct,
                    Schedule::Interval { seconds: 1 },
                )
            });

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
            rule.timeout_secs = ui.get_edit_timeout() as i64;

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
            refresh_rules(
                &ui,
                &store,
                &catalog,
                &engine,
                &rules_model,
                &rules_signature,
            );
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
        let rules_model = rules_model.clone();
        let rules_signature = rules_signature.clone();
        ui.on_request_delete(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else {
                return;
            };
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
            refresh_rules(
                &ui,
                &store,
                &catalog,
                &engine,
                &rules_model,
                &rules_signature,
            );
        });
    }

    // --- Run now ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let engine = engine.clone();
        let notice_until = notice_until.clone();
        ui.on_request_run_now(move |id| {
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else {
                return;
            };
            // A manual run respects the rule's overlap policy; if it will be
            // skipped because an instance is already running, say so instead of
            // silently doing nothing.
            let notice = match store.get_rule(uuid) {
                Ok(Some(r)) if !r.enabled => "Rule is disabled",
                Ok(Some(r))
                    if r.last_status == LastStatus::Running && r.overlap == OverlapPolicy::Skip =>
                {
                    "Already running — this run was skipped"
                }
                _ => "Run requested",
            };
            engine.run_now(uuid);
            if let Some(ui) = weak.upgrade() {
                ui.set_notice(notice.into());
            }
            *notice_until.borrow_mut() = Some(Instant::now() + Duration::from_secs(4));
        });
    }

    // --- Toggle enabled ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let engine = engine.clone();
        let catalog = catalog.clone();
        let rules_model = rules_model.clone();
        let rules_signature = rules_signature.clone();
        ui.on_request_toggle_enabled(move |id, enabled| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else {
                return;
            };
            let _ = store.set_enabled(uuid, enabled);
            engine.reload_rule(uuid);
            refresh_rules(
                &ui,
                &store,
                &catalog,
                &engine,
                &rules_model,
                &rules_signature,
            );
        });
    }

    // --- Open logs ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let current = current_logs_rule.clone();
        let selected_run = selected_run.clone();
        let runs_signature = runs_signature.clone();
        let runs_model = runs_model.clone();
        ui.on_request_open_logs(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(uuid) = Uuid::parse_str(id.as_str()) else {
                return;
            };
            *current.borrow_mut() = Some(uuid);
            let name = store
                .get_rule(uuid)
                .ok()
                .flatten()
                .map(|r| r.name)
                .unwrap_or_default();
            ui.set_logs_rule_name(name.into());
            let rows = build_run_rows(&store, uuid);
            *runs_signature.borrow_mut() = Some(run_signature(&rows));
            runs_model.set_vec(rows);
            // Start on the newest run.
            *selected_run.borrow_mut() = None;
            reselect_run(&ui, &runs_model, &selected_run);
            fill_selected_run(&store, &ui, &runs_model);
            ui.set_active_view(2);
        });
    }

    // --- Select a run in the logs view ---
    {
        let weak = ui.as_weak();
        let store = store.clone();
        let selected_run = selected_run.clone();
        let runs_model = runs_model.clone();
        ui.on_request_select_run(move |idx| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_selected_run_index(idx);
            *selected_run.borrow_mut() = runs_model
                .row_data(idx as usize)
                .and_then(|r| r.id.parse().ok());
            fill_selected_run(&store, &ui, &runs_model);
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
            // The OS login-item is the single source of truth; no DB mirror.
            let msg = if want {
                match autostart::enable() {
                    Ok(_) => "Saved. Launch at login is on.".to_string(),
                    Err(_) => "Saved, but enabling autostart failed.".to_string(),
                }
            } else {
                let _ = autostart::disable();
                "Saved. Launch at login is off.".to_string()
            };
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
                                ui.set_settings_status(
                                    format!("Exported {} rules.", rules.len()).into(),
                                );
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
        let rules_model = rules_model.clone();
        let rules_signature = rules_signature.clone();
        ui.on_import_rules(move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                match std::fs::read(&path) {
                    Ok(bytes) => match serde_json::from_slice::<Vec<Rule>>(&bytes) {
                        Ok(rules) => {
                            let mut imported = 0usize;
                            let mut copies = 0usize;
                            let mut skipped = 0usize;
                            for mut r in rules {
                                if let Err(e) = r.validate() {
                                    skipped += 1;
                                    log::warn!("import skipped invalid rule '{}': {e}", r.name);
                                    continue;
                                }
                                // Imported rules start fresh: how they ran on
                                // another machine (or before a backup) does
                                // not apply here.
                                r.last_run = None;
                                r.next_run = None;
                                r.last_exit_code = None;
                                r.last_status = LastStatus::Never;
                                // Never overwrite an existing rule: one with the
                                // same id is added as a copy. If the lookup
                                // fails, copying is the safe choice.
                                let exists =
                                    store.get_rule(r.id).map(|o| o.is_some()).unwrap_or(true);
                                if exists {
                                    r.id = Uuid::new_v4();
                                    r.name = format!("{} (copy)", r.name);
                                }
                                if store.upsert_rule(&r).is_ok() {
                                    imported += 1;
                                    if exists {
                                        copies += 1;
                                    }
                                } else {
                                    skipped += 1;
                                }
                            }
                            engine.reload_all();
                            refresh_rules(
                                &ui,
                                &store,
                                &catalog,
                                &engine,
                                &rules_model,
                                &rules_signature,
                            );
                            let mut notes = Vec::new();
                            if copies > 0 {
                                notes.push(format!("{copies} already existed, added as copies"));
                            }
                            if skipped > 0 {
                                notes.push(format!("{skipped} invalid skipped"));
                            }
                            let msg = if notes.is_empty() {
                                format!("Imported {imported} rules.")
                            } else {
                                format!("Imported {imported} rules ({}).", notes.join("; "))
                            };
                            ui.set_settings_status(msg.into());
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
        let selected_run = selected_run.clone();
        let rules_model = rules_model.clone();
        let rules_signature = rules_signature.clone();
        let runs_model = runs_model.clone();
        let runs_signature = runs_signature.clone();
        let notice_until = notice_until.clone();
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(1000),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                // Retire the transient header notice once it has expired.
                let notice_expired =
                    matches!(*notice_until.borrow(), Some(until) if Instant::now() >= until);
                if notice_expired {
                    ui.set_notice("".into());
                    *notice_until.borrow_mut() = None;
                }
                let view = ui.get_active_view();
                if view == 0 {
                    refresh_rules(
                        &ui,
                        &store,
                        &catalog,
                        &engine,
                        &rules_model,
                        &rules_signature,
                    );
                } else {
                    // keep the paused indicator fresh everywhere
                    ui.set_paused(engine.is_paused());
                }
                if view == 2
                    && let Some(uuid) = *current.borrow()
                {
                    let rows = build_run_rows(&store, uuid);
                    let signature = run_signature(&rows);
                    // Only rebuild when the list actually changed (a run started
                    // or finished), so the selected run's output is not reloaded
                    // every second; after a rebuild the selection follows its run.
                    if *runs_signature.borrow() != Some(signature) {
                        *runs_signature.borrow_mut() = Some(signature);
                        runs_model.set_vec(rows);
                        reselect_run(&ui, &runs_model, &selected_run);
                        fill_selected_run(&store, &ui, &runs_model);
                    }
                }
            },
        );
    }
    timer
}
