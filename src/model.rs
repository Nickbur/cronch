//! Core domain types for Cronch: rules, schedules, shells, run records.

use chrono::{DateTime, Duration, Local, Utc};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest accepted interval: 100 years. Far beyond any practical schedule,
/// and it keeps every date computation well inside chrono's range.
pub const MAX_INTERVAL_SECS: i64 = 100 * 366 * 86_400;

/// Longest accepted per-rule timeout: 7 days (the editor's maximum).
pub const MAX_TIMEOUT_SECS: i64 = 7 * 86_400;

/// `t + secs`, or `None` when the result would leave chrono's range.
pub fn add_secs(t: DateTime<Utc>, secs: i64) -> Option<DateTime<Utc>> {
    t.checked_add_signed(Duration::try_seconds(secs)?)
}

/// The first interval tick strictly after `now`, counting from `anchor` in
/// steps of `seconds`, so the cadence keeps its phase across any gap.
pub fn next_interval_tick(
    anchor: DateTime<Utc>,
    seconds: i64,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if seconds < 1 {
        return None;
    }
    if anchor > now {
        return Some(anchor);
    }
    let steps = (now - anchor).num_seconds() / seconds + 1;
    add_secs(anchor, steps.checked_mul(seconds)?)
}

/// Parse a cron expression with standard (Vixie cron) semantics: 5 fields, or
/// 6 with a leading seconds field; day-of-week 0-7 where both 0 and 7 are
/// Sunday; when day-of-month and day-of-week are both restricted, a day
/// matching either one fires.
pub fn parse_cron(expr: &str) -> Result<Cron, String> {
    CronParser::builder()
        .seconds(Seconds::Optional)
        .year(Year::Disallowed)
        .build()
        .parse(expr)
        .map_err(|e| format!("Invalid cron expression: {e}"))
}

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
                } else if *seconds > MAX_INTERVAL_SECS {
                    Err("Interval must be at most 100 years".into())
                } else {
                    Ok(())
                }
            }
            Schedule::Cron { expr } => parse_cron(expr).map(|_| ()),
            Schedule::Once { .. } => Ok(()),
        }
    }

    /// The next fire time strictly after `after`, if any.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Interval { seconds } => add_secs(after, *seconds),
            Schedule::Cron { expr } => {
                // Cron fields are wall-clock terms, so evaluate in local time
                // (croner also resolves DST gaps and overlaps there).
                let local = after.with_timezone(&Local);
                parse_cron(expr)
                    .ok()?
                    .find_next_occurrence(&local, false)
                    .ok()
                    .map(|t| t.with_timezone(&Utc))
            }
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

/// Placeholder in a shell argument template replaced by the rule's command
/// (as a single argument) at execution time.
pub const CMD_PLACEHOLDER: &str = "{cmd}";

/// Split an argument template into words exactly the way the executor will, so
/// validation and execution can never disagree. The platform's quoting rules
/// apply (see [`split_command_line`]); the `{cmd}` placeholder is left in place
/// for the caller to expand.
pub fn parse_arg_template(template: &str) -> Result<Vec<String>, String> {
    split_command_line(template).map_err(|e| format!("Invalid argument template: {e}"))
}

/// Split a command line into program + arguments the way the platform's own
/// tools do: POSIX shell quoting on Unix; on Windows the `CommandLineToArgvW`
/// rules, where a backslash is literal unless it precedes a double quote, so
/// `C:\Tools\app.exe` stays intact.
pub fn split_command_line(line: &str) -> Result<Vec<String>, String> {
    #[cfg(windows)]
    {
        split_windows(line)
    }
    #[cfg(not(windows))]
    {
        shell_words::split(line).map_err(|e| e.to_string())
    }
}

/// `CommandLineToArgvW`-style word splitting. Compiled on every platform so its
/// rules are unit-tested everywhere, but only Windows uses it at runtime.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn split_windows(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    // A word has started (an empty `""` still counts as one).
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let mut n = 1;
                while chars.peek() == Some(&'\\') {
                    chars.next();
                    n += 1;
                }
                if chars.peek() == Some(&'"') {
                    // 2n backslashes + quote -> n backslashes, the quote is a
                    // delimiter; 2n+1 backslashes + quote -> n backslashes + `"`.
                    cur.extend(std::iter::repeat_n('\\', n / 2));
                    if n % 2 == 1 {
                        chars.next();
                        cur.push('"');
                    }
                } else {
                    cur.extend(std::iter::repeat_n('\\', n));
                }
                in_word = true;
            }
            '"' => {
                if quoted && chars.peek() == Some(&'"') {
                    // `""` inside a quoted run is a literal quote.
                    chars.next();
                    cur.push('"');
                } else {
                    quoted = !quoted;
                }
                in_word = true;
            }
            ' ' | '\t' if !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            _ => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quoted {
        return Err("missing closing quote".into());
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
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
    TimedOut,
    /// The run did not complete because the app shut down (graceful quit or a
    /// crash that left the run open).
    Cancelled,
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
            LastStatus::TimedOut => "TimedOut",
            LastStatus::Cancelled => "Cancelled",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "Running" => LastStatus::Running,
            "Success" => LastStatus::Success,
            "Failed" => LastStatus::Failed,
            "Skipped" => LastStatus::Skipped,
            "Expired" => LastStatus::Expired,
            "TimedOut" => LastStatus::TimedOut,
            "Cancelled" => LastStatus::Cancelled,
            _ => LastStatus::Never,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// Default per-rule timeout for new rules, in seconds: 5 minutes.
pub fn default_timeout_secs() -> i64 {
    300
}

/// Timeout for rules saved before per-rule timeouts existed (imported JSON
/// without the field). They always ran without a limit, so they keep doing so.
fn legacy_timeout_secs() -> i64 {
    0
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
    /// Kill the job after this many seconds; 0 = no limit.
    #[serde(default = "legacy_timeout_secs")]
    pub timeout_secs: i64,

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
            timeout_secs: default_timeout_secs(),
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
        if self.timeout_secs < 0 {
            return Err("Timeout must be 0 (no limit) or a positive number of seconds".into());
        }
        if self.timeout_secs > MAX_TIMEOUT_SECS {
            return Err("Timeout must be at most 7 days (604800 seconds)".into());
        }
        if let ShellKind::Custom { path, arg_template } = &self.shell {
            if path.trim().is_empty() {
                return Err("Custom shell path must not be empty".into());
            }
            let words = parse_arg_template(arg_template)?;
            if !words.iter().any(|w| w.contains(CMD_PLACEHOLDER)) {
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
    /// Recorded outcome; `None` for runs stored before outcomes were recorded.
    pub status: Option<LastStatus>,
    pub stdout: String,
    pub stderr: String,
    pub trigger: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Timelike, Weekday};

    /// The next `n` local fire times of a cron expression after `from`.
    fn cron_fires(expr: &str, from: DateTime<Local>, n: usize) -> Vec<DateTime<Local>> {
        let s = Schedule::Cron { expr: expr.into() };
        let mut t = from.with_timezone(&Utc);
        (0..n)
            .map(|_| {
                t = s.next_after(t).expect("cron should fire");
                t.with_timezone(&Local)
            })
            .collect()
    }

    /// Monday 2026-10-05 00:00 local time.
    fn monday() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 5, 0, 0, 0).unwrap()
    }

    #[test]
    fn cron_weekdays_follow_standard_numbering() {
        let days: Vec<Weekday> = cron_fires("0 9 * * 1-5", monday(), 6)
            .iter()
            .map(|t| t.weekday())
            .collect();
        assert_eq!(
            days,
            vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
                Weekday::Mon
            ],
            "1-5 must mean Monday to Friday"
        );
    }

    #[test]
    fn cron_sunday_is_0_and_7() {
        for expr in ["0 9 * * 0", "0 9 * * 7", "0 9 * * SUN"] {
            let fires = cron_fires(expr, monday(), 2);
            assert!(
                fires.iter().all(|t| t.weekday() == Weekday::Sun),
                "{expr} must fire on Sundays, got {fires:?}"
            );
        }
    }

    #[test]
    fn cron_day_of_month_or_day_of_week() {
        // Standard cron: the 1st of the month OR any Monday.
        let fires = cron_fires("0 0 1 * 1", monday(), 5);
        assert!(
            fires
                .iter()
                .all(|t| t.day() == 1 || t.weekday() == Weekday::Mon),
            "got {fires:?}"
        );
        assert!(
            fires
                .iter()
                .any(|t| t.day() == 1 && t.weekday() != Weekday::Mon),
            "the 1st must fire even when it is not a Monday, got {fires:?}"
        );
    }

    #[test]
    fn cron_accepts_optional_seconds_field() {
        // 6 fields are read as seconds + the 5 standard fields.
        let fires = cron_fires("30 0 9 * * *", monday(), 1);
        assert_eq!((fires[0].hour(), fires[0].second()), (9, 30));
        assert!(
            Schedule::Cron {
                expr: "0 0 9 * * * 2026".into()
            }
            .validate()
            .is_err(),
            "a year field is not supported"
        );
    }

    #[test]
    fn interval_bounds_are_enforced() {
        assert!(
            Schedule::Interval {
                seconds: MAX_INTERVAL_SECS
            }
            .validate()
            .is_ok()
        );
        assert!(
            Schedule::Interval {
                seconds: MAX_INTERVAL_SECS + 1
            }
            .validate()
            .is_err()
        );
        assert!(Schedule::Interval { seconds: 0 }.validate().is_err());
    }

    #[test]
    fn date_math_never_overflows() {
        let now = Utc::now();
        assert_eq!(add_secs(now, i64::MAX), None);
        assert_eq!(
            Schedule::Interval { seconds: i64::MAX }.next_after(now),
            None
        );
        assert_eq!(next_interval_tick(now, i64::MAX / 2, now), None);
    }

    #[test]
    fn interval_tick_keeps_phase() {
        let anchor = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        // 10 min cadence; it is now 00:37 -> next tick 00:40, not 00:47.
        let now = anchor + Duration::minutes(37);
        assert_eq!(
            next_interval_tick(anchor, 600, now),
            Some(anchor + Duration::minutes(40))
        );
        // Exactly on a tick -> the following one (strictly after now).
        assert_eq!(
            next_interval_tick(anchor, 600, anchor + Duration::minutes(40)),
            Some(anchor + Duration::minutes(50))
        );
        // A future anchor is itself the next tick.
        assert_eq!(
            next_interval_tick(anchor, 600, anchor - Duration::seconds(1)),
            Some(anchor)
        );
    }

    #[test]
    fn windows_splitting_keeps_backslashes() {
        assert_eq!(
            split_windows(r"C:\Tools\app.exe --flag").unwrap(),
            vec![r"C:\Tools\app.exe", "--flag"]
        );
        assert_eq!(
            split_windows(r#""C:\Program Files\App\app.exe" -x"#).unwrap(),
            vec![r"C:\Program Files\App\app.exe", "-x"]
        );
        assert_eq!(
            split_windows(r"\\server\share\dir\ end").unwrap(),
            vec![r"\\server\share\dir\", "end"]
        );
    }

    #[test]
    fn windows_splitting_quote_rules() {
        // 3 backslashes + quote -> 1 backslash + a literal quote.
        assert_eq!(split_windows(r#"a\\\"b"#).unwrap(), vec![r#"a\"b"#]);
        // 2 backslashes + quote -> 1 backslash, the quote opens a quoted run.
        assert_eq!(split_windows(r#"a\\"b c""#).unwrap(), vec![r"a\b c"]);
        assert_eq!(split_windows(r#""a b" """#).unwrap(), vec!["a b", ""]);
        assert_eq!(
            split_windows(r#""say ""hi""""#).unwrap(),
            vec![r#"say "hi""#]
        );
        assert_eq!(split_windows(r#"-c "{cmd}""#).unwrap(), vec!["-c", "{cmd}"]);
        assert!(split_windows(r#""unterminated"#).is_err());
    }

    #[test]
    fn imported_rule_without_timeout_keeps_running_unbounded() {
        let mut v = serde_json::to_value(Rule::new(
            "n".into(),
            "echo hi".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 5 },
        ))
        .unwrap();
        v.as_object_mut().unwrap().remove("timeout_secs");
        let r: Rule = serde_json::from_value(v).unwrap();
        assert_eq!(r.timeout_secs, 0, "pre-timeout rules must stay unbounded");
    }

    #[test]
    fn rule_rejects_timeout_over_limit() {
        let mut r = Rule::new(
            "n".into(),
            "echo hi".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 5 },
        );
        r.timeout_secs = MAX_TIMEOUT_SECS;
        assert!(r.validate().is_ok());
        r.timeout_secs = MAX_TIMEOUT_SECS + 1;
        assert!(r.validate().is_err());
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
        let s = Schedule::Once {
            at: base - Duration::seconds(10),
        };
        assert_eq!(s.next_after(base), None);
        let s2 = Schedule::Once {
            at: base + Duration::seconds(10),
        };
        assert_eq!(s2.next_after(base), Some(base + Duration::seconds(10)));
    }

    #[test]
    fn cron_validation() {
        assert!(
            Schedule::Cron {
                expr: "0 9 * * *".into()
            }
            .validate()
            .is_ok()
        );
        assert!(
            Schedule::Cron {
                expr: "nonsense".into()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn cron_next_after_is_evaluated_in_local_time() {
        let s = Schedule::Cron {
            expr: "0 9 * * *".into(),
        };
        let next = s.next_after(Utc::now()).expect("cron should fire");
        assert_eq!(
            next.with_timezone(&Local).hour(),
            9,
            "cron must be interpreted in local time, not UTC"
        );
    }

    #[test]
    fn rule_validation() {
        let mut r = Rule::new(
            "n".into(),
            "echo hi".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 5 },
        );
        assert!(r.validate().is_ok());
        r.command = "  ".into();
        assert!(r.validate().is_err());
    }

    #[test]
    fn rule_default_timeout_is_300_seconds() {
        let r = Rule::new(
            "n".into(),
            "echo hi".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 5 },
        );
        assert_eq!(r.timeout_secs, 300, "new rules must have a default timeout");
    }

    #[test]
    fn rule_rejects_negative_timeout() {
        let mut r = Rule::new(
            "n".into(),
            "echo hi".into(),
            ShellKind::Direct,
            Schedule::Interval { seconds: 5 },
        );
        r.timeout_secs = -1;
        assert!(r.validate().is_err());
    }

    #[test]
    fn last_status_timed_out_roundtrips() {
        assert_eq!(LastStatus::TimedOut.as_str(), "TimedOut");
        assert_eq!(LastStatus::from_str_lossy("TimedOut"), LastStatus::TimedOut);
        assert_eq!(LastStatus::Cancelled.as_str(), "Cancelled");
        assert_eq!(
            LastStatus::from_str_lossy("Cancelled"),
            LastStatus::Cancelled
        );
        assert_eq!(LastStatus::from_str_lossy("unknown"), LastStatus::Never);
    }
}
