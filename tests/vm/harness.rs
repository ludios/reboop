// Model-output: Claude Opus 5.5
// Model-output: Claude Fable 5.1

//! Builds a test VM (see default.nix), starts it if it isn't already
//! running, and brings it up to where it accepts SSH connections.
//!
//! A VM outlives the test run, so that the next run doesn't have to boot it;
//! everything about it lives in target/tmp/reboop-vm-NAME.  It's replaced
//! whenever the Nix build produces something different, and stopped once no
//! run has used it for three hours.

use anyhow::{Context, Result, anyhow, bail, ensure};
use reboop::deadline::{Deadline, Permanent};
use reboop::human;
use reboop::initrd::{self, UnlockError};
use reboop::ssh::{Session, Ssh, Target};
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

/// manifest.json from the Nix build
#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    /// The machine's hostname, which is also what ssh calls it
    pub hostname: String,
    /// Its disks, which it sees as vda, vdb, and so on
    pub disk_images: Vec<PathBuf>,
    /// UEFI firmware, or none to boot from BIOS
    pub ovmf: Option<Ovmf>,
    pub qemu: PathBuf,
    pub client_key: PathBuf,
    pub host_key_pub: String,
    /// The initrd's sshd, if the machine's root is on LUKS
    pub initrd: Option<Initrd>,
    /// The two NixOS configurations; /etc/reboop-test-variant says which is
    /// which.
    pub systems: TestSystems,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Ovmf {
    pub code: PathBuf,
    /// A template for the VM's own copy of the EFI variables
    pub vars: PathBuf,
}

/// The sshd in a machine's initrd, for unlocking its LUKS root.
#[derive(Clone, Debug, Deserialize)]
pub struct Initrd {
    pub host_key_pub: String,
    pub luks_password: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TestSystems {
    pub base: String,
    pub alt: String,
}

/// What we remember about the running VM between test runs.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct State {
    bundle: PathBuf,
    pid: u32,
    ssh_port: u16,
    initrd_ssh_port: u16,
}

/// Builds the VM called `name` in tests/vm with Nix, returning the result.
fn build(dir: &Path, name: &str) -> Result<PathBuf> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vm");
    eprintln!("building the {name} test VM with nix-build...");
    let output = Command::new("nix-build")
        .arg(&source)
        .args(["-A", name])
        .arg("--out-link")
        .arg(dir.join("bundle"))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("failed to run nix-build")?;
    ensure!(output.status.success(), "nix-build {} -A {name} failed", source.display());
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

/// Two free TCP ports on 127.0.0.1, bound at once so that they differ, then
/// freed for qemu.
fn free_ports() -> Result<[u16; 2]> {
    let listeners = [TcpListener::bind("127.0.0.1:0")?, TcpListener::bind("127.0.0.1:0")?];
    Ok(listeners.map(|listener| listener.local_addr().unwrap().port()))
}

/// Starts qemu in the background with what `manifest` (from the Nix build
/// `bundle`) describes, logging its console to `dir`, and forwarding
/// `ports` (to the system's sshd, and to the initrd's) to it.  Returns what
/// to remember about it: see [`State`].
fn start_qemu(dir: &Path, bundle: &Path, manifest: &Manifest, [ssh_port, initrd_ssh_port]: [u16; 2]) -> Result<State> {
    ensure!(!manifest.disk_images.is_empty(), "{} has no disks to boot", manifest.hostname);
    eprintln!("starting the {} test VM...", manifest.hostname);
    let arg = |prefix: &str, path: &Path| format!("{prefix}{}", path.display());
    let mut command = Command::new(&manifest.qemu);
    command.args(["-name", &manifest.hostname, "-machine", "q35,accel=kvm", "-cpu", "host", "-smp", "2", "-m", "2048"]);
    if let Some(ovmf) = &manifest.ovmf {
        let vars = dir.join("OVMF_VARS.fd");
        let _ = fs::remove_file(&vars);
        fs::copy(&ovmf.vars, &vars)?;
        fs::set_permissions(&vars, fs::Permissions::from_mode(0o644))?;
        command
            .args(["-drive", &arg("if=pflash,format=raw,unit=0,readonly=on,file=", &ovmf.code)])
            .args(["-drive", &arg("if=pflash,format=raw,unit=1,file=", &vars)]);
    }
    for disk in &manifest.disk_images {
        // snapshot=on: writes go to a temporary file that's gone when qemu
        // exits, so each VM starts from the pristine image.
        command.args(["-drive", &arg("if=virtio,format=qcow2,cache=unsafe,snapshot=on,file=", disk)]);
    }

    let pidfile = dir.join("qemu.pid");
    let _ = fs::remove_file(&pidfile);
    let output = command
        .args(["-device", "virtio-rng-pci"])
        .args(["-nic", &format!("user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{ssh_port}-:22,hostfwd=tcp:127.0.0.1:{initrd_ssh_port}-:23")])
        .args(["-display", "none", "-monitor", "none"])
        .args(["-serial", &arg("file:", &dir.join("console.log"))])
        .args(["-daemonize", "-pidfile", &pidfile.display().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .context("failed to run qemu")?;
    ensure!(output.status.success(), "qemu failed: {}", String::from_utf8_lossy(&output.stderr));
    let pid = fs::read_to_string(&pidfile)?.trim().parse()?;
    Ok(State { bundle: bundle.to_path_buf(), pid, ssh_port, initrd_ssh_port })
}

/// Whether `pid` is our qemu (and not some process that reused its pid),
/// going by the console log in `dir` that it writes to.
fn qemu_is_running(dir: &Path, pid: u32) -> bool {
    let console = dir.join("console.log").display().to_string();
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(&console))
}

fn stop_qemu(dir: &Path, pid: u32) -> Result<()> {
    if !qemu_is_running(dir, pid) {
        return Ok(());
    }
    eprintln!("stopping the test VM in {}...", dir.display());
    let pid = libc::pid_t::try_from(pid)?;
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..100 {
        if !qemu_is_running(dir, pid as u32) {
            return Ok(());
        }
        sleep(Duration::from_millis(100));
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    sleep(Duration::from_millis(500));
    if qemu_is_running(dir, pid as u32) {
        bail!("couldn't kill qemu (pid {pid})");
    }
    Ok(())
}

/// Locks `file` unless something else has it locked, returning whether it did.
fn try_lock(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

/// How long a VM keeps running after the last test run that used it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

/// How often a watchdog checks on its VM.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

/// The test program's first argument when it's a VM's watchdog; the VM's
/// directory and its qemu's pid follow.
const WATCHDOG_ARG: &str = "--reboop-vm-watchdog";

/// Watches the VM in `dir`, whose qemu is `pid`, for as long as qemu runs,
/// and stops it once no test run has held the VM's lock for
/// [`IDLE_TIMEOUT`].  The lock's mtime is when the VM was last used: runs set
/// it when they get the VM, and this sets it whenever a run it saw ends.
fn watch(dir: &Path, pid: u32) -> Result<()> {
    // Opened once, so that a VM whose directory has been deleted, and which
    // no run can find anymore, is stopped too.
    let lock = File::open(dir.join("lock"))?;
    // Held for as long as this watches.  A new qemu's watchdog waits here for
    // the old one's to notice that its qemu is gone.
    let watching = File::create(dir.join("watchdog.lock"))?;
    watching.lock()?;
    loop {
        sleep(WATCHDOG_INTERVAL);
        if !try_lock(&lock)? {
            // A run is using the VM, so it was last used when the run ends.
            lock.lock()?;
            lock.set_modified(SystemTime::now())?;
        }
        if !qemu_is_running(dir, pid) {
            return Ok(());
        }
        let idle = lock.metadata()?.modified()?.elapsed().unwrap_or_default();
        if idle >= IDLE_TIMEOUT {
            eprintln!("no run has used qemu (pid {pid}) for {}", human::seconds(idle.as_secs()));
            // Holding the lock until qemu is gone, so that no run starts using it.
            return stop_qemu(dir, pid);
        }
        lock.unlock()?;
    }
}

/// Whether a watchdog is watching the VM in `dir`.
fn is_watched(dir: &Path) -> Result<bool> {
    Ok(!try_lock(&File::create(dir.join("watchdog.lock"))?)?)
}

/// Starts a watchdog (see [`watch`]) for the VM in `dir`, whose qemu is
/// `pid`: this program, in a session of its own so that it outlives the test
/// run and a Ctrl-C of it.
fn start_watchdog(dir: &Path, pid: u32) -> Result<()> {
    let log = File::options().create(true).append(true).open(dir.join("watchdog.log"))?;
    // Rather than current_exe(), which is gone if a build has replaced this
    // program since it started.
    let mut command = Command::new("/proc/self/exe");
    command
        .arg(WATCHDOG_ARG)
        .arg(dir)
        .arg(pid.to_string())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe, as pre_exec requires.
    unsafe { command.pre_exec(|| if libc::setsid() == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }) };
    command.spawn().context("failed to start the VM's watchdog")?;
    Ok(())
}

/// If [`start_watchdog`] started this process, watches the VM and exits.
pub fn be_watchdog_if_started_as_one() {
    let args: Vec<OsString> = env::args_os().collect();
    let [_, arg, dir, pid] = &args[..] else { return };
    if arg != WATCHDOG_ARG {
        return;
    }
    let pid = pid.to_string_lossy().parse().expect("the watchdog's pid isn't a number");
    if let Err(error) = watch(Path::new(dir), pid) {
        eprintln!("{error:#}");
        process::exit(1);
    }
    process::exit(0);
}

/// Starts qemu (see [`start_qemu`]) for `manifest` from `bundle` in `dir`,
/// on `ports`, remembers it in state.json, and starts its watchdog.
fn launch(dir: &Path, bundle: &Path, manifest: &Manifest, ports: [u16; 2]) -> Result<State> {
    let state = start_qemu(dir, bundle, manifest, ports)?;
    // First, so that if the watchdog doesn't start, the next run finds the
    // VM and tries again.
    fs::write(dir.join("state.json"), serde_json::to_vec_pretty(&state)?)?;
    start_watchdog(dir, state.pid)?;
    Ok(state)
}

/// Writes the client key, known_hosts and an ssh config that uses them (and
/// nothing from the user's own ssh setup), returning the config's path.
fn write_ssh_files(dir: &Path, manifest: &Manifest, state: &State) -> Result<PathBuf> {
    let key = dir.join("client_key");
    let _ = fs::remove_file(&key);
    fs::copy(&manifest.client_key, &key)?;
    // ssh refuses keys that others can read.
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600))?;

    let known_hosts = dir.join("known_hosts");
    let mut lines = format!("[127.0.0.1]:{} {}\n", state.ssh_port, manifest.host_key_pub);
    if let Some(initrd) = &manifest.initrd {
        lines.push_str(&format!("[127.0.0.1]:{} {}\n", state.initrd_ssh_port, initrd.host_key_pub));
    }
    fs::write(&known_hosts, lines)?;

    let config = dir.join("ssh_config");
    fs::write(
        &config,
        format!(
            "Host *\n  IdentityFile \"{}\"\n  IdentitiesOnly yes\n  IdentityAgent none\n  UserKnownHostsFile \"{}\"\n  \
             GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking yes\n  LogLevel ERROR\n",
            key.display(),
            known_hosts.display()
        ),
    )?;
    Ok(config)
}

/// Tests name everything they create "reboop-test-*".
const CLEAN_UP: &str = r#"
# Twice, for units that start others as they stop
systemctl stop 'reboop-test-*' 2>/dev/null
systemctl stop 'reboop-test-*' 2>/dev/null
systemctl reset-failed 'reboop-test-*' 2>/dev/null
for m in /mnt/reboop-test-*; do
    [ -d "$m" ] || continue
    btrfs scrub cancel "$m" >/dev/null 2>&1
    btrfs balance cancel "$m" >/dev/null 2>&1
    if mountpoint -q "$m"; then umount "$m" || exit 1; fi
    rmdir "$m"
done
# What the boot loader tests change
bootctl set-oneshot '' >/dev/null 2>&1
bootctl reboot-to-firmware false >/dev/null 2>&1
for f in grub.cfg kernel; do
    if [ -e /var/tmp/reboop-test-$f ]; then cp /var/tmp/reboop-test-$f "$(cat /var/tmp/reboop-test-$f-path)" || exit 1; fi
done
for i in /var/tmp/reboop-test-*.img; do
    [ -e "$i" ] || continue
    for l in $(losetup -j "$i" -n -O NAME); do losetup -d "$l"; done
done
rm -rf /var/tmp/reboop-test-*
btrfs scrub cancel / >/dev/null 2>&1
exit 0
"#;

/// The test VMs, as default.nix calls them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Name {
    /// systemd-boot from UEFI, and a LUKS root unlocked from the initrd
    SystemdBoot,
    /// GRUB from BIOS, mirrored onto two disks, and no LUKS
    Grub,
}

impl Name {
    pub fn as_str(self) -> &'static str {
        match self {
            Name::SystemdBoot => "systemd-boot",
            Name::Grub => "grub",
        }
    }
}

/// A running test VM.
pub struct Vm {
    pub ssh: Ssh,
    /// sshd on the booted system
    pub target: Target,
    /// sshd in the initrd
    pub initrd_target: Target,
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// The Nix build the VM runs from
    bundle: PathBuf,
    /// Its qemu's, which changes if it's restarted
    pid: AtomicU32,
    /// Held for as long as the tests run, so concurrent runs don't share the
    /// VM, and so its watchdog knows it's in use (see [`watch`]).
    _lock: File,
}

impl Vm {
    /// Builds, starts or reuses, and brings up the VM called `name`.
    pub fn get(name: Name) -> Result<Vm> {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("reboop-vm-{}", name.as_str()));
        fs::create_dir_all(&dir)?;
        let lock = File::create(dir.join("lock"))?;
        lock.lock().context("failed to lock the VM")?;

        let bundle = build(&dir, name.as_str())?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(bundle.join("manifest.json"))?)?;
        let state_path = dir.join("state.json");
        let old_state: Option<State> = fs::read(&state_path).ok().and_then(|json| serde_json::from_slice(&json).ok());
        let state = match old_state {
            Some(state) if state.bundle == bundle && qemu_is_running(&dir, state.pid) => {
                // In case its watchdog died.
                if !is_watched(&dir)? {
                    start_watchdog(&dir, state.pid)?;
                }
                state
            }
            old_state => {
                if let Some(old) = old_state {
                    stop_qemu(&dir, old.pid)?;
                }
                launch(&dir, &bundle, &manifest, free_ports()?)?
            }
        };
        // For the watchdog, which may not see a short run: see watch.
        lock.set_modified(SystemTime::now())?;

        let vm = Vm {
            ssh: Ssh { extra_args: vec!["-F".into(), write_ssh_files(&dir, &manifest, &state)?.display().to_string()] },
            target: Target { name: manifest.hostname.clone(), address: "127.0.0.1".into(), port: state.ssh_port },
            initrd_target: Target { name: manifest.hostname.clone(), address: "127.0.0.1".into(), port: state.initrd_ssh_port },
            manifest,
            dir,
            bundle,
            pid: AtomicU32::new(state.pid),
            _lock: lock,
        };
        vm.bring_up_or_stop()?;
        Ok(vm)
    }

    /// Whether the VM's qemu is running, which it stops doing when the guest
    /// powers off.
    pub fn is_running(&self) -> bool {
        qemu_is_running(&self.dir, self.pid.load(Ordering::Relaxed))
    }

    /// Starts the VM again, on the same ports, once its guest has powered
    /// off (so its qemu has exited), and brings it up.
    pub fn restart(&self) -> Result<()> {
        ensure!(!self.is_running(), "{}'s qemu is still running", self.manifest.hostname);
        let state = launch(&self.dir, &self.bundle, &self.manifest, [self.target.port, self.initrd_target.port])?;
        self.pid.store(state.pid, Ordering::Relaxed);
        self.bring_up_or_stop()
    }

    pub fn session(&self) -> Result<Session> {
        Session::open(&self.ssh, &self.target, Duration::from_secs(30))
    }

    /// The password of the VM's LUKS root, for tests of VMs that have one.
    pub fn luks_password(&self) -> Result<&str> {
        let initrd = self.manifest.initrd.as_ref().ok_or_else(|| anyhow!("{} has no LUKS", self.manifest.hostname))?;
        Ok(&initrd.luks_password)
    }

    /// Brings the VM up (see [`Vm::bring_up`]), stopping its qemu if that
    /// fails, so as not to leave a broken VM for the next run.
    fn bring_up_or_stop(&self) -> Result<()> {
        if let Err(error) = self.bring_up() {
            let _ = stop_qemu(&self.dir, self.pid.load(Ordering::Relaxed));
            let console = self.dir.join("console.log");
            return Err(error.context(format!("{} didn't come up; see {}", self.manifest.hostname, console.display())));
        }
        Ok(())
    }

    /// Waits for the VM to accept SSH connections, unlocking its disk if
    /// it's waiting for that.
    fn bring_up(&self) -> Result<()> {
        let deadline = Deadline::after(Duration::from_secs(300));
        loop {
            if Session::open(&self.ssh, &self.target, Duration::from_secs(10)).is_ok() {
                return Ok(());
            }
            if let Some(initrd) = &self.manifest.initrd {
                match initrd::unlock(&self.ssh, &self.initrd_target, &initrd.luks_password, deadline.at_most(Duration::from_secs(60))) {
                    Ok(_) | Err(UnlockError::Unreachable(_)) => {}
                    Err(UnlockError::Other(error)) if error.downcast_ref::<Permanent>().is_some() => return Err(error),
                    Err(error) => eprintln!("while bringing up the VM: {error}"),
                }
            }
            ensure!(!deadline.has_passed(), "timed out");
            sleep(Duration::from_secs(1));
        }
    }
}

/// Undoes whatever tests may have left behind in the VM (including tests
/// that were interrupted), so each test starts from the same state.
pub fn clean_up(session: &mut Session) -> Result<()> {
    session
        .run_ok(CLEAN_UP, Duration::from_secs(120))
        .map(drop)
        .map_err(|error| anyhow!("cleaning up the VM failed: {error:#}"))
}
