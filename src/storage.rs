//! SQLite persistence (single source of truth): rules, run history, settings.

use crate::model::{EnvVar, LastStatus, OverlapPolicy, Rule, RunRecord, Schedule, ShellKind};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS rules (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL,
    enabled        INTEGER NOT NULL,
    command        TEXT NOT NULL,
    shell          TEXT NOT NULL,
    schedule       TEXT NOT NULL,
    catch_up       INTEGER NOT NULL,
    overlap        TEXT NOT NULL,
    working_dir    TEXT,
    env            TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    last_run       TEXT,
    next_run       TEXT,
    last_exit_code INTEGER,
    last_status    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_id     TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    exit_code   INTEGER,
    success     INTEGER NOT NULL DEFAULT 0,
    stdout      TEXT NOT NULL DEFAULT '',
    stderr      TEXT NOT NULL DEFAULT '',
    trigger     TEXT NOT NULL,
    FOREIGN KEY (rule_id) REFERENCES rules(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_runs_rule ON runs(rule_id, started_at DESC);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

fn dt_to_str(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339()
}

fn parse_dt(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))
}

fn parse_opt_dt(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    match s {
        Some(v) => Ok(Some(parse_dt(&v)?)),
        None => Ok(None),
    }
}

/// Raw column values pulled from a `rules` row before typed decoding.
struct RawRule {
    id: String,
    name: String,
    enabled: i64,
    command: String,
    shell: String,
    schedule: String,
    catch_up: i64,
    overlap: String,
    working_dir: Option<String>,
    env: String,
    created_at: String,
    last_run: Option<String>,
    next_run: Option<String>,
    last_exit_code: Option<i64>,
    last_status: String,
}

impl RawRule {
    fn into_rule(self) -> Result<Rule> {
        let shell: ShellKind =
            serde_json::from_str(&self.shell).context("decode shell json")?;
        let schedule: Schedule =
            serde_json::from_str(&self.schedule).context("decode schedule json")?;
        let env: Vec<EnvVar> = serde_json::from_str(&self.env).context("decode env json")?;
        Ok(Rule {
            id: Uuid::parse_str(&self.id).context("parse rule uuid")?,
            name: self.name,
            enabled: self.enabled != 0,
            command: self.command,
            shell,
            schedule,
            catch_up: self.catch_up != 0,
            overlap: OverlapPolicy::from_str_lossy(&self.overlap),
            working_dir: self.working_dir.filter(|s| !s.is_empty()),
            env,
            created_at: parse_dt(&self.created_at)?,
            last_run: parse_opt_dt(self.last_run)?,
            next_run: parse_opt_dt(self.next_run)?,
            last_exit_code: self.last_exit_code.map(|v| v as i32),
            last_status: LastStatus::from_str_lossy(&self.last_status),
        })
    }
}

const RULE_COLUMNS: &str = "id,name,enabled,command,shell,schedule,catch_up,overlap,working_dir,env,created_at,last_run,next_run,last_exit_code,last_status";

fn map_raw_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRule> {
    Ok(RawRule {
        id: row.get(0)?,
        name: row.get(1)?,
        enabled: row.get(2)?,
        command: row.get(3)?,
        shell: row.get(4)?,
        schedule: row.get(5)?,
        catch_up: row.get(6)?,
        overlap: row.get(7)?,
        working_dir: row.get(8)?,
        env: row.get(9)?,
        created_at: row.get(10)?,
        last_run: row.get(11)?,
        next_run: row.get(12)?,
        last_exit_code: row.get(13)?,
        last_status: row.get(14)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)
            .with_context(|| format!("open sqlite db at {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("apply schema")?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn list_rules(&self) -> Result<Vec<Rule>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {RULE_COLUMNS} FROM rules ORDER BY created_at ASC");
        let mut stmt = conn.prepare(&sql)?;
        let raws: Vec<RawRule> = stmt
            .query_map([], map_raw_rule)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raws.into_iter().map(RawRule::into_rule).collect()
    }

    pub fn get_rule(&self, id: Uuid) -> Result<Option<Rule>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {RULE_COLUMNS} FROM rules WHERE id = ?1");
        let raw = conn
            .query_row(&sql, params![id.to_string()], map_raw_rule)
            .optional()?;
        match raw {
            Some(r) => Ok(Some(r.into_rule()?)),
            None => Ok(None),
        }
    }

    pub fn upsert_rule(&self, rule: &Rule) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO rules (id,name,enabled,command,shell,schedule,catch_up,overlap,working_dir,env,created_at,last_run,next_run,last_exit_code,last_status)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(id) DO UPDATE SET
               name=excluded.name, enabled=excluded.enabled, command=excluded.command,
               shell=excluded.shell, schedule=excluded.schedule, catch_up=excluded.catch_up,
               overlap=excluded.overlap, working_dir=excluded.working_dir, env=excluded.env,
               last_run=excluded.last_run, next_run=excluded.next_run,
               last_exit_code=excluded.last_exit_code, last_status=excluded.last_status",
            params![
                rule.id.to_string(),
                rule.name,
                rule.enabled as i64,
                rule.command,
                serde_json::to_string(&rule.shell)?,
                serde_json::to_string(&rule.schedule)?,
                rule.catch_up as i64,
                rule.overlap.as_str(),
                rule.working_dir,
                serde_json::to_string(&rule.env)?,
                dt_to_str(&rule.created_at),
                rule.last_run.as_ref().map(dt_to_str),
                rule.next_run.as_ref().map(dt_to_str),
                rule.last_exit_code,
                rule.last_status.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn delete_rule(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM rules WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }

    pub fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE rules SET enabled = ?2 WHERE id = ?1",
            params![id.to_string(), enabled as i64],
        )?;
        Ok(())
    }

    pub fn set_next_run(&self, id: Uuid, next: Option<DateTime<Utc>>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE rules SET next_run = ?2 WHERE id = ?1",
            params![id.to_string(), next.as_ref().map(dt_to_str)],
        )?;
        Ok(())
    }

    pub fn mark_running(&self, id: Uuid, started: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE rules SET last_run = ?2, last_status = 'Running' WHERE id = ?1",
            params![id.to_string(), dt_to_str(&started)],
        )?;
        Ok(())
    }

    pub fn set_result(&self, id: Uuid, exit_code: Option<i32>, status: LastStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE rules SET last_exit_code = ?2, last_status = ?3 WHERE id = ?1",
            params![id.to_string(), exit_code, status.as_str()],
        )?;
        Ok(())
    }

    pub fn set_status(&self, id: Uuid, status: LastStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE rules SET last_status = ?2 WHERE id = ?1",
            params![id.to_string(), status.as_str()],
        )?;
        Ok(())
    }

    /// Insert a run row in the "started" state; returns its rowid.
    pub fn begin_run(&self, rule_id: Uuid, started: DateTime<Utc>, trigger: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO runs (rule_id, started_at, trigger) VALUES (?1, ?2, ?3)",
            params![rule_id.to_string(), dt_to_str(&started), trigger],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_run(
        &self,
        run_id: i64,
        finished: DateTime<Utc>,
        exit_code: Option<i32>,
        success: bool,
        stdout: &str,
        stderr: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE runs SET finished_at=?2, exit_code=?3, success=?4, stdout=?5, stderr=?6 WHERE id=?1",
            params![run_id, dt_to_str(&finished), exit_code, success as i64, stdout, stderr],
        )?;
        Ok(())
    }

    pub fn list_runs(&self, rule_id: Uuid, limit: i64) -> Result<Vec<RunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, started_at, finished_at, exit_code, success, stdout, stderr, trigger
             FROM runs WHERE rule_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![rule_id.to_string(), limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, started, finished, exit, success, stdout, stderr, trigger) = r?;
            out.push(RunRecord {
                id,
                started_at: parse_dt(&started)?,
                finished_at: parse_opt_dt(finished)?,
                exit_code: exit.map(|v| v as i32),
                success: success != 0,
                stdout,
                stderr,
                trigger,
            });
        }
        Ok(out)
    }

    /// Delete history older than `days` days. If `days <= 0`, keep everything.
    pub fn prune_history(&self, days: i64) -> Result<usize> {
        if days <= 0 {
            return Ok(0);
        }
        let cutoff = Utc::now() - chrono::Duration::days(days);
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM runs WHERE started_at < ?1",
            params![dt_to_str(&cutoff)],
        )?;
        Ok(n)
    }

    pub fn setting_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(v)
    }

    pub fn setting_set(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let rule = Rule::new(
            "my rule".into(),
            "echo hi".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 5 },
        );
        store.upsert_rule(&rule).unwrap();
        let got = store.get_rule(rule.id).unwrap().unwrap();
        assert_eq!(got.name, "my rule");
        assert_eq!(got.command, "echo hi");
        assert!(matches!(got.schedule, Schedule::Interval { seconds: 5 }));
        assert_eq!(store.list_rules().unwrap().len(), 1);
        store.delete_rule(rule.id).unwrap();
        assert_eq!(store.list_rules().unwrap().len(), 0);
    }

    #[test]
    fn run_history_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let rule = Rule::new("r".into(), "echo".into(), ShellKind::Direct, Schedule::Interval { seconds: 5 });
        store.upsert_rule(&rule).unwrap();
        let started = Utc::now();
        let run_id = store.begin_run(rule.id, started, "manual").unwrap();
        store.finish_run(run_id, Utc::now(), Some(0), true, "out", "").unwrap();
        let runs = store.list_runs(rule.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].stdout, "out");
        assert!(runs[0].success);
    }

    #[test]
    fn settings_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.setting_get("x").unwrap(), None);
        store.setting_set("x", "1").unwrap();
        store.setting_set("x", "2").unwrap();
        assert_eq!(store.setting_get("x").unwrap(), Some("2".into()));
    }
}
