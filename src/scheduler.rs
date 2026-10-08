//! The scheduling engine: a single-task actor that arms rules, fires them on
//! time, handles catch-up (coalesced), overlap policy, and job completion.
//!
//! Design notes:
//! - One tokio task owns all scheduling state; jobs run in spawned tasks and
//!   report back over a channel, so there is a single writer for DB state.
//! - The loop wakes at least every 30s and re-evaluates, which makes clock
//!   jumps and sleep/wake "just work" without OS power events.
//! - A fire time more than [`LATE_GRACE`] in the past was *missed* (the app
//!   was off, the machine asleep, or everything paused). A rule that opts into
//!   catch-up then fires once, right away, and is advanced strictly into the
//!   future — coalescing any backlog into a single run; any other rule skips
//!   the missed occurrence. A fire time that is only slightly late (timer
//!   throttling, a quick restart) is simply due.

use crate::envres::BaseEnv;
use crate::executor::{self, Outcome, StopSignals};
use crate::model::{LastStatus, OverlapPolicy, Rule, Schedule, add_secs, next_interval_tick};
use crate::shell::ShellCatalog;
use crate::storage::Store;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

/// How late a fire time may be and still count as on time rather than missed.
/// Generous on purpose: macOS and Windows can delay the timers of a
/// background app by seconds to minutes.
const LATE_GRACE: ChronoDuration = ChronoDuration::minutes(5);

/// Settings key that persists "Pause all" across restarts.
const PAUSED_SETTING: &str = "paused";

fn is_late(due: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now - due > LATE_GRACE
}

pub enum Ctrl {
    /// Load all rules at boot: missed runs catch up per rule.
    StartupLoad,
    /// Reload all rules (e.g. after an import), without catch-up.
    ReloadAll,
    /// Reload one rule (after an edit or toggle), without catch-up.
    ReloadRule(Uuid),
    RemoveRule(Uuid),
    RunNow(Uuid),
    PauseAll,
    ResumeAll,
}

/// How (re)arming treats a fire time that has already passed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Missed {
    /// App start: rules that opt in catch up; a fire time less than
    /// [`LATE_GRACE`] overdue is simply due now.
    Startup,
    /// Resume after "Pause all": occurrences during the pause were skipped on
    /// purpose, so only rules that opt in catch up (once).
    Resume,
    /// Edit, toggle or import: never fires for the past. An unchanged timing
    /// keeps its armed time exactly; the run loop then decides if it is due.
    Reload,
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
    /// Rules armed for `now` because a fire time already passed, with that
    /// time: a run making up for a really missed time is recorded as
    /// "catch-up", and the schedule then continues from the missed time so an
    /// interval keeps its phase.
    catching_up: HashMap<Uuid, DateTime<Utc>>,
    /// Per-run cancel switches, keyed by run id: flipping one kills that run
    /// (used when its rule is deleted).
    cancels: HashMap<i64, (Uuid, watch::Sender<bool>)>,
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
    /// Compute and store the next fire time for a rule.
    fn arm_rule(&mut self, mut rule: Rule, mode: Missed) {
        let id = rule.id;
        // Never fire immediately for a rule that already has a run in flight:
        // the missed occurrences are moot, and an immediate fire would only
        // collide with (and momentarily mislabel) the run that is going.
        let busy = self.running.get(&id).copied().unwrap_or(0) > 0;
        // The fire time to keep. While the timing is unchanged, keep the armed
        // time, so a cosmetic edit, a toggle or a manual run never shifts the
        // schedule. On first load use the time the previous session stored,
        // which is also how a run that was due while the app was off is found.
        let kept = match self.rules.get(&id) {
            Some(old) if old.enabled && old.schedule == rule.schedule => {
                self.armed.get(&id).copied()
            }
            Some(_) => None,
            // For cron, only if the stored time really is an occurrence of the
            // expression as read now (a time stored by an older engine with
            // different cron rules is recomputed instead).
            None => rule.next_run.filter(|t| match &rule.schedule {
                Schedule::Cron { .. } => {
                    rule.schedule.next_after(*t - ChronoDuration::seconds(1)) == Some(*t)
                }
                _ => true,
            }),
        };
        if rule.enabled
            && let Err(e) = rule.schedule.validate()
        {
            log::warn!("rule '{}' will not run: {e}", rule.name);
        }
        if !rule.enabled {
            self.armed.remove(&id);
            self.catching_up.remove(&id);
            let _ = self.store.set_next_run(id, None);
            self.rules.insert(id, rule);
            return;
        }
        let now = Utc::now();
        let mut expired = false;
        let next = match (mode, kept) {
            (Missed::Reload, Some(t)) => Some(t),
            _ => {
                // A rule still waiting to make up for a missed run (e.g. it was
                // armed at startup while everything was paused) is judged by the
                // time it actually missed, not by when it was armed.
                let prior_missed = self.catching_up.remove(&id);
                let kept = kept.map(|k| prior_missed.unwrap_or(k));
                let fire_now = |due: DateTime<Utc>| {
                    !busy
                        && match mode {
                            Missed::Startup => rule.catch_up || !is_late(due, now),
                            Missed::Resume => rule.catch_up,
                            Missed::Reload => false,
                        }
                };
                // The fire time that already passed, if the rule is firing now
                // to make up for it.
                let mut missed = None;
                let next = match &rule.schedule {
                    Schedule::Once { at } => {
                        if *at > now {
                            Some(*at)
                        } else if rule.last_run.is_none() {
                            // Never ran: its one occurrence was missed.
                            if fire_now(*at) {
                                missed = Some(*at);
                                Some(now)
                            } else {
                                expired = true;
                                None
                            }
                        } else {
                            None
                        }
                    }
                    Schedule::Interval { seconds } => {
                        let due =
                            kept.or_else(|| rule.last_run.and_then(|t| add_secs(t, *seconds)));
                        match due {
                            Some(t) if t > now => Some(t),
                            Some(t) if fire_now(t) => {
                                missed = Some(t);
                                Some(now)
                            }
                            Some(t) => next_interval_tick(t, *seconds, now),
                            None => add_secs(now, *seconds),
                        }
                    }
                    Schedule::Cron { .. } => {
                        let due = kept
                            .or_else(|| rule.last_run.and_then(|t| rule.schedule.next_after(t)));
                        match due {
                            Some(t) if t > now => Some(t),
                            Some(t) if fire_now(t) => {
                                missed = Some(t);
                                Some(now)
                            }
                            _ => rule.schedule.next_after(now),
                        }
                    }
                };
                if let Some(t) = missed {
                    self.catching_up.insert(id, t);
                }
                next
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

    /// After a scheduled fire (or a skipped missed one), arm the next
    /// strictly-future occurrence. `anchor` is the fire time that just came
    /// due; an interval counts on from it so the cadence keeps its phase
    /// instead of drifting by wake-up jitter.
    fn advance(&mut self, id: Uuid, anchor: DateTime<Utc>) {
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
            Schedule::Interval { seconds } => next_interval_tick(anchor, *seconds, now),
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

    /// A missed occurrence of a rule without catch-up: skip it. A one-time
    /// rule that never ran is then expired.
    fn skip_missed(&mut self, id: Uuid, due: DateTime<Utc>) {
        let Some(rule) = self.rules.get_mut(&id) else {
            return;
        };
        log::info!(
            "skipping missed run of '{}' (was due {due}, catch-up is off)",
            rule.name
        );
        if matches!(rule.schedule, Schedule::Once { .. }) && rule.last_run.is_none() {
            rule.last_status = LastStatus::Expired;
            let _ = self.store.set_status(id, LastStatus::Expired);
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
                    // The rule keeps showing "Running" for the run in flight.
                    log::info!("skip overlapping run for '{}'", rule.name);
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
        let (cancel_tx, cancel) = watch::channel(false);
        self.cancels.insert(run_id, (id, cancel_tx));
        let stop = StopSignals {
            shutdown: self.shutdown_tx.subscribe(),
            cancel,
        };
        let tx = self.jobdone_tx.clone();
        let catalog = self.catalog.clone();
        let base_env = self.base_env.clone();
        log::info!("running '{}' ({trigger})", rule.name);
        tokio::spawn(async move {
            let outcome = executor::execute(&rule, &catalog, &base_env, stop).await;
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
        self.cancels.remove(&run_id);
        let status = if outcome.cancelled {
            LastStatus::Cancelled
        } else if outcome.timed_out {
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
            status,
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
            // A queued run was requested while a run was in flight. It only
            // starts if the rule is still enabled and nothing is paused.
            if self.pending.remove(&rule_id) {
                match self.rules.get(&rule_id).cloned() {
                    Some(rule) if rule.enabled && !self.paused.load(Ordering::Relaxed) => {
                        self.spawn_job(rule, "queued");
                    }
                    Some(rule) => log::info!("dropped queued run of '{}'", rule.name),
                    None => {}
                }
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

    fn load_all(&mut self, mode: Missed) {
        match self.store.list_rules() {
            Ok(rules) => {
                let ids: HashSet<Uuid> = rules.iter().map(|r| r.id).collect();
                self.armed.retain(|id, _| ids.contains(id));
                self.rules.retain(|id, _| ids.contains(id));
                self.pending.retain(|id| ids.contains(id));
                self.catching_up.retain(|id, _| ids.contains(id));
                for rule in rules {
                    self.arm_rule(rule, mode);
                }
            }
            Err(e) => log::error!("list_rules failed: {e}"),
        }
    }

    fn reload_one(&mut self, id: Uuid) {
        match self.store.get_rule(id) {
            Ok(Some(rule)) => self.arm_rule(rule, Missed::Reload),
            Ok(None) => {
                self.rules.remove(&id);
                self.armed.remove(&id);
                self.catching_up.remove(&id);
            }
            Err(e) => log::error!("get_rule failed: {e}"),
        }
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        if let Err(e) = self
            .store
            .setting_set(PAUSED_SETTING, if paused { "1" } else { "0" })
        {
            log::warn!("could not persist the paused state: {e}");
        }
    }

    /// Apply a control message. Engine shutdown is not a control message — it
    /// is delivered via `shutdown_tx` (see `run`), so a full control channel
    /// can never block quitting.
    fn handle_ctrl(&mut self, c: Ctrl) {
        match c {
            Ctrl::StartupLoad => self.load_all(Missed::Startup),
            Ctrl::ReloadAll => self.load_all(Missed::Reload),
            Ctrl::ReloadRule(id) => self.reload_one(id),
            Ctrl::RemoveRule(id) => {
                self.rules.remove(&id);
                self.armed.remove(&id);
                self.pending.remove(&id);
                self.catching_up.remove(&id);
                self.running.remove(&id);
                // Stop the deleted rule's runs that are still going.
                for (rule_id, cancel) in self.cancels.values() {
                    if *rule_id == id {
                        let _ = cancel.send(true);
                    }
                }
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
                self.set_paused(true);
                // Paused: no scheduled or queued run starts (an explicit
                // "Run now" still does).
                self.pending.clear();
                log::info!("paused all rules");
            }
            Ctrl::ResumeAll => {
                self.set_paused(false);
                // Rules that opted into "catch up if missed" fire once for the
                // occurrences they missed while paused; the rest skip them.
                self.load_all(Missed::Resume);
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

    /// Fire (or skip) every rule whose armed time has come.
    fn fire_due(&mut self) {
        let now = Utc::now();
        let due: Vec<(Uuid, DateTime<Utc>)> = self
            .armed
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(id, t)| (*id, *t))
            .collect();
        for (id, due_at) in due {
            let late = is_late(due_at, now);
            let catch_up = self.rules.get(&id).is_some_and(|r| r.catch_up);
            let missed = self.catching_up.remove(&id);
            if late && !catch_up {
                self.skip_missed(id, due_at);
            } else {
                // A run counts as catch-up only if what it makes up for was
                // really missed; a fire time that is just slightly late is on time.
                let made_up = missed.unwrap_or(due_at);
                let trigger = if is_late(made_up, now) {
                    "catch-up"
                } else {
                    "schedule"
                };
                self.fire(id, trigger);
            }
            self.advance(id, missed.unwrap_or(due_at));
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
                self.fire_due();
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
/// tokio runtime). The paused state is restored from the previous session.
pub fn create(
    store: Store,
    catalog: Arc<ShellCatalog>,
    base_env: Arc<BaseEnv>,
) -> (EngineHandle, impl std::future::Future<Output = ()>) {
    let (ctrl_tx, ctrl_rx) = mpsc::channel(128);
    let (jobdone_tx, jobdone_rx) = mpsc::channel(128);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let was_paused = store.setting_get(PAUSED_SETTING).ok().flatten().as_deref() == Some("1");
    let paused = Arc::new(AtomicBool::new(was_paused));
    let engine = Engine {
        store,
        catalog,
        base_env,
        armed: HashMap::new(),
        rules: HashMap::new(),
        running: HashMap::new(),
        pending: HashSet::new(),
        catching_up: HashMap::new(),
        cancels: HashMap::new(),
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

    #[cfg(windows)]
    const LONG_CMD: &str = "cmd /C \"ping -n 60 127.0.0.1 >NUL\"";
    #[cfg(not(windows))]
    const LONG_CMD: &str = "sh -c 'sleep 60'";

    const ECHO_CMD: &str = if cfg!(windows) {
        "cmd /C echo hi"
    } else {
        "echo hi"
    };

    /// Start an engine over `store`; returns its handle and loop task.
    fn start(store: &Store) -> (EngineHandle, tokio::task::JoinHandle<()>) {
        let env = BaseEnv::resolve();
        let (handle, fut) = create(
            store.clone(),
            Arc::new(ShellCatalog::detect(&env)),
            Arc::new(env),
        );
        (handle, tokio::spawn(fut))
    }

    async fn stop(handle: EngineHandle, jh: tokio::task::JoinHandle<()>) {
        handle.shutdown();
        let _ = tokio::time::timeout(StdDuration::from_secs(10), jh).await;
    }

    /// Wait until `rule_id` has at least `n` recorded runs.
    async fn wait_for_runs(store: &Store, rule_id: Uuid, n: usize) {
        let mut waited = StdDuration::ZERO;
        while store.list_runs(rule_id, 10).unwrap().len() < n {
            assert!(waited < StdDuration::from_secs(5), "run never started");
            tokio::time::sleep(StdDuration::from_millis(50)).await;
            waited += StdDuration::from_millis(50);
        }
    }

    fn rule(cmd: &str, schedule: Schedule) -> Rule {
        Rule::new("t".into(), cmd.into(), ShellKind::Direct, schedule)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fires_interval_rule_end_to_end() {
        let store = Store::open_in_memory().unwrap();
        let rule = rule(ECHO_CMD, Schedule::Interval { seconds: 1 });
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.startup_load();
        tokio::time::sleep(StdDuration::from_millis(2500)).await;
        stop(handle, jh).await;

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
        let mut rule = rule(LONG_CMD, Schedule::Interval { seconds: 60 });
        rule.timeout_secs = 0; // only shutdown can end this run
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.run_now(rule.id);
        wait_for_runs(&store, rule.id, 1).await;

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
        assert_eq!(runs[0].status, Some(LastStatus::Cancelled));
        let got = store.get_rule(rule.id).unwrap().unwrap();
        assert_eq!(
            got.last_status,
            LastStatus::Cancelled,
            "a cancelled run must mark the rule Cancelled, not Failed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resume_does_not_catch_up_a_busy_rule() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule(LONG_CMD, Schedule::Interval { seconds: 2 });
        rule.timeout_secs = 0;
        rule.overlap = OverlapPolicy::Parallel;
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        // Manual run only: the rule is not armed, so nothing fires on a timer.
        handle.run_now(rule.id);
        wait_for_runs(&store, rule.id, 1).await;
        // Let the 2s interval elapse so a catch-up would be due.
        tokio::time::sleep(StdDuration::from_millis(2500)).await;

        // Resume triggers a catch-up reload; because a run is in flight it must
        // be suppressed (the next regular tick is ~1.5s away), so no second
        // (Parallel) run starts right now.
        handle.resume_all();
        tokio::time::sleep(StdDuration::from_millis(500)).await;
        assert_eq!(
            store.list_runs(rule.id, 10).unwrap().len(),
            1,
            "a busy rule must not catch up on resume"
        );

        stop(handle, jh).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_now_respects_disabled_rule() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule(ECHO_CMD, Schedule::Interval { seconds: 60 });
        rule.enabled = false;
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.run_now(rule.id);
        tokio::time::sleep(StdDuration::from_millis(300)).await;
        stop(handle, jh).await;

        assert!(
            store.list_runs(rule.id, 10).unwrap().is_empty(),
            "manual run must not start a disabled rule"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_catches_up_a_missed_first_run() {
        let store = Store::open_in_memory().unwrap();
        // Never ran; its first fire time passed while the app was off.
        let mut rule = rule(ECHO_CMD, Schedule::Interval { seconds: 3600 });
        let due = Utc::now() - ChronoDuration::minutes(90);
        rule.next_run = Some(due);
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.startup_load();
        wait_for_runs(&store, rule.id, 1).await;
        stop(handle, jh).await;

        let runs = store.list_runs(rule.id, 10).unwrap();
        assert_eq!(runs.len(), 1, "the backlog is coalesced into one run");
        assert_eq!(runs[0].trigger, "catch-up");
        let got = store.get_rule(rule.id).unwrap().unwrap();
        assert_eq!(
            got.next_run,
            Some(due + ChronoDuration::hours(2)),
            "after catching up the rule continues on its own phase"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_skips_a_missed_run_without_catch_up() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule(ECHO_CMD, Schedule::Interval { seconds: 3600 });
        rule.catch_up = false;
        let due = Utc::now() - ChronoDuration::minutes(10);
        rule.next_run = Some(due);
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.startup_load();
        tokio::time::sleep(StdDuration::from_millis(500)).await;
        stop(handle, jh).await;

        assert!(
            store.list_runs(rule.id, 10).unwrap().is_empty(),
            "a missed run must be skipped when catch-up is off"
        );
        let got = store.get_rule(rule.id).unwrap().unwrap();
        assert_eq!(
            got.next_run,
            Some(due + ChronoDuration::hours(1)),
            "the next run keeps the rule's phase"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slightly_late_run_still_fires_without_catch_up() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule(ECHO_CMD, Schedule::Interval { seconds: 3600 });
        rule.catch_up = false;
        rule.next_run = Some(Utc::now() - ChronoDuration::seconds(30));
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.startup_load();
        wait_for_runs(&store, rule.id, 1).await;
        stop(handle, jh).await;

        let runs = store.list_runs(rule.id, 10).unwrap();
        assert_eq!(runs[0].trigger, "schedule", "slightly late is on time");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catch_up_waits_out_a_pause_that_spans_a_restart() {
        let store = Store::open_in_memory().unwrap();
        store.setting_set(PAUSED_SETTING, "1").unwrap();
        let mut rule = rule(ECHO_CMD, Schedule::Interval { seconds: 3600 });
        let due = Utc::now() - ChronoDuration::minutes(90);
        rule.next_run = Some(due);
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.startup_load();
        tokio::time::sleep(StdDuration::from_millis(500)).await;
        assert!(
            store.list_runs(rule.id, 10).unwrap().is_empty(),
            "nothing runs while paused"
        );
        handle.resume_all();
        wait_for_runs(&store, rule.id, 1).await;
        stop(handle, jh).await;

        let runs = store.list_runs(rule.id, 10).unwrap();
        assert_eq!(runs[0].trigger, "catch-up");
        let got = store.get_rule(rule.id).unwrap().unwrap();
        assert_eq!(
            got.next_run,
            Some(due + ChronoDuration::hours(2)),
            "the phase follows the originally missed time"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_recomputes_a_stored_time_that_is_not_an_occurrence() {
        use chrono::{Datelike, Local, TimeZone, Weekday};
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule(
            ECHO_CMD,
            Schedule::Cron {
                expr: "0 9 * * 1-5".into(),
            },
        );
        // Next Sunday 09:00 — what an older engine (Sunday = 1) could store.
        let mut day = Local::now().date_naive().succ_opt().unwrap();
        while day.weekday() != Weekday::Sun {
            day = day.succ_opt().unwrap();
        }
        let stale = Local
            .from_local_datetime(&day.and_hms_opt(9, 0, 0).unwrap())
            .single()
            .unwrap()
            .with_timezone(&Utc);
        rule.next_run = Some(stale);
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.startup_load();
        tokio::time::sleep(StdDuration::from_millis(300)).await;
        stop(handle, jh).await;

        let next = store.get_rule(rule.id).unwrap().unwrap().next_run.unwrap();
        assert_ne!(next, stale);
        let local = next.with_timezone(&Local);
        assert!(
            local.weekday().number_from_monday() <= 5,
            "rearmed on a weekday, got {local}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missed_once_without_catch_up_expires() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule(
            ECHO_CMD,
            Schedule::Once {
                at: Utc::now() - ChronoDuration::hours(1),
            },
        );
        rule.catch_up = false;
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.startup_load();
        tokio::time::sleep(StdDuration::from_millis(500)).await;
        stop(handle, jh).await;

        assert!(store.list_runs(rule.id, 10).unwrap().is_empty());
        let got = store.get_rule(rule.id).unwrap().unwrap();
        assert_eq!(got.last_status, LastStatus::Expired);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_survives_a_restart() {
        let store = Store::open_in_memory().unwrap();
        let (handle, jh) = start(&store);
        handle.pause_all();
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        assert!(handle.is_paused());
        stop(handle, jh).await;

        let (handle, jh) = start(&store);
        assert!(handle.is_paused(), "a new engine must start paused");
        handle.resume_all();
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        assert!(!handle.is_paused());
        stop(handle, jh).await;
        assert_eq!(
            store.setting_get(PAUSED_SETTING).unwrap().as_deref(),
            Some("0")
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queued_run_is_dropped_while_paused() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule("sleep 1", Schedule::Interval { seconds: 3600 });
        rule.overlap = OverlapPolicy::Queue;
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.run_now(rule.id);
        wait_for_runs(&store, rule.id, 1).await;
        handle.run_now(rule.id); // queued behind the first run
        handle.pause_all();
        tokio::time::sleep(StdDuration::from_millis(2000)).await;
        stop(handle, jh).await;

        assert_eq!(
            store.list_runs(rule.id, 10).unwrap().len(),
            1,
            "the queued run must not start while paused"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn removing_a_rule_cancels_its_running_job() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule(LONG_CMD, Schedule::Interval { seconds: 3600 });
        rule.timeout_secs = 0;
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.run_now(rule.id);
        wait_for_runs(&store, rule.id, 1).await;
        // The engine forgets the rule (the row is kept here so the run's
        // outcome can be inspected).
        handle.remove_rule(rule.id);

        let mut waited = StdDuration::ZERO;
        loop {
            let run = store.list_runs(rule.id, 1).unwrap().remove(0);
            if run.finished_at.is_some() {
                assert_eq!(run.status, Some(LastStatus::Cancelled));
                break;
            }
            assert!(
                waited < StdDuration::from_secs(10),
                "the run must be killed"
            );
            tokio::time::sleep(StdDuration::from_millis(100)).await;
            waited += StdDuration::from_millis(100);
        }
        stop(handle, jh).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn skipped_overlap_keeps_running_status() {
        let store = Store::open_in_memory().unwrap();
        let mut rule = rule(LONG_CMD, Schedule::Interval { seconds: 3600 });
        rule.timeout_secs = 0;
        store.upsert_rule(&rule).unwrap();

        let (handle, jh) = start(&store);
        handle.run_now(rule.id);
        wait_for_runs(&store, rule.id, 1).await;
        handle.run_now(rule.id); // skipped: already running
        tokio::time::sleep(StdDuration::from_millis(300)).await;
        let got = store.get_rule(rule.id).unwrap().unwrap();
        assert_eq!(got.last_status, LastStatus::Running);
        assert_eq!(store.list_runs(rule.id, 10).unwrap().len(), 1);
        stop(handle, jh).await;
    }
}
