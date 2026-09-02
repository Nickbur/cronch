//! Core domain types for Cronch: rules, schedules, shells, run records.

use chrono::{DateTime, Duration, Local, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use uuid::Uuid;

/// How often / when a rule fires.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "value")]
pub enum Schedule {
    /// Fire every `seconds` seconds.
    Interval { seconds: i64 },
    /// Fire on a cron expression (5-field standard or 6-field with seconds).
    Cron { expr: String },
    /// Fire exactly once at an absolute time.
    Once { at: DateTime<Utc> },
}

impl Schedule {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Schedule::Interval { seconds } => {
                if *seconds < 1 {
                    Err("Interval must be at least 1 second".into())
                } else {
                    Ok(())
                }
            }
            Schedule::Cron { expr } => cron::Schedule::from_str(&normalize_cron(expr))
                .map(|_| ())
                .map_err(|e| format!("Invalid cron expression: {e}")),
            Schedule::Once { .. } => Ok(()),
        }
    }

    /// The next fire time strictly after `after`, if any.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Interval { seconds } => Some(after + Duration::seconds(*seconds)),
            Schedule::Cron { expr } => cron::Schedule::from_str(&normalize_cron(expr))
                .ok()?
                .after(&after)
                .next(),
            Schedule::Once { at } => {
                if *at > after {
                    Some(*at)
                } else {
                    None
                }
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Schedule::Interval { seconds } => format!("Every {}", humanize_secs(*seconds)),
            Schedule::Cron { expr } => format!("Cron: {expr}"),
            Schedule::Once { at } => format!(
                "Once at {}",
                at.with_timezone(&Local).format("%Y-%m-%d %H:%M")
            ),
        }
    }
}

/// The cron crate expects a seconds field. Standard 5-field expressions get a
/// leading `0 ` (fire at second 0).
pub fn normalize_cron(expr: &str) -> String {
    let trimmed = expr.trim();
    match trimmed.split_whitespace().count() {
        5 => format!("0 {trimmed}"),
        _ => trimmed.to_string(),
    }
}

fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn humanize_secs(s: i64) -> String {
    if s >= 86400 && s % 86400 == 0 {
        let d = s / 86400;
        format!("{d} day{}", plural(d))
    } else if s >= 3600 && s % 3600 == 0 {
        let h = s / 3600;
        format!("{h} hour{}", plural(h))
    } else if s >= 60 && s % 60 == 0 {
        let m = s / 60;
        format!("{m} minute{}", plural(m))
    } else {
        format!("{s} second{}", plural(s))
    }
}

/// How the rule's command is executed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum ShellKind {
    /// A shell auto-detected on this machine, referenced by catalog key.
    Detected { key: String },
    /// An arbitrary shell binary plus an argument template (must contain `{cmd}`).
    Custom { path: String, arg_template: String },
    /// Run the command directly (split into program + args), no shell wrapper.
    Direct,
}

impl ShellKind {
    pub fn summary(&self) -> String {
        match self {
            ShellKind::Detected { key } => key.clone(),
            ShellKind::Custom { path, .. } => format!("custom: {path}"),
            ShellKind::Direct => "direct".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum OverlapPolicy {
    Skip,
    Queue,
    Parallel,
}

impl OverlapPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            OverlapPolicy::Skip => "Skip",
            OverlapPolicy::Queue => "Queue",
            OverlapPolicy::Parallel => "Parallel",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "Queue" => OverlapPolicy::Queue,
            "Parallel" => OverlapPolicy::Parallel,
            _ => OverlapPolicy::Skip,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum LastStatus {
    #[default]
    Never,
    Running,
    Success,
    Failed,
    Skipped,
    Expired,
}

impl LastStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LastStatus::Never => "Never",
            LastStatus::Running => "Running",
            LastStatus::Success => "Success",
            LastStatus::Failed => "Failed",
            LastStatus::Skipped => "Skipped",
            LastStatus::Expired => "Expired",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "Running" => LastStatus::Running,
            "Success" => LastStatus::Success,
            "Failed" => LastStatus::Failed,
            "Skipped" => LastStatus::Skipped,
            "Expired" => LastStatus::Expired,
            _ => LastStatus::Never,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// A scheduled task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub command: String,
    pub shell: ShellKind,
    pub schedule: Schedule,
    pub catch_up: bool,
    pub overlap: OverlapPolicy,
    /// Working directory; `None`/empty means the user's home directory.
    pub working_dir: Option<String>,
    pub env: Vec<EnvVar>,
    pub created_at: DateTime<Utc>,

    // Runtime state (persisted).
    #[serde(default)]
    pub last_run: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next_run: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    #[serde(default)]
    pub last_status: LastStatus,
}

impl Rule {
    pub fn new(name: String, command: String, shell: ShellKind, schedule: Schedule) -> Self {
        Rule {
            id: Uuid::new_v4(),
            name,
            enabled: true,
            command,
            shell,
            schedule,
            catch_up: true,
            overlap: OverlapPolicy::Skip,
            working_dir: None,
            env: Vec::new(),
            created_at: Utc::now(),
            last_run: None,
            next_run: None,
            last_exit_code: None,
            last_status: LastStatus::Never,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Name must not be empty".into());
        }
        if self.command.trim().is_empty() {
            return Err("Command must not be empty".into());
        }
        if let ShellKind::Custom { path, arg_template } = &self.shell {
            if path.trim().is_empty() {
                return Err("Custom shell path must not be empty".into());
            }
            if !arg_template.contains("{cmd}") {
                return Err("Custom argument template must contain {cmd}".into());
            }
        }
        self.schedule.validate()
    }
}

/// A single execution record (history).
#[derive(Clone, Debug)]
pub struct RunRecord {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub trigger: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn normalize_cron_adds_seconds_field() {
        assert_eq!(normalize_cron("0 9 * * *"), "0 0 9 * * *");
        assert_eq!(normalize_cron("0 0 9 * * *"), "0 0 9 * * *");
    }

    #[test]
    fn interval_next_after() {
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let s = Schedule::Interval { seconds: 60 };
        assert_eq!(s.next_after(base), Some(base + Duration::seconds(60)));
    }

    #[test]
    fn once_in_past_is_none() {
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let s = Schedule::Once { at: base - Duration::seconds(10) };
        assert_eq!(s.next_after(base), None);
        let s2 = Schedule::Once { at: base + Duration::seconds(10) };
        assert_eq!(s2.next_after(base), Some(base + Duration::seconds(10)));
    }

    #[test]
    fn cron_validation() {
        assert!(Schedule::Cron { expr: "0 9 * * *".into() }.validate().is_ok());
        assert!(Schedule::Cron { expr: "nonsense".into() }.validate().is_err());
    }

    #[test]
    fn rule_validation() {
        let mut r = Rule::new("n".into(), "echo hi".into(), ShellKind::Direct, Schedule::Interval { seconds: 5 });
        assert!(r.validate().is_ok());
        r.command = "  ".into();
        assert!(r.validate().is_err());
    }
}
