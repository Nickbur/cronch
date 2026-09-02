//! The scheduling engine: a single-task actor that arms rules, fires them on
//! time, handles catch-up (coalesced), overlap policy, and job completion.
//!
//! Design notes:
//! - One tokio task owns all scheduling state; jobs run in spawned tasks and
//!   report back over a channel, so there is a single writer for DB state.
//! - The loop wakes at least every 30s and re-evaluates, which makes clock
//!   jumps and sleep/wake "just work" without OS power events.
//! - Catch-up is applied only at startup: a past-due rule is armed for `now`
//!   (fires once), then advanced strictly into the future — coalescing any
//!   backlog into a single run.

use crate::envres::BaseEnv;
use crate::executor::{self, Outcome};
use crate::model::{LastStatus, OverlapPolicy, Rule, Schedule};
use crate::shell::ShellCatalog;
use crate::storage::Store;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::sync::mpsc;
use uuid::Uuid;

pub enum Ctrl {
    /// Load all rules WITH catch-up (used once at boot).
    StartupLoad,
    /// Reload all rules WITHOUT catch-up.
    ReloadAll,
    /// Reload one rule WITHOUT catch-up.
    ReloadRule(Uuid),
    RemoveRule(Uuid),
    RunNow(Uuid),
    PauseAll,
    ResumeAll,
    Shutdown,
}

struct JobDone {
    rule_id: Uuid,
    run_id: i64,
    outcome: Outcome,
    finished: DateTime<Utc>,
}

#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<Ctrl>,
    pub paused: Arc<AtomicBool>,
}

impl EngineHandle {
    fn send(&self, c: Ctrl) {
        if let Err(e) = self.tx.try_send(c) {
            log::warn!("engine control channel: {e}");
        }
    }
    pub fn startup_load(&self) {
        self.send(Ctrl::StartupLoad);
    }
    pub fn reload_all(&self) {
        self.send(Ctrl::ReloadAll);
    }
    pub fn reload_rule(&self, id: Uuid) {
        self.send(Ctrl::ReloadRule(id));
    }
    pub fn remove_rule(&self, id: Uuid) {
        self.send(Ctrl::RemoveRule(id));
    }
    pub fn run_now(&self, id: Uuid) {
        self.send(Ctrl::RunNow(id));
    }
    pub fn pause_all(&self) {
        self.send(Ctrl::PauseAll);
    }
    pub fn resume_all(&self) {
        self.send(Ctrl::ResumeAll);
    }
    pub fn shutdown(&self) {
        self.send(Ctrl::Shutdown);
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

struct Engine {
    store: Store,
    catalog: Arc<ShellCatalog>,
    base_env: Arc<BaseEnv>,
    armed: HashMap<Uuid, DateTime<Utc>>,
    rules: HashMap<Uuid, Rule>,
    running: HashSet<Uuid>,
    pending: HashSet<Uuid>,
    paused: Arc<AtomicBool>,
    jobdone_tx: mpsc::Sender<JobDone>,
}

impl Engine {
    fn mark_expired(&self, id: Uuid) {
        let _ = self.store.set_status(id, LastStatus::Expired);
    }

    /// Compute and store the next fire time for a rule (with optional catch-up).
    fn arm_rule(&mut self, rule: Rule, catch_up: bool) {
        let id = rule.id;
        if !rule.enabled {
            self.armed.remove(&id);
            self.rules.insert(id, rule);
            return;
        }
        let now = Utc::now();
        let next: Option<DateTime<Utc>> = match &rule.schedule {
            Schedule::Interval { seconds } => {
                let anchor = rule.last_run.unwrap_or(now);
                let mut n = anchor + ChronoDuration::seconds(*seconds);
                if n <= now {
                    n = if catch_up {
                        now
                    } else {
                        now + ChronoDuration::seconds(*seconds)
                    };
                }
                Some(n)
            }
            Schedule::Cron { .. } => {
                if catch_up {
                    match rule.last_run.and_then(|last| rule.schedule.next_after(last)) {
                        Some(missed) if missed <= now => Some(now),
                        _ => rule.schedule.next_after(now),
                    }
                } else {
                    rule.schedule.next_after(now)
                }
            }
            Schedule::Once { at } => {
                if *at > now {
                    Some(*at)
                } else if rule.last_run.is_none() {
                    if catch_up {
                        Some(now)
                    } else {
                        self.mark_expired(id);
                        None
                    }
                } else {
                    None
                }
            }
        };
        match next {
            Some(t) => {
                self.armed.insert(id, t);
                let _ = self.store.set_next_run(id, Some(t));
            }
            None => {
                self.armed.remove(&id);
                let _ = self.store.set_next_run(id, None);
            }
        }
        self.rules.insert(id, rule);
    }

    /// After a scheduled fire, arm the next strictly-future occurrence.
    fn advance(&mut self, id: Uuid) {
        let now = Utc::now();
        let (enabled, schedule) = match self.rules.get(&id) {
            Some(r) => (r.enabled, r.schedule.clone()),
            None => return,
        };
        if !enabled {
            self.armed.remove(&id);
            let _ = self.store.set_next_run(id, None);
            return;
        }
        let next = match &schedule {
            Schedule::Once { .. } => None,
            s => s.next_after(now),
        };
        match next {
            Some(t) => {
                self.armed.insert(id, t);
                let _ = self.store.set_next_run(id, Some(t));
            }
            None => {
                self.armed.remove(&id);
                let _ = self.store.set_next_run(id, None);
            }
        }
    }

    fn fire(&mut self, id: Uuid, trigger: &'static str) {
        let rule = match self.rules.get(&id) {
            Some(r) => r.clone(),
            None => return,
        };
        if !rule.enabled {
            return;
        }
        if self.running.contains(&id) {
            match rule.overlap {
                OverlapPolicy::Skip => {
                    log::info!("skip overlapping run for '{}'", rule.name);
                    let _ = self.store.set_status(id, LastStatus::Skipped);
                    return;
                }
                OverlapPolicy::Queue => {
                    self.pending.insert(id);
                    return;
                }
                OverlapPolicy::Parallel => {}
            }
        }
        self.spawn_job(rule, trigger);
    }

    fn spawn_job(&mut self, rule: Rule, trigger: &'static str) {
        let id = rule.id;
        let now = Utc::now();
        self.running.insert(id);
        let run_id = match self.store.begin_run(id, now, trigger) {
            Ok(r) => r,
            Err(e) => {
                log::error!("begin_run failed: {e}");
                self.running.remove(&id);
                return;
            }
        };
        let _ = self.store.mark_running(id, now);
        if let Some(r) = self.rules.get_mut(&id) {
            r.last_run = Some(now);
            r.last_status = LastStatus::Running;
        }
        let tx = self.jobdone_tx.clone();
        let catalog = self.catalog.clone();
        let base_env = self.base_env.clone();
        log::info!("running '{}' ({trigger})", rule.name);
        tokio::spawn(async move {
            let outcome = executor::execute(&rule, &catalog, &base_env).await;
            let _ = tx
                .send(JobDone {
                    rule_id: id,
                    run_id,
                    outcome,
                    finished: Utc::now(),
                })
                .await;
        });
    }

    fn handle_done(&mut self, done: JobDone) {
        let JobDone {
            rule_id,
            run_id,
            outcome,
            finished,
        } = done;
        self.running.remove(&rule_id);
        let status = if outcome.success {
            LastStatus::Success
        } else {
            LastStatus::Failed
        };
        let _ = self.store.finish_run(
            run_id,
            finished,
            outcome.exit_code,
            outcome.success,
            &outcome.stdout,
            &outcome.stderr,
        );
        let _ = self.store.set_result(rule_id, outcome.exit_code, status);
        if let Some(r) = self.rules.get_mut(&rule_id) {
            r.last_exit_code = outcome.exit_code;
            r.last_status = status;
        }
        log::info!(
            "finished '{}' -> {} (exit {:?})",
            self.rules.get(&rule_id).map(|r| r.name.as_str()).unwrap_or("?"),
            status.as_str(),
            outcome.exit_code
        );
        // A queued run was requested while this one was in flight.
        if self.pending.remove(&rule_id) {
            if let Some(rule) = self.rules.get(&rule_id).cloned() {
                self.spawn_job(rule, "queued");
            }
        }
    }

    fn load_all(&mut self, catch_up: bool) {
        match self.store.list_rules() {
            Ok(rules) => {
                let ids: HashSet<Uuid> = rules.iter().map(|r| r.id).collect();
                self.armed.retain(|id, _| ids.contains(id));
                self.rules.retain(|id, _| ids.contains(id));
                self.pending.retain(|id| ids.contains(id));
                for rule in rules {
                    self.arm_rule(rule, catch_up);
                }
            }
            Err(e) => log::error!("list_rules failed: {e}"),
        }
    }

    fn reload_one(&mut self, id: Uuid, catch_up: bool) {
        match self.store.get_rule(id) {
            Ok(Some(rule)) => self.arm_rule(rule, catch_up),
            Ok(None) => {
                self.rules.remove(&id);
                self.armed.remove(&id);
            }
            Err(e) => log::error!("get_rule failed: {e}"),
        }
    }

    /// Returns true if the engine should shut down.
    fn handle_ctrl(&mut self, c: Ctrl) -> bool {
        match c {
            Ctrl::StartupLoad => self.load_all(true),
            Ctrl::ReloadAll => self.load_all(false),
            Ctrl::ReloadRule(id) => self.reload_one(id, false),
            Ctrl::RemoveRule(id) => {
                self.rules.remove(&id);
                self.armed.remove(&id);
                self.pending.remove(&id);
            }
            Ctrl::RunNow(id) => {
                if let Ok(Some(rule)) = self.store.get_rule(id) {
                    self.rules.insert(id, rule.clone());
                    self.spawn_job(rule, "manual");
                }
            }
            Ctrl::PauseAll => {
                self.paused.store(true, Ordering::Relaxed);
                log::info!("paused all rules");
            }
            Ctrl::ResumeAll => {
                self.paused.store(false, Ordering::Relaxed);
                self.load_all(false);
                log::info!("resumed all rules");
            }
            Ctrl::Shutdown => return true,
        }
        false
    }

    fn next_sleep(&self) -> StdDuration {
        const MAX: StdDuration = StdDuration::from_secs(30);
        const MIN: StdDuration = StdDuration::from_millis(200);
        if self.paused.load(Ordering::Relaxed) {
            return MAX;
        }
        match self.armed.values().min() {
            Some(next) => match (*next - Utc::now()).to_std() {
                Ok(d) => d.clamp(MIN, MAX),
                Err(_) => MIN,
            },
            None => MAX,
        }
    }

    fn prune_history_setting(&self) {
        let days = self
            .store
            .setting_get("history_retention_days")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(30);
        if let Ok(n) = self.store.prune_history(days) {
            if n > 0 {
                log::info!("pruned {n} old run records");
            }
        }
    }

    async fn run(
        mut self,
        mut ctrl_rx: mpsc::Receiver<Ctrl>,
        mut jobdone_rx: mpsc::Receiver<JobDone>,
    ) {
        self.prune_history_setting();
        log::info!("scheduler engine started");
        loop {
            if !self.paused.load(Ordering::Relaxed) {
                let now = Utc::now();
                let due: Vec<Uuid> = self
                    .armed
                    .iter()
                    .filter(|(_, t)| **t <= now)
                    .map(|(id, _)| *id)
                    .collect();
                for id in due {
                    self.fire(id, "schedule");
                    self.advance(id);
                }
            }
            let sleep_dur = self.next_sleep();
            tokio::select! {
                maybe = ctrl_rx.recv() => {
                    match maybe {
                        Some(c) => if self.handle_ctrl(c) { break; },
                        None => break,
                    }
                }
                Some(done) = jobdone_rx.recv() => self.handle_done(done),
                _ = tokio::time::sleep(sleep_dur) => {}
            }
        }
        log::info!("scheduler engine stopped");
    }
}

/// Build the engine. Returns a handle and the loop future (spawn/await it on a
/// tokio runtime).
pub fn create(
    store: Store,
    catalog: Arc<ShellCatalog>,
    base_env: Arc<BaseEnv>,
) -> (EngineHandle, impl std::future::Future<Output = ()>) {
    let (ctrl_tx, ctrl_rx) = mpsc::channel(128);
    let (jobdone_tx, jobdone_rx) = mpsc::channel(128);
    let paused = Arc::new(AtomicBool::new(false));
    let engine = Engine {
        store,
        catalog,
        base_env,
        armed: HashMap::new(),
        rules: HashMap::new(),
        running: HashSet::new(),
        pending: HashSet::new(),
        paused: paused.clone(),
        jobdone_tx,
    };
    let handle = EngineHandle { tx: ctrl_tx, paused };
    (handle, engine.run(ctrl_rx, jobdone_rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ShellKind;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fires_interval_rule_end_to_end() {
        let store = Store::open_in_memory().unwrap();
        let cmd = if cfg!(windows) { "cmd /C echo hi" } else { "echo hi" };
        let rule = Rule::new(
            "t".into(),
            cmd.into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 1 },
        );
        store.upsert_rule(&rule).unwrap();

        let (handle, fut) = create(
            store.clone(),
            Arc::new(ShellCatalog::detect()),
            Arc::new(BaseEnv::resolve()),
        );
        let jh = tokio::spawn(fut);
        handle.startup_load();
        tokio::time::sleep(StdDuration::from_millis(2500)).await;
        handle.shutdown();
        let _ = jh.await;

        let runs = store.list_runs(rule.id, 10).unwrap();
        assert!(!runs.is_empty(), "expected at least one run to be recorded");
        assert!(
            runs.iter().any(|r| r.success),
            "expected at least one successful run"
        );
    }
}
