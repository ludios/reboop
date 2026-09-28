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
}

/// Combines a line of machines.jsonl with the defaults, and checks the result.
fn resolve(defaults: &Defaults, line: MachineLine) -> Result<Machine> {
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
        hostname: line.hostname,
        ipv4: line.ipv4,
    };
    ensure!(machine.ssh_port != 0 && machine.initrd_ssh_port != 0, "port 0 for {}", machine.hostname);
    for mount in &machine.scrub_mounts {
        ensure!(mount.starts_with('/'), "scrub mount {mount:?} of {} isn't an absolute path", machine.hostname);
    }
    ensure!(
        machine.max_load_average_1min.is_finite() && machine.max_load_average_1min >= 0.0,
        "bad max_load_average_1min for {}",
        machine.hostname
    );
    Ok(machine)
}

/// Parses the contents of defaults.json (if any) and machines.jsonl.
pub fn parse(defaults: Option<&str>, machines: &str) -> Result<Vec<Machine>> {
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
        let machine = resolve(&defaults, line).with_context(context)?;
        ensure!(hostnames.insert(machine.hostname.clone()), "{} is configured twice", machine.hostname);
        result.push(machine);
    }
    Ok(result)
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
    parse(defaults.as_deref(), &machines).with_context(|| format!("bad configuration in {}", dir.display()))
}

/// ~/.config/reboop, or its equivalent under $XDG_CONFIG_HOME.
pub fn config_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("$HOME isn't set"))?).join(".config"),
    };
    Ok(base.join("reboop"))
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
    }"#;

    #[test]
    fn machines_override_defaults() {
        let machines = "{\"hostname\": \"one\", \"ipv4\": \"10.0.0.1\", \"ssh_port\": 22}\n\n\
                        {\"hostname\": \"two\", \"ipv4\": \"10.0.0.2\", \"scrub_mounts\": [\"/\", \"/small\"]}\n";
        let machines = parse(Some(DEFAULTS), machines).unwrap();
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
            }
        );
        assert_eq!(machines[1].ssh_port, 904);
        assert_eq!(machines[1].scrub_mounts, ["/", "/small"]);
        assert_eq!(machines[1].target(), Target { name: "two".into(), address: "10.0.0.2".into(), port: 904 });
    }

    #[test]
    fn rejects_mistakes() {
        let line = |json: &str| parse(Some(DEFAULTS), json);
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.1", "ssh_prot": 22}"#).is_err());
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.256"}"#).is_err());
        assert!(line(r#"{"hostname": "../one", "ipv4": "10.0.0.1"}"#).is_err());
        assert!(line(r#"{"hostname": "one", "ipv4": "10.0.0.1", "scrub_mounts": ["small"]}"#).is_err());
        assert!(line("{\"hostname\": \"one\", \"ipv4\": \"10.0.0.1\"}\n{\"hostname\": \"one\", \"ipv4\": \"10.0.0.2\"}").is_err());
        assert!(parse(Some(r#"{"ssh_port": 904, "extra": 1}"#), "").is_err());
        let error = parse(None, r#"{"hostname": "one", "ipv4": "10.0.0.1"}"#).unwrap_err();
        assert!(format!("{error:#}").contains("no ssh_port"), "{error:#}");
    }
}
