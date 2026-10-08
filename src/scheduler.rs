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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::{mpsc, watch};
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
    /// Engine-stop signal, broadcast directly (not through the control channel)
    /// so a full channel can never prevent shutdown. In-flight jobs subscribe to
    /// the same sender and cancel their process groups when it flips.
    shutdown_tx: watch::Sender<bool>,
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
        self.shutdown_tx.send_replace(true);
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
    /// Number of in-flight runs per rule (Parallel can run several at once).
    running: HashMap<Uuid, usize>,
    pending: HashSet<Uuid>,
    paused: Arc<AtomicBool>,
    jobdone_tx: mpsc::Sender<JobDone>,
    /// Broadcast to in-flight jobs: flipping this to `true` asks each job to
    /// kill its process group and report back, so quitting leaves nothing behind.
    shutdown_tx: watch::Sender<bool>,
    /// Last time history retention was applied (the engine prunes at most once
    /// a day, so a long-running app does not grow history unbounded).
    last_prune: Instant,
}

impl Engine {
    /// Compute and store the next fire time for a rule (with optional catch-up).
    fn arm_rule(&mut self, mut rule: Rule, catch_up: bool) {
        let id = rule.id;
        // Honor the per-rule "Catch up if missed" toggle: engine catch-up (used
        // on startup and on resume) applies only when the rule itself opts in.
        let catch_up = catch_up && rule.catch_up;
        if !rule.enabled {
            self.armed.remove(&id);
            self.rules.insert(id, rule);
            return;
        }
        let now = Utc::now();
        let mut expired = false;
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
                    match rule
                        .last_run
                        .and_then(|last| rule.schedule.next_after(last))
                    {
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
                        expired = true;
                        None
                    }
                } else {
                    None
                }
            }
        };
        if expired {
            let _ = self.store.set_status(id, LastStatus::Expired);
            rule.last_status = LastStatus::Expired;
        }
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
            Schedule::Interval { seconds } => {
                // Anchor on the scheduled time that just fired so the cadence
                // keeps its phase instead of drifting by wake-up jitter.
                let anchor = self.armed.get(&id).copied().unwrap_or(now);
                let mut t = anchor + ChronoDuration::seconds(*seconds);
                if t <= now {
                    t = now + ChronoDuration::seconds(*seconds);
                }
                Some(t)
            }
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
        if self.running.get(&id).copied().unwrap_or(0) > 0 {
            match rule.overlap {
                OverlapPolicy::Skip => {
                    log::info!("skip overlapping run for '{}'", rule.name);
                    let _ = self.store.set_status(id, LastStatus::Skipped);
                    if let Some(r) = self.rules.get_mut(&id) {
                        r.last_status = LastStatus::Skipped;
                    }
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
        let running_count = self.running.get(&id).copied().unwrap_or(0);
        self.running.insert(id, running_count + 1);
        let run_id = match self.store.begin_run(id, now, trigger) {
            Ok(r) => r,
            Err(e) => {
                log::error!("begin_run failed: {e}");
                self.decrement_running(&id);
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
        let shutdown = self.shutdown_tx.subscribe();
        log::info!("running '{}' ({trigger})", rule.name);
        tokio::spawn(async move {
            let outcome = executor::execute(&rule, &catalog, &base_env, shutdown).await;
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

    /// Decrement the in-flight counter for a rule; returns whether other
    /// instances of the same rule are still running.
    fn decrement_running(&mut self, id: &Uuid) -> bool {
        match self.running.get_mut(id) {
            Some(c) => {
                *c -= 1;
                let still = *c > 0;
                if !still {
                    self.running.remove(id);
                }
                still
            }
            None => false,
        }
    }

    fn handle_done(&mut self, done: JobDone) {
        let JobDone {
            rule_id,
            run_id,
            outcome,
            finished,
        } = done;
        let status = if outcome.timed_out {
            LastStatus::TimedOut
        } else if outcome.success {
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
        let still_running = self.decrement_running(&rule_id);
        if !still_running {
            let _ = self.store.set_result(rule_id, outcome.exit_code, status);
            if let Some(r) = self.rules.get_mut(&rule_id) {
                r.last_exit_code = outcome.exit_code;
                r.last_status = status;
            }
            log::info!(
                "finished '{}' -> {} (exit {:?})",
                self.rules
                    .get(&rule_id)
                    .map(|r| r.name.as_str())
                    .unwrap_or("?"),
                status.as_str(),
                outcome.exit_code
            );
            // A queued run was requested while a run was in flight.
            if self.pending.remove(&rule_id)
                && let Some(rule) = self.rules.get(&rule_id).cloned()
            {
                self.spawn_job(rule, "queued");
            }
        } else {
            log::info!(
                "parallel run of '{}' finished; {} still running",
                self.rules
                    .get(&rule_id)
                    .map(|r| r.name.as_str())
                    .unwrap_or("?"),
                self.running.get(&rule_id).copied().unwrap_or(0)
            );
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

    /// Apply a control message. Engine shutdown is not a control message — it
    /// is delivered via `shutdown_tx` (see `run`), so a full control channel
    /// can never block quitting.
    fn handle_ctrl(&mut self, c: Ctrl) {
        match c {
            Ctrl::StartupLoad => self.load_all(true),
            Ctrl::ReloadAll => self.load_all(false),
            Ctrl::ReloadRule(id) => self.reload_one(id, false),
            Ctrl::RemoveRule(id) => {
                self.rules.remove(&id);
                self.armed.remove(&id);
                self.pending.remove(&id);
                self.running.remove(&id);
            }
            Ctrl::RunNow(id) => {
                if let Ok(Some(rule)) = self.store.get_rule(id) {
                    self.rules.insert(id, rule);
                    // Route through fire() so manual runs respect the rule's
                    // enabled flag and overlap policy (Skip/Queue/Parallel).
                    self.fire(id, "manual");
                }
            }
            Ctrl::PauseAll => {
                self.paused.store(true, Ordering::Relaxed);
                log::info!("paused all rules");
            }
            Ctrl::ResumeAll => {
                self.paused.store(false, Ordering::Relaxed);
                // Resume mirrors startup: rules that opted into "catch up if
                // missed" fire once for the backlog they accrued while paused.
                self.load_all(true);
                log::info!("resumed all rules");
            }
        }
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
        if let Ok(n) = self.store.prune_history(days)
            && n > 0
        {
            log::info!("pruned {n} old run records");
        }
    }

    /// After the shutdown broadcast, wait (bounded) for in-flight jobs to
    /// report back so their run rows are closed instead of left dangling.
    async fn drain(&mut self, jobdone_rx: &mut mpsc::Receiver<JobDone>) {
        self.pending.clear();
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        while !self.running.is_empty() {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            match tokio::time::timeout(deadline - now, jobdone_rx.recv()).await {
                Ok(Some(done)) => self.handle_done(done),
                _ => break,
            }
        }
    }

    async fn run(
        mut self,
        mut ctrl_rx: mpsc::Receiver<Ctrl>,
        mut jobdone_rx: mpsc::Receiver<JobDone>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) {
        self.prune_history_setting();
        log::info!("scheduler engine started");
        loop {
            if self.last_prune.elapsed() >= StdDuration::from_secs(24 * 60 * 60) {
                self.prune_history_setting();
                self.last_prune = Instant::now();
            }
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
                        Some(c) => self.handle_ctrl(c),
                        None => break,
                    }
                }
                Some(done) = jobdone_rx.recv() => self.handle_done(done),
                _ = shutdown_rx.changed() => {
                    // Stop requested: the same broadcast already told in-flight
                    // jobs to kill their process groups. Wait for them to report
                    // so their run rows are closed, then exit.
                    self.drain(&mut jobdone_rx).await;
                    break;
                }
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
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let paused = Arc::new(AtomicBool::new(false));
    let engine = Engine {
        store,
        catalog,
        base_env,
        armed: HashMap::new(),
        rules: HashMap::new(),
        running: HashMap::new(),
        pending: HashSet::new(),
        paused: paused.clone(),
        jobdone_tx,
        shutdown_tx: shutdown_tx.clone(),
        last_prune: Instant::now(),
    };
    let handle = EngineHandle {
        tx: ctrl_tx,
        paused,
        shutdown_tx,
    };
    (handle, engine.run(ctrl_rx, jobdone_rx, shutdown_rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ShellKind;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fires_interval_rule_end_to_end() {
        let store = Store::open_in_memory().unwrap();
        let cmd = if cfg!(windows) {
            "cmd /C echo hi"
        } else {
            "echo hi"
        };
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_cancels_running_job_and_closes_row() {
        let store = Store::open_in_memory().unwrap();
        #[cfg(windows)]
        let cmd = "cmd /C \"ping -n 60 127.0.0.1 >NUL\"";
        #[cfg(not(windows))]
        let cmd = "sh -c 'sleep 60'";
        let mut rule = Rule::new(
            "t".into(),
            cmd.into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        rule.timeout_secs = 0; // only shutdown can end this run
        store.upsert_rule(&rule).unwrap();

        let (handle, fut) = create(
            store.clone(),
            Arc::new(ShellCatalog::detect()),
            Arc::new(BaseEnv::resolve()),
        );
        let jh = tokio::spawn(fut);
        handle.run_now(rule.id);

        // Wait until the run has actually started (a row exists).
        let mut waited = StdDuration::ZERO;
        while store.list_runs(rule.id, 10).unwrap().is_empty() {
            assert!(waited < StdDuration::from_secs(5), "run never started");
            tokio::time::sleep(StdDuration::from_millis(50)).await;
            waited += StdDuration::from_millis(50);
        }

        handle.shutdown();
        tokio::time::timeout(StdDuration::from_secs(10), jh)
            .await
            .expect("engine must stop after shutdown")
            .unwrap();

        let runs = store.list_runs(rule.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(
            runs[0].finished_at.is_some(),
            "run row must be closed on shutdown"
        );
        assert!(!runs[0].success, "cancelled run must not be a success");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_now_respects_disabled_rule() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = Rule::new(
            "t".into(),
            "echo hi".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 60 },
        );
        rule.enabled = false;
        store.upsert_rule(&rule).unwrap();

        let (handle, fut) = create(
            store.clone(),
            Arc::new(ShellCatalog::detect()),
            Arc::new(BaseEnv::resolve()),
        );
        let jh = tokio::spawn(fut);
        handle.run_now(rule.id);
        tokio::time::sleep(StdDuration::from_millis(300)).await;
        handle.shutdown();
        let _ = jh.await;

        assert!(
            store.list_runs(rule.id, 10).unwrap().is_empty(),
            "manual run must not start a disabled rule"
        );
    }
}
