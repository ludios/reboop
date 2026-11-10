//! `reboop check`: whether machines are okay to reboot, why they need one,
//! and the facts behind the answers.

use crate::btrfs::{Device, ScrubState};
use crate::config::{self, Machine, TmuxPolicy};
use crate::facts::{self, Inhibitor, Rebuild};
use crate::human::{self, Align::{Center, Left, Right}, Cell, Style, Style::{Bold, Green, Plain, Red, Yellow}};
use crate::preflight::{self, DiskFacts, Facts};
use crate::processes::Activity;
use crate::resume::{self, Capture, Purpose};
use crate::smart::Smart;
use crate::ssh::{OPEN_TIMEOUT, Session, Ssh};
use crate::tmux::{self, Running};
use anyhow::{Result, ensure};
use jiff::tz::TimeZone;
use serde::Serialize;
use std::collections::BTreeSet;
use std::thread;
use tracing::warn;

/// The facts about a machine, the reasons not to reboot it (none if it's
/// okay), and whether a nixos-rebuild would change it (if its check_rebuild
/// is on), or why it couldn't be checked.
type Outcome = Result<(Facts, Vec<String>, Option<Rebuild>)>;

/// Checks `machine` over `ssh` (see [`Outcome`]), and saves what's in its
/// tmux servers on it, for after a crash, only warning if that fails.
fn check_machine(ssh: &Ssh, machine: &Machine) -> Outcome {
    let mut session = Session::open(ssh, &machine.target(), OPEN_TIMEOUT)?;
    let facts = preflight::gather(&mut session, &machine.hostname)?;
    let blockers = preflight::blockers(machine, &facts);
    // A record of what was in tmux, for after a crash (see resume::DIR),
    // which isn't worth losing the facts over if it can't be written
    if let Err(error) = resume::save(&mut session, &Capture::new(Purpose::Check, &facts.boot_id, facts.tmux.clone()), false) {
        warn!("couldn't save what's in tmux on {}: {error:#}", machine.hostname);
    }
    // After the facts, which the evaluation would skew, e.g. the load
    let rebuild = machine.check_rebuild.then(|| facts::rebuild(&mut session, &facts.systems.default));
    Ok((facts, blockers, rebuild))
}

/// The activities that have columns of their own.
const ACTIVITY_COLUMNS: [Activity; 2] = [Activity::Nix, Activity::Copy];

const HEADER: [&str; 16] = ["MACHINE", "OKAY", "SCRUB", "NIX", "TMUX", "COPY", "NET", "LOAD", "ROOT", "SMART", "OTHER", "REBUILD", "REBOOT", "KERNEL", "BUILT", "BOOTED"];

/// The headers centered over their columns; the rest are flush left.
const CENTERED: [&str; 2] = ["NET", "OTHER"];

/// Short names for the reasons in `facts` not to reboot that have no column
/// of their own: boot, then btrfs, processes, inhibitors and jobs, as in
/// [`preflight::blockers`].
fn other_reasons(facts: &Facts) -> Vec<String> {
    let mut reasons = Vec::new();
    if !preflight::boot_problems(facts).is_empty() {
        reasons.push("boot".to_string());
    }
    let operations: BTreeSet<_> = facts.btrfs.iter().map(|fs| fs.exclusive_operation.as_str()).filter(|&op| op != "none").collect();
    reasons.extend(operations.into_iter().map(|op| format!("btrfs {op}")));
    if facts.btrfs.iter().flat_map(|fs| &fs.devices).any(Device::has_trouble) {
        reasons.push("btrfs device trouble".to_string());
    }
    for activity in facts.busy_processes.keys().filter(|activity| !ACTIVITY_COLUMNS.contains(activity)) {
        reasons.push(match activity {
            Activity::SwitchToConfiguration => "switch".to_string(),
            _ => activity.to_string(),
        });
    }
    if facts.inhibitors.iter().any(Inhibitor::blocks_shutdown) {
        reasons.push("inhibitor".to_string());
    }
    if !facts.lasting_jobs.is_empty() {
        reasons.push("jobs".to_string());
    }
    reasons
}

/// The SMART cell: the disks in trouble (without /dev/), "ok" if none is
/// and every disk's SMART could be read in full, "?" if some disk's
/// couldn't, or nothing if no disk was checked (no smartctl, or no disk
/// beneath a btrfs filesystem).
fn smart_cell(facts: &Facts) -> Cell {
    let Some(disks) = facts.disks.as_deref().filter(|disks| !disks.is_empty()) else { return Plain.cell("") };
    let name = |disk: &DiskFacts| disk.disk.path.strip_prefix("/dev/").unwrap_or(&disk.disk.path).to_string();
    let troubled: Vec<String> = disks.iter().filter(|disk| disk.smart.trouble().is_some()).map(name).collect();
    if !troubled.is_empty() {
        Red.cell(troubled.join(","))
    } else if disks.iter().any(|disk| matches!(disk.smart, Smart::Read { incomplete: Some(_), .. })) {
        Plain.cell("?")
    } else {
        Green.cell("ok")
    }
}

/// The TMUX cell: nothing if the machine has no tmux server; with `tmux`
/// block, how many servers it has, in red, as they block a reboot; with
/// resume, "?" in red if a server couldn't be listed, which blocks a
/// reboot, else how many panes are running something, in red if any of
/// that blocks a reboot, else in green.
fn tmux_cell(machine: &Machine, facts: &Facts) -> Cell {
    if facts.tmux.is_empty() {
        return Plain.cell("");
    }
    let cell = match machine.tmux {
        TmuxPolicy::Block => Red.cell(facts.tmux.len().to_string()),
        TmuxPolicy::Resume if facts.tmux.iter().any(|server| server.error.is_some()) => Red.cell("?"),
        TmuxPolicy::Resume => {
            let running = facts.tmux.iter().flat_map(|server| server.panes()).filter(|(_, pane)| pane.running != Running::Idle).count();
            let blocks = !tmux::blockers(machine.tmux, &machine.resumable_commands, &facts.tmux).is_empty();
            (if blocks { Red } else { Green }).cell(running.to_string())
        }
    };
    cell.aligned(Right)
}

/// The table row about `machine`: the facts that decide whether to reboot
/// it, red where they block a reboot, whether a nixos-rebuild would change
/// it ("ok" if not, "stale" if so, "?" if that couldn't be evaluated, or
/// nothing if it wasn't checked), why it needs a reboot (if it does),
/// the kernel it runs (and the one it would boot, if different), and when
/// that kernel was built and booted, in `zone`. If `outcome` says that
/// `machine` couldn't be checked, the row has only the hostname and
/// "error", in yellow.
fn table_row(machine: &Machine, outcome: &Outcome, zone: &TimeZone) -> Vec<Cell> {
    let Ok((facts, blockers, rebuild)) = outcome else {
        let mut row = vec![Plain.cell(machine.hostname.clone()), Yellow.cell("error")];
        row.resize(HEADER.len(), Plain.cell(""));
        return row;
    };
    let list = |style: Style, items: Vec<&str>| if items.is_empty() { Plain.cell("") } else { style.cell(items.join(",")) };
    let scrubbing = facts.btrfs.iter().filter(|fs| fs.scrub.state == ScrubState::Running).map(|fs| fs.filesystem.mountpoint.as_str());
    let count = |activity| facts.busy_processes.get(&activity).map_or(Plain.cell(""), |processes| Red.cell(processes.len().to_string())).aligned(Right);
    let number = |over_limit: bool, text: String| (if over_limit { Red } else { Green }).cell(text).aligned(Right);
    let systems = &facts.systems;
    let reboot_reasons = systems.reboot_reasons();
    let kernel = if systems.default_kernel == systems.running_kernel {
        systems.running_kernel.clone()
    } else {
        format!("{} → {}", systems.running_kernel, systems.default_kernel)
    };
    vec![
        Plain.cell(machine.hostname.clone()),
        if blockers.is_empty() { Green.cell("yes") } else { Red.cell("no") },
        list(Red, scrubbing.collect()),
        count(Activity::Nix),
        tmux_cell(machine, facts),
        count(Activity::Copy),
        number(preflight::network_over_limit(machine, facts), human::rate(facts.network_bytes_per_sec)),
        number(preflight::load_over_limit(machine, facts), format!("{:.2}", facts.load_average_1min)),
        number(preflight::root_full(machine, facts), format!("{}%", facts.root_used_percent)),
        smart_cell(facts),
        list(Red, other_reasons(facts).iter().map(String::as_str).collect()),
        match rebuild {
            Some(Rebuild::Fresh { .. }) => Green.cell("ok"),
            Some(Rebuild::Stale { .. }) => Plain.cell("stale"),
            Some(Rebuild::Failed { .. }) => Plain.cell("?"),
            None => Plain.cell(""),
        },
        list(Plain, reboot_reasons.iter().map(String::as_str).collect()),
        Plain.cell(kernel),
        systems.running_kernel_built_at.map_or(Plain.cell("?"), |time| human::minute(time, zone)),
        human::minute(facts.booted_at, zone),
    ]
}

/// A table with a row per machine, styled if `color`, with times in `zone`.
fn table(outcomes: &[(&Machine, Outcome)], color: bool, zone: &TimeZone) -> String {
    let mut rows = vec![Vec::from(HEADER.map(|title| Bold.cell(title).aligned(if CENTERED.contains(&title) { Center } else { Left })))];
    rows.extend(outcomes.iter().map(|(machine, outcome)| table_row(machine, outcome, zone)));
    human::table(&rows, color).join("\n") + "\n"
}

/// The messages to go below the table about each machine: the error, if
/// it couldn't be checked; otherwise each reason not to reboot it, and
/// what a nixos-rebuild would change or why its configuration couldn't be
/// evaluated.  The first line of each message starts with the
/// machine's hostname, and the message's further lines are indented.  A
/// blank line comes before each machine's messages.
fn details(outcomes: &[(&Machine, Outcome)]) -> String {
    let mut text = String::new();
    for (machine, outcome) in outcomes {
        let lines: Vec<String> = match outcome {
            Ok((facts, blockers, rebuild)) => blockers.iter().cloned().chain(rebuild.as_ref().and_then(|rebuild| rebuild.trouble(&facts.systems.default))).collect(),
            Err(error) => vec![format!("{error:#}")],
        };
        if !lines.is_empty() {
            text.push('\n');
            for line in lines {
                text.push_str(&format!("{}: {}\n", machine.hostname, line.replace('\n', "\n    ")));
            }
        }
    }
    text
}

/// The JSON about a machine: its facts, the reasons not to reboot it
/// (`blockers`, empty if it's okay), why it needs a reboot
/// (`reboot_reasons`, empty if it doesn't) and whether a nixos-rebuild would
/// change it (`rebuild`, null if not checked), or why it couldn't be checked.
#[derive(Serialize)]
#[serde(untagged)]
enum JsonReport<'a> {
    Checked {
        machine: &'a str,
        okay_to_reboot: bool,
        blockers: &'a [String],
        needs_reboot: bool,
        reboot_reasons: Vec<String>,
        rebuild: Option<&'a Rebuild>,
        facts: &'a Facts,
    },
    /// `okay_to_reboot` is always false.
    Failed { machine: &'a str, okay_to_reboot: bool, error: String },
}

fn json_report<'a>(machine: &'a Machine, outcome: &'a Outcome) -> JsonReport<'a> {
    let machine = &machine.hostname;
    match outcome {
        Ok((facts, blockers, rebuild)) => {
            let reboot_reasons = facts.systems.reboot_reasons();
            let needs_reboot = !reboot_reasons.is_empty();
            JsonReport::Checked { machine, okay_to_reboot: blockers.is_empty(), blockers, needs_reboot, reboot_reasons, rebuild: rebuild.as_ref(), facts }
        }
        Err(error) => JsonReport::Failed { machine, okay_to_reboot: false, error: format!("{error:#}") },
    }
}

/// 0 if every machine is okay to reboot, 2 if some aren't, or 1 if some
/// couldn't be checked.
fn exit_status(outcomes: &[(&Machine, Outcome)]) -> u8 {
    if outcomes.iter().any(|(_, outcome)| outcome.is_err()) {
        1
    } else if outcomes.iter().any(|(_, outcome)| matches!(outcome, Ok((_, blockers, _)) if !blockers.is_empty())) {
        2
    } else {
        0
    }
}

/// The machines called `hostnames` (each once, in order of first mention),
/// or all of `machines` if none are.
fn select<'a>(machines: &'a [Machine], hostnames: &[String]) -> Result<Vec<&'a Machine>> {
    if hostnames.is_empty() {
        ensure!(!machines.is_empty(), "no machines are configured");
        return Ok(machines.iter().collect());
    }
    let mut selected: Vec<&Machine> = Vec::new();
    for hostname in hostnames {
        let machine = config::find(machines, hostname)?;
        if !selected.iter().any(|&chosen| std::ptr::eq(chosen, machine)) {
            selected.push(machine);
        }
    }
    Ok(selected)
}

/// Checks the configured machines called `hostnames` (all of them if
/// empty) at the same time, and prints a table about them (styled if
/// `color`, with times in the local time zone), or JSON if `json`.  Below
/// the table, if `verbose` or if `hostnames` isn't empty, prints the
/// [`details`].  Returns the exit status: see [`exit_status`].
pub fn run(hostnames: &[String], json: bool, verbose: bool, color: bool) -> Result<u8> {
    let machines = config::load(&config::config_dir()?)?;
    let machines = select(&machines, hostnames)?;
    let ssh = &Ssh::default();
    let outcomes: Vec<(&Machine, Outcome)> = thread::scope(|scope| {
        let threads: Vec<_> = machines.iter().map(|&machine| (machine, scope.spawn(move || check_machine(ssh, machine)))).collect();
        threads
            .into_iter()
            .map(|(machine, thread)| (machine, thread.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))))
            .collect()
    });
    if json {
        let reports: Vec<_> = outcomes.iter().map(|(machine, outcome)| json_report(machine, outcome)).collect();
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        print!("{}", table(&outcomes, color, &TimeZone::system()));
        if verbose || !hostnames.is_empty() {
            print!("{}", details(&outcomes));
        }
    }
    Ok(exit_status(&outcomes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::Job;
    use crate::preflight::{idle_facts, test_machine};
    use crate::processes::Process;
    use crate::smart::Disk;
    use anyhow::anyhow;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    /// The outcome of checking [`test_machine`] while it has something wrong
    /// for each column and each of the other reasons, and a nixos-rebuild
    /// would change it.
    fn blocked_outcome() -> Outcome {
        let mut facts = idle_facts();
        facts.systems.default_kernel = "6.18.55".into();
        facts.systems.default_parts.kernel = "/nix/store/bbb-linux-6.18.55".into();
        facts.systems.default_parts.systemd = "/nix/store/bbb-systemd-260.5".into();
        facts.systems.running_kernel_built_at = None;
        facts.btrfs[0].exclusive_operation = "balance".into();
        facts.btrfs[0].devices[0].missing = true;
        facts.disks.as_mut().unwrap()[0].smart = Smart::Read { passed: Some(false), warnings: BTreeMap::from([("Reallocated_Sector_Ct".into(), 5)]), incomplete: None };
        facts.tmux.push(tmux::tests::test_server());
        let switch = Process { pid: 1236, ppid: 1, user: "root".into(), args: "/run/current-system/bin/switch-to-configuration boot".into() };
        facts.busy_processes.insert(Activity::SwitchToConfiguration, vec![switch]);
        let reencrypt = Process { pid: 1235, ppid: 1, user: "root".into(), args: "cryptsetup reencrypt /dev/sda2".into() };
        facts.busy_processes.insert(Activity::Cryptsetup, vec![reencrypt]);
        facts.network_bytes_per_sec = 12_500_000.0;
        facts.load_average_1min = 12.5;
        facts.root_used_percent = 98;
        let crawl = Inhibitor {
            what: "shutdown".into(),
            who: "crawl".into(),
            why: "archiving".into(),
            mode: "block".into(),
            pid: 42,
            uid: 1000,
            user: "at".into(),
        };
        facts.inhibitors.push(crawl);
        facts.lasting_jobs.push(Job { id: 361, unit: "nixos-upgrade.service".into(), job_type: "start".into(), state: "running".into() });
        let blockers = preflight::blockers(&test_machine(), &facts);
        let rebuild = Rebuild::Stale { toplevel: "/nix/store/ccc-nixos-system-one-26.05".into(), nix_path: "nixpkgs=/var/nixpkgs".into() };
        Ok((facts, blockers, Some(rebuild)))
    }

    /// The outcome of checking an idle machine, given whether a nixos-rebuild
    /// would change it.
    fn idle_outcome(rebuild: Option<Rebuild>) -> Outcome {
        Ok((idle_facts(), vec![], rebuild))
    }

    #[test]
    fn tables() {
        let named = |hostname: &str| Machine { hostname: hostname.into(), ..test_machine() };
        let machines = [test_machine(), named("two"), named("three"), named("four"), named("five")];
        let fresh = Rebuild::Fresh { toplevel: idle_facts().systems.default, nix_path: "nixpkgs=/var/nixpkgs".into() };
        let error = "error:\n       … while evaluating the attribute 'config'\n\n       error: attribute 'boot' missing\n       at /etc/nixos/configuration.nix:3:5:";
        let failed = Rebuild::Failed { error: error.into(), nix_path: Some("nixpkgs=/var/nixpkgs".into()) };
        let outcomes = [
            (&machines[0], blocked_outcome()),
            (&machines[1], idle_outcome(Some(fresh))),
            (&machines[2], Err(anyhow!("no route to host\nsecond line").context("failed to open a session"))),
            (&machines[3], idle_outcome(Some(failed))),
            (&machines[4], idle_outcome(None)),
        ];
        assert_eq!(
            table(&outcomes, false, &TimeZone::UTC),
            "MACHINE  OKAY   SCRUB  NIX  TMUX  COPY     NET      LOAD   ROOT  SMART                                 OTHER                                 REBUILD  REBOOT                      KERNEL             BUILT        BOOTED\n\
             one      no                    1        12.50 MB/s  12.50   98%  sda    btrfs balance,btrfs device trouble,switch,cryptsetup,inhibitor,jobs  stale    new kernel,rebuilt systemd  6.18.54 → 6.18.55  ?            09-30T08:27\n\
             two      yes                             1.00 kB/s   0.50   45%  ok                                                                          ok                                   6.18.54            09-25T14:35  09-30T08:27\n\
             three    error\n\
             four     yes                             1.00 kB/s   0.50   45%  ok                                                                          ?                                    6.18.54            09-25T14:35  09-30T08:27\n\
             five     yes                             1.00 kB/s   0.50   45%  ok                                                                                                               6.18.54            09-25T14:35  09-30T08:27\n"
        );
        assert_eq!(
            details(&outcomes),
            "\n\
             one: btrfs on /: balance\n\
             one: btrfs on /: device 1 is missing\n\
             one: smart on /dev/sda (QEMU HARDDISK, serial QM00001): overall health FAILED, Reallocated_Sector_Ct=5\n\
             one: switch-to-configuration: pid 1236 (root): /run/current-system/bin/switch-to-configuration boot\n\
             one: cryptsetup: pid 1235 (root): cryptsetup reencrypt /dev/sda2\n\
             one: tmux: at's server at /tmp/tmux-1000/default has 1 session and 3 panes (tmux is \"block\")\n\
             one: inhibitor: crawl (archiving), pid 42 (at)\n\
             one: systemd job: start nixos-upgrade.service (running) for 5s or more\n\
             one: root filesystem: 98% used, and it's full at 97%\n\
             one: network: 12.50 MB/s is over the limit of 1.00 MB/s\n\
             one: load average: 12.50 is over the limit of 2\n\
             one: nixos-rebuild would change the system: /nix/store/aaa-nixos-system-one-26.05 → /nix/store/ccc-nixos-system-one-26.05\n\
             \n\
             three: failed to open a session: no route to host\n    second line\n\
             \n\
             four: couldn't evaluate the configuration: attribute 'boot' missing\n    at /etc/nixos/configuration.nix:3:5:\n"
        );
        let styles = |(machine, outcome): &(&Machine, Outcome)| table_row(machine, outcome, &TimeZone::UTC).into_iter().map(|cell| cell.spans[0].0).collect::<Vec<_>>();
        assert_eq!(styles(&outcomes[0]), [Plain, Red, Plain, Plain, Red, Plain, Red, Red, Red, Red, Red, Plain, Plain, Plain, Plain, Plain]);
        assert_eq!(styles(&outcomes[1]), [Plain, Green, Plain, Plain, Plain, Plain, Green, Green, Green, Green, Plain, Green, Plain, Plain, Plain, Plain]);
        assert_eq!(styles(&outcomes[2])[..2], [Plain, Yellow]);
        assert_eq!(styles(&outcomes[3])[11], Plain);
        assert_eq!(exit_status(&outcomes), 1);
        assert!(CENTERED.iter().all(|title| HEADER.contains(title)), "a centered header isn't in HEADER");
        assert_eq!(exit_status(&outcomes[..2]), 2);
        assert_eq!(exit_status(&outcomes[1..2]), 0);
    }

    #[test]
    fn counts_tmux_by_policy() {
        let cell = |machine: &Machine, facts: &Facts| {
            let cell = tmux_cell(machine, facts);
            (cell.spans[0].0, cell.spans[0].1.clone())
        };
        let mut facts = idle_facts();
        let block = test_machine();
        let resume = Machine { tmux: TmuxPolicy::Resume, resumable_commands: tmux::tests::patterns(&["rsync .*"]), ..test_machine() };
        assert_eq!(cell(&block, &facts), (Plain, "".into()));
        facts.tmux.push(tmux::tests::test_server());
        assert_eq!(cell(&block, &facts), (Red, "1".into()));
        // Two panes run something, and htop can't be resumed
        assert_eq!(cell(&resume, &facts), (Red, "2".into()));
        let lenient = Machine { resumable_commands: tmux::tests::patterns(&["rsync .*", "htop"]), ..resume.clone() };
        assert_eq!(cell(&lenient, &facts), (Green, "2".into()));
        facts.tmux[0].sessions.clear();
        assert_eq!(cell(&lenient, &facts), (Green, "0".into()));
        facts.tmux[0].error = Some("protocol version mismatch".into());
        assert_eq!(cell(&lenient, &facts), (Red, "?".into()));
        assert_eq!(cell(&block, &facts), (Red, "1".into()));
    }

    #[test]
    fn names_boot_trouble() {
        let mut facts = idle_facts();
        assert_eq!(other_reasons(&facts), Vec::<String>::new());
        facts.boot[0].missing_files.push("/boot/EFI/nixos/initrd.efi".into());
        assert_eq!(other_reasons(&facts), ["boot"]);
    }

    #[test]
    fn shows_smart_in_one_cell() {
        let cell = |facts: &Facts| {
            let cell = smart_cell(facts);
            (cell.spans[0].0, cell.spans[0].1.clone())
        };
        let disk = |path: &str, smart: Smart| DiskFacts { disk: Disk { path: path.into(), model: None, serial: None }, smart };
        let mut facts = idle_facts();
        assert_eq!(cell(&facts), (Green, "ok".into()));
        let disks = facts.disks.as_mut().unwrap();
        disks.push(disk("/dev/vda", Smart::Read { passed: None, warnings: BTreeMap::new(), incomplete: Some("no".into()) }));
        assert_eq!(cell(&facts), (Plain, "?".into()));
        let disks = facts.disks.as_mut().unwrap();
        disks[0].smart = Smart::Read { passed: Some(false), warnings: BTreeMap::new(), incomplete: None };
        disks.push(disk("/dev/sdb", Smart::Read { passed: Some(true), warnings: BTreeMap::from([("media_errors".into(), 1)]), incomplete: None }));
        disks.push(disk("/dev/sdc", Smart::NoAnswer { no_answer_within_secs: 10 }));
        assert_eq!(cell(&facts), (Red, "sda,sdb,sdc".into()));
        facts.disks = Some(vec![]);
        assert_eq!(cell(&facts), (Plain, "".into()));
        facts.disks = None;
        assert_eq!(cell(&facts), (Plain, "".into()));
    }

    #[test]
    fn names_each_btrfs_operation_once() {
        let mut facts = idle_facts();
        facts.btrfs[0].exclusive_operation = "device replace".into();
        facts.btrfs[0].devices[0].missing = true;
        facts.btrfs.push(facts.btrfs[0].clone());
        assert_eq!(other_reasons(&facts), ["btrfs device replace", "btrfs device trouble"]);
    }

    #[test]
    fn json_reports() {
        let machine = test_machine();
        let json = |outcome: &Outcome| -> Value { serde_json::to_value(json_report(&machine, outcome)).unwrap() };
        let report = json(&blocked_outcome());
        let text = serde_json::to_string(&json_report(&machine, &idle_outcome(None))).unwrap();
        assert!(text.starts_with(r#"{"machine":"one","okay_to_reboot":true,"blockers":[],"needs_reboot":false,"reboot_reasons":[],"rebuild":null,"facts":{"boot_id":"#), "{text}");
        assert_eq!(report["okay_to_reboot"], false);
        assert_eq!(report["needs_reboot"], true);
        assert_eq!(report["reboot_reasons"], json!(["new kernel", "rebuilt systemd"]));
        assert_eq!(report["rebuild"], json!({"state": "stale", "toplevel": "/nix/store/ccc-nixos-system-one-26.05", "nix_path": "nixpkgs=/var/nixpkgs"}));
        assert_eq!(report["facts"]["systems"]["default_parts"]["kernel"], "/nix/store/bbb-linux-6.18.55");
        assert_eq!(report["blockers"][0], "btrfs on /: balance");
        assert_eq!(report["blockers"].as_array().unwrap().len(), 11);
        assert_eq!(report["facts"]["disks"][0]["smart"], json!({"passed": false, "warnings": {"Reallocated_Sector_Ct": 5}, "incomplete": null}));
        assert_eq!(report["facts"]["disks"][0]["serial"], "QM00001");
        assert_eq!(report["facts"]["lasting_jobs"][0]["type"], "start");
        assert_eq!(report["facts"]["root_used_percent"], 98);
        assert_eq!(report["facts"]["busy_processes"]["cryptsetup"][0]["pid"], 1235);
        assert_eq!(report["facts"]["btrfs"][0]["devices"][0]["missing"], true);
        assert_eq!(report["facts"]["inhibitors"][0]["who"], "crawl");
        assert_eq!(report["facts"]["tmux"][0]["pid"], 1234);
        assert_eq!(report["facts"]["tmux"][0]["sessions"][0]["windows"][0]["panes"][1]["running"], json!({"state": "typed", "command": "rsync -a /data/ /backup/"}));
        assert_eq!(report["facts"]["btrfs"][0]["mountpoint"], "/");
        assert_eq!(report["facts"]["btrfs"][0]["scrub"]["state"], "finished");
        assert_eq!(report["facts"]["systems"]["default_kernel"], "6.18.55");
        assert_eq!(report["facts"]["systems"]["running_kernel_built_at"], Value::Null);
        assert_eq!(report["facts"]["booted_at"], "2026-09-30T08:27:56Z");
        let report = json(&Err(anyhow!("no route to host")));
        assert_eq!(report, json!({"machine": "one", "okay_to_reboot": false, "error": "no route to host"}));
    }

    #[test]
    fn selects_machines() {
        let machines = [test_machine(), Machine { hostname: "two".into(), ..test_machine() }];
        let hostnames = |selected: Vec<&Machine>| selected.iter().map(|machine| machine.hostname.clone()).collect::<Vec<_>>();
        assert_eq!(hostnames(select(&machines, &[]).unwrap()), ["one", "two"]);
        assert_eq!(hostnames(select(&machines, &["two".into(), "one".into(), "two".into()]).unwrap()), ["two", "one"]);
        assert!(select(&machines, &["three".into()]).is_err());
        assert!(select(&[], &[]).is_err());
    }
}
