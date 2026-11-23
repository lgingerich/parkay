//! Machine hygiene checks and sleep protection for benchmarking.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{ChildGuard, output};

/// A process using this share of one core or more makes the machine too busy to
/// measure on. Spotlight indexing (97% of a core) skewed earlier runs.
const BUSY_PERCENT_MIN: f64 = 50.0;

/// Times the machine is re-checked after the build, two seconds apart, for build
/// processes to wind down before the run is refused.
const SETTLE_CHECKS_MAX: u32 = 5;

/// Upper bound on bytes read from `pmset -g log` so a pathological or runaway log
/// stream cannot stall or exhaust memory.
const PMSET_LOG_BYTES_MAX: u64 = 128 * 1024 * 1024;

/// What the machine was doing, and what about it makes a measurement untrustworthy.
pub struct Conditions {
    pub notes: Vec<String>,
    pub problems: Vec<String>,
    /// True if at least one problem is non-transient (e.g. battery power or low power
    /// mode) and will not clear by waiting a few seconds in [`settle`].
    pub has_persistent_problem: bool,
}

fn parse_cpuinfo_model(info: &str) -> Option<String> {
    info.lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_owned())
}

/// The host and CPU, so results from one machine are not mistaken for another's.
pub fn machine() -> String {
    let host = output(&mut Command::new("hostname")).unwrap_or_default();
    let cpu = if cfg!(target_os = "macos") {
        output(Command::new("sysctl").args(["-n", "machdep.cpu.brand_string"]))
    } else {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|info| parse_cpuinfo_model(&info))
    };
    format!("{} / {}", host.trim(), cpu.unwrap_or_default().trim())
}

pub fn conditions() -> Conditions {
    let mut notes = Vec::new();
    let mut problems = Vec::new();
    let mut has_persistent_problem = false;
    if cfg!(target_os = "macos") {
        power(&mut notes, &mut problems, &mut has_persistent_problem);
    } else {
        // Simplification: power and sleep are checked on macOS only, where parkay is
        // measured today. On Linux, read /sys/class/power_supply and the journal.
        notes.push("power: not checked on this OS".into());
    }
    busy(&mut notes, &mut problems, &mut has_persistent_problem);
    Conditions {
        notes,
        problems,
        has_persistent_problem,
    }
}

fn power(notes: &mut Vec<String>, problems: &mut Vec<String>, persistent: &mut bool) {
    match output(Command::new("pmset").args(["-g", "batt"])) {
        Some(batt) if batt.contains("Battery Power") => {
            notes.push("power: battery".into());
            problems.push("on battery power: plug in to AC".into());
            *persistent = true;
        }
        Some(_) => notes.push("power: AC".into()),
        None => {
            problems.push("`pmset -g batt` failed: power source unknown".into());
            *persistent = true;
        }
    }
    let settings = output(Command::new("pmset").arg("-g")).unwrap_or_default();
    for key in ["lowpowermode", "powermode"] {
        let value = settings.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some(key)).then(|| words.next().unwrap_or("").to_owned())
        });
        if let Some(value) = value {
            notes.push(format!("{key}: {value}"));
            // `powermode` 1 and `lowpowermode` 1 are both low power mode.
            if value == "1" {
                problems.push(format!("low power mode is on ({key} 1)"));
                *persistent = true;
            }
        }
    }
    let thermal = output(Command::new("pmset").args(["-g", "therm"])).unwrap_or_default();
    for line in thermal.lines().filter(|l| l.contains("CPU_Speed_Limit")) {
        let limit = line.rsplit('=').next().unwrap_or("").trim();
        notes.push(format!("cpu speed limit: {limit}"));
        if limit != "100" {
            problems.push(format!("the CPU is thermally throttled (limit {limit})"));
        }
    }
}

fn busy(notes: &mut Vec<String>, problems: &mut Vec<String>, persistent: &mut bool) {
    let Some(listing) = output(Command::new("ps").args(["-Ao", "pid=,%cpu=,comm="])) else {
        problems.push("`ps` failed: load unknown".into());
        *persistent = true;
        return;
    };
    let own = std::process::id().to_string();
    let mut processes: Vec<(f64, String)> = listing
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?;
            let cpu: f64 = words.next()?.parse().ok()?;
            let command: Vec<&str> = words.collect();
            let command = command.join(" ");
            let program = command.rsplit('/').next().unwrap_or(&command).to_owned();
            (pid != own).then_some((cpu, program))
        })
        .collect();
    processes.sort_by(|x, y| y.0.total_cmp(&x.0));
    let top: Vec<String> = processes
        .iter()
        .take(3)
        .map(|(cpu, program)| format!("{cpu:.0}% {program}"))
        .collect();
    notes.push(format!("busiest: {}", top.join(", ")));
    for (cpu, program) in processes.iter().filter(|(cpu, _)| *cpu >= BUSY_PERCENT_MIN) {
        problems.push(format!("{program} is using {cpu:.0}% of a core"));
    }
}

pub fn refuse_if_noisy(conditions: &Conditions, allow_noisy: bool) -> Result<(), String> {
    if conditions.problems.is_empty() || allow_noisy {
        return Ok(());
    }
    Err(format!(
        "the machine is not fit to measure on:\n  {}\nFix these, or pass --allow-noisy to \
         measure anyway (the run records that it did).",
        conditions.problems.join("\n  ")
    ))
}

fn should_retry_settle(conditions: &Conditions, allow_noisy: bool) -> bool {
    !allow_noisy && !conditions.problems.is_empty() && !conditions.has_persistent_problem
}

/// Checks the machine again after the build, giving transient build processes or thermal
/// cooldown a few seconds to wind down, and returns the conditions the measurement starts
/// under. Persistent problems (battery power, low power mode) fail immediately.
pub fn settle(allow_noisy: bool) -> Result<Conditions, String> {
    let mut latest = conditions();
    for _ in 1..SETTLE_CHECKS_MAX {
        if !should_retry_settle(&latest, allow_noisy) {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
        latest = conditions();
    }
    refuse_if_noisy(&latest, allow_noisy)?;
    Ok(latest)
}

/// Keeps the machine from idle and system sleep while measuring (macOS
/// `caffeinate`). Closing the lid still sleeps it; [`sleeps_between`] catches that.
pub struct KeepAwake {
    _guard: Option<ChildGuard>,
}

impl KeepAwake {
    pub fn start() -> Self {
        if !cfg!(target_os = "macos") {
            return Self { _guard: None };
        }
        let pid = std::process::id().to_string();
        Self {
            _guard: Command::new("caffeinate")
                .args(["-i", "-s", "-w", &pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
                .map(ChildGuard::new),
        }
    }
}

/// The local time as `pmset -g log` prints it, so the two compare as strings.
pub fn local_time() -> String {
    output(Command::new("date").arg("+%Y-%m-%d %H:%M:%S")).map_or_default(|s| s.trim().to_owned())
}

/// Streams a `pmset -g log` reader line-by-line (reusing a single line buffer and
/// capping total bytes read) and returns the `YYYY-MM-DD HH:MM:SS` timestamps of
/// `Sleep` events in `[start, end]`.
///
/// Because `pmset -g log` emits entries oldest-first, hitting `max_bytes` before EOF
/// means the recent `[start, end]` window at the end of the log may have been cut off.
/// In that case a marker entry is appended instead of silently reporting "no sleep".
fn parse_sleeps(reader: impl Read, start: &str, end: &str, max_bytes: u64) -> Vec<String> {
    if start.len() != 19 || end.len() != 19 {
        return Vec::new();
    }
    let start_bytes = start.as_bytes();
    let end_bytes = end.as_bytes();
    // Allow reading one byte past `max_bytes` so we can distinguish exact EOF from
    // truncation.
    let mut reader = BufReader::new(reader.take(max_bytes.saturating_add(1)));
    let mut buf = Vec::new();
    let mut sleeps = Vec::new();
    let mut bytes_read = 0_u64;
    let mut truncated = false;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                bytes_read = bytes_read.saturating_add(count as u64);
                if bytes_read > max_bytes {
                    truncated = true;
                    break;
                }
            }
        }
        // Fast byte-level timestamp prefix check before UTF-8 validation or splitting:
        // `2026-11-19 17:53:49 -0600 Sleep   Entering Sleep state ...`
        if buf.len() <= 19 || !buf[0].is_ascii_digit() || buf[4] != b'-' || buf[10] != b' ' {
            continue;
        }
        let stamp = &buf[..19];
        if !stamp.is_ascii() || stamp < start_bytes || stamp > end_bytes {
            continue;
        }
        if let Ok(line) = std::str::from_utf8(&buf)
            && line.split_whitespace().nth(3) == Some("Sleep")
        {
            sleeps.push(line[..19].to_owned());
        }
    }
    if truncated {
        sleeps.push("unknown (pmset log truncated at byte cap)".to_owned());
    }
    sleeps
}

/// The times the machine went to sleep between `start` and `end`.
pub fn sleeps_between(start: &str, end: &str) -> Vec<String> {
    if !cfg!(target_os = "macos") || start.len() != 19 || end.len() != 19 {
        return Vec::new();
    }
    let Ok(mut child) = Command::new("pmset")
        .args(["-g", "log"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let Some(stdout) = child.stdout.take() else {
        let _guard = ChildGuard::new(child);
        return Vec::new();
    };
    let guard = ChildGuard::new(child);
    let sleeps = parse_sleeps(stdout, start, end, PMSET_LOG_BYTES_MAX);
    // Drop `guard` so if we stopped at `PMSET_LOG_BYTES_MAX`, `pmset` is killed cleanly
    // instead of blocking on a broken pipe; if EOF was reached, it exits immediately.
    drop(guard);
    sleeps
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{Conditions, parse_cpuinfo_model, parse_sleeps, should_retry_settle};

    #[test]
    fn settle_retries_only_transient_problems() {
        let clean = Conditions {
            notes: Vec::new(),
            problems: Vec::new(),
            has_persistent_problem: false,
        };
        assert!(!should_retry_settle(&clean, false));

        let transient = Conditions {
            notes: Vec::new(),
            problems: vec!["rustc is using 90% of a core".into()],
            has_persistent_problem: false,
        };
        assert!(should_retry_settle(&transient, false));
        assert!(!should_retry_settle(&transient, true));

        let persistent = Conditions {
            notes: Vec::new(),
            problems: vec!["on battery power: plug in to AC".into()],
            has_persistent_problem: true,
        };
        assert!(!should_retry_settle(&persistent, false));
    }

    #[test]
    fn cpuinfo_model_strips_key_and_whitespace() {
        let cpuinfo = "\
processor\t: 0
vendor_id\t: AuthenticAMD
model name\t: AMD EPYC 7763 64-Core Processor
cpu MHz\t\t: 2450.000
";
        assert_eq!(
            parse_cpuinfo_model(cpuinfo).as_deref(),
            Some("AMD EPYC 7763 64-Core Processor")
        );
        assert_eq!(parse_cpuinfo_model("processor\t: 0\n"), None);
    }

    #[test]
    fn parse_sleeps_streams_and_filters_window_and_kind() {
        let log = "\
2026-11-19 17:50:00 -0600 Sleep   Entering Sleep state (before window)
2026-11-19 17:52:10 -0600 Assertions   PID 123 created PreventUserIdleSystemSleep
2026-11-19 17:53:49 -0600 Sleep   Entering Sleep state due to Clamshell Sleep
2026-11-19 17:54:00 -0600 Wake    DarkWake to FullWake
2026-11-19 17:59:59 -0600 Sleep   Entering Sleep state (after window)
";
        let sleeps = parse_sleeps(
            Cursor::new(log.as_bytes()),
            "2026-11-19 17:51:00",
            "2026-11-19 17:55:00",
            1024 * 1024,
        );
        assert_eq!(sleeps, vec!["2026-11-19 17:53:49"]);
    }

    #[test]
    fn parse_sleeps_flags_truncation_instead_of_silently_missing_recent_sleeps() {
        let log = "\
2026-11-19 17:50:00 -0600 Assertions   padding line one
2026-11-19 17:53:49 -0600 Sleep   Entering Sleep state
";
        let sleeps = parse_sleeps(
            Cursor::new(log.as_bytes()),
            "2026-11-19 17:50:00",
            "2026-11-19 17:55:00",
            40,
        );
        assert_eq!(
            sleeps,
            vec!["unknown (pmset log truncated at byte cap)".to_owned()]
        );

        // Fitting exactly within `max_bytes` is not truncated.
        let exact = parse_sleeps(
            Cursor::new(log.as_bytes()),
            "2026-11-19 17:50:00",
            "2026-11-19 17:55:00",
            log.len() as u64,
        );
        assert_eq!(exact, vec!["2026-11-19 17:53:49"]);
    }
}
