// Model-output: Claude Opus 5.5

//! `reboop check`: whether machines are okay to reboot, and the facts behind
//! the answer.

use crate::btrfs::{Device, ScrubState};
use crate::config::{self, Machine};
use crate::facts::Inhibitor;
use crate::human::{self, Align::{Center, Left, Right}, Cell, Style::{Bold, Gray, Green, Plain, Red}};
use crate::preflight::{self, Facts};
use crate::processes::Activity;
use crate::ssh::{OPEN_TIMEOUT, Session, Ssh};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::BTreeSet;
use std::thread;

/// The facts about a machine and the reasons not to reboot it (none if it's
/// okay), or why it couldn't be checked.
type Outcome = Result<(Facts, Vec<String>)>;

fn check_machine(ssh: &Ssh, machine: &Machine) -> Outcome {
    let mut session = Session::open(ssh, &machine.target(), OPEN_TIMEOUT)?;
    let facts = preflight::gather(&mut session, &machine.hostname)?;
    let blockers = preflight::blockers(machine, &facts);
    Ok((facts, blockers))
}

/// The activities that have columns of their own.
const ACTIVITY_COLUMNS: [Activity; 3] = [Activity::Nix, Activity::Tmux, Activity::Rsync];

const HEADER: [&str; 11] = ["MACHINE", "OKAY", "SCRUB", "NIX", "TMUX", "RSYNC", "NET", "LOAD", "ROOT", "OTHER", "KERNEL"];

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

/// The table row about `machine`: the facts that decide whether to reboot
/// it, red where they block a reboot, and the kernel it runs (and the one it
/// would boot, if different).
fn table_row(machine: &Machine, outcome: &Outcome) -> Vec<Cell> {
    let Ok((facts, blockers)) = outcome else {
        let mut row = vec![Plain.cell(machine.hostname.clone()), Red.cell("error")];
        row.resize(HEADER.len(), Plain.cell(""));
        return row;
    };
    let list = |items: Vec<&str>| if items.is_empty() { Gray.cell("-") } else { Red.cell(items.join(",")) };
    let scrubbing = facts.btrfs.iter().filter(|fs| fs.scrub.state == ScrubState::Running).map(|fs| fs.filesystem.mountpoint.as_str());
    let count = |activity| match facts.busy_processes.get(&activity) {
        Some(processes) => Red.cell(processes.len().to_string()).aligned(Right),
        None            => Gray.cell("-"),
    };
    let number = |over_limit: bool, text: String| (if over_limit { Red } else { Green }).cell(text).aligned(Right);
    let systems = &facts.systems;
    let kernel = if systems.default_kernel == systems.running_kernel {
        systems.running_kernel.clone()
    } else {
        format!("{} → {}", systems.running_kernel, systems.default_kernel)
    };
    vec![
        Plain.cell(machine.hostname.clone()),
        if blockers.is_empty() { Green.cell("yes") } else { Red.cell("no") },
        list(scrubbing.collect()),
        count(Activity::Nix),
        count(Activity::Tmux),
        count(Activity::Rsync),
        number(preflight::network_over_limit(machine, facts), human::column_rate(facts.network_bytes_per_sec)),
        number(preflight::load_over_limit(machine, facts), format!("{:.2}", facts.load_average_1min)),
        number(preflight::root_full(machine, facts), format!("{}%", facts.root_used_percent)),
        list(other_reasons(facts).iter().map(String::as_str).collect()),
        Plain.cell(kernel),
    ]
}

/// A table with a row per machine, followed by a line per reason not to
/// reboot a machine and per machine that couldn't be checked (with any
/// further lines of the error indented), a blank line before each machine's.
/// The table is styled if `color`.
fn table(outcomes: &[(&Machine, Outcome)], color: bool) -> String {
    let mut rows = vec![Vec::from(HEADER.map(|title| Bold.cell(title).aligned(if CENTERED.contains(&title) { Center } else { Left })))];
    rows.extend(outcomes.iter().map(|(machine, outcome)| table_row(machine, outcome)));
    let mut text = human::table(&rows, color);

    for (machine, outcome) in outcomes {
        let details: Vec<String> = match outcome {
            Ok((_, blockers)) => blockers.iter().map(|blocker| format!("{}: {blocker}", machine.hostname)).collect(),
            Err(error) => vec![format!("{}: {}", machine.hostname, format!("{error:#}").replace('\n', "\n    "))],
        };
        if !details.is_empty() {
            text.push('\n');
            for line in details {
                text.push_str(&line);
                text.push('\n');
            }
        }
    }
    text
}

/// The JSON about a machine: its facts and the reasons not to reboot it
/// (`blockers`, empty if it's okay), or why it couldn't be checked.
#[derive(Serialize)]
#[serde(untagged)]
enum JsonReport<'a> {
    Checked { machine: &'a str, okay_to_reboot: bool, blockers: &'a [String], facts: &'a Facts },
    /// `okay_to_reboot` is always false.
    Failed { machine: &'a str, okay_to_reboot: bool, error: String },
}

fn json_report<'a>(machine: &'a Machine, outcome: &'a Outcome) -> JsonReport<'a> {
    let machine = &machine.hostname;
    match outcome {
        Ok((facts, blockers)) => JsonReport::Checked { machine, okay_to_reboot: blockers.is_empty(), blockers, facts },
        Err(error) => JsonReport::Failed { machine, okay_to_reboot: false, error: format!("{error:#}") },
    }
}

/// 0 if every machine is okay to reboot, 2 if some aren't, or 1 if some
/// couldn't be checked.
fn exit_status(outcomes: &[(&Machine, Outcome)]) -> u8 {
    if outcomes.iter().any(|(_, outcome)| outcome.is_err()) {
        1
    } else if outcomes.iter().any(|(_, outcome)| matches!(outcome, Ok((_, blockers)) if !blockers.is_empty())) {
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
/// `color`), or JSON if `json`.  Returns the exit status: see
/// [`exit_status`].
pub fn run(hostnames: &[String], json: bool, color: bool) -> Result<u8> {
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
        print!("{}", table(&outcomes, color));
    }
    Ok(exit_status(&outcomes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::Job;
    use crate::preflight::{idle_facts, test_machine};
    use crate::processes::Process;
    use anyhow::anyhow;
    use serde_json::{Value, json};

    /// The outcome of checking [`test_machine`] while it has something wrong
    /// for each column and each of the other reasons.
    fn blocked_outcome() -> Outcome {
        let mut facts = idle_facts();
        facts.systems.default_kernel = "6.18.55".into();
        facts.btrfs[0].exclusive_operation = "balance".into();
        facts.btrfs[0].devices[0].missing = true;
        let tmux = Process { pid: 1234, ppid: 1, user: "at".into(), args: "tmux new -s work".into() };
        facts.busy_processes.insert(Activity::Tmux, vec![tmux]);
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
        Ok((facts, blockers))
    }

    #[test]
    fn tables() {
        let machines = [test_machine(), Machine { hostname: "two".into(), ..test_machine() }, Machine { hostname: "three".into(), ..test_machine() }];
        let outcomes = [
            (&machines[0], blocked_outcome()),
            (&machines[1], Ok((idle_facts(), vec![]))),
            (&machines[2], Err(anyhow!("no route to host\nsecond line").context("failed to open a session"))),
        ];
        assert_eq!(
            table(&outcomes, false),
            "MACHINE  OKAY   SCRUB  NIX  TMUX  RSYNC     NET      LOAD   ROOT                                 OTHER                                 KERNEL\n\
             one      no     -      -       1  -      12.50 MB/s  12.50   98%  btrfs balance,btrfs device trouble,switch,cryptsetup,inhibitor,jobs  6.18.54 → 6.18.55\n\
             two      yes    -      -    -     -       1.00 kB/s   0.50   45%  -                                                                    6.18.54\n\
             three    error\n\
             \n\
             one: btrfs on /: balance\n\
             one: btrfs on /: device 1 is missing\n\
             one: switch-to-configuration: pid 1236 (root): /run/current-system/bin/switch-to-configuration boot\n\
             one: tmux: pid 1234 (at): tmux new -s work\n\
             one: cryptsetup: pid 1235 (root): cryptsetup reencrypt /dev/sda2\n\
             one: inhibitor: crawl (archiving), pid 42 (at)\n\
             one: systemd job: start nixos-upgrade.service (running) for 5s or more\n\
             one: network: 12.50 MB/s is over the limit of 1.00 MB/s\n\
             one: load average: 12.50 is over the limit of 2\n\
             one: root filesystem: 98% used, and it's full at 97%\n\
             \n\
             three: failed to open a session: no route to host\n    second line\n"
        );
        let styles = |(machine, outcome): &(&Machine, Outcome)| table_row(machine, outcome).into_iter().map(|cell| cell.style).collect::<Vec<_>>();
        assert_eq!(styles(&outcomes[0]), [Plain, Red, Gray, Gray, Red, Gray, Red, Red, Red, Red, Plain]);
        assert_eq!(styles(&outcomes[1]), [Plain, Green, Gray, Gray, Gray, Gray, Green, Green, Green, Gray, Plain]);
        assert_eq!(styles(&outcomes[2])[..2], [Plain, Red]);
        assert_eq!(exit_status(&outcomes), 1);
        assert!(CENTERED.iter().all(|title| HEADER.contains(title)), "a centered header isn't in HEADER");
        assert_eq!(exit_status(&outcomes[..2]), 2);
        assert_eq!(exit_status(&outcomes[1..2]), 0);
    }

    #[test]
    fn names_boot_trouble() {
        let mut facts = idle_facts();
        assert_eq!(other_reasons(&facts), Vec::<String>::new());
        facts.boot[0].missing_files.push("/boot/EFI/nixos/initrd.efi".into());
        assert_eq!(other_reasons(&facts), ["boot"]);
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
        let text = serde_json::to_string(&json_report(&machine, &Ok((idle_facts(), vec![])))).unwrap();
        assert!(text.starts_with(r#"{"machine":"one","okay_to_reboot":true,"blockers":[],"facts":{"boot_id":"#), "{text}");
        assert_eq!(report["okay_to_reboot"], false);
        assert_eq!(report["blockers"][0], "btrfs on /: balance");
        assert_eq!(report["blockers"].as_array().unwrap().len(), 10);
        assert_eq!(report["facts"]["lasting_jobs"][0]["type"], "start");
        assert_eq!(report["facts"]["root_used_percent"], 98);
        assert_eq!(report["facts"]["busy_processes"]["cryptsetup"][0]["pid"], 1235);
        assert_eq!(report["facts"]["btrfs"][0]["devices"][0]["missing"], true);
        assert_eq!(report["facts"]["inhibitors"][0]["who"], "crawl");
        assert_eq!(report["facts"]["busy_processes"]["tmux"][0]["pid"], 1234);
        assert_eq!(report["facts"]["btrfs"][0]["mountpoint"], "/");
        assert_eq!(report["facts"]["btrfs"][0]["scrub"]["state"], "finished");
        assert_eq!(report["facts"]["systems"]["default_kernel"], "6.18.55");
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
