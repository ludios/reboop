//! What the kernel's EDAC drivers say about a machine's RAM: whether it uses
//! ECC, and how many memory errors they counted.

use crate::ssh::{QUICK, Session};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

/// What the EDAC memory controllers in /sys/devices/system/edac/mc say.
/// What a DIMM's mode tells depends on the driver: amd64_edac (AMD), and
/// sb_edac, skx_edac, i10nm_edac and igen6_edac (Intel), register a memory
/// controller only if ECC is on; i7core_edac (Intel) registers one either
/// way, with the mode "None" if ECC is off; ie31200_edac (Intel, for Xeon
/// E3, E-2100 and E-2200, and 12th- to 14th-generation Core on W680)
/// registers one if the CPU can do ECC, whether or not ECC is on, with each
/// DIMM's mode "Unknown"; and ghes_edac (where the firmware reports memory
/// errors, as on HPE servers) takes each DIMM's mode from what SMBIOS says
/// about the DIMM, not from the memory controller.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Edac {
    /// Each populated DIMM's (or rank's) error correction, as its
    /// dimm_edac_mode says: e.g. "SECDED", "S4ECD4ED" (chipkill), "None" or
    /// "Unknown".  Empty if no memory controller is registered, as when ECC
    /// is off, or when Linux has no EDAC driver for the machine's memory
    /// controller.
    pub dimm_modes: Vec<String>,
    /// The corrected errors (CEs) that the memory controllers counted, in
    /// all, since their counters were reset: when their driver loaded
    /// (normally at boot), or when something wrote to their reset_counters.
    pub corrected_errors: u64,
    /// The uncorrected errors (UEs) that they counted, likewise.
    pub uncorrected_errors: u64,
}

/// Whether `mode` (a dimm_edac_mode) corrects errors; `None` if it doesn't
/// say, like "Unknown" and "Reserved", or if it's newer than this function.
fn corrects(mode: &str) -> Option<bool> {
    match mode {
        "SECDED" | "S2ECD2ED" | "S4ECD4ED" | "S8ECD8ED" | "S16ECD16ED" => Some(true),
        // Parity and "EC" only detect errors.
        "None" | "PARITY" | "EC" => Some(false),
        _ => None,
    }
}

impl Edac {
    /// Whether the RAM uses ECC: `Some(true)` if every populated DIMM's mode
    /// corrects errors; `Some(false)` if some DIMM's mode doesn't; otherwise
    /// `None`, as no driver says, as when no memory controller is registered
    /// (no DIMM has a mode), or when a DIMM's mode is "Unknown".
    pub fn ecc(&self) -> Option<bool> {
        let any = |answer| self.dimm_modes.iter().any(|mode| corrects(mode) == answer);
        if any(Some(false)) {
            Some(false)
        } else if self.dimm_modes.is_empty() || any(None) {
            None
        } else {
            Some(true)
        }
    }
}

/// Prints "PATH VALUE" for each file that [`parse`] reads: each populated
/// DIMM's (or rank's) dimm_edac_mode, and each memory controller's ce_count
/// and ue_count.  (Not the ce_count of each csrow*, which the controller's
/// own ce_count includes.)
const READ_FILES: &str = r#"
for file in /sys/devices/system/edac/mc/mc*/dimm*/dimm_edac_mode /sys/devices/system/edac/mc/mc*/rank*/dimm_edac_mode /sys/devices/system/edac/mc/mc*/ce_count /sys/devices/system/edac/mc/mc*/ue_count; do
    # A pattern that matches nothing stays as it is.
    if [ -e "$file" ]; then
        value=$(cat "$file") || exit 1
        echo "$file $value"
    fi
done
"#;

/// Parses `output`, which is what [`READ_FILES`] prints: a "PATH VALUE" line
/// for each file.  Returns the DIMMs' modes in the order of their lines, and
/// the counts summed over the memory controllers.
fn parse(output: &str) -> Result<Edac> {
    let mut edac = Edac::default();
    for line in output.lines() {
        let context = || format!("unexpected EDAC line {line:?}");
        let (path, value) = line.split_once(' ').filter(|(_, value)| !value.is_empty()).ok_or_else(|| anyhow!(context()))?;
        // The kernel's counts are u32s, so no sum of them overflows a u64.
        let count = || value.parse::<u32>().map(u64::from).with_context(context);
        match path.rsplit_once('/').map(|(_, name)| name) {
            Some("dimm_edac_mode") => edac.dimm_modes.push(value.to_string()),
            Some("ce_count")       => edac.corrected_errors += count()?,
            Some("ue_count")       => edac.uncorrected_errors += count()?,
            _ => bail!(context()),
        }
    }
    Ok(edac)
}

/// What the EDAC memory controllers say about the RAM of the machine at the
/// other end of `session`: [`Edac::default`] if none is registered.  Fails
/// if one of their files can't be read, or says something unexpected.
pub fn read(session: &mut Session) -> Result<Edac> {
    parse(&session.run_ok(READ_FILES, QUICK)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    /// An [`Edac`] with no errors and these `modes`.
    fn modes(modes: &[&str]) -> Edac {
        Edac { dimm_modes: modes.iter().map(|mode| mode.to_string()).collect(), ..Edac::default() }
    }

    #[test]
    fn parses() {
        let output = "\
            /sys/devices/system/edac/mc/mc0/dimm0/dimm_edac_mode SECDED\n\
            /sys/devices/system/edac/mc/mc0/dimm2/dimm_edac_mode SECDED\n\
            /sys/devices/system/edac/mc/mc1/rank0/dimm_edac_mode S4ECD4ED\n\
            /sys/devices/system/edac/mc/mc0/ce_count 3\n\
            /sys/devices/system/edac/mc/mc1/ce_count 4\n\
            /sys/devices/system/edac/mc/mc0/ue_count 0\n\
            /sys/devices/system/edac/mc/mc1/ue_count 1\n";
        let edac = parse(output).unwrap();
        assert_eq!(edac, Edac { dimm_modes: vec!["SECDED".into(), "SECDED".into(), "S4ECD4ED".into()], corrected_errors: 7, uncorrected_errors: 1 });
        assert_eq!(parse("").unwrap(), Edac::default());
    }

    /// Runs [`READ_FILES`] here, on the files under `root` rather than under
    /// /sys/devices/system/edac/mc, and returns what it prints.
    fn read_files_under(root: &Path) -> String {
        let script = READ_FILES.replace("/sys/devices/system/edac/mc", root.to_str().unwrap());
        let output = Command::new("sh").args(["-c", &script]).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn reads_files() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_files_under(root.path()), "");
        let files = [
            ("mc0/dimm0/dimm_edac_mode", "SECDED"),
            ("mc0/ce_count", "2"),
            ("mc0/ue_count", "0"),
            // Included in mc0/ce_count
            ("mc0/csrow0/ce_count", "2"),
            ("mc1/rank3/dimm_edac_mode", "S4ECD4ED"),
            ("mc1/ce_count", "1"),
            ("mc1/ue_count", "1"),
        ];
        for (file, value) in files {
            let path = root.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("{value}\n")).unwrap();
        }
        let edac = parse(&read_files_under(root.path())).unwrap();
        assert_eq!(edac, Edac { dimm_modes: vec!["SECDED".into(), "S4ECD4ED".into()], corrected_errors: 3, uncorrected_errors: 1 });
    }

    #[test]
    fn rejects_garbage() {
        let lines = [
            "SECDED",
            "/sys/devices/system/edac/mc/mc0/size_mb 4096",
            "/sys/devices/system/edac/mc/mc0/ce_count",
            "/sys/devices/system/edac/mc/mc0/ue_count -1",
            "/sys/devices/system/edac/mc/mc0/ue_count 4294967296",
            "/sys/devices/system/edac/mc/mc0/dimm0/dimm_edac_mode ",
        ];
        for output in lines {
            assert!(parse(output).is_err(), "{output:?}");
        }
    }

    #[test]
    fn knows_whether_ecc_is_on() {
        assert_eq!(modes(&[]).ecc(), None);
        assert_eq!(modes(&["SECDED", "S8ECD8ED"]).ecc(), Some(true));
        assert_eq!(modes(&["SECDED", "None"]).ecc(), Some(false));
        assert_eq!(modes(&["PARITY"]).ecc(), Some(false));
        assert_eq!(modes(&["EC", "SECDED"]).ecc(), Some(false));
        assert_eq!(modes(&["Unknown", "SECDED"]).ecc(), None);
        assert_eq!(modes(&["Unknown", "None"]).ecc(), Some(false));
        assert_eq!(modes(&["Reserved"]).ecc(), None);
    }
}
