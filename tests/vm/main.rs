// Model-output: Claude Opus 5.5

//! Tests of reboop's primitives against a NixOS VM (configuration.nix) with
//! a LUKS-encrypted btrfs root.
//!
//! The VM keeps running after the tests so that later runs start quickly.
//! To stop it:
//!
//!     kill $(cat target/tmp/reboop-vm/qemu.pid)

mod harness;

use anyhow::{Result, bail, ensure};
use harness::{Vm, clean_up};
use libtest_mimic::{Arguments, Failed, Trial};
use reboop::bounce::{self, Outcome, Printer};
use reboop::btrfs::{self, Device, ScrubState};
use reboop::config::Machine;
use reboop::deadline::{Deadline, Permanent};
use reboop::facts;
use reboop::initrd::{self, UnlockError};
use reboop::preflight;
use reboop::processes::{self, Activity};
use reboop::reboot::{self, Stopped};
use reboop::ssh::{Session, wait_for_session};
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
    sh(&mut session, "command -v systemctl btrfs findmnt ps dmesg base64 mktemp")?;
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

    // An unknown host key won't go away, so there's no retrying.
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
    assert_eq!(facts::hostname(&mut session)?, harness::HOSTNAME);
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
    assert_eq!(systems.running_kernel, sh(&mut session, "uname -r")?.trim_end());

    let load = facts::load_average_1min(&mut session)?;
    assert!((0.0..100.0).contains(&load), "{load}");
    Ok(())
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

/// Starts a real tmux server, owned by someone other than root.  (Transient
/// units get a minimal PATH, hence the -E PATH.)
const START_TMUX: &str = "systemd-run --quiet --unit=reboop-test-tmux -E PATH -p RemainAfterExit=yes --uid=tester tmux new-session -d sleep 600";

fn activities(session: &mut Session) -> Result<BTreeSet<Activity>> {
    Ok(processes::list(session)?.iter().filter_map(processes::activity).collect())
}

fn activities_are_detected(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    assert_eq!(activities(&mut session)?, BTreeSet::new(), "{:#?}", processes::list(&mut session)?);

    sh(&mut session, START_TMUX)?;
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

    // A btrfs receive and a cryptsetup, each waiting for its input
    sh(&mut session, "mkdir /var/tmp/reboop-test-receive && truncate -s 32M /var/tmp/reboop-test-luks.img && \
                      systemd-run --quiet --unit=reboop-test-receive -E PATH sh -c 'sleep 600 | btrfs receive /var/tmp/reboop-test-receive' && \
                      systemd-run --quiet --unit=reboop-test-cryptsetup -E PATH \
                          sh -c 'sleep 600 | cryptsetup luksFormat --batch-mode --key-file=- /var/tmp/reboop-test-luks.img'")?;

    // Wait for them all to start, including the build's builder, which
    // runs as a nixbld user.
    let expected = BTreeSet::from([
        Activity::Nix,
        Activity::SwitchToConfiguration,
        Activity::Tmux,
        Activity::Rsync,
        Activity::BtrfsSendReceive,
        Activity::Cryptsetup,
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
        hostname: harness::HOSTNAME.into(),
        ipv4: Ipv4Addr::LOCALHOST,
        ssh_port: vm.target.port,
        initrd_ssh_port: vm.initrd_target.port,
        scrub_mounts: vec!["/".into()],
        max_network_transfer_bytes_per_sec: 1_000_000,
        // The VM's load depends on whatever else its host is doing.
        max_load_average_1min: 100.0,
        root_full_percent: 97,
        luks_signing_key: "/nonexistent".into(),
        stop_services: vec!["reboop-test-sleep.service".into(), "reboop-test-nonexistent.service".into()],
    }
}

fn preflight_finds_blockers(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let error = preflight::gather(&mut session, "someone-else").unwrap_err();
    assert!(format!("{error:#}").contains(r#"calls itself "reboop-test", not "someone-else""#), "{error:#}");
    // A fully qualified name for the machine will do.
    preflight::gather(&mut session, &format!("{}.example.com", harness::HOSTNAME))?;

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
    wait_for(|| Ok(activities(&mut session)?.contains(&Activity::Tmux)))?;
    let blockers = preflight::blockers(&machine, &preflight::gather(&mut session, &machine.hostname)?);
    assert!(!blockers.is_empty() && blockers.iter().all(|blocker| blocker.starts_with("tmux: pid ")), "{blockers:?}");
    clean_up(&mut session)?;

    // Programs holding inhibitor locks, one of which only delays a reboot.
    // (See START_TMUX about -E PATH.)
    for (name, mode) in [("reboop-test-inhibit", "block"), ("reboop-test-delay", "delay")] {
        sh(&mut session, &format!("systemd-run --quiet --unit={name} -E PATH systemd-inhibit --what=shutdown --who={name} --why=testing --mode={mode} sleep 600"))?;
    }
    wait_for(|| Ok(facts::inhibitors(&mut session)?.len() == 2))?;
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
    assert_ne!(btrfs::scrub_status(&mut session, "/")?.state, ScrubState::Running);
    assert_eq!(btrfs::devices(&mut session, &filesystems[0])?, [Device { devid: 1, missing: false, errors: BTreeMap::new() }]);
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
    btrfs::start_scrub(&mut session, &mountpoint, MINUTE)?;
    let status = btrfs::scrub_status(&mut session, &mountpoint)?;
    assert_eq!(status.state, ScrubState::Running);
    assert!(status.summary().starts_with("scrub has "), "{}", status.summary());
    // A scrub isn't an exclusive operation.
    assert_eq!(btrfs::exclusive_operation(&mut session, &filesystem)?, "none");
    assert!(btrfs::start_scrub(&mut session, &mountpoint, MINUTE).is_err());
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
    btrfs::start_scrub(&mut session, "/", MINUTE)?;
    let mut summaries = Vec::new();
    let status = btrfs::wait_for_scrub(&mut session, "/", Duration::from_secs(1), Deadline::after(5 * MINUTE), |status| {
        summaries.push(status.summary());
    })?;
    eprintln!("{}", summaries.join("\n"));
    assert_eq!(status.state, ScrubState::Finished);
    assert!(status.errors.is_empty(), "{status:?}");
    assert!(status.scrubbed_bytes > 500_000_000, "{status:?}");
    assert!(status.summary().ends_with("no errors found"), "{}", status.summary());
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

    btrfs::start_scrub(&mut session, &mountpoint, MINUTE)?;
    let status = btrfs::wait_for_scrub(&mut session, &mountpoint, Duration::from_millis(500), Deadline::after(MINUTE), |_| {})?;
    assert_eq!(status.state, ScrubState::Finished);
    assert!(status.errors.get("csum_errors").is_some_and(|&n| n > 0), "{status:?}");
    assert!(status.summary().contains("ERRORS FOUND"), "{}", status.summary());

    // The device remembers the corruption.
    let filesystem = btrfs::filesystems(&mut session)?.into_iter().find(|fs| fs.mountpoint == mountpoint).unwrap();
    let devices = btrfs::devices(&mut session, &filesystem)?;
    assert!(devices[0].errors.get("corruption_errs").is_some_and(|&n| n > 0), "{devices:?}");
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

    sh(&mut session, "echo '<3>reboop test error' >/dev/kmsg")?;
    assert!(facts::kernel_errors(&mut session)?.contains("reboop test error"));

    clean_up(&mut session)?;
    assert_eq!(facts::wait_until_booted(&mut session, MINUTE)?, "running");
    Ok(())
}

fn luks_password_is_tested(vm: &Vm) -> Result<()> {
    assert_eq!(initrd::luks_devices(&mut vm.session()?)?, ["/dev/vda2"]);
    let test = |password| initrd::test_luks_password(&vm.ssh, &vm.target, password, Deadline::after(MINUTE));
    assert_eq!(test(&vm.manifest.luks_password)?, ("/dev/vda2".into(), true));
    assert_eq!(test("not the password")?, ("/dev/vda2".into(), false));
    Ok(())
}

fn reboot_with_wrong_password_first(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let boot_id = facts::boot_id(&mut session)?;
    reboot::reboot(session)?;
    let deadline = Deadline::after(5 * MINUTE);
    let interval = Duration::from_secs(1);
    let error = initrd::wait_and_unlock(&vm.ssh, &vm.initrd_target, "not the password", interval, deadline).unwrap_err();
    let wrong = matches!(error.downcast_ref(), Some(UnlockError::WrongPassword { .. }));
    ensure!(wrong, "expected the password to be rejected, got: {error:#}");
    let prompts = initrd::wait_and_unlock(&vm.ssh, &vm.initrd_target, &vm.manifest.luks_password, interval, deadline)?;
    eprintln!("answered {prompts:?}");

    let mut session = wait_for_session(&vm.ssh, &vm.target, interval, deadline)?;
    assert_eq!(facts::wait_until_booted(&mut session, 5 * MINUTE)?, "running");
    assert_ne!(facts::boot_id(&mut session)?, boot_id);
    Ok(())
}

fn bounce_into_new_default_configuration(vm: &Vm) -> Result<()> {
    let mut session = clean_session(vm)?;
    let machine = vm_machine(vm);
    let password = || Ok(vm.manifest.luks_password.clone());
    // Bounces the VM, printing what it says, and returns that too.
    let bounce_vm = || -> Result<(Outcome, String)> {
        let mut printed = Vec::new();
        let outcome = bounce::bounce(&vm.ssh, &machine, password, &mut Printer::new(&mut printed, false, None));
        let printed = String::from_utf8(printed)?;
        print!("{printed}");
        Ok((outcome?, printed))
    };

    // Not while someone has a tmux, and without stopping anything
    let is_active = "systemctl is-active --quiet reboop-test-sleep.service";
    sh(&mut session, "systemd-run --quiet --unit=reboop-test-sleep sleep 600")?;
    sh(&mut session, START_TMUX)?;
    wait_for(|| Ok(activities(&mut session)?.contains(&Activity::Tmux)))?;
    let boot_id = facts::boot_id(&mut session)?;
    let (Outcome::NotOkay(blockers), _) = bounce_vm()? else {
        bail!("bounced despite the tmux");
    };
    assert!(!blockers.is_empty() && blockers.iter().all(|blocker| blocker.starts_with("tmux: ")), "{blockers:?}");
    assert_eq!(facts::boot_id(&mut session)?, boot_id);
    sh(&mut session, is_active)?;
    clean_up(&mut session)?;

    let before = facts::systems(&mut session)?;
    let systems = &vm.manifest.systems;
    let (next, next_variant) = if before.current == systems.base { (&systems.alt, "alt") } else { (&systems.base, "base") };

    // What `nixos-rebuild boot` does
    sh(&mut session, &format!("nix-env -p /nix/var/nix/profiles/system --set {next} && {next}/bin/switch-to-configuration boot"))?;
    let expected = facts::systems(&mut session)?;
    assert_eq!((&expected.current, &expected.default), (&before.current, next));

    // One of the stop_services is running, and one doesn't exist.
    sh(&mut session, "systemd-run --quiet --unit=reboop-test-sleep sleep 600")?;
    let (outcome, printed) = bounce_vm()?;
    assert!(matches!(&outcome, Outcome::Bounced(problems) if problems.is_empty()), "{outcome:?}");
    assert!(printed.contains("\nreboop-test-sleep.service is stopped\nThere's no reboop-test-nonexistent.service to stop\n"));
    let mut session = vm.session()?;
    assert_ne!(facts::boot_id(&mut session)?, boot_id);
    assert_eq!(btrfs::scrub_status(&mut session, "/")?.state, ScrubState::Finished);
    let after = facts::systems(&mut session)?;
    assert_eq!((&after.booted, &after.current), (next, next));
    assert_eq!(after.running_kernel, expected.default_kernel);
    assert_eq!(sh(&mut session, "cat /etc/reboop-test-variant")?, next_variant);
    Ok(())
}

/// Runs `test` against the VM, which is set up by the first test to run.
fn run(test: Test) -> Result<(), Failed> {
    static VM: OnceLock<Result<Vm, String>> = OnceLock::new();
    let vm = VM.get_or_init(|| Vm::get().map_err(|error| format!("{error:?}"))).as_ref()?;
    test(vm).map_err(|error| format!("{error:?}").into())
}

fn main() {
    let mut args = Arguments::from_args();
    // The tests share the VM, and some reboot it.
    args.test_threads = Some(1);
    let tests: &[(&str, Test)] = &[
        ("session_runs_commands", session_runs_commands),
        ("session_breaks_on_timeout", session_breaks_on_timeout),
        ("unreachable_ports_fail_fast", unreachable_ports_fail_fast),
        ("identity_and_systems", identity_and_systems),
        ("network_sample_sees_traffic", network_sample_sees_traffic),
        ("activities_are_detected", activities_are_detected),
        ("preflight_finds_blockers", preflight_finds_blockers),
        ("btrfs_root_is_idle", btrfs_root_is_idle),
        ("btrfs_missing_device_is_detected", btrfs_missing_device_is_detected),
        ("btrfs_running_scrub_and_balance_are_detected", btrfs_running_scrub_and_balance_are_detected),
        ("scrub_of_root_finishes_clean", scrub_of_root_finishes_clean),
        ("scrub_finds_corruption", scrub_finds_corruption),
        ("stop_unit", stop_unit),
        ("postflight_facts", postflight_facts),
        ("luks_password_is_tested", luks_password_is_tested),
        ("reboot_with_wrong_password_first", reboot_with_wrong_password_first),
        ("bounce_into_new_default_configuration", bounce_into_new_default_configuration),
    ];
    let trials = tests.iter().map(|&(name, test)| Trial::test(name, move || run(test))).collect();
    libtest_mimic::run(&args, trials).exit();
}
