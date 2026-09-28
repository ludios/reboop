// Model-output: Claude Opus 5.5

//! Builds the test VM (see default.nix), starts it if it isn't already
//! running, and brings it up to where it accepts SSH connections.
//!
//! The VM outlives the test run, so that the next run doesn't have to boot
//! it; everything about it lives in target/tmp/reboop-vm.  It's replaced
//! whenever the Nix build produces something different.

use anyhow::{Context, Result, anyhow, bail, ensure};
use reboop::deadline::{Deadline, Permanent};
use reboop::initrd::{self, UnlockError};
use reboop::ssh::{Session, Ssh, Target};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

/// The VM's hostname, which is also what ssh calls it.
pub const HOSTNAME: &str = "reboop-test";

/// manifest.json from the Nix build
#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub disk_image: PathBuf,
    pub ovmf_code: PathBuf,
    pub ovmf_vars: PathBuf,
    pub qemu: PathBuf,
    pub client_key: PathBuf,
    pub host_key_pub: String,
    pub initrd_key_pub: String,
    pub luks_password: String,
    /// The two NixOS configurations; /etc/reboop-test-variant says which is
    /// which.
    pub systems: TestSystems,
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

/// Builds tests/vm with Nix, returning the result.
fn build(dir: &Path) -> Result<PathBuf> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vm");
    eprintln!("building the test VM with nix-build...");
    let output = Command::new("nix-build")
        .arg(&source)
        .arg("--out-link")
        .arg(dir.join("bundle"))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("failed to run nix-build")?;
    ensure!(output.status.success(), "nix-build {} failed", source.display());
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

fn start_qemu(dir: &Path, bundle: &Path, manifest: &Manifest) -> Result<State> {
    eprintln!("starting the test VM...");
    let vars = dir.join("OVMF_VARS.fd");
    let _ = fs::remove_file(&vars);
    fs::copy(&manifest.ovmf_vars, &vars)?;
    fs::set_permissions(&vars, fs::Permissions::from_mode(0o644))?;

    // Bind two free ports at once, so they differ, then free them for qemu.
    let listeners = [TcpListener::bind("127.0.0.1:0")?, TcpListener::bind("127.0.0.1:0")?];
    let [ssh_port, initrd_ssh_port] = listeners.map(|listener| listener.local_addr().unwrap().port());

    let pidfile = dir.join("qemu.pid");
    let _ = fs::remove_file(&pidfile);
    let arg = |prefix: &str, path: &Path| format!("{prefix}{}", path.display());
    let output = Command::new(&manifest.qemu)
        .args(["-name", "reboop-test", "-machine", "q35,accel=kvm", "-cpu", "host", "-smp", "2", "-m", "2048"])
        .args(["-drive", &arg("if=pflash,format=raw,unit=0,readonly=on,file=", &manifest.ovmf_code)])
        .args(["-drive", &arg("if=pflash,format=raw,unit=1,file=", &vars)])
        // snapshot=on: writes go to a temporary file that's gone when qemu
        // exits, so each VM starts from the pristine image.
        .args(["-drive", &arg("if=virtio,format=qcow2,cache=unsafe,snapshot=on,file=", &manifest.disk_image)])
        .args(["-device", "virtio-rng-pci"])
        .args(["-nic", &format!("user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{ssh_port}-:904,hostfwd=tcp:127.0.0.1:{initrd_ssh_port}-:23")])
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

/// Whether `pid` is our qemu (and not some process that reused its pid).
fn qemu_is_running(dir: &Path, pid: u32) -> bool {
    let vars = dir.join("OVMF_VARS.fd").display().to_string();
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(&vars))
}

fn stop_qemu(dir: &Path, pid: u32) -> Result<()> {
    if !qemu_is_running(dir, pid) {
        return Ok(());
    }
    eprintln!("stopping the old test VM...");
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

/// Writes the client key, known_hosts and an ssh config that uses them (and
/// nothing from the user's own ssh setup), returning the config's path.
fn write_ssh_files(dir: &Path, manifest: &Manifest, state: &State) -> Result<PathBuf> {
    let key = dir.join("client_key");
    let _ = fs::remove_file(&key);
    fs::copy(&manifest.client_key, &key)?;
    // ssh refuses keys that others can read.
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600))?;

    let known_hosts = dir.join("known_hosts");
    fs::write(
        &known_hosts,
        format!(
            "[127.0.0.1]:{} {}\n[127.0.0.1]:{} {}\n",
            state.ssh_port, manifest.host_key_pub, state.initrd_ssh_port, manifest.initrd_key_pub
        ),
    )?;

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
systemctl stop 'reboop-test-*' 2>/dev/null
systemctl reset-failed 'reboop-test-*' 2>/dev/null
for m in /mnt/reboop-test-*; do
    [ -d "$m" ] || continue
    btrfs scrub cancel "$m" >/dev/null 2>&1
    btrfs balance cancel "$m" >/dev/null 2>&1
    if mountpoint -q "$m"; then umount "$m" || exit 1; fi
    rmdir "$m"
done
for i in /var/tmp/reboop-test-*.img; do
    [ -e "$i" ] || continue
    for l in $(losetup -j "$i" -n -O NAME); do losetup -d "$l"; done
done
rm -rf /var/tmp/reboop-test-*
btrfs scrub cancel / >/dev/null 2>&1
exit 0
"#;

/// The running test VM.
pub struct Vm {
    pub ssh: Ssh,
    /// sshd on the booted system
    pub target: Target,
    /// sshd in the initrd
    pub initrd_target: Target,
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// Held for as long as the tests run, so concurrent runs don't share the VM.
    _lock: File,
}

impl Vm {
    /// Builds, starts or reuses, and brings up the VM.
    pub fn get() -> Result<Vm> {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("reboop-vm");
        fs::create_dir_all(&dir)?;
        let lock = File::create(dir.join("lock"))?;
        lock.lock().context("failed to lock the VM")?;

        let bundle = build(&dir)?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(bundle.join("manifest.json"))?)?;
        let state_path = dir.join("state.json");
        let old_state: Option<State> = fs::read(&state_path).ok().and_then(|json| serde_json::from_slice(&json).ok());
        let state = match old_state {
            Some(state) if state.bundle == bundle && qemu_is_running(&dir, state.pid) => state,
            old_state => {
                if let Some(old) = old_state {
                    stop_qemu(&dir, old.pid)?;
                }
                let state = start_qemu(&dir, &bundle, &manifest)?;
                fs::write(&state_path, serde_json::to_vec_pretty(&state)?)?;
                state
            }
        };

        let vm = Vm {
            ssh: Ssh { extra_args: vec!["-F".into(), write_ssh_files(&dir, &manifest, &state)?.display().to_string()] },
            target: Target { name: HOSTNAME.into(), address: "127.0.0.1".into(), port: state.ssh_port },
            initrd_target: Target { name: HOSTNAME.into(), address: "127.0.0.1".into(), port: state.initrd_ssh_port },
            manifest,
            dir,
            _lock: lock,
        };
        if let Err(error) = vm.bring_up() {
            // Don't leave a broken VM for the next run.
            let _ = stop_qemu(&vm.dir, state.pid);
            return Err(error.context(format!("the VM didn't come up; see {}", vm.dir.join("console.log").display())));
        }
        Ok(vm)
    }

    pub fn session(&self) -> Result<Session> {
        Session::open(&self.ssh, &self.target, Duration::from_secs(30))
    }

    /// Waits for the VM to accept SSH connections, unlocking its disk if
    /// it's waiting for that.
    fn bring_up(&self) -> Result<()> {
        let deadline = Deadline::after(Duration::from_secs(300));
        loop {
            if Session::open(&self.ssh, &self.target, Duration::from_secs(10)).is_ok() {
                return Ok(());
            }
            match initrd::unlock(&self.ssh, &self.initrd_target, &self.manifest.luks_password, deadline.at_most(Duration::from_secs(60))) {
                Ok(_) | Err(UnlockError::Unreachable(_)) => {}
                Err(UnlockError::Other(error)) if error.downcast_ref::<Permanent>().is_some() => return Err(error),
                Err(error) => eprintln!("while bringing up the VM: {error}"),
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
