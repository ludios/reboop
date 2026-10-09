// Model-output: Claude Fable 5.1

//! What SMART says about a machine's disks, via smartctl: whether any is
//! about to fail.

use crate::human;
use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// How long each smartctl gets, so that a disk that doesn't answer can't
/// hold up the session (which gives the whole batch [`QUICK`]).
const SMARTCTL_TIMEOUT: Duration = Duration::from_secs(20);

/// The IDs of the ATA SMART attributes whose raw values count sectors gone
/// bad: reallocated (5), reported uncorrectable (187), pending reallocation
/// (197), and offline uncorrectable (198).  Drives name them differently,
/// so reports use smartctl's names.
const ATA_WARNING_ATTRIBUTES: [u8; 4] = [5, 187, 197, 198];

/// A whole disk, as lsblk sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Disk {
    /// e.g. "/dev/sda"
    pub path: String,
    pub model: Option<String>,
    pub serial: Option<String>,
}

impl fmt::Display for Disk {
    /// e.g. "/dev/sda (QEMU HARDDISK, serial QM00001)", or just the path if
    /// that's all that's known.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let serial = self.serial.as_ref().map(|serial| format!("serial {serial}"));
        let details: Vec<String> = [self.model.clone(), serial].into_iter().flatten().collect();
        if details.is_empty() { write!(f, "{}", self.path) } else { write!(f, "{} ({})", self.path, details.join(", ")) }
    }
}

/// Parses `lsblk --inverse --json` output: a tree from each device asked
/// about down to the disks beneath it.  Returns the disks, each once, in
/// order of first appearance.
fn parse_lsblk(json: &str) -> Result<Vec<Disk>> {
    #[derive(Deserialize)]
    struct Lsblk {
        blockdevices: Vec<Node>,
    }
    #[derive(Deserialize)]
    struct Node {
        name: String,
        #[serde(rename = "type")]
        kind: String,
        model: Option<String>,
        serial: Option<String>,
        #[serde(default)]
        children: Vec<Node>,
    }
    fn collect(node: Node, disks: &mut Vec<Disk>) {
        let Node { name, kind, model, serial, children } = node;
        if kind == "disk" && !disks.iter().any(|disk| disk.path == name) {
            disks.push(Disk { path: name, model, serial });
        }
        for child in children {
            collect(child, disks);
        }
    }
    let lsblk: Lsblk = serde_json::from_str(json).with_context(|| format!("unexpected lsblk output {json:?}"))?;
    let mut disks = Vec::new();
    for node in lsblk.blockdevices {
        collect(node, &mut disks);
    }
    Ok(disks)
}

/// Whether the machine has smartctl.
pub fn is_available(session: &mut Session) -> Result<bool> {
    Ok(session.run("command -v smartctl >/dev/null", QUICK)?.status == 0)
}

/// The whole disks beneath the block devices at `paths` (partitions, LUKS
/// mappings, md arrays and so on), each once.  A loop device has none.
pub fn disks_beneath(session: &mut Session, paths: &[String]) -> Result<Vec<Disk>> {
    // Without arguments, lsblk would list every device.
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let quoted: Vec<String> = paths.iter().map(|path| shell_quote(path)).collect();
    let script = format!("lsblk --inverse --json --paths --output NAME,TYPE,MODEL,SERIAL {}", quoted.join(" "));
    parse_lsblk(&session.run_ok(&script, QUICK)?)
}

/// What SMART says about a disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Smart {
    /// smartctl couldn't get SMART data from it, and `unreadable` says why.
    /// Virtual disks have none, and smartctl doesn't know every USB bridge
    /// or RAID controller.
    Unreadable { unreadable: String },
    Health {
        /// Whether the disk's overall self-assessment passed.
        passed: bool,
        /// The signs of a failing disk that aren't zero, by smartctl's name
        /// for them: on ATA disks, the raw values of
        /// [`ATA_WARNING_ATTRIBUTES`]; on NVMe disks, the critical warning
        /// bits and the media errors.
        warnings: BTreeMap<String, u64>,
    },
}

impl Smart {
    /// What's wrong with the disk, for people, e.g. "overall health FAILED,
    /// Current_Pending_Sector=2"; `None` if nothing is, or it's unreadable.
    /// Trouble calls for a human to look before the next power cycle, which
    /// a disk with sectors gone bad often doesn't come back from.
    pub fn trouble(&self) -> Option<String> {
        let Smart::Health { passed, warnings } = self else { return None };
        let mut signs = Vec::new();
        if !passed {
            signs.push("overall health FAILED".to_string());
        }
        if !warnings.is_empty() {
            signs.push(human::counters(warnings));
        }
        if signs.is_empty() { None } else { Some(signs.join(", ")) }
    }
}

/// Parses the output of `smartctl --json -H -A`.
fn parse_smartctl(json: &str) -> Result<Smart> {
    #[derive(Deserialize)]
    struct Output {
        smartctl: Smartctl,
        smart_status: Option<SmartStatus>,
        ata_smart_attributes: Option<AtaAttributes>,
        nvme_smart_health_information_log: Option<NvmeLog>,
    }
    #[derive(Deserialize)]
    struct Smartctl {
        exit_status: i32,
        #[serde(default)]
        messages: Vec<Message>,
    }
    #[derive(Deserialize)]
    struct Message {
        string: String,
        severity: String,
    }
    #[derive(Deserialize)]
    struct SmartStatus {
        passed: bool,
    }
    #[derive(Deserialize)]
    struct AtaAttributes {
        table: Vec<Attribute>,
    }
    #[derive(Deserialize)]
    struct Attribute {
        id: u8,
        name: String,
        raw: Raw,
    }
    #[derive(Deserialize)]
    struct Raw {
        value: u64,
    }
    #[derive(Deserialize)]
    struct NvmeLog {
        critical_warning: u64,
        media_errors: u64,
    }
    let output: Output = serde_json::from_str(json).with_context(|| format!("unexpected smartctl output {json:?}"))?;
    let Some(status) = output.smart_status else {
        let errors: Vec<&str> = output.smartctl.messages.iter().filter(|message| message.severity == "error").map(|message| message.string.as_str()).collect();
        let unreadable = if errors.is_empty() {
            format!("smartctl found no SMART health status (exit status {})", output.smartctl.exit_status)
        } else {
            errors.join("; ")
        };
        return Ok(Smart::Unreadable { unreadable });
    };
    let mut warnings = BTreeMap::new();
    for attribute in output.ata_smart_attributes.into_iter().flat_map(|attributes| attributes.table) {
        if ATA_WARNING_ATTRIBUTES.contains(&attribute.id) && attribute.raw.value > 0 {
            warnings.insert(attribute.name, attribute.raw.value);
        }
    }
    if let Some(log) = output.nvme_smart_health_information_log {
        for (name, value) in [("critical_warning", log.critical_warning), ("media_errors", log.media_errors)] {
            if value > 0 {
                warnings.insert(name.to_string(), value);
            }
        }
    }
    Ok(Smart::Health { passed: status.passed, warnings })
}

/// Parses what [`health`]'s script prints about each of `disks`: a line with
/// smartctl's exit status, then a line with its output.
fn parse_health(text: &str, disks: &[Disk]) -> Result<Vec<Smart>> {
    let lines: Vec<&str> = text.lines().collect();
    ensure!(lines.len() == 2 * disks.len(), "expected 2 lines about each of {} disks from smartctl, got:\n{text}", disks.len());
    disks
        .iter()
        .zip(lines.chunks(2))
        .map(|(disk, pair)| {
            let [status, output] = pair else { unreachable!() };
            let status: i32 = status.parse().with_context(|| format!("unexpected smartctl exit status {status:?} for {}", disk.path))?;
            // smartctl prints JSON whatever happens, unless it was killed.
            if output.trim().is_empty() {
                let unreadable = if status == 124 {
                    format!("smartctl didn't answer within {}", human::seconds(SMARTCTL_TIMEOUT.as_secs()))
                } else {
                    format!("smartctl exited with status {status} and printed nothing")
                };
                return Ok(Smart::Unreadable { unreadable });
            }
            parse_smartctl(output).map_err(|error| anyhow!("{error:#} (for {})", disk.path))
        })
        .collect()
}

/// What SMART says about each of `disks`, in order, asking smartctl about
/// all of them at once: each takes a moment to answer, and a failing one
/// can take long.  A disk in standby is woken up.
pub fn health(session: &mut Session, disks: &[Disk]) -> Result<Vec<Smart>> {
    if disks.is_empty() {
        return Ok(vec![]);
    }
    let quoted: Vec<String> = disks.iter().map(|disk| shell_quote(&disk.path)).collect();
    let script = format!(
        r#"set -u
        t=$(mktemp -d) || exit 1
        trap 'rm -rf "$t"' EXIT
        set -- {}
        i=0
        for d in "$@"; do
            i=$((i + 1))
            {{ timeout {} smartctl --json=c -H -A "$d" >"$t/$i" 2>&1; echo $? >"$t/$i.status"; }} &
        done
        wait
        i=0
        for d in "$@"; do
            i=$((i + 1))
            cat "$t/$i.status"
            tr '\n' ' ' <"$t/$i"
            echo
        done"#,
        quoted.join(" "),
        SMARTCTL_TIMEOUT.as_secs()
    );
    parse_health(&session.run_ok(&script, QUICK)?, disks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(path: &str) -> Disk {
        Disk { path: path.into(), model: None, serial: None }
    }

    /// `attributes` go in an ATA disk's table, each as (id, name, raw value).
    fn ata_json(passed: bool, attributes: &[(u8, &str, u64)]) -> String {
        let table: Vec<String> = attributes
            .iter()
            .map(|(id, name, raw)| {
                format!(
                    r#"{{"id":{id},"name":"{name}","value":100,"worst":100,"thresh":10,"when_failed":"","flags":{{"value":51,"string":"PO--CK "}},"raw":{{"value":{raw},"string":"{raw}"}}}}"#
                )
            })
            .collect();
        format!(
            r#"{{"json_format_version":[1,0],"smartctl":{{"version":[7,5],"exit_status":0}},"device":{{"name":"/dev/sda","type":"sat","protocol":"ATA"}},
            "model_name":"QEMU HARDDISK","serial_number":"QM00001","smart_status":{{"passed":{passed}}},
            "ata_smart_attributes":{{"revision":16,"table":[{}]}}}}"#,
            table.join(",")
        )
    }

    fn nvme_json(critical_warning: u64, media_errors: u64) -> String {
        format!(
            r#"{{"json_format_version":[1,0],"smartctl":{{"version":[7,5],"exit_status":0}},"device":{{"name":"/dev/nvme0","type":"nvme","protocol":"NVMe"}},
            "smart_status":{{"passed":{},"nvme":{{"value":{critical_warning}}}}},
            "nvme_smart_health_information_log":{{"critical_warning":{critical_warning},"temperature":35,"available_spare":100,"available_spare_threshold":10,
            "percentage_used":1,"media_errors":{media_errors},"num_err_log_entries":0}}}}"#,
            critical_warning == 0
        )
    }

    const UNKNOWN_USB_BRIDGE: &str = r#"{"json_format_version":[1,0],"smartctl":{"version":[7,5],"pre_release":false,"platform_info":"x86_64-linux-6.18.55","build_info":"(local build)","argv":["smartctl","--json=c","-H","-A","/dev/sda"],"messages":[{"string":"/dev/sda: Unknown USB bridge [0x090c:0x1000 (0x1100)]","severity":"error"}],"exit_status":1},"local_time":{"time_t":1791581486,"asctime":"Fri Oct  9 21:31:26 2026 UTC"}}"#;

    #[test]
    fn parses_ata() {
        let healthy = parse_smartctl(&ata_json(true, &[(5, "Reallocated_Sector_Ct", 0), (9, "Power_On_Hours", 12345), (197, "Current_Pending_Sector", 0)])).unwrap();
        assert_eq!(healthy, Smart::Health { passed: true, warnings: BTreeMap::new() });
        assert_eq!(healthy.trouble(), None);
        let failing = parse_smartctl(&ata_json(false, &[(5, "Retired_Block_Count", 3), (187, "Reported_Uncorrect", 0), (197, "Current_Pending_Sector", 2), (198, "Offline_Uncorrectable", 2)])).unwrap();
        let warnings = BTreeMap::from([("Retired_Block_Count".into(), 3), ("Current_Pending_Sector".into(), 2), ("Offline_Uncorrectable".into(), 2)]);
        assert_eq!(failing, Smart::Health { passed: false, warnings });
        assert_eq!(failing.trouble().unwrap(), "overall health FAILED, Current_Pending_Sector=2 Offline_Uncorrectable=2 Retired_Block_Count=3");
        let no_attributes = parse_smartctl(&ata_json(true, &[]).replace(r#","ata_smart_attributes""#, r#","other""#)).unwrap();
        assert_eq!(no_attributes.trouble(), None);
    }

    #[test]
    fn parses_nvme() {
        assert_eq!(parse_smartctl(&nvme_json(0, 0)).unwrap().trouble(), None);
        let spare_low = parse_smartctl(&nvme_json(1, 0)).unwrap();
        assert_eq!(spare_low.trouble().unwrap(), "overall health FAILED, critical_warning=1");
        let media_errors = parse_smartctl(&nvme_json(0, 7)).unwrap();
        assert_eq!(media_errors, Smart::Health { passed: true, warnings: BTreeMap::from([("media_errors".into(), 7)]) });
        assert_eq!(media_errors.trouble().unwrap(), "media_errors=7");
    }

    #[test]
    fn parses_unreadable() {
        let unknown_bridge = parse_smartctl(UNKNOWN_USB_BRIDGE).unwrap();
        assert_eq!(unknown_bridge, Smart::Unreadable { unreadable: "/dev/sda: Unknown USB bridge [0x090c:0x1000 (0x1100)]".into() });
        assert_eq!(unknown_bridge.trouble(), None);
        let no_messages = parse_smartctl(r#"{"json_format_version":[1,0],"smartctl":{"version":[7,5],"exit_status":4}}"#).unwrap();
        assert_eq!(no_messages, Smart::Unreadable { unreadable: "smartctl found no SMART health status (exit status 4)".into() });
        assert!(parse_smartctl("").is_err());
        assert!(parse_smartctl(r#"{"smart_status":{"passed":true}}"#).is_err());
    }

    #[test]
    fn parses_health_of_each_disk() {
        let disks = [disk("/dev/sda"), disk("/dev/sdb"), disk("/dev/sdc")];
        // The script puts each smartctl's output on one line.
        let text = format!("0\n{}\n124\n\n1\n{UNKNOWN_USB_BRIDGE}\n", ata_json(true, &[]).replace('\n', " "));
        let health = parse_health(&text, &disks).unwrap();
        assert_eq!(health[0], Smart::Health { passed: true, warnings: BTreeMap::new() });
        assert_eq!(health[1], Smart::Unreadable { unreadable: "smartctl didn't answer within 20s".into() });
        assert!(matches!(&health[2], Smart::Unreadable { unreadable } if unreadable.contains("Unknown USB bridge")));
        assert!(parse_health(&text, &disks[..2]).is_err());
        assert!(parse_health("0\n\n", &disks[..1]).is_ok());
        assert!(parse_health("x\n\n", &disks[..1]).is_err());
        assert!(parse_health("0\nnot json\n", &disks[..1]).is_err());
    }

    #[test]
    fn parses_lsblk() {
        // A LUKS mapping on a partition, a loop device, and a partition of
        // the same disk as the mapping's
        let json = r#"{"blockdevices": [
            {"name": "/dev/mapper/root", "type": "crypt", "model": null, "serial": null, "children": [
                {"name": "/dev/sda2", "type": "part", "model": null, "serial": null, "children": [
                    {"name": "/dev/sda", "type": "disk", "model": "QEMU HARDDISK", "serial": "QM00001"}
                ]}
            ]},
            {"name": "/dev/loop0", "type": "loop", "model": null, "serial": null},
            {"name": "/dev/sda1", "type": "part", "model": null, "serial": null, "children": [
                {"name": "/dev/sda", "type": "disk", "model": "QEMU HARDDISK", "serial": "QM00001"}
            ]}
        ]}"#;
        let disks = parse_lsblk(json).unwrap();
        assert_eq!(disks, [Disk { path: "/dev/sda".into(), model: Some("QEMU HARDDISK".into()), serial: Some("QM00001".into()) }]);
        assert_eq!(disks[0].to_string(), "/dev/sda (QEMU HARDDISK, serial QM00001)");
        assert_eq!(disk("/dev/sdb").to_string(), "/dev/sdb");
        assert!(parse_lsblk("").is_err());
    }
}
