// Model-output: Claude Opus 5.5
// Model-output: Claude Fable 5.1

//! Facts about a remote NixOS machine, from before and after rebooting it.

use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Context, Result, anyhow, bail, ensure};
use jiff::Timestamp;
use jiff::fmt::strtime;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::Duration;

pub fn hostname(session: &mut Session) -> Result<String> {
    Ok(session.run_ok("cat /proc/sys/kernel/hostname", QUICK)?.trim_end().to_string())
}

/// A random ID the kernel picks at every boot, so if it changed, the machine
/// rebooted.
pub fn boot_id(session: &mut Session) -> Result<String> {
    let id = session.run_ok("cat /proc/sys/kernel/random/boot_id", QUICK)?.trim_end().to_string();
    ensure!(id.len() == 36, "unexpected boot_id {id:?}");
    Ok(id)
}

/// Parses /proc/stat's "btime SECONDS" line.
fn parse_btime(line: &str) -> Result<Timestamp> {
    let context = || format!("unexpected btime line {line:?}");
    let seconds = line.trim_end().strip_prefix("btime ").ok_or_else(|| anyhow!(context()))?;
    Timestamp::from_second(seconds.parse().with_context(context)?).with_context(context)
}

/// When the machine booted, by its clock as it is now.
pub fn booted_at(session: &mut Session) -> Result<Timestamp> {
    parse_btime(&session.run_ok("grep '^btime ' /proc/stat", QUICK)?)
}

pub fn load_average_1min(session: &mut Session) -> Result<f64> {
    let loadavg = session.run_ok("cat /proc/loadavg", QUICK)?;
    let first = loadavg.split_whitespace().next().unwrap_or_default();
    first.parse().with_context(|| format!("unexpected /proc/loadavg {loadavg:?}"))
}

/// Parses a path in the store printed by a command, returning its store
/// entry: the /nix/store/HASH-NAME directory, without any path beneath it.
fn store_entry(output: &str) -> Result<String> {
    let path = output.trim_end_matches('\n');
    let context = || anyhow!("not a path in the store: {path:?}");
    let rest = path.strip_prefix("/nix/store/").ok_or_else(context)?;
    let entry = rest.split_once('/').map_or(rest, |(entry, _)| entry);
    let (hash, name) = entry.split_at_checked(32).and_then(|(hash, rest)| Some((hash, rest.strip_prefix('-')?))).ok_or_else(context)?;
    ensure!(hash.bytes().all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase()) && !name.is_empty(), context());
    Ok(format!("/nix/store/{entry}"))
}

/// Parses a store path printed by a command, checking that it is one: a
/// store entry (see [`store_entry`]) with nothing beneath it.
fn store_path(output: &str) -> Result<String> {
    let path = output.trim_end_matches('\n');
    let entry = store_entry(path)?;
    ensure!(entry == path, "not a store path: {path:?}");
    Ok(entry)
}

/// The name of a store entry from [`store_entry`]: what follows its hash,
/// like "linux-6.18.55", which usually carries the version.
fn store_name(entry: &str) -> &str {
    entry.split_once('-').map_or(entry, |(_, name)| name)
}

/// The release of `system`'s kernel (a toplevel store path), as `uname -r`
/// would show it once booted: the name of its modules directory.
pub fn kernel_release(session: &mut Session, system: &str) -> Result<String> {
    let script = format!("ls -1 {}/kernel-modules/lib/modules", shell_quote(system));
    let listing = session.run_ok(&script, QUICK)?;
    let releases: Vec<_> = listing.lines().collect();
    let [release] = releases[..] else {
        bail!("expected one kernel release for {system}, found {releases:?}");
    };
    Ok(release.to_string())
}

/// When a kernel was built, from the end of its `uname -v`, like "#1-NixOS
/// SMP PREEMPT_DYNAMIC Fri Sep 25 14:35:54 UTC 2026".  None if that's not a
/// UTC time, as `date` prints it, with the right weekday.
fn parse_kernel_built_at(version: &str) -> Option<Timestamp> {
    let words: Vec<&str> = version.split_whitespace().collect();
    let time = words[words.len().checked_sub(6)?..].join(" ");
    let datetime = strtime::parse("%a %b %e %H:%M:%S UTC %Y", time).ok()?.to_datetime().ok()?;
    TimeZone::UTC.to_timestamp(datetime).ok()
}

/// The parts of a NixOS configuration that only a reboot puts into use, as
/// store entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BootParts {
    pub kernel: String,
    pub kernel_modules: String,
    /// None if the configuration has no initrd (boot.initrd.enable off).
    pub initrd: Option<String>,
    pub systemd: String,
}

/// The parts of `system` (a toplevel store path) that only a reboot puts
/// into use: what its kernel, kernel-modules, systemd and initrd links lead
/// to.  A configuration with boot.initrd.enable off has no initrd link.
fn boot_parts(session: &mut Session, system: &str) -> Result<BootParts> {
    let quoted = shell_quote(system);
    let script = format!(
        "readlink -v -- {quoted}/kernel {quoted}/kernel-modules {quoted}/systemd && if test -L {quoted}/initrd; then readlink -v -- {quoted}/initrd; fi"
    );
    let mut entries: Vec<String> = session.run_ok(&script, QUICK)?.lines().map(store_entry).collect::<Result<_>>()?;
    let initrd = if entries.len() == 4 { entries.pop() } else { None };
    let [kernel, kernel_modules, systemd] =
        <[String; 3]>::try_from(entries).map_err(|entries| anyhow!("expected 3 or 4 links in {system}, found {entries:?}"))?;
    Ok(BootParts { kernel, kernel_modules, initrd, systemd })
}

/// The NixOS configurations and kernels involved in a reboot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Systems {
    /// The configuration that's active now (it changes on switch).
    pub current: String,
    /// The configuration the machine booted into.
    pub booted: String,
    /// The release of the running kernel, as in `uname -r`.
    pub running_kernel: String,
    /// When the running kernel was built, if known: see
    /// [`parse_kernel_built_at`].
    pub running_kernel_built_at: Option<Timestamp>,
    /// The configuration of the system profile, which is what
    /// `nixos-rebuild boot` or `switch` makes the default boot entry.
    pub default: String,
    /// The release of the default configuration's kernel.
    pub default_kernel: String,
    /// The parts in use: the booted configuration's kernel, kernel modules
    /// and initrd, and the systemd that PID 1 runs, which switching
    /// configurations re-executes.
    pub running_parts: BootParts,
    /// The default configuration's parts.
    pub default_parts: BootParts,
}

impl Systems {
    /// Why the machine needs a reboot, if it does: "new system" if the
    /// default configuration isn't the current one (after `nixos-rebuild
    /// boot`); then, for each of the parts in use that the default
    /// configuration's differs from, "new X" if the default's has another
    /// name (so, usually, another version), or "rebuilt X" if only its hash
    /// differs.  A kernel's modules and initrd come with it, so they're
    /// only named when the kernel is the same, and the initrd only when the
    /// modules are too.
    pub fn reboot_reasons(&self) -> Vec<String> {
        let differs = |part: &str, of: fn(&BootParts) -> Option<&str>| match (of(&self.running_parts), of(&self.default_parts)) {
            (running, default) if running == default => None,
            (Some(running), Some(default)) if store_name(running) == store_name(default) => Some(format!("rebuilt {part}")),
            _ => Some(format!("new {part}")),
        };
        let system = (self.current != self.default).then(|| "new system".to_string());
        let kernel = differs("kernel", |parts| Some(parts.kernel.as_str()))
            .or_else(|| differs("kernel modules", |parts| Some(parts.kernel_modules.as_str())))
            .or_else(|| differs("initrd", |parts| parts.initrd.as_deref()));
        let systemd = differs("systemd", |parts| Some(parts.systemd.as_str()));
        [system, kernel, systemd].into_iter().flatten().collect()
    }
}

/// Finds out which configurations and kernels the machine is running and
/// would boot, and their parts that only a reboot puts into use.
pub fn systems(session: &mut Session) -> Result<Systems> {
    let current = store_path(&session.run_ok("readlink /run/current-system", QUICK)?)?;
    let booted = store_path(&session.run_ok("readlink /run/booted-system", QUICK)?)?;
    let running_kernel = session.run_ok("uname -r", QUICK)?.trim_end().to_string();
    let running_kernel_built_at = parse_kernel_built_at(&session.run_ok("uname -v", QUICK)?);
    let default = store_path(&session.run_ok("readlink -f /nix/var/nix/profiles/system", QUICK)?)?;
    let default_kernel = kernel_release(session, &default)?;
    let booted_parts = boot_parts(session, &booted)?;
    let default_parts = if default == booted { booted_parts.clone() } else { boot_parts(session, &default)? };
    let systemd = store_entry(&session.run_ok("readlink -v /proc/1/exe", QUICK)?)?;
    let running_parts = BootParts { systemd, ..booted_parts };
    Ok(Systems { current, booted, running_kernel, running_kernel_built_at, default, default_kernel, running_parts, default_parts })
}

/// Seconds since boot and each interface's (received, sent) byte counters.
type NetSnapshot = (f64, BTreeMap<String, (u64, u64)>);

/// Parses /proc/uptime followed by /proc/net/dev.
fn parse_net_snapshot(output: &str) -> Result<NetSnapshot> {
    let context = || anyhow!("unexpected /proc/uptime and /proc/net/dev: {output:?}");
    let mut lines = output.lines();
    let uptime = lines.next().and_then(|line| line.split_whitespace().next()).ok_or_else(context)?;
    let uptime = uptime.parse().with_context(context)?;
    let mut counters = BTreeMap::new();
    // Lines after the two header lines look like "  eth0: RX_BYTES ... TX_BYTES ...",
    // with 8 receive fields then 8 transmit fields.
    for line in lines.skip(2) {
        let (name, fields) = line.split_once(':').ok_or_else(context)?;
        let fields: Vec<u64> = fields.split_whitespace().map(str::parse).collect::<Result<_, _>>().with_context(context)?;
        ensure!(fields.len() == 16, context());
        counters.insert(name.trim().to_string(), (fields[0], fields[8]));
    }
    Ok((uptime, counters))
}

/// Traffic through each network interface during a sampling period.
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkSample {
    pub seconds: f64,
    /// Bytes (received, sent) by each interface, except loopback.
    pub interfaces: BTreeMap<String, (u64, u64)>,
}

impl NetworkSample {
    /// Bytes received plus sent per second, over all interfaces.  (Traffic
    /// through both a VPN and the interface under it counts twice.)
    pub fn bytes_per_sec(&self) -> f64 {
        let total: u64 = self.interfaces.values().map(|(received, sent)| received + sent).sum();
        total as f64 / self.seconds
    }
}

/// The traffic between two snapshots.
fn network_sample(before: &NetSnapshot, after: &NetSnapshot) -> Result<NetworkSample> {
    let seconds = after.0 - before.0;
    ensure!(seconds > 0.0, "uptime didn't advance while sampling the network");
    let mut interfaces = BTreeMap::new();
    for (name, (received, sent)) in &after.1 {
        // Interfaces that came or went during the sample are ignored, as are
        // those whose byte counters went backwards: they were recreated.
        let Some((received_before, sent_before)) = before.1.get(name) else { continue };
        if name == "lo" {
            continue;
        }
        let Some(deltas) = received.checked_sub(*received_before).zip(sent.checked_sub(*sent_before)) else { continue };
        interfaces.insert(name.clone(), deltas);
    }
    Ok(NetworkSample { seconds, interfaces })
}

/// Measures network traffic on the remote machine over `duration`.
pub fn sample_network(session: &mut Session, duration: Duration) -> Result<NetworkSample> {
    let script = "cat /proc/uptime /proc/net/dev";
    let before = parse_net_snapshot(&session.run_ok(script, QUICK)?)?;
    sleep(duration);
    let after = parse_net_snapshot(&session.run_ok(script, QUICK)?)?;
    network_sample(&before, &after)
}

/// The state that `systemctl is-system-running` prints, running it with
/// `options`, for up to `timeout`.
fn is_system_running(session: &mut Session, options: &str, timeout: Duration) -> Result<String> {
    let output = session.run(&format!("systemctl is-system-running {options}"), timeout)?;
    let state = output.stdout_text().trim().to_string();
    ensure!(
        !state.is_empty() && !state.contains(char::is_whitespace),
        "unexpected output from systemctl is-system-running: {:?} {:?}",
        state,
        output.stderr_text()
    );
    Ok(state)
}

/// systemd's view of the system: "running" if all is well, "degraded" if
/// some unit failed, "stopping" if it's shutting down, or something else
/// from systemctl(1)'s is-system-running.
pub fn system_state(session: &mut Session) -> Result<String> {
    is_system_running(session, "", QUICK)
}

/// Waits for the machine to finish booting (or `timeout`) and returns its
/// [`system_state`].
pub fn wait_until_booted(session: &mut Session, timeout: Duration) -> Result<String> {
    is_system_running(session, "--wait", timeout)
}

/// The names of units that have failed.
pub fn failed_units(session: &mut Session) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Unit {
        unit: String,
    }
    let json = session.run_ok("systemctl list-units --failed --no-pager --output=json", QUICK)?;
    let units: Vec<Unit> = serde_json::from_str(&json).with_context(|| format!("unexpected systemctl output {json:?}"))?;
    Ok(units.into_iter().map(|unit| unit.unit).collect())
}

/// Kernel log messages at the error level and above.
pub fn kernel_errors(session: &mut Session) -> Result<String> {
    session.run_ok("dmesg --level=err,crit,alert,emerg", QUICK)
}

/// Parses `df --output=pcent`: a header, then e.g. " 45%".
fn parse_df_percent(output: &str) -> Result<u8> {
    let context = || format!("unexpected df output {output:?}");
    let [_, line] = output.lines().collect::<Vec<_>>()[..] else { bail!(context()) };
    let percent = line.trim().strip_suffix('%').ok_or_else(|| anyhow!(context()))?;
    let percent: u16 = percent.parse().with_context(context)?;
    // Over 100% when a filesystem reports negative space available
    Ok(percent.min(100) as u8)
}

/// How much of the filesystem at / is used, as a percentage rounded up, the
/// way df(1) shows it.
pub fn root_used_percent(session: &mut Session) -> Result<u8> {
    parse_df_percent(&session.run_ok("df --output=pcent /", QUICK)?)
}

/// A job that systemd has queued or is running, like starting a unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Job {
    pub id: u64,
    pub unit: String,
    /// e.g. "start", "stop" or "restart"
    #[serde(rename = "type")]
    pub job_type: String,
    /// "waiting" or "running"
    pub state: String,
}

/// Parses `systemctl list-jobs --no-legend`: a line of "ID UNIT TYPE STATE"
/// per job.  (Unit names can't contain spaces.)
fn parse_jobs(output: &str) -> Result<Vec<Job>> {
    output
        .lines()
        .map(|line| {
            let [id, unit, job_type, state] = line.split_whitespace().collect::<Vec<_>>()[..] else {
                bail!("unexpected systemctl list-jobs line {line:?}");
            };
            let id = id.parse().with_context(|| format!("unexpected systemctl list-jobs line {line:?}"))?;
            Ok(Job { id, unit: unit.into(), job_type: job_type.into(), state: state.into() })
        })
        .collect()
}

/// systemd's jobs.
pub fn jobs(session: &mut Session) -> Result<Vec<Job>> {
    parse_jobs(&session.run_ok("systemctl list-jobs --no-legend --no-pager", QUICK)?)
}

/// A lock that a program holds to delay or block shutdown, sleep, etc.: see
/// systemd-inhibit(1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Inhibitor {
    /// What it inhibits, e.g. "shutdown:sleep"
    pub what: String,
    pub who: String,
    pub why: String,
    /// "block", "block-weak" (which privileged users can override), or
    /// "delay"
    pub mode: String,
    pub pid: u32,
    pub uid: u32,
    /// The user name, or the numeric uid if the user has no name.
    pub user: String,
}

impl Inhibitor {
    /// Whether it asks for the machine not to shut down or reboot.  That
    /// includes block-weak locks, which root could override.
    pub fn blocks_shutdown(&self) -> bool {
        self.what.split(':').any(|what| what == "shutdown") && (self.mode == "block" || self.mode == "block-weak")
    }
}

/// Parses logind's ListInhibitors reply, as `busctl --json=short` prints it,
/// into inhibitors whose `user` is their numeric uid.
fn parse_inhibitors(json: &str) -> Result<Vec<Inhibitor>> {
    /// (what, who, why, mode, uid, pid)
    type Lock = (String, String, String, String, u32, u32);
    #[derive(Deserialize)]
    struct Reply {
        #[serde(rename = "type")]
        signature: String,
        /// The one return value: all the locks
        data: (Vec<Lock>,),
    }
    let context = || format!("unexpected ListInhibitors reply {json:?}");
    let reply: Reply = serde_json::from_str(json).with_context(context)?;
    ensure!(reply.signature == "a(ssssuu)", context());
    let (locks,) = reply.data;
    let inhibitor = |(what, who, why, mode, uid, pid): Lock| Inhibitor {
        what,
        who,
        why,
        mode,
        pid,
        uid,
        user: uid.to_string(),
    };
    Ok(locks.into_iter().map(inhibitor).collect())
}

/// The inhibitor locks held on the machine.  (Straight from logind, since
/// `systemd-inhibit --list` has no JSON before systemd 260.)
pub fn inhibitors(session: &mut Session) -> Result<Vec<Inhibitor>> {
    let call = "busctl --json=short call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager ListInhibitors";
    let mut inhibitors = parse_inhibitors(&session.run_ok(call, QUICK)?)?;
    for inhibitor in &mut inhibitors {
        let output = session.run(&format!("id -nu -- {}", inhibitor.uid), QUICK)?;
        if output.status == 0 {
            inhibitor.user = output.stdout_text().trim_end().to_string();
        }
    }
    Ok(inhibitors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::idle_facts;

    #[test]
    fn parses_jobs() {
        let output = "361 reboop-test-job.service start running\n362 nginx.service   stop  waiting\n";
        let job = |id, unit: &str, job_type: &str, state: &str| Job { id, unit: unit.into(), job_type: job_type.into(), state: state.into() };
        assert_eq!(parse_jobs(output).unwrap(), [job(361, "reboop-test-job.service", "start", "running"), job(362, "nginx.service", "stop", "waiting")]);
        assert_eq!(parse_jobs("").unwrap(), []);
        assert!(parse_jobs("No jobs running.").is_err());
    }

    #[test]
    fn parses_df() {
        assert_eq!(parse_df_percent("Use%\n 45%\n").unwrap(), 45);
        assert_eq!(parse_df_percent("Use%\n100%\n").unwrap(), 100);
        assert!(parse_df_percent("Use%\n  -\n").is_err());
        assert!(parse_df_percent("Use%\n 45%\n 46%\n").is_err());
        assert_eq!(parse_df_percent("Use%\n103%\n").unwrap(), 100);
    }

    #[test]
    fn parses_inhibitors() {
        assert_eq!(parse_inhibitors(r#"{"type":"a(ssssuu)","data":[[]]}"#).unwrap(), []);
        let json = r#"{"type":"a(ssssuu)","data":[[["shutdown","delayer","d","delay",0,937],
                                                    ["sleep:shutdown","crawl","archiving","block",1000,932],
                                                    ["shutdown","weak","w","block-weak",0,934],
                                                    ["handle-power-key","xfce4-power-manager","","block",1000,2000]]]}"#;
        let inhibitors = parse_inhibitors(json).unwrap();
        let crawl = Inhibitor {
            what: "sleep:shutdown".into(),
            who: "crawl".into(),
            why: "archiving".into(),
            mode: "block".into(),
            pid: 932,
            uid: 1000,
            user: "1000".into(),
        };
        assert_eq!(inhibitors[1], crawl);
        assert_eq!(inhibitors.iter().map(Inhibitor::blocks_shutdown).collect::<Vec<_>>(), [false, true, true, false]);
        assert!(parse_inhibitors(r#"{"type":"a(ss)","data":[[]]}"#).is_err());
        assert!(parse_inhibitors("").is_err());
    }

    const NET_DEV_HEADER: &str = "Inter-|   Receive                                                |  Transmit\n \
         face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n";

    fn snapshot(uptime: &str, eth0: (u64, u64)) -> String {
        format!(
            "{uptime} 1234.00\n{NET_DEV_HEADER}    lo: 9999 10 0 0 0 0 0 0 9999 10 0 0 0 0 0 0\n  eth0: {} 5 0 0 0 0 0 0 {} 7 0 0 0 0 0 0\n",
            eth0.0, eth0.1
        )
    }

    #[test]
    fn samples_network() {
        let before = parse_net_snapshot(&snapshot("100.50", (1_000, 2_000))).unwrap();
        let after = parse_net_snapshot(&snapshot("102.50", (5_001_000, 1_002_000))).unwrap();
        let sample = network_sample(&before, &after).unwrap();
        assert_eq!(sample.interfaces, BTreeMap::from([("eth0".to_string(), (5_000_000, 1_000_000))]));
        assert_eq!(sample.bytes_per_sec(), 3_000_000.0);
    }

    #[test]
    fn ignores_recreated_interfaces() {
        let before = parse_net_snapshot(&snapshot("100", (1_000, 2_000))).unwrap();
        let after = parse_net_snapshot(&snapshot("101", (500, 2_000))).unwrap();
        assert_eq!(network_sample(&before, &after).unwrap().interfaces, BTreeMap::new());
    }

    #[test]
    fn store_paths() {
        let foo = format!("/nix/store/{}-foo", "a".repeat(32));
        assert_eq!(store_path(&format!("{foo}\n")).unwrap(), foo);
        assert!(store_path(&format!("{foo}/bin")).is_err());
        assert!(store_path("/run/current-system").is_err());
        assert!(store_path("/nix/store/abc-foo").is_err());
        assert_eq!(store_entry(&format!("{foo}/bin/sh\n")).unwrap(), foo);
        assert_eq!(store_entry(&format!("{foo}/lib/systemd/systemd (deleted)")).unwrap(), foo);
        assert!(store_entry(&format!("/nix/store/{}-", "a".repeat(32))).is_err());
        assert!(store_entry(&format!("/nix/store/{}foo", "a".repeat(32))).is_err());
        assert!(store_entry("/nix/store/").is_err());
        assert_eq!(store_name(&foo), "foo");
        assert_eq!(store_name("/nix/store/aaa-linux-6.18.55"), "linux-6.18.55");
    }

    #[test]
    fn finds_reboot_reasons() {
        let parts = |hash: &str, kernel: &str, systemd: &str| BootParts {
            kernel: format!("/nix/store/{hash}-linux-{kernel}"),
            kernel_modules: format!("/nix/store/{hash}-linux-{kernel}-modules"),
            initrd: Some(format!("/nix/store/{hash}-initrd-linux-{kernel}")),
            systemd: format!("/nix/store/{hash}-systemd-{systemd}"),
        };
        let same = Systems { running_parts: parts("aaa", "6.18.54", "260.5"), default_parts: parts("aaa", "6.18.54", "260.5"), ..idle_facts().systems };
        assert_eq!(same.reboot_reasons(), Vec::<String>::new());
        // After `nixos-rebuild boot` to a configuration with a newer kernel
        let newer = BootParts { systemd: same.running_parts.systemd.clone(), ..parts("bbb", "6.18.55", "260.5") };
        let booted = Systems { default: "/nix/store/bbb-nixos-system-one-26.05".into(), default_parts: newer, ..same.clone() };
        assert_eq!(booted.reboot_reasons(), ["new system", "new kernel"]);
        // After `nixos-rebuild switch` to a rebuilt kernel and a newer systemd
        let switched = Systems { current: booted.default.clone(), default: booted.default.clone(), default_parts: parts("bbb", "6.18.54", "260.6"), ..same.clone() };
        assert_eq!(switched.reboot_reasons(), ["rebuilt kernel", "new systemd"]);
        // Extra modules, which the initrd picks up; a changed initrd; a rebuilt systemd
        let rebuilt = parts("bbb", "6.18.54", "260.5");
        let modules = Systems { default_parts: BootParts { kernel_modules: rebuilt.kernel_modules.clone(), initrd: rebuilt.initrd.clone(), ..same.running_parts.clone() }, ..same.clone() };
        assert_eq!(modules.reboot_reasons(), ["rebuilt kernel modules"]);
        let initrd = Systems { default_parts: BootParts { initrd: rebuilt.initrd.clone(), ..same.running_parts.clone() }, ..same.clone() };
        assert_eq!(initrd.reboot_reasons(), ["rebuilt initrd"]);
        let no_initrd = Systems { default_parts: BootParts { initrd: None, ..same.running_parts.clone() }, ..same.clone() };
        assert_eq!(no_initrd.reboot_reasons(), ["new initrd"]);
        let systemd = Systems { default_parts: BootParts { systemd: rebuilt.systemd.clone(), ..same.running_parts.clone() }, ..same.clone() };
        assert_eq!(systemd.reboot_reasons(), ["rebuilt systemd"]);
    }

    #[test]
    fn parses_times() {
        let time = |text: &str| text.parse::<Timestamp>().unwrap();
        assert_eq!(parse_btime("btime 1790756876\n").unwrap(), time("2026-09-30T08:27:56Z"));
        assert!(parse_btime("btime\n").is_err());
        assert!(parse_btime("ctxt 1790756876\n").is_err());
        assert_eq!(parse_kernel_built_at("#1-NixOS SMP PREEMPT_DYNAMIC Fri Sep 25 14:35:54 UTC 2026\n"), Some(time("2026-09-25T14:35:54Z")));
        assert_eq!(parse_kernel_built_at("#1 SMP Sat Sep  5 04:05:06 UTC 2026"), Some(time("2026-09-05T04:05:06Z")));
        assert_eq!(parse_kernel_built_at("#1 SMP Fri Sep  5 04:05:06 UTC 2026"), None);
        assert_eq!(parse_kernel_built_at("#1 SMP Sat Sep  5 04:05:06 CEST 2026"), None);
        assert_eq!(parse_kernel_built_at("UTC 2026"), None);
        assert_eq!(parse_kernel_built_at(""), None);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_net_snapshot("").is_err());
        assert!(parse_net_snapshot(&format!("1.0 2.0\n{NET_DEV_HEADER}eth0 1 2 3\n")).is_err());
    }
}
