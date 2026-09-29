// Model-output: Claude Opus 5.5

//! Finding out whether a machine is okay to reboot.

use crate::boot::{self, DefaultBoot};
use crate::btrfs::{self, Device, Filesystem, ScrubState, ScrubStatus};
use crate::config::Machine;
use crate::facts::{self, Inhibitor, Job, Systems};
use crate::human;
use crate::processes::{self, Activity, Process};
use crate::ssh::Session;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

/// How long [`gather`] watches network traffic.
pub const NETWORK_SAMPLE: Duration = Duration::from_secs(5);

/// How many of the processes doing each activity [`blockers`] names at most.
const PROCESSES_NAMED: usize = 3;

/// How much of a process's command line [`blockers`] shows.
const ARGS_SHOWN: usize = 100;

/// A mounted btrfs filesystem and what it's busy with.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BtrfsFacts {
    #[serde(flatten)]
    pub filesystem: Filesystem,
    /// As from [`btrfs::exclusive_operation`]: "none" if there's none.
    pub exclusive_operation: String,
    pub scrub: ScrubStatus,
    pub devices: Vec<Device>,
}

/// What a machine is doing that a reboot would interrupt, and what a reboot
/// would change.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Facts {
    pub boot_id: String,
    pub systems: Systems,
    /// What the boot loader will boot next, as from [`boot::default_boots`]
    pub boot: Vec<DefaultBoot>,
    /// As from [`boot::firmware_overrides`]
    pub firmware_overrides: Vec<String>,
    pub load_average_1min: f64,
    /// Bytes received plus sent per second over [`NETWORK_SAMPLE`].
    pub network_bytes_per_sec: f64,
    /// As from [`facts::root_used_percent`]
    pub root_used_percent: u8,
    /// Processes that a reboot would interrupt, by what they're doing.
    pub busy_processes: BTreeMap<Activity, Vec<Process>>,
    /// All inhibitor locks, including those that don't block a reboot.
    pub inhibitors: Vec<Inhibitor>,
    /// systemd's jobs that were there both before and after
    /// [`NETWORK_SAMPLE`], like a unit that's slow to start or stop.
    /// (Leaving out the rest, like those that logging in starts.)
    pub lasting_jobs: Vec<Job>,
    /// Every mounted btrfs filesystem, not just those to scrub.
    pub btrfs: Vec<BtrfsFacts>,
}

/// Collects the facts about the machine at the other end of `session`, after
/// making sure it calls itself `hostname`, or the first label of `hostname`
/// if that's a fully qualified name (NixOS host names have no dots).  Takes
/// [`NETWORK_SAMPLE`] or a bit longer.
pub fn gather(session: &mut Session, hostname: &str) -> Result<Facts> {
    let actual = facts::hostname(session)?;
    let fqdn_of_actual = hostname.strip_prefix(actual.as_str()).is_some_and(|domain| domain.starts_with('.'));
    ensure!(actual == hostname || fqdn_of_actual, "{} calls itself {actual:?}, not {hostname:?}", session.target());

    // The sample comes first, so that the rest is as fresh as can be.
    let jobs_before = facts::jobs(session)?;
    let network_bytes_per_sec = facts::sample_network(session, NETWORK_SAMPLE)?.bytes_per_sec();
    let mut lasting_jobs = facts::jobs(session)?;
    lasting_jobs.retain(|job| jobs_before.iter().any(|before| before.id == job.id));

    let mut busy_processes: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for process in processes::list(session)? {
        if let Some(activity) = processes::activity(&process) {
            busy_processes.entry(activity).or_default().push(process);
        }
    }
    let mut btrfs = Vec::new();
    for filesystem in btrfs::filesystems(session)? {
        let exclusive_operation = btrfs::exclusive_operation(session, &filesystem)?;
        let scrub = btrfs::scrub_status(session, &filesystem.mountpoint)?;
        let devices = btrfs::devices(session, &filesystem)?;
        btrfs.push(BtrfsFacts { filesystem, exclusive_operation, scrub, devices });
    }
    Ok(Facts {
        boot_id: facts::boot_id(session)?,
        systems: facts::systems(session)?,
        boot: boot::default_boots(session)?,
        firmware_overrides: boot::firmware_overrides(session)?,
        load_average_1min: facts::load_average_1min(session)?,
        network_bytes_per_sec,
        root_used_percent: facts::root_used_percent(session)?,
        busy_processes,
        inhibitors: facts::inhibitors(session)?,
        lasting_jobs,
        btrfs,
    })
}

/// Whether `facts` show more network traffic than `machine` allows for a
/// reboot.
pub fn network_over_limit(machine: &Machine, facts: &Facts) -> bool {
    facts.network_bytes_per_sec > machine.max_network_transfer_bytes_per_sec as f64
}

/// Whether `facts` show a higher load average than `machine` allows for a
/// reboot.
pub fn load_over_limit(machine: &Machine, facts: &Facts) -> bool {
    facts.load_average_1min > machine.max_load_average_1min
}

/// Why what the machine's boot loader will boot next (in `facts`) isn't what
/// a reboot should boot: the system profile, from files that are there,
/// without the firmware doing something else.  One line each for people;
/// empty if all is well.
pub fn boot_problems(facts: &Facts) -> Vec<String> {
    let mut problems: Vec<_> = facts.firmware_overrides.iter().map(|what| format!("boot: {what}")).collect();
    if facts.boot.is_empty() {
        problems.push("boot: found neither systemd-boot's default entry nor a GRUB menu, so it's unclear what the machine will boot".into());
    }
    for boot in &facts.boot {
        if boot.is_missing() {
            problems.push(format!("boot: {} is missing", boot.loader));
            continue;
        }
        let default = format!("boot: {}'s default, {:?},", boot.loader, boot.entry);
        match boot.system() {
            None => problems.push(format!("{default} doesn't boot a NixOS system (no init=/nix/store/…/init)")),
            Some(system) if system != facts.systems.default => {
                problems.push(format!("{default} boots {system}, not the system profile's {}", facts.systems.default));
            }
            Some(_) => {}
        }
        for file in &boot.missing_files {
            problems.push(format!("{default} needs {file}, which is missing"));
        }
    }
    problems
}

/// Whether `facts` show that `machine`'s / is too full to reboot: it might
/// not boot properly.
pub fn root_full(machine: &Machine, facts: &Facts) -> bool {
    facts.root_used_percent >= machine.root_full_percent
}

/// Like [`blockers`], but leaving out the limits on load average and network
/// traffic.
pub fn blockers_ignoring_load_and_network(machine: &Machine, facts: &Facts) -> Vec<String> {
    let mut blockers = boot_problems(facts);
    for fs in &facts.btrfs {
        // Linux hangs on shutdown while a scrub is running.
        if fs.scrub.state == ScrubState::Running {
            blockers.push(format!("btrfs on {}: {}", fs.filesystem.mountpoint, fs.scrub.summary()));
        }
        // Includes "balance paused": a paused balance resumes at the next
        // mount (unless mounted with skip_balance).
        if fs.exclusive_operation != "none" {
            blockers.push(format!("btrfs on {}: {}", fs.filesystem.mountpoint, fs.exclusive_operation));
        }
        // A filesystem missing a device won't mount at boot without the
        // degraded option.  One with errors may have a failing device.
        let mountpoint = &fs.filesystem.mountpoint;
        for device in &fs.devices {
            if device.missing {
                blockers.push(format!("btrfs on {mountpoint}: device {} is missing", device.devid));
            }
            if let Some(errors) = device.errors.as_ref().filter(|errors| !errors.is_empty()) {
                let errors = btrfs::format_counters(errors);
                let reset = format!("once dealt with, `btrfs device stats -z {mountpoint}` resets them");
                blockers.push(format!("btrfs on {mountpoint}: device {} has had errors: {errors} ({reset})", device.devid));
            }
        }
    }
    for (activity, processes) in &facts.busy_processes {
        // Only name processes that others in the group didn't start, as
        // those are often copies of their parent (like rsync's).
        let pids: HashSet<u32> = processes.iter().map(|process| process.pid).collect();
        let named: Vec<_> = processes.iter().filter(|process| !pids.contains(&process.ppid)).take(PROCESSES_NAMED).collect();
        for process in &named {
            let args = human::truncate(&process.args, ARGS_SHOWN);
            blockers.push(format!("{activity}: pid {} ({}): {args}", process.pid, process.user));
        }
        match processes.len() - named.len() {
            0 => {}
            1 => blockers.push(format!("{activity}: 1 more process")),
            more => blockers.push(format!("{activity}: {more} more processes")),
        }
    }
    for inhibitor in facts.inhibitors.iter().filter(|inhibitor| inhibitor.blocks_shutdown()) {
        let why = if inhibitor.why.is_empty() { String::new() } else { format!(" ({})", inhibitor.why) };
        blockers.push(format!("inhibitor: {}{why}, pid {} ({})", inhibitor.who, inhibitor.pid, inhibitor.user));
    }
    let seconds = NETWORK_SAMPLE.as_secs();
    for job in &facts.lasting_jobs {
        blockers.push(format!("systemd job: {} {} ({}) for {seconds}s or more", job.job_type, job.unit, job.state));
    }
    if root_full(machine, facts) {
        blockers.push(format!("root filesystem: {}% used, and it's full at {}%", facts.root_used_percent, machine.root_full_percent));
    }
    blockers
}

/// The reasons not to reboot `machine`, given `facts` about it, one line
/// each for people.  Empty if it's okay to reboot.
pub fn blockers(machine: &Machine, facts: &Facts) -> Vec<String> {
    let mut blockers = blockers_ignoring_load_and_network(machine, facts);
    if network_over_limit(machine, facts) {
        let (rate, limit) = (human::rate(facts.network_bytes_per_sec), human::rate(machine.max_network_transfer_bytes_per_sec as f64));
        blockers.push(format!("network: {rate} is over the limit of {limit}"));
    }
    if load_over_limit(machine, facts) {
        blockers.push(format!("load average: {:.2} is over the limit of {}", facts.load_average_1min, machine.max_load_average_1min));
    }
    blockers
}

/// Facts about a machine that's okay to reboot, for tests.
#[cfg(test)]
pub(crate) fn idle_facts() -> Facts {
    let system = "/nix/store/aaa-nixos-system-one-26.05".to_string();
    let scrub = ScrubStatus {
        state: ScrubState::Finished,
        started: Some("Mon Sep 28 10:00:00 2026".into()),
        total_bytes: Some(1_000_000),
        scrubbed_bytes: 1_000_000,
        bytes_per_sec: Some(100_000),
        seconds_left: None,
        errors: BTreeMap::new(),
    };
    let boot = DefaultBoot {
        loader: "systemd-boot".into(),
        entry: "nixos-generation-24.conf".into(),
        options: format!("init={system}/init console=ttyS0 ip=10.0.0.1::10.0.0.254:255.255.255.0:one::none"),
        files: vec!["/boot/EFI/nixos/bzImage.efi".into(), "/boot/EFI/nixos/initrd.efi".into()],
        missing_files: vec![],
    };
    Facts {
        boot_id: "4f6f3fbb-4f0e-4ba0-9a8d-2f53f2c4f59e".into(),
        systems: Systems {
            current: system.clone(),
            booted: system.clone(),
            running_kernel: "6.18.54".into(),
            default: system,
            default_kernel: "6.18.54".into(),
        },
        boot: vec![boot],
        firmware_overrides: vec![],
        load_average_1min: 0.5,
        network_bytes_per_sec: 1_000.0,
        root_used_percent: 45,
        busy_processes: BTreeMap::new(),
        inhibitors: vec![],
        lasting_jobs: vec![],
        btrfs: vec![BtrfsFacts {
            filesystem: Filesystem { uuid: "4c8a".into(), mountpoint: "/".into() },
            exclusive_operation: "none".into(),
            scrub,
            devices: vec![Device { devid: 1, missing: false, errors: Some(BTreeMap::new()) }],
        }],
    }
}

/// A machine with a load average limit of 2, a network limit of 1MB/s, and a
/// root that's full at 97%, for tests.
#[cfg(test)]
pub(crate) fn test_machine() -> Machine {
    Machine {
        hostname: "one".into(),
        ipv4: std::net::Ipv4Addr::new(10, 0, 0, 1),
        ssh_port: 22,
        initrd_ssh_port: 23,
        scrub_mounts: vec!["/".into()],
        max_network_transfer_bytes_per_sec: 1_000_000,
        max_load_average_1min: 2.0,
        root_full_percent: 97,
        luks_signing_key: "/home/user/.ssh/id_ed25519.pub".into(),
        stop_services: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, ppid: u32, user: &str, args: &str) -> Process {
        Process { pid, ppid, user: user.into(), args: args.into() }
    }

    #[test]
    fn idle_machine_is_okay_up_to_the_limits() {
        let mut facts = idle_facts();
        assert_eq!(blockers(&test_machine(), &facts), Vec::<String>::new());
        facts.load_average_1min = 2.0;
        facts.network_bytes_per_sec = 1_000_000.0;
        facts.root_used_percent = 96;
        assert_eq!(blockers(&test_machine(), &facts), Vec::<String>::new());
    }

    #[test]
    fn busy_machine_has_blockers() {
        let mut facts = idle_facts();
        facts.btrfs[0].scrub.state = ScrubState::Running;
        facts.btrfs[0].scrub.seconds_left = Some(210);
        facts.btrfs[0].scrub.scrubbed_bytes = 500_000;
        facts.btrfs.push(BtrfsFacts {
            filesystem: Filesystem { uuid: "bbb".into(), mountpoint: "/small".into() },
            exclusive_operation: "balance paused".into(),
            scrub: idle_facts().btrfs[0].scrub.clone(),
            devices: vec![
                Device { devid: 1, missing: false, errors: Some(BTreeMap::from([("corruption_errs".into(), 3), ("read_errs".into(), 1)])) },
                Device { devid: 2, missing: true, errors: Some(BTreeMap::new()) },
            ],
        });
        facts.busy_processes.insert(Activity::Tmux, vec![process(1234, 1, "at", "tmux new -s work")]);
        let inhibitor = |mode: &str, why: &str| Inhibitor {
            what: "shutdown".into(),
            who: "crawl".into(),
            why: why.into(),
            mode: mode.into(),
            pid: 42,
            uid: 1000,
            user: "at".into(),
        };
        facts.inhibitors = vec![inhibitor("block", "archiving"), inhibitor("delay", "flushing"), inhibitor("block-weak", "")];
        let job = Job { id: 361, unit: "nixos-upgrade.service".into(), job_type: "start".into(), state: "running".into() };
        facts.lasting_jobs = vec![job];
        facts.load_average_1min = 2.01;
        facts.network_bytes_per_sec = 1_500_000.0;
        facts.root_used_percent = 97;
        let all = blockers(&test_machine(), &facts);
        assert_eq!(
            all,
            [
                "btrfs on /: scrub has 3m 30s left, 500.00 kB of 1.00 MB (50.00%) scrubbed at 100.00 kB/s, no errors found",
                "btrfs on /small: balance paused",
                "btrfs on /small: device 1 has had errors: corruption_errs=3 read_errs=1 (once dealt with, `btrfs device stats -z /small` resets them)",
                "btrfs on /small: device 2 is missing",
                "tmux: pid 1234 (at): tmux new -s work",
                "inhibitor: crawl (archiving), pid 42 (at)",
                "inhibitor: crawl, pid 42 (at)",
                "systemd job: start nixos-upgrade.service (running) for 5s or more",
                "root filesystem: 97% used, and it's full at 97%",
                "network: 1.50 MB/s is over the limit of 1.00 MB/s",
                "load average: 2.01 is over the limit of 2",
            ]
        );
        assert_eq!(blockers_ignoring_load_and_network(&test_machine(), &facts), all[..all.len() - 2]);
    }

    #[test]
    fn finds_boot_problems() {
        let mut facts = idle_facts();
        assert_eq!(boot_problems(&facts), Vec::<String>::new());

        // GRUB mirrors: one that's behind, one that lost its initrd, and one
        // that lost its grub.cfg
        let grub = |boot_path: &str, system: &str, missing: &str| {
            let loader = format!("{boot_path}/grub/grub.cfg");
            let files = vec![loader.clone(), format!("{boot_path}/kernels/initrd")];
            DefaultBoot {
                missing_files: files.iter().filter(|file| file.ends_with(missing)).cloned().collect(),
                loader,
                entry: "NixOS".into(),
                options: format!("init={system}/init"),
                files,
            }
        };
        let profile = facts.systems.default.clone();
        facts.boot = vec![
            grub("/boot", "/nix/store/zzz-nixos-system-one-26.05", "-"),
            grub("/boot-fallback", &profile, "/initrd"),
            grub("/boot-spare", &profile, "/grub.cfg"),
        ];
        facts.firmware_overrides = vec!["the firmware will open its setup at the next boot (OsIndications)".into()];
        assert_eq!(
            boot_problems(&facts),
            [
                "boot: the firmware will open its setup at the next boot (OsIndications)",
                "boot: /boot/grub/grub.cfg's default, \"NixOS\", boots /nix/store/zzz-nixos-system-one-26.05, not the system profile's /nix/store/aaa-nixos-system-one-26.05",
                "boot: /boot-fallback/grub/grub.cfg's default, \"NixOS\", needs /boot-fallback/kernels/initrd, which is missing",
                "boot: /boot-spare/grub/grub.cfg is missing",
            ]
        );

        facts.firmware_overrides.clear();
        facts.boot[0].options = "quiet".into();
        assert!(boot_problems(&facts)[0].ends_with("doesn't boot a NixOS system (no init=/nix/store/…/init)"));
        facts.boot.clear();
        assert!(boot_problems(&facts)[0].starts_with("boot: found neither"));
        assert_eq!(blockers(&test_machine(), &facts).len(), 1);
    }

    #[test]
    fn names_a_few_processes_per_activity() {
        let mut facts = idle_facts();
        let builders = (1..=5).map(|n| process(100 + n, 50, &format!("nixbld{n}"), &format!("bash -e builder-{n}.sh {}", "x".repeat(200))));
        facts.busy_processes.insert(Activity::Nix, builders.collect());
        // A local copy: rsync, and its copies of itself
        let rsyncs = [(200, 1), (201, 200)].map(|(pid, ppid)| process(pid, ppid, "root", "rsync -a /a /b"));
        facts.busy_processes.insert(Activity::Rsync, rsyncs.to_vec());
        let blockers = blockers(&test_machine(), &facts);
        assert_eq!(blockers.len(), 6, "{blockers:#?}");
        assert!(blockers[0].starts_with("nix: pid 101 (nixbld1): bash -e builder-1.sh xxx"), "{}", blockers[0]);
        assert!(blockers[0].ends_with("x…"), "{}", blockers[0]);
        assert_eq!(blockers[3], "nix: 2 more processes");
        assert_eq!(blockers[4..], ["rsync: pid 200 (root): rsync -a /a /b", "rsync: 1 more process"]);
    }
}
