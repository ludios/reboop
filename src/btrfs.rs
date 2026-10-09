// Model-output: Claude Opus 5.5
// Model-output: Claude Fable 5.1

//! btrfs filesystems on a remote machine: operations a reboot would
//! interrupt, and scrubs.

use crate::deadline::Deadline;
use crate::human;
use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::Duration;
use tracing::debug;

/// A mounted btrfs filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Filesystem {
    pub uuid: String,
    /// One of the places it's mounted.
    pub mountpoint: String,
}

/// Parses `findmnt --json` output into filesystems, one per UUID.
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

/// The exclusive operation (balance, device replace, add, remove, resize…)
/// in progress on `filesystem`, or "none".  Includes "balance paused".
pub fn exclusive_operation(session: &mut Session, filesystem: &Filesystem) -> Result<String> {
    let path = format!("/sys/fs/btrfs/{}/exclusive_operation", filesystem.uuid);
    Ok(session.run_ok(&format!("cat {}", shell_quote(&path)), QUICK)?.trim_end().to_string())
}

/// The nonzero ones among the counters called `names`, all of which
/// `counter` must know, so that a change in their source can't hide errors.
fn nonzero_counters(names: &[&str], counter: impl Fn(&str) -> Result<u64>) -> Result<BTreeMap<String, u64>> {
    let mut nonzero = BTreeMap::new();
    for &name in names {
        let count = counter(name)?;
        if count > 0 {
            nonzero.insert(name.to_string(), count);
        }
    }
    Ok(nonzero)
}

/// A device of a mounted btrfs filesystem, and its troubles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Device {
    pub devid: u64,
    /// Whether the filesystem is running without it.
    pub missing: bool,
    /// Its error counters (e.g. "read_errs") that aren't zero, or `None` if
    /// the kernel has none for it (e.g. on a filesystem mounted with
    /// rescue=ibadroots).  They count since the filesystem was made or they
    /// were reset (`btrfs device stats -z`).
    pub errors: Option<BTreeMap<String, u64>>,
}

impl Device {
    /// Whether it's missing or has had errors, which calls for a human to
    /// look before rebooting.
    pub fn has_trouble(&self) -> bool {
        self.missing || self.errors.as_ref().is_some_and(|errors| !errors.is_empty())
    }
}

/// The counters in sysfs' error_stats.
const DEVICE_ERROR_COUNTERS: &[&str] = &["write_errs", "read_errs", "flush_errs", "corruption_errs", "generation_errs"];

/// Parses lines of "DEVID missing 0|1", and "DEVID COUNTER VALUE" or "DEVID
/// invalid" (when the kernel has no counters), into devices, in order of
/// devid.
fn parse_devices(text: &str) -> Result<Vec<Device>> {
    /// What the lines say about a device.
    #[derive(Default)]
    struct Lines<'a> {
        fields: BTreeMap<&'a str, u64>,
        invalid_counters: bool,
    }
    let context = || format!("unexpected btrfs device info:\n{text}");
    let mut devices: BTreeMap<u64, Lines> = BTreeMap::new();
    for line in text.lines() {
        let (devid, rest) = line.split_once(' ').ok_or_else(|| anyhow!(context()))?;
        let device = devices.entry(devid.parse().with_context(context)?).or_default();
        if rest == "invalid" {
            device.invalid_counters = true;
        } else {
            let (name, value) = rest.split_once(' ').ok_or_else(|| anyhow!(context()))?;
            device.fields.insert(name, value.parse().with_context(context)?);
        }
    }
    ensure!(!devices.is_empty(), "no devices\n{}", context());
    devices
        .into_iter()
        .map(|(devid, device)| {
            let field = |name: &str| device.fields.get(name).copied().ok_or_else(|| anyhow!("no {name} for device {devid}\n{}", context()));
            let missing = match field("missing")? {
                0 => false,
                1 => true,
                other => bail!("device {devid} has missing={other}\n{}", context()),
            };
            let errors = if device.invalid_counters { None } else { Some(nonzero_counters(DEVICE_ERROR_COUNTERS, field)?) };
            Ok(Device { devid, missing, errors })
        })
        .collect()
}

/// The /dev path of a block device from the name of its sysfs entry, which
/// spells a "/" in the name as "!" (like cciss!c0d0 for /dev/cciss/c0d0).
fn device_path(sysfs_name: &str) -> String {
    format!("/dev/{}", sysfs_name.replace('!', "/"))
}

/// The block devices (as /dev paths) that `filesystems` are on, from sysfs,
/// which leaves out a missing device.
pub fn device_paths(session: &mut Session, filesystems: &[Filesystem]) -> Result<Vec<String>> {
    let dirs: Vec<String> = filesystems.iter().map(|fs| shell_quote(&format!("/sys/fs/btrfs/{}/devices", fs.uuid))).collect();
    let script = format!("set -e\nfor d in {}; do ls -1 \"$d\"; done", dirs.join(" "));
    let listing = session.run_ok(&script, QUICK)?;
    Ok(listing.lines().map(device_path).collect())
}

/// The devices of `filesystem`, from sysfs.
pub fn devices(session: &mut Session, filesystem: &Filesystem) -> Result<Vec<Device>> {
    let dir = format!("/sys/fs/btrfs/{}/devinfo", filesystem.uuid);
    let script = format!(
        r#"set -eu
        cd {}
        for d in *; do
            missing=$(cat "$d/missing")
            echo "$d missing $missing"
            sed "s|^|$d |" "$d/error_stats"
        done"#,
        shell_quote(&dir)
    );
    parse_devices(&session.run_ok(&script, QUICK)?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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

/// Parses the output of `btrfs scrub status --raw` (`summary`, with sizes in
/// bytes) and then `btrfs scrub status -R` (`raw`, with counters).  The state
/// and counters come from `raw`, so they agree even if the scrub moved on
/// between the two commands; `summary` provides progress estimates.
fn parse_scrub_status(summary: &str, raw: &str) -> Result<ScrubStatus> {
    let context = || format!("unexpected btrfs scrub status output:\n{summary}\n{raw}");
    let estimates = colon_fields(summary);
    let fields = colon_fields(raw);
    let counter = |name: &str| -> Result<u64> {
        let value = fields.get(name).ok_or_else(|| anyhow!("no {name}"))?;
        Ok(value.parse()?)
    };
    let estimate = |name: &str, suffix: &str| -> Result<Option<u64>> {
        let Some(value) = estimates.get(name) else { return Ok(None) };
        let digits = value.split(suffix).next().unwrap_or_default();
        Ok(Some(digits.parse().with_context(|| format!("{name}: {value:?}"))?))
    };

    let state = match fields.get("Status").map(String::as_str) {
        Some("running") => ScrubState::Running,
        Some("finished") => ScrubState::Finished,
        Some("aborted") => ScrubState::Aborted,
        Some("interrupted") => ScrubState::Interrupted,
        None if raw.contains("no stats available") => ScrubState::NeverRan,
        other => bail!("unknown scrub state {other:?}\n{}", context()),
    };
    let errors = nonzero_counters(ERROR_COUNTERS, counter).with_context(context)?;
    // Counters only grow, so errors in the earlier summary must show up in
    // the later counters, or we're missing a counter.
    let summary_has_errors = estimates.get("Error summary").is_some_and(|summary| summary != "no errors found");
    ensure!(!summary_has_errors || !errors.is_empty(), "the summary reports errors that no known counter has\n{}", context());
    let seconds_left = match estimates.get("Time left") {
        Some(time) => Some(parse_hms(time).with_context(context)?),
        None => None,
    };

    Ok(ScrubStatus {
        state,
        started: fields.get("Scrub started").or(fields.get("Scrub resumed")).cloned(),
        total_bytes: estimate("Total to scrub", " ").with_context(context)?,
        scrubbed_bytes: counter("data_bytes_scrubbed").with_context(context)? + counter("tree_bytes_scrubbed").with_context(context)?,
        bytes_per_sec: estimate("Rate", "/s").with_context(context)?,
        seconds_left,
        errors,
    })
}

/// What `btrfs scrub status` says when another btrfs command has the status
/// file locked, such as a scrub rewriting it, which one does every 5s.  It
/// reads that file whenever it can't ask the scrub over its socket, which
/// `btrfs scrub start` removes as the scrub goes into the background, and
/// doesn't wait for the lock.
const STATUS_FILE_LOCKED: &str = "failed to open status file: Resource temporarily unavailable";

/// How long to wait before each retry of a `btrfs scrub status` that found
/// the status file locked.
const LOCKED_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

/// Runs `script`, a `btrfs scrub status` command, and returns its stdout.
/// While it finds the status file locked, tries again after each of
/// [`LOCKED_RETRY_DELAYS`].
fn run_scrub_status(session: &mut Session, script: &str) -> Result<String> {
    for delay in LOCKED_RETRY_DELAYS {
        match session.run_ok(script, QUICK) {
            Err(error) if error.to_string().contains(STATUS_FILE_LOCKED) => {
                debug!("{error:#}; trying again in {delay:?}");
                sleep(delay);
            }
            result => return result,
        }
    }
    session.run_ok(script, QUICK).with_context(|| format!("tried {} times", LOCKED_RETRY_DELAYS.len() + 1))
}

/// The state of the latest scrub of the btrfs filesystem mounted at
/// `mountpoint`.
pub fn scrub_status(session: &mut Session, mountpoint: &str) -> Result<ScrubStatus> {
    let mountpoint = shell_quote(mountpoint);
    let summary = run_scrub_status(session, &format!("btrfs scrub status --raw -- {mountpoint}"))?;
    let raw = run_scrub_status(session, &format!("btrfs scrub status -R -- {mountpoint}"))?;
    parse_scrub_status(&summary, &raw)
}

/// Starts scrubbing the btrfs filesystem mounted at `mountpoint`, and waits
/// (up to `timeout`) until btrfs-progs reports it as the latest scrub.
pub fn start_scrub(session: &mut Session, mountpoint: &str, timeout: Duration) -> Result<()> {
    let deadline = Deadline::after(timeout);
    let before = scrub_status(session, mountpoint)?;
    ensure!(before.state != ScrubState::Running, "a scrub is already running on {mountpoint}");
    session.run_ok(&format!("btrfs scrub start -- {}", shell_quote(mountpoint)), QUICK)?;
    // For a moment, the new scrub has "no stats available".  (Start times
    // are to the second, so a scrub that started and ended within the same
    // second as this one would fool us, but that's no real filesystem.)
    loop {
        let started = scrub_status(session, mountpoint)?.started;
        if started.is_some() && started != before.started {
            return Ok(());
        }
        ensure!(!deadline.has_passed(), "the scrub of {mountpoint} didn't start");
        sleep(Duration::from_millis(200).min(deadline.remaining()));
    }
}

/// Checks the scrub of `mountpoint` that [`start_scrub`] started every
/// `interval`, passing each status to `progress`, until two checks in a row
/// find that it ended the same way, then returns its final status.
pub fn wait_for_scrub(
    session: &mut Session,
    mountpoint: &str,
    interval: Duration,
    deadline: Deadline,
    mut progress: impl FnMut(&ScrubStatus),
) -> Result<ScrubStatus> {
    let mut previous = ScrubState::Running;
    loop {
        let status = scrub_status(session, mountpoint)?;
        progress(&status);
        // Until the scrub has recorded how it ended, which takes it a moment
        // after the kernel is done, btrfs-progs can say that it was
        // interrupted, or that none ran.
        if status.state != ScrubState::Running && status.state == previous {
            return Ok(status);
        }
        previous = status.state;
        ensure!(!deadline.has_passed(), "the scrub of {mountpoint} is still running at the deadline");
        sleep(interval.min(deadline.remaining()));
    }
}

impl ScrubStatus {
    /// One line for people, e.g. "scrub has 3m 30s left, 130.50 GB of
    /// 391.56 GB (33.33%) scrubbed at 1.39 GB/s, no errors found".
    pub fn summary(&self) -> String {
        let scrubbed = human::bytes(self.scrubbed_bytes);
        let errors = if self.errors.is_empty() {
            "no errors found".to_string()
        } else {
            format!("ERRORS FOUND: {}", human::counters(&self.errors))
        };
        match self.state {
            ScrubState::NeverRan => "no scrub has run".to_string(),
            ScrubState::Running => {
                let left = self.seconds_left.map_or("unknown time".to_string(), human::seconds);
                let total = self.total_bytes.unwrap_or(0);
                let percent = if total == 0 { 0.0 } else { 100.0 * self.scrubbed_bytes as f64 / total as f64 };
                let rate = self.bytes_per_sec.map_or("?/s".to_string(), |bytes_per_sec| human::rate(bytes_per_sec as f64));
                format!("scrub has {left} left, {scrubbed} of {} ({percent:.2}%) scrubbed at {rate}, {errors}", human::bytes(total))
            }
            ScrubState::Finished => format!("scrub finished, {scrubbed} scrubbed, {errors}"),
            ScrubState::Aborted => format!("scrub was cancelled after {scrubbed}, {errors}"),
            ScrubState::Interrupted => format!("scrub was interrupted after {scrubbed}, {errors}"),
        }
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
        assert_eq!(status.summary(), "scrub has 3m 30s left, 130.50 GB of 391.56 GB (33.33%) scrubbed at 1.39 GB/s, no errors found");
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
        assert_eq!(status.summary(), "scrub finished, 130.50 GB scrubbed, ERRORS FOUND: csum_errors=3 uncorrectable_errors=3");
    }

    #[test]
    fn parses_never_scrubbed() {
        let summary = "UUID:             4c8a\n\tno stats available\nTotal to scrub:   1000\nRate:             0/s\nError summary:    no errors found\n";
        let counters = RAW_COUNTERS.replace("130000000000", "0").replace("500000000", "0");
        let status = parse_scrub_status(summary, &format!("UUID:             4c8a\n\tno stats available\n{counters}")).unwrap();
        assert_eq!((status.state, status.started, status.scrubbed_bytes), (ScrubState::NeverRan, None, 0));
    }

    #[test]
    fn refuses_missing_counters_and_unknown_states() {
        let summary = "Total to scrub:   1000\nRate:             10/s\nError summary:    no errors found\n";
        let without_csum = RAW_COUNTERS.replace("\tcsum_errors: 0\n", "");
        assert!(parse_scrub_status(summary, &raw("x", "finished", &without_csum)).is_err());
        assert!(parse_scrub_status(summary, &raw("x", "exploded", RAW_COUNTERS)).is_err());
        // Errors that the counters we know don't account for
        let with_errors = summary.replace("no errors found", "novel=1");
        assert!(parse_scrub_status(&with_errors, &raw("x", "finished", RAW_COUNTERS)).is_err());
    }

    #[test]
    fn takes_state_and_errors_from_the_same_output() {
        // The scrub found an error between the two commands.
        let summary = "Status:           running\nTotal to scrub:   1000\nRate:             10/s\nError summary:    no errors found\n";
        let counters = RAW_COUNTERS.replace("csum_errors: 0", "csum_errors: 1");
        let status = parse_scrub_status(summary, &raw("x", "finished", &counters)).unwrap();
        assert_eq!(status.state, ScrubState::Finished);
        assert_eq!(status.errors, BTreeMap::from([("csum_errors".into(), 1)]));
    }

    #[test]
    fn parses_devices() {
        let counters = |devid: u64, corruption: u64| {
            format!("{devid} write_errs 0\n{devid} read_errs 0\n{devid} flush_errs 0\n{devid} corruption_errs {corruption}\n{devid} generation_errs 0\n")
        };
        let text = format!("1 missing 0\n{}2 missing 1\n{}", counters(1, 3), counters(2, 0));
        assert_eq!(
            parse_devices(&text).unwrap(),
            [
                Device { devid: 1, missing: false, errors: Some(BTreeMap::from([("corruption_errs".into(), 3)])) },
                Device { devid: 2, missing: true, errors: Some(BTreeMap::new()) },
            ]
        );
        let invalid = parse_devices("1 missing 1\n1 invalid\n").unwrap();
        assert_eq!(invalid, [Device { devid: 1, missing: true, errors: None }]);
        assert!(invalid[0].has_trouble());
        assert!(!parse_devices(&format!("1 missing 0\n{}", counters(1, 0))).unwrap()[0].has_trouble());
        assert!(parse_devices("").is_err());
        assert!(parse_devices(&format!("1 missing 0\n{}", counters(1, 0).replace("1 read_errs 0\n", ""))).is_err());
        assert!(parse_devices(&format!("1 missing 2\n{}", counters(1, 0))).is_err());
        assert!(parse_devices(&counters(1, 0)).is_err());
    }

    #[test]
    fn decodes_sysfs_names() {
        assert_eq!(device_path("dm-0"), "/dev/dm-0");
        assert_eq!(device_path("cciss!c0d0p1"), "/dev/cciss/c0d0p1");
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
}
