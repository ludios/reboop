//! Tests of reboop against NixOS VMs: systemd-boot.nix, with a LUKS-encrypted
//! btrfs root, and grub.nix, which boots from BIOS and has no LUKS.
//!
//! The VMs keep running for three hours after the tests so that later runs
//! start quickly.  To stop them sooner:
//!
//!     pkill -f 'qemu.*target/tmp/reboop-vm-'

mod harness;

use anyhow::{Result, anyhow, bail, ensure};
use harness::{Name, Vm, clean_up};
use jiff::Timestamp;
use libtest_mimic::{Arguments, Failed, Trial};
use reboop::boot;
use reboop::bounce::{self, Outcome, Printer};
use reboop::btrfs::{self, Device, ScrubState};
use reboop::config::{Machine, TmuxPolicy};
use reboop::deadline::{Deadline, Permanent};
use reboop::facts::{self, Rebuild};
use reboop::initrd::{self, UnlockError};
use reboop::preflight;
use reboop::processes::{self, Activity};
use reboop::reboot::{self, Down, Stopped};
use reboop::resume::{self, Capture, Purpose};
use reboop::smart::Smart;
use reboop::ssh::{Session, shell_quote, wait_for_session};
use reboop::tmux::{self, CommandPatterns, Running, Server};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::Ipv4Addr;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

const MINUTE: Duration = Duration::from_secs(60);

type Test = fn(&Vm) -> Result<()>;

/// A session in which everything left over from earlier tests is gone.
fn clean_session(vm: &Vm) -> Result<Session> {
    let mut session = vm.session()?;
    clean_up(&mut session)?;
    Ok(session)
}

fn sh(session: &mut Session, script: &str) -> Result<String> {
    session.run_ok(script, MINUTE)
}

/// Waits up to a minute for `condition` to be true.
fn wait_for(mut condition: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Deadline::after(MINUTE);
    while !condition()? {
        ensure!(!deadline.has_passed(), "timed out waiting for a condition");
        sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn session_runs_commands(vm: &Vm) -> Result<()> {
    let mut session = vm.session()?;

    let output = session.run("printf 'out\\0put'; echo err >&2; exit 3", MINUTE)?;
    assert_eq!((output.status, &output.stdout[..], output.stderr_text().as_str()), (3, &b"out\0put"[..], "err"));

    // Scripts pass through root's login shell (zsh) untouched.
    assert!(sh(&mut session, "getent passwd root")?.trim_end().ends_with("/zsh"));
    let tricky = r#"printf '%s|' '$HOME' "it's" "$(echo 'a b')" ~ \\"#;
    assert_eq!(sh(&mut session, tricky)?, r"$HOME|it's|a b|/root|\|");

    // stdin is empty, so commands can't eat the ones that follow.
    assert_eq!(sh(&mut session, "cat")?, "");
    assert_eq!(sh(&mut session, "echo still here")?, "still here\n");

    // Large output
    let big = sh(&mut session, "seq 1 300000")?;
    assert_eq!(big.lines().count(), 300_000);

    // The tools the primitives use are on the PATH.
    sh(&mut session, "command -v systemctl btrfs findmnt ps dmesg base64 mktemp ss stat stty tmux systemd-run systemd-escape")?;
    Ok(())
}

fn session_breaks_on_timeout(vm: &Vm) -> Result<()> {
    let mut observer = vm.session()?;
    let count_temporary_dirs = "find /tmp -maxdepth 1 -name 'tmp.*' | wc -l";
    let temporary_dirs = sh(&mut observer, count_temporary_dirs)?;

    let mut session = vm.session()?;
    let started = Instant::now();
    assert!(session.run("sleep 3", Duration::from_secs(1)).is_err());
    assert!(started.elapsed() < Duration::from_secs(10));
    // The sleep might still be running, so the session is done for.
    assert!(session.run("true", MINUTE).is_err());

    // Once the sleep ends, the remote shell cleans up after itself.
    sleep(Duration::from_secs(4));
    assert_eq!(sh(&mut observer, count_temporary_dirs)?, temporary_dirs);
    Ok(())
}

fn unreachable_ports_fail_fast(vm: &Vm) -> Result<()> {
    // Nothing listens on the initrd's port once the machine has booted.
    let started = Instant::now();
    assert!(Session::open(&vm.ssh, &vm.initrd_target, MINUTE).is_err());
    let result = initrd::unlock(&vm.ssh, &vm.initrd_target, "password", Deadline::after(MINUTE));
    assert!(matches!(result, Err(UnlockError::Unreachable(_))), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(20));

    let started = Instant::now();
    let deadline = Deadline::after(Duration::from_secs(3));
    assert!(wait_for_session(&vm.ssh, &vm.initrd_target, Duration::from_secs(1), deadline).is_err());
    assert!(started.elapsed() < Duration::from_secs(10));

    // An unknown host key isn't accepted, though the ssh config says
    // accept-new, and won't go away, so there's no retrying.
    let mut ssh = vm.ssh.clone();
    ssh.extra_args.extend(["-o".into(), "UserKnownHostsFile=/dev/null".into()]);
    let started = Instant::now();
    let Err(error) = wait_for_session(&ssh, &vm.target, Duration::from_secs(1), Deadline::after(MINUTE)) else {
        bail!("connected despite the unknown host key");
    };
    assert!(error.downcast_ref::<Permanent>().is_some(), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(10));
    Ok(())
}

fn identity_and_systems(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    assert_eq!(facts::hostname(&mut session)?, vm.manifest.hostname);
    let boot_id = facts::boot_id(&mut session)?;
    assert_eq!(facts::boot_id(&mut session)?, boot_id);

    let systems = facts::systems(&mut session)?;
    let ours = [&vm.manifest.systems.base, &vm.manifest.systems.alt];
    assert!(ours.contains(&&systems.booted), "{systems:?}");
    assert_eq!(systems.current, systems.booted);
    assert!(ours.contains(&&systems.default), "{systems:?}");
    // The kernel release we'd expect after booting the current system is
    // the one that's running.
    assert_eq!(facts::kernel_release(&mut session, &systems.current)?, systems.running_kernel);
    // The parts a reboot would replace are the running kernel's packages,
    // its initrd, and systemd; nothing needs a reboot while the system
    // profile is what booted.
    let parts   = &systems.running_parts;
    let release = &systems.running_kernel;
    assert!(parts.kernel.ends_with(&format!("-linux-{release}")), "{parts:?}");
    assert!(parts.kernel_modules.ends_with(&format!("-linux-{release}-modules")), "{parts:?}");
    assert!(parts.initrd.as_ref().is_some_and(|initrd| initrd.ends_with(&format!("-initrd-linux-{release}"))), "{parts:?}");
    assert!(parts.systemd.contains("-systemd-"), "{parts:?}");
    if systems.booted == systems.default {
        assert_eq!(systems.reboot_reasons(), Vec::<String>::new(), "{systems:?}");
    }
    assert_eq!(systems.running_kernel, sh(&mut session, "uname -r")?.trim_end());
    let built = systems.running_kernel_built_at.expect("NixOS kernels say when they were built in `uname -v`");
    let booted = facts::booted_at(&mut session)?;
    assert!(built < booted && booted < Timestamp::now(), "built at {built}, booted at {booted}");

    let load = facts::load_average_1min(&mut session)?;
    assert!((0.0..100.0).contains(&load), "{load}");
    Ok(())
}

/// The VMs' NIX_PATH, from nix.nixPath in common.nix.
const NIX_PATH: &str = "nixpkgs=/etc/reboop-test-nixpkgs:nixos-config=/etc/nixos/configuration.nix";

/// Makes the configuration of the VM at the other end of `session` evaluate
/// to `toplevel`, by way of the stub nixpkgs in common.nix.
fn set_evaluated_toplevel(session: &mut Session, toplevel: &str) -> Result<()> {
    sh(session, &format!("printf %s {} >/var/tmp/reboop-test-toplevel", shell_quote(toplevel))).map(drop)
}

fn rebuild_is_evaluated(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let profile = facts::systems(&mut session)?.default;
    let systems = &vm.manifest.systems;
    let other = if profile == systems.base { &systems.alt } else { &systems.base };

    // NIX_PATH reaches the session, and the stub has nothing to evaluate to yet.
    let Rebuild::Failed { error, nix_path } = facts::rebuild(&mut session, &profile) else {
        bail!("evaluated without a toplevel to evaluate to");
    };
    assert_eq!(nix_path.as_deref(), Some(NIX_PATH));
    assert_eq!(facts::nix_error_gist(&error).lines().next(), Some("path '/var/tmp/reboop-test-toplevel' does not exist"), "{error}");

    set_evaluated_toplevel(&mut session, &profile)?;
    assert_eq!(facts::rebuild(&mut session, &profile), Rebuild::Fresh { toplevel: profile.clone(), nix_path: NIX_PATH.into() });
    set_evaluated_toplevel(&mut session, other)?;
    assert_eq!(facts::rebuild(&mut session, &profile), Rebuild::Stale { toplevel: other.clone(), nix_path: NIX_PATH.into() });
    clean_up(&mut session)
}

fn network_sample_sees_traffic(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let idle = facts::sample_network(&mut session, Duration::from_secs(2))?;
    assert!(idle.bytes_per_sec() < 100_000.0, "{idle:?}");
    assert!(!idle.interfaces.contains_key("lo"));

    // Push zeros into the VM over another connection while sampling.
    let mut sink = vm.ssh.command(&vm.target, &["-T"], "cat >/dev/null").stdin(Stdio::piped()).stdout(Stdio::null()).spawn()?;
    let mut stdin = sink.stdin.take().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_writer = Arc::clone(&stop);
    let writer = thread::spawn(move || {
        let zeros = vec![0; 1 << 16];
        while !stop_writer.load(Ordering::Relaxed) && stdin.write_all(&zeros).is_ok() {}
    });
    sleep(Duration::from_millis(500));
    let busy = facts::sample_network(&mut session, Duration::from_secs(2));
    // Killing ssh first unblocks the writer, should it be stuck writing.
    stop.store(true, Ordering::Relaxed);
    sink.kill()?;
    sink.wait()?;
    writer.join().unwrap();
    let busy = busy?;
    assert!(busy.bytes_per_sec() > 1_000_000.0, "{busy:?}");
    Ok(())
}

/// Starts a real tmux server, owned by someone other than root, with a
/// session "work" whose window 0 is a zsh pane in /var/tmp.  (Transient
/// units get a minimal PATH, hence the -E PATH.)
const START_TMUX: &str = "systemd-run --quiet --unit=reboop-test-tmux -E PATH -p RemainAfterExit=yes --uid=tester tmux new-session -d -s work -x 100 -y 30 -c /var/tmp";

/// Where tester's tmux server (see [`START_TMUX`]) listens.
const TESTER_SOCKET: &str = "/tmp/tmux-1000/default";

/// Waits for tester's tmux server to be listed, and returns it as listed.
fn tester_tmux(session: &mut Session) -> Result<Server> {
    let deadline = Deadline::after(MINUTE);
    loop {
        if let Some(server) = tmux::capture(session)?.into_iter().find(|server| server.user == "tester") {
            return Ok(server);
        }
        ensure!(!deadline.has_passed(), "tester's tmux server didn't show up");
        sleep(Duration::from_millis(200));
    }
}

/// Types `text` and Enter into pane `target` of tester's tmux, once the
/// pane's shell is at its prompt.
fn type_into(session: &mut Session, target: &str, text: &str) -> Result<()> {
    let ready = format!("stty -F \"$(tmux -S {TESTER_SOCKET} display -p -t {target} -F '#{{pane_tty}}')\" -a | grep -q -- -icanon");
    wait_for(|| Ok(session.run(&ready, MINUTE)?.status == 0))?;
    sh(session, &format!("tmux -S {TESTER_SOCKET} send-keys -t {target} {} Enter", shell_quote(text))).map(drop)
}

/// The pane at `location` (like "work:0.0") in `server`.
fn pane<'a>(server: &'a Server, location: &str) -> Result<&'a tmux::Pane> {
    server.panes().find(|(at, _)| at == location).map(|(_, pane)| pane).ok_or_else(|| anyhow!("no pane {location} in {server:#?}"))
}

/// Makes sure the pane at `location` in `server` is in `cwd` and `running`.
fn expect_pane(server: &Server, location: &str, cwd: &str, running: Running) -> Result<()> {
    let pane = pane(server, location)?;
    ensure!(pane.cwd == cwd && pane.running == running, "{location}: {pane:#?}");
    Ok(())
}

fn activities(session: &mut Session) -> Result<BTreeSet<Activity>> {
    Ok(processes::list(session)?.iter().filter_map(processes::activity).collect())
}

fn activities_are_detected(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    assert_eq!(activities(&mut session)?, BTreeSet::new(), "{:#?}", processes::list(&mut session)?);

    // A real, slow rsync (see START_TMUX about -E PATH)
    sh(&mut session, "head -c 10M /dev/zero >/var/tmp/reboop-test-rsync && \
                      systemd-run --quiet --unit=reboop-test-rsync -E PATH rsync --bwlimit=10 /var/tmp/reboop-test-rsync /var/tmp/reboop-test-rsync-copy")?;
    // A real Nix build that sleeps.  (The expression is in a file because
    // systemd would expand the ${} in a command line.)
    sh(&mut session, r#"coreutils=$(readlink -f "$(command -v sleep)" | cut -d/ -f1-4)
        printf 'derivation { name = "reboop-test-build"; system = builtins.currentSystem; builder = "${builtins.storePath "%s"}/bin/sleep"; args = [ "600" ]; }' \
            "$coreutils" >/var/tmp/reboop-test-build.nix
        systemd-run --quiet --unit=reboop-test-nix -E PATH nix-build --no-out-link /var/tmp/reboop-test-build.nix"#)?;
    // switch-to-configuration as NixOS's wrapper script runs it, with exec -a.
    // (Not with sleep, which is coreutils and goes by its argv[0]; the
    // "; true" keeps bash from exec'ing sleep.)
    sh(&mut session, "systemd-run --quiet --unit=reboop-test-stc -E PATH bash -c \
                      'exec -a /run/current-system/bin/switch-to-configuration bash -c \"sleep 600; true\"'")?;

    // A btrfs receive, a cryptsetup and an fdisk, each waiting for its input
    sh(&mut session, "mkdir /var/tmp/reboop-test-receive && truncate -s 32M /var/tmp/reboop-test-luks.img && \
                      truncate -s 1M /var/tmp/reboop-test-fdisk.img && \
                      systemd-run --quiet --unit=reboop-test-receive -E PATH sh -c 'sleep 600 | btrfs receive /var/tmp/reboop-test-receive' && \
                      systemd-run --quiet --unit=reboop-test-cryptsetup -E PATH \
                          sh -c 'sleep 600 | cryptsetup luksFormat --batch-mode --key-file=- /var/tmp/reboop-test-luks.img' && \
                      systemd-run --quiet --unit=reboop-test-fdisk -E PATH sh -c 'sleep 600 | fdisk /var/tmp/reboop-test-fdisk.img'")?;

    // Wait for them all to start, including the build's builder, which
    // runs as a nixbld user.
    let expected = BTreeSet::from([
        Activity::Nix,
        Activity::SwitchToConfiguration,
        Activity::Copy,
        Activity::BtrfsSendReceive,
        Activity::Cryptsetup,
        Activity::DiskTool,
    ]);
    let deadline = Deadline::after(MINUTE);
    loop {
        let listing: Vec<_> = processes::list(&mut session)?.into_iter().filter(|p| processes::activity(p).is_some()).collect();
        let found: BTreeSet<_> = listing.iter().filter_map(processes::activity).collect();
        let builder = listing.iter().any(|p| p.user.starts_with("nixbld"));
        if found == expected && builder {
            break;
        }
        ensure!(!deadline.has_passed(), "found {found:?} (builder: {builder}) in {listing:#?}");
        sleep(Duration::from_millis(500));
    }

    // The builder goes away a moment after the build is stopped.
    clean_up(&mut session)?;
    wait_for(|| Ok(activities(&mut session)?.is_empty()))?;
    Ok(())
}

/// The VM as a configured machine, with a limit on network traffic but not
/// really on load.
fn vm_machine(vm: &Vm) -> Machine {
    Machine {
        hostname: vm.manifest.hostname.clone(),
        ipv4: Ipv4Addr::LOCALHOST,
        ssh_port: vm.target.port,
        initrd_ssh_port: vm.initrd_target.port,
        scrub_mounts: vec!["/".into()],
        scrub_every_days: 14,
        max_network_transfer_bytes_per_sec: 1_000_000,
        // The VM's load depends on whatever else its host is doing.
        max_load_average_1min: 100.0,
        root_full_percent: 97,
        check_rebuild: true,
        luks_signing_key: "/nonexistent".into(),
        stop_services: vec!["reboop-test-sleep.service".into(), "reboop-test-nonexistent.service".into()],
        tmux: TmuxPolicy::Block,
        resumable_commands: patterns(&[]),
    }
}

fn patterns(patterns: &[&str]) -> CommandPatterns {
    CommandPatterns::new(patterns.iter().map(|&pattern| pattern.into()).collect()).unwrap()
}

/// The VM as a configured machine whose tmux is "resume", with `patterns`
/// as its resumable_commands.
fn resuming_machine(vm: &Vm, patterns: &[&str]) -> Machine {
    Machine { tmux: TmuxPolicy::Resume, resumable_commands: self::patterns(patterns), ..vm_machine(vm) }
}

fn preflight_finds_blockers(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let error = preflight::gather(&mut session, "someone-else").unwrap_err();
    let expected = format!(r#"calls itself "{}", not "someone-else""#, vm.manifest.hostname);
    assert!(format!("{error:#}").contains(&expected), "{error:#}");
    // A fully qualified name for the machine will do.
    facts::ensure_hostname(&mut session, &format!("{}.example.com", vm.manifest.hostname))?;

    let machine = vm_machine(vm);
    let facts = preflight::gather(&mut session, &machine.hostname)?;
    assert_eq!(facts.systems, facts::systems(&mut session)?);
    assert!(facts.btrfs.iter().any(|fs| fs.filesystem.mountpoint == "/"), "{facts:#?}");
    assert_eq!(preflight::blockers(&machine, &facts), Vec::<String>::new(), "{facts:#?}");
    // The VM's root is far from full, but not empty.
    let used = facts.root_used_percent;
    assert!((1..90).contains(&used), "{used}");
    let easily_full = Machine { root_full_percent: used, ..machine.clone() };
    assert_eq!(preflight::blockers(&easily_full, &facts), [format!("root filesystem: {used}% used, and it's full at {used}%")]);

    sh(&mut session, START_TMUX)?;
    tester_tmux(&mut session)?;
    let blockers = preflight::blockers(&machine, &preflight::gather(&mut session, &machine.hostname)?);
    assert_eq!(blockers, [format!("tmux: tester's server at {TESTER_SOCKET} has 1 session and 1 pane (tmux is \"block\")")]);
    clean_up(&mut session)?;

    // Programs holding inhibitor locks, one of which only delays a reboot.
    // (See START_TMUX about -E PATH.)
    for (name, mode) in [("reboop-test-inhibit", "block"), ("reboop-test-delay", "delay")] {
        sh(&mut session, &format!("systemd-run --quiet --unit={name} -E PATH systemd-inhibit --what=shutdown --who={name} --why=testing --mode={mode} sleep 600"))?;
    }
    wait_for(|| {
        let inhibitors = facts::inhibitors(&mut session)?;
        Ok(["reboop-test-inhibit", "reboop-test-delay"].iter().all(|&name| inhibitors.iter().any(|inhibitor| inhibitor.who == name)))
    })?;
    let blockers = preflight::blockers(&machine, &preflight::gather(&mut session, &machine.hostname)?);
    let [blocker] = &blockers[..] else { bail!("{blockers:?}") };
    assert!(blocker.starts_with("inhibitor: reboop-test-inhibit (testing), pid ") && blocker.ends_with(" (root)"), "{blocker}");
    clean_up(&mut session)?;

    // A unit that takes its time to start
    sh(&mut session, "systemd-run --quiet --no-block --unit=reboop-test-job -p Type=oneshot sleep 600")?;
    let blockers = preflight::blockers(&machine, &preflight::gather(&mut session, &machine.hostname)?);
    assert_eq!(blockers, ["systemd job: start reboop-test-job.service (running) for 5s or more"]);
    clean_up(&mut session)?;
    Ok(())
}

fn btrfs_root_is_idle(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let filesystems = btrfs::filesystems(&mut session)?;
    assert_eq!(filesystems.len(), 1, "{filesystems:?}");
    assert_eq!(filesystems[0].mountpoint, "/");
    assert_eq!(btrfs::exclusive_operation(&mut session, &filesystems[0])?, "none");
    let scrub = btrfs::scrub_status(&mut session, "/")?;
    assert_ne!(scrub.state, ScrubState::Running);
    assert_eq!(scrub.uuid, filesystems[0].uuid);
    assert_eq!(btrfs::last_clean_scrub(&mut session, &scrub.uuid)?, None);
    // The root spans every disk.
    let devices = (1..=vm.manifest.disk_images.len() as u64).map(|devid| Device { devid, missing: false, errors: Some(BTreeMap::new()) });
    assert_eq!(btrfs::devices(&mut session, &filesystems[0])?, devices.collect::<Vec<_>>());
    Ok(())
}

fn btrfs_missing_device_is_detected(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    // A two-device RAID1, mounted without its second device
    sh(
        &mut session,
        "set -e
        for i in 1 2; do truncate -s 1G /var/tmp/reboop-test-raid-$i.img; done
        a=$(losetup -f --show /var/tmp/reboop-test-raid-1.img)
        b=$(losetup -f --show /var/tmp/reboop-test-raid-2.img)
        mkfs.btrfs -q -d raid1 -m raid1 $a $b
        # Detaching waits for whoever has it open, like udev probing it.
        udevadm settle
        losetup -d $b
        while losetup $b >/dev/null 2>&1; do sleep 0.1; done
        btrfs device scan --forget
        mkdir -p /mnt/reboop-test-raid
        mount -o degraded $a /mnt/reboop-test-raid",
    )?;
    let filesystem = btrfs::filesystems(&mut session)?.into_iter().find(|fs| fs.mountpoint == "/mnt/reboop-test-raid").unwrap();
    let devices = btrfs::devices(&mut session, &filesystem)?;
    assert_eq!(devices.iter().map(|device| (device.devid, device.missing)).collect::<Vec<_>>(), [(1, false), (2, true)]);

    let machine = vm_machine(vm);
    let blockers = preflight::blockers(&machine, &preflight::gather(&mut session, &machine.hostname)?);
    assert_eq!(blockers, ["btrfs on /mnt/reboop-test-raid: device 2 is missing"]);
    Ok(())
}

fn smart_is_read_from_disks(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let machine = vm_machine(vm);
    // The disks beneath the btrfs filesystems, none of which may block
    let gather_disks = |session: &mut Session| -> Result<Vec<preflight::DiskFacts>> {
        let facts = preflight::gather(session, &machine.hostname)?;
        assert_eq!(preflight::blockers(&machine, &facts), Vec::<String>::new(), "{facts:#?}");
        facts.disks.ok_or_else(|| anyhow!("the VM has no smartctl"))
    };
    // The VM's own disks are virtio, which has no SMART.
    let disks = gather_disks(&mut session)?;
    let virtio: Vec<String> = (0..vm.manifest.disk_images.len()).map(|i| format!("/dev/vd{}", char::from(b'a' + i as u8))).collect();
    assert_eq!(disks.iter().map(|disk| disk.disk.path.clone()).collect::<Vec<_>>(), virtio);
    for disk in &disks {
        assert!(matches!(&disk.smart, Smart::Read { passed: None, incomplete: Some(why), .. } if why.contains("Unable to detect device type")), "{disk:?}");
    }

    // btrfs on the scratch SATA and NVMe disks (see the harness), which
    // qemu gives SMART data
    let listing = sh(&mut session, "lsblk --nodeps --noheadings --raw --output PATH,SERIAL")?;
    let scratch: Vec<&str> = listing.lines().filter_map(|line| line.split_once(' ')).filter(|(_, serial)| serial.starts_with("reboop-test-")).map(|(path, _)| path).collect();
    ensure!(scratch.len() == 2, "expected two scratch disks:\n{listing}");
    for path in &scratch {
        let name = path.strip_prefix("/dev/").unwrap();
        sh(&mut session, &format!("set -e; mkfs.btrfs -q {path}; mkdir -p /mnt/reboop-test-{name}; mount {path} /mnt/reboop-test-{name}"))?;
    }
    let disks = gather_disks(&mut session)?;
    assert_eq!(disks.len(), virtio.len() + 2, "{disks:#?}");
    for path in &scratch {
        let disk = disks.iter().find(|disk| disk.disk.path == *path).unwrap();
        assert_eq!(disk.smart, Smart::Read { passed: Some(true), warnings: BTreeMap::new(), incomplete: None }, "{disk:?}");
        assert!(disk.disk.model.as_deref().is_some_and(|model| model.starts_with("QEMU")), "{disk:?}");
        assert!(disk.disk.serial.as_deref().is_some_and(|serial| serial.starts_with("reboop-test-")), "{disk:?}");
    }
    clean_up(&mut session)?;
    Ok(())
}

/// Makes a btrfs filesystem in a file and mounts it at /mnt/reboop-test-NAME.
/// `setup` runs with the mountpoint as $m, e.g. to add files.  Returns the
/// mountpoint.
fn test_filesystem(session: &mut Session, name: &str, setup: &str) -> Result<String> {
    let mountpoint = format!("/mnt/reboop-test-{name}");
    sh(
        session,
        &format!(
            "set -e
            img=/var/tmp/reboop-test-{name}.img m={mountpoint}
            truncate -s 1G $img
            mkfs.btrfs -q --data single --metadata dup $img
            mkdir -p $m
            mount -o loop $img $m
            {setup}
            sync"
        ),
    )?;
    Ok(mountpoint)
}

fn btrfs_running_scrub_and_balance_are_detected(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let mountpoint = test_filesystem(&mut session, "busy", "for i in 1 2 3 4; do head -c 100M /dev/urandom >$m/$i; done")?;
    let filesystem = btrfs::filesystems(&mut session)?.into_iter().find(|fs| fs.mountpoint == mountpoint).unwrap();

    // Throttle scrubbing so that it's still running when we look.
    sh(&mut session, &format!("echo 1m >/sys/fs/btrfs/{}/devinfo/1/scrub_speed_max", filesystem.uuid))?;
    btrfs::start_scrub(&mut session, &mountpoint)?;
    let status = btrfs::scrub_status(&mut session, &mountpoint)?;
    assert_eq!(status.state, ScrubState::Running);
    assert!(status.summary().starts_with("scrub has "), "{}", status.summary());
    // A scrub isn't an exclusive operation.
    assert_eq!(btrfs::exclusive_operation(&mut session, &filesystem)?, "none");
    assert!(btrfs::start_scrub(&mut session, &mountpoint).is_err());
    sh(&mut session, &format!("btrfs scrub cancel {mountpoint}"))?;
    assert_eq!(btrfs::scrub_status(&mut session, &mountpoint)?.state, ScrubState::Aborted);

    // Pause a balance so it stays in progress.  (`start --bg` returns before
    // the balance has started.)
    sh(&mut session, &format!("btrfs balance start --bg --full-balance {mountpoint}"))?;
    wait_for(|| Ok(btrfs::exclusive_operation(&mut session, &filesystem)? == "balance"))?;
    sh(&mut session, &format!("btrfs balance pause {mountpoint}"))?;
    assert_eq!(btrfs::exclusive_operation(&mut session, &filesystem)?, "balance paused");
    sh(&mut session, &format!("btrfs balance cancel {mountpoint}"))?;
    assert_eq!(btrfs::exclusive_operation(&mut session, &filesystem)?, "none");
    Ok(())
}

fn scrub_of_root_finishes_clean(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    btrfs::start_scrub(&mut session, "/")?;
    let mut summaries = Vec::new();
    let status = btrfs::wait_for_scrub(&mut session, "/", Duration::from_secs(1), Deadline::after(5 * MINUTE), |status| {
        summaries.push(status.summary());
    })?;
    eprintln!("{}", summaries.join("\n"));
    assert_eq!(status.state, ScrubState::Finished);
    assert!(status.errors.is_empty(), "{status:?}");
    assert!(status.scrubbed_bytes > 500_000_000, "{status:?}");
    assert!(status.summary().ends_with("no errors found"), "{}", status.summary());
    // Which is worth remembering
    btrfs::note_scrub(&mut session, &status)?;
    assert!(btrfs::last_clean_scrub(&mut session, &status.uuid)?.is_some_and(|ago| ago < MINUTE));
    Ok(())
}

fn scrub_finds_corruption(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let marker = "REBOOP-TEST-CORRUPT-ME";
    let mountpoint = test_filesystem(&mut session, "corrupt", &format!("yes {marker} | head -c 1M >$m/file"))?;

    // Overwrite a bit of the file's data behind btrfs's back.
    sh(
        &mut session,
        &format!(
            "set -e
            loop=$(losetup -j /var/tmp/reboop-test-corrupt.img -n -O NAME)
            offset=$(grep -m1 -obUa {marker} $loop | head -n1 | cut -d: -f1)
            printf GARBAGE | dd of=$loop bs=1 seek=$offset conv=notrunc status=none
            sync"
        ),
    )?;

    btrfs::start_scrub(&mut session, &mountpoint)?;
    let status = btrfs::wait_for_scrub(&mut session, &mountpoint, Duration::from_millis(500), Deadline::after(MINUTE), |_| {})?;
    assert_eq!(status.state, ScrubState::Finished);
    assert!(status.errors.get("csum_errors").is_some_and(|&n| n > 0), "{status:?}");
    assert!(status.summary().contains("ERRORS FOUND"), "{}", status.summary());

    // Which ends the memory of a clean scrub
    let uuid = &status.uuid;
    sh(&mut session, &format!("mkdir -p {dir} && touch {dir}/{uuid}", dir = btrfs::LAST_CLEAN_SCRUB_DIR))?;
    assert!(btrfs::last_clean_scrub(&mut session, uuid)?.is_some());
    btrfs::note_scrub(&mut session, &status)?;
    assert_eq!(btrfs::last_clean_scrub(&mut session, uuid)?, None);

    // The device remembers the corruption.
    let filesystem = btrfs::filesystems(&mut session)?.into_iter().find(|fs| fs.mountpoint == mountpoint).unwrap();
    assert_eq!(&filesystem.uuid, uuid);
    let devices = btrfs::devices(&mut session, &filesystem)?;
    let corruption = devices[0].errors.as_ref().and_then(|errors| errors.get("corruption_errs"));
    assert!(corruption.is_some_and(|&n| n > 0), "{devices:?}");
    Ok(())
}

fn scrub_status_outlasts_a_locked_status_file(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let mountpoint = test_filesystem(&mut session, "locked", "")?;
    // In the foreground, so that it's over, and won't replace the status
    // file once that's locked.
    sh(&mut session, &format!("btrfs scrub start -B {mountpoint} >/dev/null"))?;
    let filesystem = btrfs::filesystems(&mut session)?.into_iter().find(|fs| fs.mountpoint == mountpoint).unwrap();

    // Lock it from the background for 2s: past the first retry, but not all
    // of them.
    let file = format!("/var/lib/btrfs/scrub.status.{}", filesystem.uuid);
    sh(&mut session, &format!("flock {file} sleep 2 >/dev/null 2>&1 &\nwhile flock -n {file} true; do sleep 0.1; done"))?;
    let locked = session.run(&format!("btrfs scrub status {mountpoint}"), MINUTE)?;
    let stderr = locked.stderr_text();
    assert!(
        locked.status != 0 && stderr.contains("failed to open status file: Resource temporarily unavailable"),
        "status {}: {stderr}",
        locked.status
    );
    assert_eq!(btrfs::scrub_status(&mut session, &mountpoint)?.state, ScrubState::Finished);
    Ok(())
}

fn stop_unit(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    assert_eq!(reboot::stop_unit(&mut session, "reboop-test-nonexistent.service", MINUTE)?, Stopped::NotLoaded);
    sh(&mut session, "systemd-run --quiet --unit=reboop-test-sleep sleep 600")?;
    assert_eq!(reboot::stop_unit(&mut session, "reboop-test-sleep.service", MINUTE)?, Stopped::Cleanly);
    let output = session.run("systemctl is-active reboop-test-sleep.service", MINUTE)?;
    assert_ne!(output.stdout_text().trim(), "active");

    // One that ignores SIGTERM, so systemd has to kill it, once it's
    // ignoring it.  (See START_TMUX about -E PATH.)
    sh(&mut session, "systemd-run --quiet --unit=reboop-test-stubborn -E PATH -p TimeoutStopSec=2 \
                      sh -c 'trap \"\" TERM; touch /var/tmp/reboop-test-stubborn; while :; do sleep 1; done'")?;
    wait_for(|| Ok(session.run("test -e /var/tmp/reboop-test-stubborn", MINUTE)?.status == 0))?;
    assert_eq!(reboot::stop_unit(&mut session, "reboop-test-stubborn.service", MINUTE)?, Stopped::Uncleanly("timeout".into()));
    Ok(())
}

fn postflight_facts(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    assert_eq!(facts::wait_until_booted(&mut session, MINUTE)?, "running");
    assert_eq!(facts::failed_units(&mut session)?, Vec::<String>::new());

    session.run("systemd-run --quiet --unit=reboop-test-fail --wait false", MINUTE)?;
    assert_eq!(facts::failed_units(&mut session)?, vec!["reboop-test-fail.service".to_string()]);
    assert_eq!(facts::wait_until_booted(&mut session, MINUTE)?, "degraded");
    assert_eq!(facts::system_state(&mut session)?, "degraded");

    sh(&mut session, "echo '<3>reboop test error' >/dev/kmsg")?;
    assert!(facts::kernel_errors(&mut session)?.contains("reboop test error"));

    clean_up(&mut session)?;
    assert_eq!(facts::wait_until_booted(&mut session, MINUTE)?, "running");
    Ok(())
}

fn systemd_boots_default_is_checked(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let default = facts::systems(&mut session)?.default;
    let [boot] = &boot::default_boots(&mut session)?[..] else { bail!("not one default boot") };
    assert_eq!((boot.loader.as_str(), boot.system(), boot.missing_files.len()), ("systemd-boot", Some(default.as_str()), 0), "{boot:?}");

    // A one-time boot into the firmware's setup, by systemd-boot and by the
    // firmware itself
    sh(&mut session, "bootctl set-oneshot auto-reboot-to-firmware-setup && bootctl reboot-to-firmware true")?;
    let machine = vm_machine(vm);
    let facts = preflight::gather(&mut session, &machine.hostname);
    clean_up(&mut session)?;
    assert_eq!(
        preflight::blockers(&machine, &facts?),
        [
            "boot: the firmware will open its setup at the next boot (OsIndications)",
            r#"boot: systemd-boot's default, "auto-reboot-to-firmware-setup", doesn't boot a NixOS system (no init=/nix/store/…/init)"#,
        ]
    );
    Ok(())
}

fn grubs_defaults_are_checked(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let default = facts::systems(&mut session)?.default;
    let boots = boot::default_boots(&mut session)?;
    let loaders: Vec<_> = boots.iter().map(|boot| boot.loader.as_str()).collect();
    assert_eq!(loaders, ["/boot/grub/grub.cfg", "/boot-fallback/grub/grub.cfg"]);
    for boot in &boots {
        assert_eq!((boot.entry.as_str(), boot.system(), boot.missing_files.len()), ("NixOS", Some(default.as_str()), 0), "{boot:?}");
    }

    // Set aside `file`, and have clean_up put it back.
    let set_aside = |name: &str, file: &str| {
        let backup = format!("/var/tmp/reboop-test-{name}");
        let file = shell_quote(file);
        format!("cp {file} {backup} && printf %s {file} >{backup}-path")
    };
    let machine = vm_machine(vm);

    // A mirror that's behind, and a kernel gone from the other /boot
    let stale = "/nix/store/00000000000000000000000000000000-nixos-system-stale";
    let kernel = &boots[0].files[0];
    sh(
        &mut session,
        &format!(
            "set -e
            {}
            sed -i 's|init={default}/init|init={stale}/init|' /boot-fallback/grub/grub.cfg
            {}
            rm {}",
            set_aside("grub.cfg", "/boot-fallback/grub/grub.cfg"),
            set_aside("kernel", kernel),
            shell_quote(kernel)
        ),
    )?;
    let facts = preflight::gather(&mut session, &machine.hostname);
    clean_up(&mut session)?;
    assert_eq!(
        preflight::blockers(&machine, &facts?),
        [
            format!(r#"boot: /boot/grub/grub.cfg's default, "NixOS", needs {kernel}, which is missing"#),
            format!(r#"boot: /boot-fallback/grub/grub.cfg's default, "NixOS", boots {stale}, not the system profile's {default}"#),
        ]
    );

    // A mirror without its grub.cfg
    sh(&mut session, &format!("{} && rm /boot-fallback/grub/grub.cfg", set_aside("grub.cfg", "/boot-fallback/grub/grub.cfg")))?;
    let facts = preflight::gather(&mut session, &machine.hostname);
    clean_up(&mut session)?;
    assert_eq!(preflight::blockers(&machine, &facts?), ["boot: /boot-fallback/grub/grub.cfg is missing"]);
    Ok(())
}

fn luks_password_is_tested(vm: &Vm) -> Result<()> {
    assert_eq!(initrd::luks_devices(&mut vm.session()?)?, ["/dev/vda2"]);
    let test = |password| initrd::test_luks_password(&vm.ssh, &vm.target, password, Deadline::after(MINUTE));
    assert_eq!(test(vm.luks_password()?)?, ("/dev/vda2".into(), true));
    assert_eq!(test("not the password")?, ("/dev/vda2".into(), false));

    // Even with a keyring token that opens the device by itself, as it does
    // from another connection: ssh sessions share sshd's session keyring,
    // there being no pam_keyinit.  (clean_up removes both.)
    let mut session = clean_session(vm)?;
    let add_token = format!(
        "cryptsetup token add --token-id 0 --key-description reboop-test /dev/vda2 && \
         keyctl add user reboop-test {} @s >/dev/null",
        shell_quote(vm.luks_password()?)
    );
    let wrong = sh(&mut session, &add_token)
        .and_then(|_| sh(&mut vm.session()?, "cryptsetup open --test-passphrase --token-only /dev/vda2"))
        .and_then(|_| test("not the password"));
    clean_up(&mut session)?;
    assert_eq!(wrong?, ("/dev/vda2".into(), false));
    Ok(())
}

/// Catches the VM (see [`bounce::catch`]) with `password`, printing what it
/// says, and returns its problems and what it said.
fn catch_vm(vm: &Vm, password: Option<&str>) -> Result<(Vec<String>, String)> {
    let mut printed = Vec::new();
    let problems = bounce::catch(&vm.ssh, &vm_machine(vm), password, &mut Printer::new(&mut printed, false, None, None));
    let printed = String::from_utf8(printed)?;
    print!("{printed}");
    Ok((problems?, printed))
}

fn catch_when_up(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let boot_id = facts::boot_id(&mut session)?;
    let (problems, printed) = catch_vm(vm, None)?;
    assert_eq!(problems, Vec::<String>::new());
    let no_password = format!("There's no stored LUKS password for {}, so its initrd won't be unlocked\n", vm.manifest.hostname);
    assert!(printed.starts_with(&no_password), "{printed}");
    assert_eq!(facts::boot_id(&mut session)?, boot_id);
    assert!(printed.contains("\nScrubbing btrfs on /\n"), "{printed}");

    // Having just found / clean, the next catch spares it, as long as
    // btrfs-progs' own record agrees and the memory of it isn't too old.
    let scrub = btrfs::scrub_status(&mut session, "/")?;
    let uuid = &scrub.uuid;
    assert!(btrfs::last_clean_scrub(&mut session, uuid)?.is_some_and(|ago| ago < MINUTE));
    let (problems, printed) = catch_vm(vm, None)?;
    assert_eq!(problems, Vec::<String>::new());
    let spared = printed.contains("\nNot scrubbing btrfs on /, which a scrub found clean ") && printed.contains(" ago (scrub_every_days is 14)\n");
    assert!(spared, "{printed}");
    assert_eq!(btrfs::scrub_status(&mut session, "/")?.started, scrub.started);
    // Not after a scrub that didn't end clean, here one cancelled (throttled
    // so that it's still running when cancelled)
    let speed_max = format!("/sys/fs/btrfs/{uuid}/devinfo/1/scrub_speed_max");
    sh(&mut session, &format!("echo 1m >{speed_max}"))?;
    btrfs::start_scrub(&mut session, "/")?;
    sh(&mut session, &format!("btrfs scrub cancel / && echo 0 >{speed_max}"))?;
    assert_eq!(btrfs::scrub_status(&mut session, "/")?.state, ScrubState::Aborted);
    let (problems, printed) = catch_vm(vm, None)?;
    assert_eq!(problems, Vec::<String>::new());
    assert!(printed.contains("\nScrubbing btrfs on /\n"), "{printed}");
    assert!(btrfs::last_clean_scrub(&mut session, uuid)?.is_some_and(|ago| ago < MINUTE));
    // Nor once the memory is old
    sh(&mut session, &format!("touch -d '15 days ago' {}/{uuid}", btrfs::LAST_CLEAN_SCRUB_DIR))?;
    let (problems, printed) = catch_vm(vm, None)?;
    assert_eq!(problems, Vec::<String>::new());
    assert!(printed.contains("\nScrubbing btrfs on /\n"), "{printed}");
    assert!(btrfs::last_clean_scrub(&mut session, uuid)?.is_some_and(|ago| ago < MINUTE));
    // A scrub's result is shown even when remembering it fails.
    sh(&mut session, &format!("rm -rf {dir} && touch {dir}", dir = btrfs::LAST_CLEAN_SCRUB_DIR))?;
    let (problems, printed) = catch_vm(vm, None)?;
    assert!(printed.contains("\nbtrfs on /: scrub finished, ") && printed.contains(" scrubbed, no errors found\n"), "{printed}");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("btrfs on /: couldn't record how the scrub went: "), "{problems:?}");

    let someone_else = Machine { hostname: "someone-else".into(), ..vm_machine(vm) };
    let mut printed = Vec::new();
    let error = bounce::catch(&vm.ssh, &someone_else, None, &mut Printer::new(&mut printed, false, None, None)).unwrap_err();
    assert!(format!("{error:#}").contains(r#"not "someone-else""#), "{error:#}");
    assert!(!String::from_utf8(printed)?.contains("systemd says"));
    Ok(())
}

fn catch_with_wrong_password_first(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let boot_id = facts::boot_id(&mut session)?;
    reboot::ask(session, Down::Reboot)?;
    let error = catch_vm(vm, Some("not the password")).unwrap_err();
    let wrong = matches!(error.downcast_ref(), Some(UnlockError::WrongPassword { .. }));
    ensure!(wrong, "expected the password to be rejected, got: {error:#}");
    // Unlocked some other way, as at the console
    initrd::unlock(&vm.ssh, &vm.initrd_target, vm.luks_password()?, Deadline::after(MINUTE))?;
    let (problems, printed) = catch_vm(vm, Some(vm.luks_password()?))?;
    assert_eq!(problems, Vec::<String>::new());
    assert!(!printed.contains("Answered"), "{printed}");
    assert_ne!(facts::boot_id(&mut vm.session()?)?, boot_id);
    Ok(())
}

fn bounce_into_new_default_configuration(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let machine = vm_machine(vm);
    let hostname = &machine.hostname;
    let password = || vm.luks_password().map(str::to_string);
    // Bounces the VM as `machine`, printing what it says, and returns that too.
    let bounce_vm = |machine: &Machine| -> Result<(Outcome, String)> {
        let mut printed = Vec::new();
        let outcome = bounce::bounce(&vm.ssh, machine, password, &mut Printer::new(&mut printed, false, None, None));
        let printed = String::from_utf8(printed)?;
        print!("{printed}");
        Ok((outcome?, printed))
    };

    // Not while someone has a tmux, and without stopping anything
    let is_active = "systemctl is-active --quiet reboop-test-sleep.service";
    sh(&mut session, "systemd-run --quiet --unit=reboop-test-sleep sleep 600")?;
    sh(&mut session, START_TMUX)?;
    tester_tmux(&mut session)?;
    let boot_id = facts::boot_id(&mut session)?;
    let (Outcome::NotOkay(blockers), _) = bounce_vm(&machine)? else {
        bail!("bounced despite the tmux");
    };
    assert!(!blockers.is_empty() && blockers.iter().all(|blocker| blocker.starts_with("tmux: tester's server at ")), "{blockers:?}");
    assert_eq!(facts::boot_id(&mut session)?, boot_id);
    sh(&mut session, is_active)?;
    clean_up(&mut session)?;

    // Nor when something starts while the stop_services stop, here a job
    // that stopping one starts, which is left stopped.  (Absolute paths,
    // since transient units get a minimal PATH.)
    sh(&mut session, r#"systemd-run --quiet --unit=reboop-test-sleep \
        -p "ExecStopPost=$(command -v systemd-run) --quiet --no-block --unit=reboop-test-job -p Type=oneshot $(command -v sleep) 600" sleep 600"#)?;
    let (Outcome::NotOkay(blockers), printed) = bounce_vm(&machine)? else {
        bail!("bounced despite the job");
    };
    assert_eq!(blockers, ["systemd job: start reboop-test-job.service (running) for 5s or more"]);
    assert!(printed.contains(&format!("\nStill stopped on {hostname}:\n    reboop-test-sleep.service\n")), "{printed}");
    assert_eq!(facts::boot_id(&mut session)?, boot_id);
    assert_ne!(session.run(is_active, MINUTE)?.status, 0);
    clean_up(&mut session)?;

    let before = facts::systems(&mut session)?;
    let systems = &vm.manifest.systems;
    let (next, next_variant) = if before.current == systems.base { (&systems.alt, "alt") } else { (&systems.base, "base") };

    // What `nixos-rebuild boot` does
    sh(&mut session, &format!("nix-env -p /nix/var/nix/profiles/system --set {next} && {next}/bin/switch-to-configuration boot"))?;
    let expected = facts::systems(&mut session)?;
    assert_eq!((&expected.current, &expected.default), (&before.current, next));
    assert_eq!(expected.reboot_reasons(), ["new system"], "{expected:?}");

    // One of the stop_services is running, and one doesn't exist.  The
    // configuration on disk is the old one, which only gets a warning.
    sh(&mut session, "systemd-run --quiet --unit=reboop-test-sleep sleep 600")?;
    set_evaluated_toplevel(&mut session, &before.current)?;
    // And tester has a tmux with a resumable command typed into it, which
    // the machine's tmux setting has brought back after the reboot.
    sh(&mut session, START_TMUX)?;
    tester_tmux(&mut session)?;
    type_into(&mut session, "work:0.0", "sleep 601")?;
    wait_for(|| Ok(matches!(pane(&tester_tmux(&mut session)?, "work:0.0")?.running, Running::Typed { .. })))?;
    let (outcome, printed) = bounce_vm(&resuming_machine(vm, &["sleep 601"]))?;
    assert!(matches!(&outcome, Outcome::Done(problems) if problems.is_empty()), "{outcome:?}");
    assert!(printed.contains(".reboot.json, to bring back after the reboot: 1 tmux server with 1 session and 1 pane\n"), "{printed}");
    assert!(printed.contains("\nBringing back what was in tmux, from /var/lib/reboop/tmux/"), "{printed}");
    assert!(printed.contains(&format!("\nBringing back tester's tmux server at {TESTER_SOCKET}: 1 session and 1 pane\nResumed tester's work:0.0 in /var/tmp: sleep 601\n")), "{printed}");
    let warning = format!("\n{hostname}: nixos-rebuild would change the system: {next} → {}\n", before.current);
    assert!(printed.contains(&warning), "{printed}");
    let stopping = format!(
        "\nreboop-test-sleep.service is stopped\nThere's no reboop-test-nonexistent.service to stop\n\
         Checking again whether {hostname} is okay to reboot\nSaved what's in tmux to /var/lib/reboop/tmux/"
    );
    assert!(printed.contains(&stopping), "{printed}");
    assert!(printed.contains(&format!("\nCommands to run again then:\n    tester's work:0.0 in /var/tmp: sleep 601\nAsking {hostname} to reboot\n")), "{printed}");
    if vm.manifest.initrd.is_some() {
        assert!(printed.contains("\nThe stored LUKS password opens /dev/vda2\n"));
    } else {
        assert!(printed.contains("\nThere's no LUKS device beneath /, so there'll be nothing to unlock\n"));
    }
    let mut session = vm.session()?;
    assert_ne!(facts::boot_id(&mut session)?, boot_id);
    assert_eq!(btrfs::scrub_status(&mut session, "/")?.state, ScrubState::Finished);
    let after = facts::systems(&mut session)?;
    assert_eq!((&after.booted, &after.current), (next, next));
    assert_eq!(after.reboot_reasons(), Vec::<String>::new(), "{after:?}");
    assert_eq!(after.running_kernel, expected.default_kernel);
    assert_eq!(sh(&mut session, "cat /etc/reboop-test-variant")?, next_variant);
    // tester's tmux is back, with the command running again, as zsh-spy
    // recorded it being typed again; and the capture isn't pending anymore.
    wait_for(|| Ok(matches!(pane(&tester_tmux(&mut session)?, "work:0.0")?.running, Running::Typed { .. })))?;
    expect_pane(&tester_tmux(&mut session)?, "work:0.0", "/var/tmp", Running::Typed { command: "sleep 601".into() })?;
    assert!(sh(&mut session, "ps -u tester -o args=")?.lines().any(|args| args == "sleep 601"));
    assert!(resume::pending(&mut session)?.is_none());
    Ok(())
}

fn stop_powers_off(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let machine = vm_machine(vm);
    let hostname = &machine.hostname;
    // Stops the VM, printing what it says, and returns that too.
    let stop_vm = || -> Result<(Outcome, String)> {
        let mut printed = Vec::new();
        let outcome = bounce::stop(&vm.ssh, &machine, &mut Printer::new(&mut printed, false, None, None));
        let printed = String::from_utf8(printed)?;
        print!("{printed}");
        Ok((outcome?, printed))
    };

    // Not while someone has a tmux
    sh(&mut session, START_TMUX)?;
    tester_tmux(&mut session)?;
    let (Outcome::NotOkay(blockers), printed) = stop_vm()? else {
        bail!("stopped despite the tmux");
    };
    assert!(!blockers.is_empty() && blockers.iter().all(|blocker| blocker.starts_with("tmux: tester's server at ")), "{blockers:?}");
    assert!(printed.contains(&format!("\nNot shutting down {hostname}:\n")), "{printed}");
    clean_up(&mut session)?;

    // One of the stop_services is running, and one doesn't exist.
    sh(&mut session, "systemd-run --quiet --unit=reboop-test-sleep sleep 600")?;
    let (outcome, printed) = stop_vm()?;
    let Outcome::Done(problems) = &outcome else {
        bail!("didn't shut down: {outcome:?}");
    };
    // qemu exits once the guest has powered off.  The VM is started again
    // before the checks below, so that a failed check doesn't leave it off
    // for the tests that follow.
    wait_for(|| Ok(!vm.is_running()))?;
    vm.restart()?;
    assert!(problems.is_empty(), "{problems:?}");
    let stopping = format!(
        "\nreboop-test-sleep.service is stopped\nThere's no reboop-test-nonexistent.service to stop\n\
         Checking again whether {hostname} is okay to shut down\nAsking {hostname} to shut down\n\
         Waiting for {} to stop accepting SSH\n{hostname} is down, and all is well\n",
        machine.target()
    );
    assert!(printed.contains(&stopping), "{printed}");
    Ok(())
}

fn tmux_is_captured_stopped_and_started(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    assert_eq!(tmux::capture(&mut session)?, Vec::<Server>::new());
    let tmux_cmd = format!("tmux -S {TESTER_SOCKET}");

    // tester's server: session work with window 0 (a zsh pane in /var/tmp,
    // then one in /home/tester), window 3, "jobs", started with sleep 600 in
    // /tmp, and window 5 in a directory that'll be gone; and session other,
    // with a background job, and a line typed with a leading space.
    sh(&mut session, START_TMUX)?;
    tester_tmux(&mut session)?;
    sh(
        &mut session,
        &format!(
            "mkdir -p /var/tmp/reboop-test-gone && chown tester /var/tmp/reboop-test-gone && \
             {tmux_cmd} split-window -d -h -t work:0 -c /home/tester && {tmux_cmd} new-window -d -t work:3 -n jobs -c /tmp 'sleep 600' && \
             {tmux_cmd} new-window -d -t work:5 -c /var/tmp/reboop-test-gone && {tmux_cmd} new-session -d -s other -c /tmp && {tmux_cmd} new-window -d -t other:1 -c /tmp"
        ),
    )?;
    type_into(&mut session, "work:0.0", "sleep 601")?;
    type_into(&mut session, "work:5.0", "sleep 604")?;
    type_into(&mut session, "other:0.0", "sleep 602 &")?;
    type_into(&mut session, "other:1.0", " sleep 603")?;
    // zsh-spy has written about each once its command is running.
    wait_for(|| {
        let server = tester_tmux(&mut session)?;
        Ok(matches!(pane(&server, "other:0.0")?.running, Running::Background { .. }) && matches!(pane(&server, "other:1.0")?.running, Running::Unknown { .. }))
    })?;
    let server = tester_tmux(&mut session)?;
    assert_eq!((server.user.as_str(), server.socket.as_str(), &server.error), ("tester", TESTER_SOCKET, &None));
    let typed = |command: &str| Running::Typed { command: command.into() };
    expect_pane(&server, "work:0.0", "/var/tmp", typed("sleep 601"))?;
    expect_pane(&server, "work:0.1", "/home/tester", Running::Idle)?;
    expect_pane(&server, "work:3.0", "/tmp", Running::Started { command: "\"sleep 600\"".into() })?;
    expect_pane(&server, "work:5.0", "/var/tmp/reboop-test-gone", typed("sleep 604"))?;
    expect_pane(&server, "other:0.0", "/tmp", Running::Background { jobs: vec![Some("sleep 602".into())] })?;
    expect_pane(&server, "other:1.0", "/tmp", Running::Unknown { program: "sleep".into() })?;
    assert_eq!(pane(&server, "work:0.0")?.mountpoint, "/");
    let work = server.sessions.iter().find(|tmux_session| tmux_session.name == "work").unwrap();
    assert_eq!(work.windows.iter().map(|window| (window.index, window.name.as_deref(), window.panes.len())).collect::<Vec<_>>(), [(0, None, 2), (3, Some("jobs"), 1), (5, None, 1)]);
    assert!(work.windows[0].active && work.windows[0].panes[0].active && !work.windows[0].panes[1].active, "{work:#?}");

    // What blocks a reboot: with tmux "block", the server; with "resume",
    // the background job and the line typed with a leading space.
    let machine = vm_machine(vm);
    let resuming = resuming_machine(vm, &["sleep 60[14]", "\"sleep 600\""]);
    let facts = preflight::gather(&mut session, &machine.hostname)?;
    assert_eq!(preflight::blockers(&machine, &facts), [format!("tmux: tester's server at {TESTER_SOCKET} has 2 sessions and 6 panes (tmux is \"block\")")]);
    assert_eq!(
        preflight::blockers(&resuming, &facts),
        [
            "tmux: tester's other:0.0 in /tmp: a background job is still running: sleep 602",
            "tmux: tester's other:1.0 in /tmp: running sleep, but what was typed to start it is unknown",
        ]
    );
    // `reboop tmux list` says as much, and saves a look at the servers.
    let mut printed = Vec::new();
    resume::list(&vm.ssh, &resuming, None, &mut Printer::new(&mut printed, false, None, None))?;
    let printed = String::from_utf8(printed)?;
    print!("{printed}");
    let listing = format!("tester's tmux server at {TESTER_SOCKET} (pid {}): 2 sessions and 6 panes\n    other:0.0 in /tmp: a background job is still running: sleep 602\n", server.pid);
    assert!(printed.contains(&listing), "{printed}");
    assert!(printed.contains("\n    work:0.0 in /var/tmp: sleep 601 (resumable)\n    work:0.1 in /home/tester: idle\n    work:3.0 in /tmp: \"sleep 600\" (resumable)\n"), "{printed}");
    let saved = sh(&mut session, &format!("ls {}", resume::DIR))?;
    assert!(saved.lines().count() == 1 && saved.ends_with(".check.json\n"), "{saved}");

    // `reboop tmux stop` refuses for the same reasons.
    let mut printed = Vec::new();
    let refused = resume::stop(&vm.ssh, &resuming, &mut Printer::new(&mut printed, false, None, None))?;
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert!(String::from_utf8(printed)?.starts_with(&format!("Not stopping tmux on {}:\n    tmux: tester's other:0.0", machine.hostname)));
    tester_tmux(&mut session)?;
    // Without those, it stops the server, having saved what's in it.
    sh(&mut session, &format!("{tmux_cmd} kill-session -t other"))?;
    let geometry = |session: &mut Session| sh(session, &format!("{tmux_cmd} list-panes -a -F '#{{session_name}}:#{{window_index}}.#{{pane_index}} #{{pane_left}},#{{pane_top}} #{{pane_width}}x#{{pane_height}}'"));
    let before = geometry(&mut session)?;
    let mut printed = Vec::new();
    let refused = resume::stop(&vm.ssh, &resuming, &mut Printer::new(&mut printed, false, None, None))?;
    let printed = String::from_utf8(printed)?;
    print!("{printed}");
    assert_eq!(refused, Vec::<String>::new());
    let to_run = "\nCommands to run again then:\n    tester's work:0.0 in /var/tmp: sleep 601\n    tester's work:3.0 in /tmp: \"sleep 600\"\n    tester's work:5.0 in /var/tmp/reboop-test-gone: sleep 604\n";
    assert!(printed.contains(to_run) && printed.contains(&format!("\nStopped tester's tmux server at {TESTER_SOCKET}\n")), "{printed}");
    assert_eq!(tmux::capture(&mut session)?, Vec::<Server>::new());
    let (path, pending) = resume::pending(&mut session)?.ok_or_else(|| anyhow!("nothing is pending"))?;
    assert_eq!(pending.purpose, Purpose::TmuxStop);
    assert!(path.starts_with("/var/lib/reboop/tmux/") && path.ends_with(".tmux-stop.json"), "{path}");

    // `reboop tmux start` brings it all back, but for the pane whose
    // directory is gone: the server runs in a logind session of tester's.
    sh(&mut session, "rmdir /var/tmp/reboop-test-gone")?;
    let mut printed = Vec::new();
    let problems = resume::start(&vm.ssh, &resuming, None, &mut Printer::new(&mut printed, false, None, None))?;
    let printed = String::from_utf8(printed)?;
    print!("{printed}");
    assert_eq!(problems, ["tester's work:5.0 in /var/tmp/reboop-test-gone: sleep 604 wasn't run again: its shell is in \"/home/tester\" (does /var/tmp/reboop-test-gone exist?)"]);
    assert!(printed.contains("\nResumed tester's work:0.0 in /var/tmp: sleep 601\n") && printed.contains("\nResumed tester's work:3.0 in /tmp: \"sleep 600\"\n"), "{printed}");
    assert!(resume::pending(&mut session)?.is_none());
    wait_for(|| Ok(matches!(pane(&tester_tmux(&mut session)?, "work:0.0")?.running, Running::Typed { .. })))?;
    let server = tester_tmux(&mut session)?;
    expect_pane(&server, "work:0.0", "/var/tmp", typed("sleep 601"))?;
    expect_pane(&server, "work:0.1", "/home/tester", Running::Idle)?;
    expect_pane(&server, "work:3.0", "/tmp", Running::Started { command: "\"sleep 600\"".into() })?;
    expect_pane(&server, "work:5.0", "/home/tester", Running::Idle)?;
    assert_eq!(server.sessions[0].windows.iter().map(|window| (window.index, window.name.as_deref())).collect::<Vec<_>>(), [(0, None), (3, Some("jobs")), (5, None)]);
    assert_eq!(geometry(&mut session)?, before);
    let cgroup = sh(&mut session, &format!("cat /proc/{}/cgroup", server.pid))?;
    assert!(cgroup.contains("/user-1000.slice/session-") && cgroup.trim_end().ends_with(".scope"), "{cgroup}");
    assert!(sh(&mut session, "loginctl list-users")?.contains("tester"));
    // Nothing is pending anymore, so `start` needs a saved capture, whose
    // sessions are then already there.
    assert!(resume::start(&vm.ssh, &resuming, None, &mut Printer::new(&mut Vec::new(), false, None, None)).is_err());
    let problems = resume::start(&vm.ssh, &resuming, Some(&path), &mut Printer::new(&mut Vec::new(), false, None, None))?;
    assert_eq!(problems, [format!("tester's tmux at {TESTER_SOCKET} already has a session called work, which was left alone")]);
    // A capture pending for a reboot that hasn't happened isn't brought back.
    let boot_id = facts::boot_id(&mut session)?;
    resume::save(&mut session, &Capture::new(Purpose::Reboot, &boot_id, vec![server.clone()]), true)?;
    let mut printed = Vec::new();
    assert_eq!(resume::restore_pending(&mut session, &resuming, &mut Printer::new(&mut printed, false, None, None))?, Vec::<String>::new());
    assert!(String::from_utf8(printed)?.contains(" is from this boot, so what it describes is still running; not bringing it back\n"));
    assert!(resume::pending(&mut session)?.is_none());
    clean_up(&mut session)
}

/// Runs `test` against the VM called `name`, which is set up by the first
/// test to run on it.
fn run(name: Name, test: Test) -> Result<(), Failed> {
    static VMS: [OnceLock<Result<Vm, String>>; 2] = [OnceLock::new(), OnceLock::new()];
    let vm = VMS[name as usize].get_or_init(|| Vm::get(name).map_err(|error| format!("{error:?}"))).as_ref()?;
    test(vm).map_err(|error| format!("{error:?}").into())
}

fn main() {
    harness::be_watchdog_if_started_as_one();
    let mut args = Arguments::from_args();
    // The tests share the VMs, and some reboot them.
    args.test_threads = Some(1);
    // The tests for each VM.  Those about the machine rather than its boot
    // loader run on both.
    let systemd_boot: &[(&str, Test)] = &[
        ("session_runs_commands", session_runs_commands),
        ("session_breaks_on_timeout", session_breaks_on_timeout),
        ("unreachable_ports_fail_fast", unreachable_ports_fail_fast),
        ("identity_and_systems", identity_and_systems),
        ("rebuild_is_evaluated", rebuild_is_evaluated),
        ("network_sample_sees_traffic", network_sample_sees_traffic),
        ("activities_are_detected", activities_are_detected),
        ("preflight_finds_blockers", preflight_finds_blockers),
        ("btrfs_root_is_idle", btrfs_root_is_idle),
        ("btrfs_missing_device_is_detected", btrfs_missing_device_is_detected),
        ("btrfs_running_scrub_and_balance_are_detected", btrfs_running_scrub_and_balance_are_detected),
        ("smart_is_read_from_disks", smart_is_read_from_disks),
        ("scrub_of_root_finishes_clean", scrub_of_root_finishes_clean),
        ("scrub_finds_corruption", scrub_finds_corruption),
        ("scrub_status_outlasts_a_locked_status_file", scrub_status_outlasts_a_locked_status_file),
        ("stop_unit", stop_unit),
        ("postflight_facts", postflight_facts),
        ("tmux_is_captured_stopped_and_started", tmux_is_captured_stopped_and_started),
        ("systemd_boots_default_is_checked", systemd_boots_default_is_checked),
        ("luks_password_is_tested", luks_password_is_tested),
        ("catch_with_wrong_password_first", catch_with_wrong_password_first),
        ("bounce_into_new_default_configuration", bounce_into_new_default_configuration),
        ("stop_powers_off", stop_powers_off),
    ];
    let grub: &[(&str, Test)] = &[
        ("identity_and_systems", identity_and_systems),
        ("rebuild_is_evaluated", rebuild_is_evaluated),
        ("preflight_finds_blockers", preflight_finds_blockers),
        ("btrfs_root_is_idle", btrfs_root_is_idle),
        ("smart_is_read_from_disks", smart_is_read_from_disks),
        ("postflight_facts", postflight_facts),
        ("grubs_defaults_are_checked", grubs_defaults_are_checked),
        ("catch_when_up", catch_when_up),
        ("bounce_into_new_default_configuration", bounce_into_new_default_configuration),
        ("stop_powers_off", stop_powers_off),
    ];
    let trials = [(Name::SystemdBoot, systemd_boot), (Name::Grub, grub)]
        .into_iter()
        .flat_map(|(vm, tests)| tests.iter().map(move |&(name, test)| Trial::test(format!("{}::{name}", vm.as_str()), move || run(vm, test))))
        .collect();
    libtest_mimic::run(&args, trials).exit();
}
