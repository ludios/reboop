// Model-output: Claude Opus 5.5

//! btrfs filesystems on a remote machine: operations a reboot would
//! interrupt, and scrubs.

use crate::deadline::Deadline;
use crate::ssh::{Session, shell_quote};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::Duration;

const QUICK: Duration = Duration::from_secs(30);

/// A mounted btrfs filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filesystem {
    pub uuid: String,
    /// One of the places it's mounted.
    pub mountpoint: String,
}

/// Lists mounted btrfs filesystems.
pub fn filesystems(session: &mut Session) -> Result<Vec<Filesystem>> {
    let output = session.run("findmnt --list --json --types btrfs --output UUID,TARGET", QUICK)?;
    // findmnt exits 1 without output when nothing matches.
    if output.status == 1 && output.stdout.is_empty() {
        return Ok(vec![]);
    }
    ensure!(output.status == 0, "findmnt exited with status {}: {}", output.status, output.stderr_text());
    parse_findmnt(&output.stdout_text())
}

fn parse_findmnt(json: &str) -> Result<Vec<Filesystem>> {
    #[derive(Deserialize)]
    struct Findmnt {
        filesystems: Vec<Mount>,
    }
    #[derive(Deserialize)]
    struct Mount {
        uuid: Option<String>,
        target: String,
    }
    let findmnt: Findmnt = serde_json::from_str(json).with_context(|| format!("unexpected findmnt output {json:?}"))?;
    let mut filesystems: Vec<Filesystem> = Vec::new();
    for mount in findmnt.filesystems {
        let uuid = mount.uuid.ok_or_else(|| anyhow!("findmnt doesn't know the UUID of the btrfs at {}", mount.target))?;
        // The same filesystem can be mounted several times, e.g. as subvolumes.
        if !filesystems.iter().any(|fs| fs.uuid == uuid) {
            filesystems.push(Filesystem { uuid, mountpoint: mount.target });
        }
    }
    Ok(filesystems)
}

/// The exclusive operation (balance, device replace, add, remove, resize…)
/// in progress on `filesystem`, or "none".  Includes "balance paused".
pub fn exclusive_operation(session: &mut Session, filesystem: &Filesystem) -> Result<String> {
    let path = format!("/sys/fs/btrfs/{}/exclusive_operation", filesystem.uuid);
    Ok(session.run_ok(&format!("cat {}", shell_quote(&path)), QUICK)?.trim_end().to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrubState {
    /// btrfs-progs has no record of a scrub.
    NeverRan,
    Running,
    Finished,
    /// Cancelled.
    Aborted,
    /// Stopped without finishing or being cancelled, e.g. by a reboot.
    Interrupted,
}

/// The state of the latest scrub of a filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrubStatus {
    pub state: ScrubState,
    /// When the scrub started or was last resumed, as btrfs-progs prints it.
    pub started: Option<String>,
    /// Roughly how many bytes the scrub has to read in total.
    pub total_bytes: Option<u64>,
    pub scrubbed_bytes: u64,
    pub bytes_per_sec: Option<u64>,
    /// Only known while running.
    pub seconds_left: Option<u64>,
    /// Counters of errors found, by name (e.g. "csum_errors"), if nonzero.
    pub errors: BTreeMap<String, u64>,
}

/// btrfs-progs' error counters, all of which we require to be present so
/// that a change in its output can't hide errors.
const ERROR_COUNTERS: &[&str] = &[
    "read_errors",
    "csum_errors",
    "verify_errors",
    "super_errors",
    "malloc_errors",
    "uncorrectable_errors",
    "unverified_errors",
    "corrected_errors",
];

pub fn scrub_status(session: &mut Session, mountpoint: &str) -> Result<ScrubStatus> {
    let mountpoint = shell_quote(mountpoint);
    let summary = session.run_ok(&format!("btrfs scrub status --raw -- {mountpoint}"), QUICK)?;
    let raw = session.run_ok(&format!("btrfs scrub status -R -- {mountpoint}"), QUICK)?;
    parse_scrub_status(&summary, &raw)
}

/// Parses the output of `btrfs scrub status --raw` (`summary`, which has
/// sizes in bytes) and `btrfs scrub status -R` (`raw`, which has counters).
fn parse_scrub_status(summary: &str, raw: &str) -> Result<ScrubStatus> {
    let context = || format!("unexpected btrfs scrub status output:\n{summary}\n{raw}");
    let fields = colon_fields(summary);
    let counters = colon_fields(raw);
    let counter = |name: &str| -> Result<u64> {
        let value = counters.get(name).ok_or_else(|| anyhow!("no {name}"))?;
        Ok(value.parse()?)
    };
    let number = |name: &str, suffix: &str| -> Result<Option<u64>> {
        let Some(value) = fields.get(name) else { return Ok(None) };
        let digits = value.split(suffix).next().unwrap_or_default();
        Ok(Some(digits.parse().with_context(|| format!("{name}: {value:?}"))?))
    };

    let state = match fields.get("Status").map(String::as_str) {
        Some("running") => ScrubState::Running,
        Some("finished") => ScrubState::Finished,
        Some("aborted") => ScrubState::Aborted,
        Some("interrupted") => ScrubState::Interrupted,
        None if summary.contains("no stats available") => ScrubState::NeverRan,
        other => bail!("unknown scrub state {other:?}\n{}", context()),
    };
    let mut errors = BTreeMap::new();
    for &name in ERROR_COUNTERS {
        let count = counter(name).with_context(context)?;
        if count > 0 {
            errors.insert(name.to_string(), count);
        }
    }
    let error_summary = fields.get("Error summary").ok_or_else(|| anyhow!("no error summary")).with_context(context)?;
    ensure!(errors.is_empty() == (error_summary == "no errors found"), "error counters disagree with the summary\n{}", context());
    let seconds_left = match fields.get("Time left") {
        Some(time) => Some(parse_hms(time).with_context(context)?),
        None => None,
    };

    Ok(ScrubStatus {
        state,
        started: fields.get("Scrub started").or(fields.get("Scrub resumed")).cloned(),
        total_bytes: number("Total to scrub", " ").with_context(context)?,
        scrubbed_bytes: (counter("data_bytes_scrubbed")? + counter("tree_bytes_scrubbed")?),
        bytes_per_sec: number("Rate", "/s").with_context(context)?,
        seconds_left,
        errors,
    })
}

/// Parses lines of "Key: value" into a map, ignoring other lines.
fn colon_fields(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// Parses "H:MM:SS" into seconds.
fn parse_hms(time: &str) -> Result<u64> {
    let parts: Vec<u64> = time.split(':').map(str::parse).collect::<Result<_, _>>()?;
    let [hours, minutes, seconds] = parts[..] else { bail!("not H:MM:SS: {time:?}") };
    Ok(hours * 3600 + minutes * 60 + seconds)
}

/// Starts scrubbing the btrfs filesystem mounted at `mountpoint`, and waits
/// (up to `timeout`) until btrfs-progs reports it as the latest scrub.
pub fn start_scrub(session: &mut Session, mountpoint: &str, timeout: Duration) -> Result<()> {
    let deadline = Deadline::after(timeout);
    let before = scrub_status(session, mountpoint)?;
    ensure!(before.state != ScrubState::Running, "a scrub is already running on {mountpoint}");
    session.run_ok(&format!("btrfs scrub start -- {}", shell_quote(mountpoint)), QUICK)?;
    while scrub_status(session, mountpoint)?.started == before.started {
        ensure!(!deadline.has_passed(), "the scrub of {mountpoint} didn't start");
        sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Checks the scrub of `mountpoint` every `interval`, passing each status to
/// `progress`, until it's no longer running, then returns its final status.
pub fn wait_for_scrub(
    session: &mut Session,
    mountpoint: &str,
    interval: Duration,
    deadline: Deadline,
    mut progress: impl FnMut(&ScrubStatus),
) -> Result<ScrubStatus> {
    loop {
        let status = scrub_status(session, mountpoint)?;
        progress(&status);
        if status.state != ScrubState::Running {
            return Ok(status);
        }
        ensure!(!deadline.has_passed(), "the scrub of {mountpoint} is still running at the deadline");
        sleep(interval);
    }
}

impl ScrubStatus {
    /// One line for people, e.g. "scrub has 3m 30s left, 130.50GB of
    /// 391.56GB (33.33%) scrubbed at 1.39GB/s, no errors found".
    pub fn summary(&self) -> String {
        let scrubbed = format_bytes(self.scrubbed_bytes);
        let errors = if self.errors.is_empty() {
            "no errors found".to_string()
        } else {
            let counts: Vec<_> = self.errors.iter().map(|(name, count)| format!("{name}={count}")).collect();
            format!("ERRORS FOUND: {}", counts.join(" "))
        };
        match self.state {
            ScrubState::NeverRan => "no scrub has run".to_string(),
            ScrubState::Running => {
                let left = self.seconds_left.map_or("unknown time".to_string(), format_seconds);
                let total = self.total_bytes.unwrap_or(0);
                let percent = if total == 0 { 0.0 } else { 100.0 * self.scrubbed_bytes as f64 / total as f64 };
                let rate = self.bytes_per_sec.map_or("?".to_string(), format_bytes);
                format!("scrub has {left} left, {scrubbed} of {} ({percent:.2}%) scrubbed at {rate}/s, {errors}", format_bytes(total))
            }
            ScrubState::Finished => format!("scrub finished, {scrubbed} scrubbed, {errors}"),
            ScrubState::Aborted => format!("scrub was cancelled after {scrubbed}, {errors}"),
            ScrubState::Interrupted => format!("scrub was interrupted after {scrubbed}, {errors}"),
        }
    }
}

/// Formats a byte count with SI units, e.g. "130.50GB".
fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = next;
    }
    format!("{value:.2}{unit}")
}

/// Formats seconds like "1h 2m 3s", leaving out leading zero units.
fn format_seconds(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m {seconds}s"),
        _ => format!("{hours}h {minutes}m {seconds}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW_COUNTERS: &str = "\tdata_extents_scrubbed: 10\n\ttree_extents_scrubbed: 5\n\
        \tdata_bytes_scrubbed: 130000000000\n\ttree_bytes_scrubbed: 500000000\n\
        \tread_errors: 0\n\tcsum_errors: 0\n\tverify_errors: 0\n\tno_csum: 7\n\tcsum_discards: 0\n\
        \tsuper_errors: 0\n\tmalloc_errors: 0\n\tuncorrectable_errors: 0\n\tunverified_errors: 0\n\
        \tcorrected_errors: 0\n\tlast_physical: 12345\n";

    fn raw(started: &str, status: &str, counters: &str) -> String {
        format!("UUID:             4c8a\nScrub started:    {started}\nStatus:           {status}\nDuration:         0:01:34\n{counters}")
    }

    #[test]
    fn parses_running_scrub() {
        let summary = "UUID:             4c8a\nScrub started:    Mon Sep 28 10:00:00 2026\nStatus:           running\n\
            Duration:         0:01:34\nTime left:        0:03:30\nETA:              Mon Sep 28 10:05:04 2026\n\
            Total to scrub:   391560000000\nBytes scrubbed:   130500000000  (33.33%)\n\
            Rate:             1390000000/s (limit 2000000000/s)\nError summary:    no errors found\n";
        let status = parse_scrub_status(summary, &raw("Mon Sep 28 10:00:00 2026", "running", RAW_COUNTERS)).unwrap();
        assert_eq!(
            status,
            ScrubStatus {
                state: ScrubState::Running,
                started: Some("Mon Sep 28 10:00:00 2026".into()),
                total_bytes: Some(391_560_000_000),
                scrubbed_bytes: 130_500_000_000,
                bytes_per_sec: Some(1_390_000_000),
                seconds_left: Some(210),
                errors: BTreeMap::new(),
            }
        );
        assert_eq!(status.summary(), "scrub has 3m 30s left, 130.50GB of 391.56GB (33.33%) scrubbed at 1.39GB/s, no errors found");
    }

    #[test]
    fn parses_finished_scrub_with_errors() {
        let summary = "UUID:             4c8a\nScrub started:    Mon Sep 28 10:00:00 2026\nStatus:           finished\n\
            Duration:         0:04:40\nTotal to scrub:   1000\nRate:             10/s\n\
            Error summary:    csum=3\n  Corrected:      0\n  Uncorrectable:  3\n  Unverified:     0\n";
        let counters = RAW_COUNTERS.replace("csum_errors: 0", "csum_errors: 3").replace("uncorrectable_errors: 0", "uncorrectable_errors: 3");
        let status = parse_scrub_status(summary, &raw("Mon Sep 28 10:00:00 2026", "finished", &counters)).unwrap();
        assert_eq!(status.state, ScrubState::Finished);
        assert_eq!(status.seconds_left, None);
        assert_eq!(status.errors, BTreeMap::from([("csum_errors".into(), 3), ("uncorrectable_errors".into(), 3)]));
        assert_eq!(status.summary(), "scrub finished, 130.50GB scrubbed, ERRORS FOUND: csum_errors=3 uncorrectable_errors=3");
    }

    #[test]
    fn parses_never_scrubbed() {
        let summary = "UUID:             4c8a\n\tno stats available\nTotal to scrub:   1000\nRate:             0/s\nError summary:    no errors found\n";
        let counters = RAW_COUNTERS.replace("130000000000", "0").replace("500000000", "0");
        let status = parse_scrub_status(summary, &format!("UUID:             4c8a\n\tno stats available\n{counters}")).unwrap();
        assert_eq!((status.state, status.started, status.scrubbed_bytes), (ScrubState::NeverRan, None, 0));
    }

    #[test]
    fn refuses_missing_counters_and_disagreements() {
        let summary = "Status:           finished\nTotal to scrub:   1000\nRate:             10/s\nError summary:    no errors found\n";
        let without_csum = RAW_COUNTERS.replace("\tcsum_errors: 0\n", "");
        assert!(parse_scrub_status(summary, &raw("x", "finished", &without_csum)).is_err());
        let with_errors = RAW_COUNTERS.replace("read_errors: 0", "read_errors: 1");
        assert!(parse_scrub_status(summary, &raw("x", "finished", &with_errors)).is_err());
        assert!(parse_scrub_status(&summary.replace("finished", "exploded"), &raw("x", "finished", RAW_COUNTERS)).is_err());
    }

    #[test]
    fn parses_findmnt() {
        let json = r#"{"filesystems": [
            {"uuid": "aaa", "target": "/"},
            {"uuid": "bbb", "target": "/mnt/with space"},
            {"uuid": "aaa", "target": "/home"}
        ]}"#;
        let filesystems = parse_findmnt(json).unwrap();
        assert_eq!(
            filesystems,
            vec![
                Filesystem { uuid: "aaa".into(), mountpoint: "/".into() },
                Filesystem { uuid: "bbb".into(), mountpoint: "/mnt/with space".into() },
            ]
        );
    }

    #[test]
    fn formats() {
        assert_eq!(format_bytes(999), "999B");
        assert_eq!(format_bytes(1_000), "1.00kB");
        assert_eq!(format_bytes(391_560_000_000), "391.56GB");
        assert_eq!(format_seconds(5), "5s");
        assert_eq!(format_seconds(210), "3m 30s");
        assert_eq!(format_seconds(3723), "1h 2m 3s");
    }
}
