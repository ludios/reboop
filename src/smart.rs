// Model-output: Claude Fable 5.1

//! What SMART says about a machine's disks, via smartctl: whether any is
//! about to fail.

use crate::human;
use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// How long each smartctl gets before it's told to stop.
const SMARTCTL_TIMEOUT: Duration = Duration::from_secs(20);

/// How long [`health`] waits for its smartctls at most: long enough past
/// [`SMARTCTL_TIMEOUT`] for one that stops when told to count as timed out,
/// and short enough under the session's [`QUICK`] to leave behind one that
/// can't stop, being stuck in the kernel on a disk that doesn't answer.
pub const PATIENCE: Duration = Duration::from_secs(25);

const _: () = assert!(PATIENCE.as_secs() > SMARTCTL_TIMEOUT.as_secs() && PATIENCE.as_secs() + 3 < QUICK.as_secs());

/// The IDs of the ATA SMART attributes whose raw values count sectors gone
/// bad: reallocated (5), reported uncorrectable (187), pending reallocation
/// (197), and offline uncorrectable (198).
const ATA_WARNING_ATTRIBUTES: [u8; 4] = [5, 187, 197, 198];

/// Words in the names that smartctl's drive database gives those attributes
/// when they do count sectors (or flash blocks) gone bad, like
/// Reallocated_Sector_Ct, Retired_Block_Count, New_Bad_Blk_Count,
/// Uncorrectable_Error_Cnt, Offline_UErr_Media_Scan, Reported_UE_Counts,
/// Current_Pending_Sector and Read_Failure_Blk_Ct.  Some vendors put other
/// counters at the same IDs, like Host_Reads_GiB at 198, or ECC_Error_Count
/// (errors that were corrected) at 197.
const BAD_SECTOR_WORDS: [&str; 8] = ["realloc", "retired", "bad_bl", "unc", "uerr", "reported_ue", "pending", "read_fail"];

/// The bit of NVMe's critical warning that says the temperature is over or
/// under a threshold, which passes here: it's a condition, not damage.
const NVME_TEMPERATURE_WARNING: u64 = 0x02;

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
/// about down to the disks beneath it.  Returns the disks at the bottom
/// (the "disk" nodes with nothing beneath them, which leaves out stacked
/// devices like bcache that lsblk also calls disks), each once, in order of
/// first appearance.
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
        if kind == "disk" && children.is_empty() && !disks.iter().any(|disk| disk.path == name) {
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
/// mappings, md arrays and so on), each once.  A loop device has none, and
/// neither does a device that's gone from /dev, as one btrfs lost may be.
pub fn disks_beneath(session: &mut Session, paths: &[String]) -> Result<Vec<Disk>> {
    // Without arguments, lsblk would list every device.
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let quoted: Vec<String> = paths.iter().map(|path| shell_quote(path)).collect();
    let script = format!(
        r#"set -- {}
        devices=
        for d in "$@"; do
            if [ -b "$d" ]; then devices="$devices $d"; fi
        done
        if [ -n "$devices" ]; then
            lsblk --inverse --json --paths --output NAME,TYPE,MODEL,SERIAL $devices
        else
            echo '{{"blockdevices": []}}'
        fi"#,
        quoted.join(" ")
    );
    parse_lsblk(&session.run_ok(&script, QUICK)?)
}

/// What SMART says about a disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Smart {
    /// smartctl didn't answer about it within `no_answer_within_secs`
    /// ([`SMARTCTL_TIMEOUT`]), as it doesn't about a disk that's failing,
    /// or behind a controller that's hung.
    NoAnswer { no_answer_within_secs: u64 },
    Read {
        /// Whether the disk's overall self-assessment passed, or `None` if
        /// smartctl couldn't read it.
        passed: Option<bool>,
        /// The signs of a failing disk that aren't zero, by smartctl's name
        /// for them: on ATA disks, the raw values of the
        /// [`ATA_WARNING_ATTRIBUTES`] named by [`BAD_SECTOR_WORDS`]; on NVMe
        /// disks, the critical warning bits (except
        /// [`NVME_TEMPERATURE_WARNING`]) and the media errors.
        warnings: BTreeMap<String, u64>,
        /// Why smartctl couldn't read all of that, if it couldn't: a virtual
        /// disk has no SMART, smartctl doesn't know every USB bridge or RAID
        /// controller, and a SAS disk reports its defects differently.
        incomplete: Option<String>,
    },
}

impl Smart {
    /// What's wrong with the disk, for people, e.g. "overall health FAILED,
    /// Current_Pending_Sector=2"; `None` if nothing is, as far as could be
    /// read.  Trouble calls for a human to look before the next power
    /// cycle, which a disk with sectors gone bad often doesn't come back
    /// from.
    pub fn trouble(&self) -> Option<String> {
        let Smart::Read { passed, warnings, .. } = self else {
            return Some(format!("didn't answer smartctl within {}", human::seconds(SMARTCTL_TIMEOUT.as_secs())));
        };
        let mut signs = Vec::new();
        if *passed == Some(false) {
            signs.push("overall health FAILED".to_string());
        }
        if !warnings.is_empty() {
            signs.push(human::counters(warnings));
        }
        if signs.is_empty() { None } else { Some(signs.join(", ")) }
    }
}

/// Interprets what `smartctl --json -H -A` printed, from its exit `status`,
/// `stdout` and `stderr`.  Whatever it managed to read counts; what it
/// didn't is noted as incomplete.
fn parse_smartctl(status: &str, stdout: &str, stderr: &str) -> Smart {
    #[derive(Deserialize)]
    struct Output {
        smartctl: Smartctl,
        smart_status: Option<SmartStatus>,
        ata_smart_attributes: Option<AtaAttributes>,
        nvme_smart_health_information_log: Option<NvmeLog>,
    }
    #[derive(Deserialize)]
    struct Smartctl {
        exit_status: u32,
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
    let unread = |incomplete: String| Smart::Read { passed: None, warnings: BTreeMap::new(), incomplete: Some(incomplete) };
    // timeout exits 124 after stopping a smartctl that took too long, which
    // then prints nothing.
    if status == "124" {
        return Smart::NoAnswer { no_answer_within_secs: SMARTCTL_TIMEOUT.as_secs() };
    }
    let Ok(output) = serde_json::from_str::<Output>(stdout) else {
        let text = human::truncate(format!("{stdout} {stderr}").trim(), 200);
        return unread(format!("smartctl exited with status {status} without JSON output: {text}"));
    };

    let mut warnings = BTreeMap::new();
    let mut passed = output.smart_status.map(|status| status.passed);
    let ata = output.ata_smart_attributes.is_some();
    for attribute in output.ata_smart_attributes.into_iter().flat_map(|attributes| attributes.table) {
        let name = attribute.name.to_lowercase();
        let counts_bad_sectors = ATA_WARNING_ATTRIBUTES.contains(&attribute.id) && BAD_SECTOR_WORDS.iter().any(|word| name.contains(word));
        if counts_bad_sectors && attribute.raw.value > 0 {
            warnings.insert(attribute.name, attribute.raw.value);
        }
    }
    let nvme = output.nvme_smart_health_information_log.is_some();
    if let Some(log) = output.nvme_smart_health_information_log {
        // smartctl's self-assessment of an NVMe disk is just whether the
        // critical warning is zero.
        let critical_warning = log.critical_warning & !NVME_TEMPERATURE_WARNING;
        passed = Some(critical_warning == 0);
        for (name, value) in [("critical_warning", critical_warning), ("media_errors", log.media_errors)] {
            if value > 0 {
                warnings.insert(name.to_string(), value);
            }
        }
    }

    // smartctl's exit status has a bit each for not identifying or opening
    // the device, and for a SMART command failing.
    let messages: Vec<&str> = output.smartctl.messages.iter().filter(|message| message.severity != "info").map(|message| message.string.as_str()).collect();
    let why = if messages.is_empty() { format!("smartctl exited with status {}", output.smartctl.exit_status) } else { messages.join("; ") };
    let incomplete = match (passed.is_some(), ata || nvme) {
        (false, false) => Some(if messages.is_empty() { format!("no SMART data ({why})") } else { why }),
        (false, true) => Some(format!("no overall self-assessment ({why})")),
        (true, false) => Some(if messages.is_empty() { "no ATA attributes or NVMe health log".to_string() } else { format!("no ATA attributes or NVMe health log ({why})") }),
        (true, true) if output.smartctl.exit_status & 0b111 != 0 => Some(why),
        (true, true) => None,
    };
    Smart::Read { passed, warnings, incomplete }
}

/// Parses what [`health`]'s script prints about each of `disks`: a line with
/// smartctl's exit status (or "none" if it hasn't finished), then one with
/// its stdout, then one with its stderr.
fn parse_health(text: &str, disks: &[Disk]) -> Result<Vec<Smart>> {
    let lines: Vec<&str> = text.lines().collect();
    ensure!(lines.len() == 3 * disks.len(), "expected 3 lines about each of {} disks from smartctl, got:\n{text}", disks.len());
    let smart = lines.chunks(3).map(|lines| {
        let [status, stdout, stderr] = lines else { unreachable!() };
        if *status == "none" { Smart::NoAnswer { no_answer_within_secs: SMARTCTL_TIMEOUT.as_secs() } } else { parse_smartctl(status, stdout, stderr) }
    });
    Ok(smart.collect())
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
            {{ timeout {} smartctl --json=c -H -A "$d" >"$t/$i.out" 2>"$t/$i.err"; echo $? >"$t/$i.status"; }} &
        done
        # Not `wait`: one stuck in the kernel can't stop, and is left behind.
        n=0
        while [ "$(ls "$t"/*.status 2>/dev/null | wc -l)" -lt $# ] && [ $n -lt {} ]; do
            sleep 1
            n=$((n + 1))
        done
        i=0
        for d in "$@"; do
            i=$((i + 1))
            cat "$t/$i.status" 2>/dev/null || echo none
            tr '\n' ' ' <"$t/$i.out"; echo
            tr '\n' ' ' <"$t/$i.err"; echo
        done"#,
        quoted.join(" "),
        SMARTCTL_TIMEOUT.as_secs(),
        PATIENCE.as_secs()
    );
    parse_health(&session.run_ok(&script, QUICK)?, disks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(path: &str) -> Disk {
        Disk { path: path.into(), model: None, serial: None }
    }

    fn read(passed: Option<bool>, warnings: &[(&str, u64)], incomplete: Option<&str>) -> Smart {
        let warnings = warnings.iter().map(|(name, value)| (name.to_string(), *value)).collect();
        Smart::Read { passed, warnings, incomplete: incomplete.map(str::to_string) }
    }

    /// smartctl's output about an ATA disk: `status`, e.g.
    /// `"smart_status":{"passed":true},` or nothing, then `attributes` in
    /// its table (id, name, raw value) if `with_table`, with `exit_status`.
    fn ata_json(status: &str, with_table: bool, attributes: &[(u8, &str, u64)], exit_status: u32) -> String {
        let rows: Vec<String> = attributes
            .iter()
            .map(|(id, name, raw)| {
                format!(r#"{{"id":{id},"name":"{name}","value":100,"worst":100,"thresh":10,"when_failed":"","flags":{{"value":51,"string":"PO--CK "}},"raw":{{"value":{raw},"string":"{raw}"}}}}"#)
            })
            .collect();
        let table = if with_table { format!(r#","ata_smart_attributes":{{"revision":16,"table":[{}]}}"#, rows.join(",")) } else { String::new() };
        format!(
            r#"{{"json_format_version":[1,0],"smartctl":{{"version":[7,5],"exit_status":{exit_status}}},"device":{{"name":"/dev/sda","type":"sat","protocol":"ATA"}},"model_name":"QEMU HARDDISK","serial_number":"QM00001",{status}"power_on_time":{{"hours":1}}{table}}}"#
        )
    }

    const PASSED: &str = r#""smart_status":{"passed":true},"#;
    const FAILED: &str = r#""smart_status":{"passed":false},"#;

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

    /// What qemu's virtio disk gets.
    const UNDETECTED: &str = r#"{"json_format_version":[1,0],"smartctl":{"version":[7,5],"pre_release":false,"platform_info":"x86_64-linux-6.18.55","build_info":"(local build)","argv":["smartctl","--json=c","-H","-A","/dev/vda"],"messages":[{"string":"/dev/vda: Unable to detect device type","severity":"error"}],"exit_status":1},"local_time":{"time_t":1791583333,"asctime":"Fri Oct  9 22:02:13 2026 UTC"}}"#;

    #[test]
    fn parses_ata() {
        let healthy = parse_smartctl("0", &ata_json(PASSED, true, &[(5, "Reallocated_Sector_Ct", 0), (9, "Power_On_Hours", 12345), (197, "Current_Pending_Sector", 0)], 0), "");
        assert_eq!(healthy, read(Some(true), &[], None));
        assert_eq!(healthy.trouble(), None);
        let bad = [(5, "Retired_Block_Count", 3), (187, "Reported_Uncorrect", 0), (197, "Current_Pending_Sector", 2), (198, "Offline_Uncorrectable", 2)];
        let failing = parse_smartctl("8", &ata_json(FAILED, true, &bad, 8), "");
        assert_eq!(failing, read(Some(false), &[("Current_Pending_Sector", 2), ("Offline_Uncorrectable", 2), ("Retired_Block_Count", 3)], None));
        assert_eq!(failing.trouble().unwrap(), "overall health FAILED, Current_Pending_Sector=2 Offline_Uncorrectable=2 Retired_Block_Count=3");
    }

    #[test]
    fn knows_which_attributes_count_bad_sectors() {
        // Vendors' names for them, and other things at the same IDs
        let table = [
            (5, "New_Bad_Blk_Count", 1),
            (5, "Retried_Blk_Ct", 7),
            (187, "Uncorrectable_Error_Cnt", 2),
            (187, "Reported_UE_Counts", 3),
            (197, "Total_Unc_Read_Failures", 4),
            (197, "ECC_Error_Count", 1000),
            (198, "Offline_UErr_Media_Scan", 5),
            (198, "Host_Reads_GiB", 9999),
            (196, "Reallocated_Event_Count", 6),
        ];
        let smart = parse_smartctl("0", &ata_json(PASSED, true, &table, 0), "");
        let expected = [("New_Bad_Blk_Count", 1), ("Uncorrectable_Error_Cnt", 2), ("Reported_UE_Counts", 3), ("Total_Unc_Read_Failures", 4), ("Offline_UErr_Media_Scan", 5)];
        assert_eq!(smart, read(Some(true), &expected, None));
    }

    #[test]
    fn parses_nvme() {
        assert_eq!(parse_smartctl("0", &nvme_json(0, 0), ""), read(Some(true), &[], None));
        let spare_low = parse_smartctl("8", &nvme_json(1, 0), "");
        assert_eq!(spare_low.trouble().unwrap(), "overall health FAILED, critical_warning=1");
        let media_errors = parse_smartctl("0", &nvme_json(0, 7), "");
        assert_eq!(media_errors, read(Some(true), &[("media_errors", 7)], None));
        assert_eq!(media_errors.trouble().unwrap(), "media_errors=7");
        // A temperature out of range is no failure, even though smartctl says FAILED
        assert_eq!(parse_smartctl("8", &nvme_json(2, 0), ""), read(Some(true), &[], None));
        assert_eq!(parse_smartctl("8", &nvme_json(6, 0), ""), read(Some(false), &[("critical_warning", 4)], None));
    }

    #[test]
    fn notes_what_it_couldnt_read() {
        let unknown_bridge = parse_smartctl("1", UNKNOWN_USB_BRIDGE, "");
        assert_eq!(unknown_bridge, read(None, &[], Some("/dev/sda: Unknown USB bridge [0x090c:0x1000 (0x1100)]")));
        assert_eq!(unknown_bridge.trouble(), None);
        assert_eq!(parse_smartctl("1", UNDETECTED, ""), read(None, &[], Some("/dev/vda: Unable to detect device type")));
        let nothing = parse_smartctl("4", r#"{"json_format_version":[1,0],"smartctl":{"version":[7,5],"exit_status":4}}"#, "");
        assert_eq!(nothing, read(None, &[], Some("no SMART data (smartctl exited with status 4)")));
        // The self-assessment failed to read, but the attributes did: they still count.
        let pending = [(197, "Current_Pending_Sector", 8)];
        let no_status = parse_smartctl("4", &ata_json("", true, &pending, 4), "");
        assert_eq!(no_status, read(None, &[("Current_Pending_Sector", 8)], Some("no overall self-assessment (smartctl exited with status 4)")));
        assert_eq!(no_status.trouble().unwrap(), "Current_Pending_Sector=8");
        // The other way around, or a SAS disk
        let no_table = parse_smartctl("4", &ata_json(PASSED, false, &[], 4), "");
        assert_eq!(no_table, read(Some(true), &[], Some("no ATA attributes or NVMe health log")));
        let with_message = ata_json(PASSED, false, &[], 4).replace(r#""exit_status":4"#, r#""exit_status":4,"messages":[{"string":"Read SMART Data failed: scsi error","severity":"error"}]"#);
        assert_eq!(parse_smartctl("4", &with_message, ""), read(Some(true), &[], Some("no ATA attributes or NVMe health log (Read SMART Data failed: scsi error)")));
        // Both read, but a command failed or a checksum was wrong
        let checksum = parse_smartctl("4", &ata_json(PASSED, true, &pending, 4), "");
        assert_eq!(checksum, read(Some(true), &[("Current_Pending_Sector", 8)], Some("smartctl exited with status 4")));
        // Not JSON at all
        let usage = parse_smartctl("1", "smartctl 6.6 2017-11-05 r4594 Usage: smartctl [options] device", "");
        assert_eq!(usage, read(None, &[], Some("smartctl exited with status 1 without JSON output: smartctl 6.6 2017-11-05 r4594 Usage: smartctl [options] device")));
        assert_eq!(parse_smartctl("127", "", "sh: smartctl: not found"), read(None, &[], Some("smartctl exited with status 127 without JSON output: sh: smartctl: not found")));
    }

    #[test]
    fn counts_no_answer_as_trouble() {
        let no_answer = Smart::NoAnswer { no_answer_within_secs: 20 };
        assert_eq!(parse_smartctl("124", "", ""), no_answer);
        assert_eq!(no_answer.trouble().unwrap(), "didn't answer smartctl within 20s");
    }

    #[test]
    fn parses_health_of_each_disk() {
        let disks = [disk("/dev/sda"), disk("/dev/sdb"), disk("/dev/sdc"), disk("/dev/sdd")];
        // The script puts each smartctl's output on one line.
        let healthy = ata_json(PASSED, true, &[], 0).replace('\n', " ");
        let text = format!("0\n{healthy}\n\n124\n\n\nnone\n\n\n1\n{UNKNOWN_USB_BRIDGE}\n\n");
        let health = parse_health(&text, &disks).unwrap();
        assert_eq!(health[0], read(Some(true), &[], None));
        assert_eq!(health[1], Smart::NoAnswer { no_answer_within_secs: 20 });
        assert_eq!(health[2], Smart::NoAnswer { no_answer_within_secs: 20 });
        assert!(matches!(&health[3], Smart::Read { incomplete: Some(why), .. } if why.contains("Unknown USB bridge")));
        assert!(parse_health(&text, &disks[..3]).is_err());
        assert!(parse_health("0\n\n", &disks[..1]).is_err());
    }

    #[test]
    fn parses_lsblk() {
        // A LUKS mapping on a partition, a loop device, a partition of the
        // same disk as the mapping's, and a bcache device over two disks
        let json = r#"{"blockdevices": [
            {"name": "/dev/mapper/root", "type": "crypt", "model": null, "serial": null, "children": [
                {"name": "/dev/sda2", "type": "part", "model": null, "serial": null, "children": [
                    {"name": "/dev/sda", "type": "disk", "model": "QEMU HARDDISK", "serial": "QM00001"}
                ]}
            ]},
            {"name": "/dev/loop0", "type": "loop", "model": null, "serial": null},
            {"name": "/dev/sda1", "type": "part", "model": null, "serial": null, "children": [
                {"name": "/dev/sda", "type": "disk", "model": "QEMU HARDDISK", "serial": "QM00001"}
            ]},
            {"name": "/dev/bcache0", "type": "disk", "model": null, "serial": null, "children": [
                {"name": "/dev/sdb1", "type": "part", "model": null, "serial": null, "children": [
                    {"name": "/dev/sdb", "type": "disk", "model": "WDC WD40EFRX", "serial": "WD-1"}
                ]},
                {"name": "/dev/nvme0n1", "type": "disk", "model": "KINGSTON", "serial": "K1"}
            ]}
        ]}"#;
        let disks = parse_lsblk(json).unwrap();
        let paths: Vec<&str> = disks.iter().map(|disk| disk.path.as_str()).collect();
        assert_eq!(paths, ["/dev/sda", "/dev/sdb", "/dev/nvme0n1"]);
        assert_eq!(disks[0], Disk { path: "/dev/sda".into(), model: Some("QEMU HARDDISK".into()), serial: Some("QM00001".into()) });
        assert_eq!(disks[0].to_string(), "/dev/sda (QEMU HARDDISK, serial QM00001)");
        assert_eq!(disk("/dev/sdb").to_string(), "/dev/sdb");
        assert_eq!(parse_lsblk(r#"{"blockdevices": []}"#).unwrap(), []);
        assert!(parse_lsblk("").is_err());
    }
}
