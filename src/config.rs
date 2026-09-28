// Model-output: Claude Opus 5.5

//! reboop's configuration, in ~/.config/reboop: defaults.json (JSON5, so
//! comments and trailing commas are fine) with settings for all machines,
//! and machines.jsonl with one JSON object per machine, whose settings take
//! precedence.

use crate::ssh::Target;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// Checks that `hostname` is a plain hostname, which also makes it safe to
/// use as a file name.
pub fn check_hostname(hostname: &str) -> Result<()> {
    let valid = !hostname.is_empty()
        && hostname.len() <= 253
        && !hostname.starts_with(['.', '-'])
        && hostname.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if !valid {
        bail!("{hostname:?} isn't a valid hostname");
    }
    Ok(())
}

/// A machine and all its settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub hostname: String,
    pub ipv4: Ipv4Addr,
    pub ssh_port: u16,
    /// The port of the initrd's sshd.
    pub initrd_ssh_port: u16,
    /// Mountpoints of the btrfs filesystems to scrub after rebooting.
    pub scrub_mounts: Vec<String>,
    pub max_network_transfer_bytes_per_sec: u64,
    pub max_load_average_1min: f64,
    /// The SSH key whose signature protects the machine's LUKS password: see
    /// [`crate::passwords::master_key`].
    pub luks_signing_key: PathBuf,
    /// systemd units to stop, in order, before rebooting, e.g. "postgresql";
    /// those the machine doesn't have are skipped.
    pub stop_services: Vec<String>,
}

impl Machine {
    pub fn target(&self) -> Target {
        Target { name: self.hostname.clone(), address: self.ipv4.to_string(), port: self.ssh_port }
    }

    pub fn initrd_target(&self) -> Target {
        Target { name: self.hostname.clone(), address: self.ipv4.to_string(), port: self.initrd_ssh_port }
    }
}

/// defaults.json
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    ssh_port: Option<u16>,
    initrd_ssh_port: Option<u16>,
    scrub_mounts: Option<Vec<String>>,
    max_network_transfer_bytes_per_sec: Option<u64>,
    max_load_average_1min: Option<f64>,
    luks_signing_key: Option<String>,
    stop_services: Option<Vec<String>>,
}

/// A line of machines.jsonl.  (serde can't combine `flatten` with
/// `deny_unknown_fields`, hence the repetition.)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineLine {
    hostname: String,
    ipv4: Ipv4Addr,
    ssh_port: Option<u16>,
    initrd_ssh_port: Option<u16>,
    scrub_mounts: Option<Vec<String>>,
    max_network_transfer_bytes_per_sec: Option<u64>,
    max_load_average_1min: Option<f64>,
    luks_signing_key: Option<String>,
    stop_services: Option<Vec<String>>,
}

/// The luks_signing_key of machines that aren't configured with one.
const DEFAULT_LUKS_SIGNING_KEY: &str = "~/.ssh/id_ed25519.pub";

/// Expands `path`'s leading "~/" (if any) to `home`, and checks that the
/// result is absolute.
fn expand_home(path: &str, home: &Path) -> Result<PathBuf> {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    };
    ensure!(expanded.is_absolute(), "{path:?} is neither an absolute path nor one starting with ~/");
    Ok(expanded)
}

/// Combines a line of machines.jsonl with the defaults, and checks the
/// result.  `home` is the user's home directory, for expanding "~/".
fn resolve(defaults: &Defaults, line: MachineLine, home: &Path) -> Result<Machine> {
    check_hostname(&line.hostname)?;
    let missing = |name: &str| anyhow!("{} has no {name}, and defaults.json doesn't either", line.hostname);
    let machine = Machine {
        ssh_port: line.ssh_port.or(defaults.ssh_port).ok_or_else(|| missing("ssh_port"))?,
        initrd_ssh_port: line.initrd_ssh_port.or(defaults.initrd_ssh_port).ok_or_else(|| missing("initrd_ssh_port"))?,
        scrub_mounts: line.scrub_mounts.clone().or(defaults.scrub_mounts.clone()).ok_or_else(|| missing("scrub_mounts"))?,
        max_network_transfer_bytes_per_sec: line
            .max_network_transfer_bytes_per_sec
            .or(defaults.max_network_transfer_bytes_per_sec)
            .ok_or_else(|| missing("max_network_transfer_bytes_per_sec"))?,
        max_load_average_1min: line
            .max_load_average_1min
            .or(defaults.max_load_average_1min)
            .ok_or_else(|| missing("max_load_average_1min"))?,
        luks_signing_key: expand_home(
            line.luks_signing_key.as_deref().or(defaults.luks_signing_key.as_deref()).unwrap_or(DEFAULT_LUKS_SIGNING_KEY),
            home,
        )
        .with_context(|| format!("bad luks_signing_key for {}", line.hostname))?,
        stop_services: line.stop_services.or(defaults.stop_services.clone()).unwrap_or_default(),
        hostname: line.hostname,
        ipv4: line.ipv4,
    };
    ensure!(machine.ssh_port != 0 && machine.initrd_ssh_port != 0, "port 0 for {}", machine.hostname);
    for mount in &machine.scrub_mounts {
        ensure!(mount.starts_with('/'), "scrub mount {mount:?} of {} isn't an absolute path", machine.hostname);
    }
    for service in &machine.stop_services {
        ensure!(
            !service.is_empty() && !service.contains(char::is_whitespace),
            "{service:?} in stop_services of {} isn't a unit name",
            machine.hostname
        );
    }
    ensure!(
        machine.max_load_average_1min.is_finite() && machine.max_load_average_1min >= 0.0,
        "bad max_load_average_1min for {}",
        machine.hostname
    );
    Ok(machine)
}

/// Parses the contents of defaults.json (if any) and machines.jsonl.  `home`
/// is the user's home directory, for expanding "~/".
pub fn parse(defaults: Option<&str>, machines: &str, home: &Path) -> Result<Vec<Machine>> {
    let defaults: Defaults = match defaults {
        Some(text) => json5::from_str(text).context("in defaults.json")?,
        None => Defaults::default(),
    };
    let mut hostnames = HashSet::new();
    let mut result = Vec::new();
    for (index, line) in machines.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let context = || format!("on line {} of machines.jsonl", index + 1);
        let line: MachineLine = serde_json::from_str(line).with_context(context)?;
        let machine = resolve(&defaults, line, home).with_context(context)?;
        ensure!(hostnames.insert(machine.hostname.clone()), "{} is configured twice", machine.hostname);
        result.push(machine);
    }
    Ok(result)
}

/// The machine called `hostname` among `machines`.
pub fn find<'a>(machines: &'a [Machine], hostname: &str) -> Result<&'a Machine> {
    machines.iter().find(|machine| machine.hostname == hostname).ok_or_else(|| anyhow!("{hostname:?} isn't in machines.jsonl"))
}

/// The user's home directory, from $HOME.
fn home_dir() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("$HOME isn't set"))?);
    ensure!(home.is_absolute(), "$HOME ({:?}) isn't an absolute path", home);
    Ok(home)
}

/// Reads the machines configured in `dir`.  defaults.json is optional.
pub fn load(dir: &Path) -> Result<Vec<Machine>> {
    let defaults_path = dir.join("defaults.json");
    let defaults = match fs::read_to_string(&defaults_path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).context(format!("failed to read {}", defaults_path.display())),
    };
    let machines_path = dir.join("machines.jsonl");
    let machines = fs::read_to_string(&machines_path).with_context(|| format!("failed to read {}", machines_path.display()))?;
    parse(defaults.as_deref(), &machines, &home_dir()?).with_context(|| format!("bad configuration in {}", dir.display()))
}

/// ~/.config/reboop, or its equivalent under $XDG_CONFIG_HOME.
pub fn config_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home_dir()?.join(".config"),
    };
    Ok(base.join("reboop"))
}

/// The luks directory in [`config_dir`], where machines' LUKS passwords are
/// kept: see [`crate::passwords`].
pub fn passwords_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("luks"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULTS: &str = r#"{
        // comments are fine
        "ssh_port": 904,
        "initrd_ssh_port": 23,
        "scrub_mounts": ["/"],
        "max_network_transfer_bytes_per_sec": 1000000,
        "max_load_average_1min": 2,
        "luks_signing_key": "~/keys/all.pub",
        "stop_services": ["postgresql"],
    }"#;

    const HOME: &str = "/home/user";

    #[test]
    fn machines_override_defaults() {
        let machines = "{\"hostname\": \"one\", \"ipv4\": \"10.0.0.1\", \"ssh_port\": 22}\n\n\
                        {\"hostname\": \"two\", \"ipv4\": \"10.0.0.2\", \"scrub_mounts\": [\"/\", \"/small\"], \"luks_signing_key\": \"/keys/two.pub\", \"stop_services\": []}\n";
        let machines = parse(Some(DEFAULTS), machines, Path::new(HOME)).unwrap();
        assert_eq!(
            machines[0],
            Machine {
                hostname: "one".into(),
                ipv4: Ipv4Addr::new(10, 0, 0, 1),
                ssh_port: 22,
                initrd_ssh_port: 23,
                scrub_mounts: vec!["/".into()],
                max_network_transfer_bytes_per_sec: 1_000_000,
                max_load_average_1min: 2.0,
                luks_signing_key: "/home/user/keys/all.pub".into(),
                stop_services: vec!["postgresql".into()],
            }
        );
        assert_eq!(machines[1].ssh_port, 904);
        assert_eq!(machines[1].scrub_mounts, ["/", "/small"]);
        assert_eq!(machines[1].luks_signing_key, Path::new("/keys/two.pub"));
        assert_eq!(machines[1].stop_services, Vec::<String>::new());
        assert_eq!(machines[1].target(), Target { name: "two".into(), address: "10.0.0.2".into(), port: 904 });
        assert_eq!(find(&machines, "two").unwrap(), &machines[1]);
        assert!(find(&machines, "three").is_err());
    }

    #[test]
    fn optional_settings_have_defaults() {
        let line = r#"{"hostname": "one", "ipv4": "10.0.0.1", "ssh_port": 22, "initrd_ssh_port": 23, "scrub_mounts": [], "max_network_transfer_bytes_per_sec": 1, "max_load_average_1min": 1}"#;
        let machines = parse(None, line, Path::new(HOME)).unwrap();
        assert_eq!(machines[0].luks_signing_key, Path::new("/home/user/.ssh/id_ed25519.pub"));
        assert_eq!(machines[0].stop_services, Vec::<String>::new());
    }

    #[test]
    fn rejects_mistakes() {
        let line = |json: &str| parse(Some(DEFAULTS), json, Path::new(HOME));
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.1", "ssh_prot": 22}"#).is_err());
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.256"}"#).is_err());
        assert!(line(r#"{"hostname": "../one", "ipv4": "10.0.0.1"}"#).is_err());
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.1", "scrub_mounts": ["small"]}"#).is_err());
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.1", "luks_signing_key": ".ssh/id_ed25519.pub"}"#).is_err());
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.1", "stop_services": [""]}"#).is_err());
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.1", "stop_services": ["postgresql nginx"]}"#).is_err());
        assert!(line("{\"hostname\": \"one\", \"ipv4\": \"10.0.0.1\"}\n{\"hostname\": \"one\", \"ipv4\": \"10.0.0.2\"}").is_err());
        assert!(parse(Some(r#"{"ssh_port": 904, "extra": 1}"#), "", Path::new(HOME)).is_err());
        let error = parse(None, r#"{"hostname": "one", "ipv4": "10.0.0.1"}"#, Path::new(HOME)).unwrap_err();
        assert!(format!("{error:#}").contains("no ssh_port"), "{error:#}");
    }
}
