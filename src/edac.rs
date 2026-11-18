//! What the kernel's EDAC drivers say about a machine's RAM: whether it uses
//! ECC, and how many errors that has found.

use crate::ssh::{QUICK, Session};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

/// What the EDAC memory controllers in /sys/devices/system/edac/mc say.  The
/// drivers for AMD's memory controllers and for Intel's server ones register
/// a memory controller only if ECC is on, but Intel's ie31200_edac (for
/// Xeon E3 and E-2000, and Core on W680) registers one whenever the CPU can
/// do ECC, and says that each DIMM's mode is "Unknown".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Edac {
    /// Each populated DIMM's (or rank's) error correction, as its
    /// dimm_edac_mode says: e.g. "SECDED", "S4ECD4ED" (chipkill), "None" or
    /// "Unknown".  Empty if no memory controller is registered, as when ECC
    /// is off, or when Linux has no EDAC driver for the machine's memory
    /// controller.
    pub dimm_modes: Vec<String>,
    /// The errors that ECC corrected, on all memory controllers, since their
    /// counters were reset (when their driver loaded, normally at boot).
    pub corrected_errors: u64,
    /// The errors that ECC found but couldn't correct, counted likewise.
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
    /// corrects errors; `Some(false)` if some DIMM's mode doesn't, or if no
    /// memory controller is registered; otherwise `None`, as the driver
    /// doesn't say.
    pub fn ecc(&self) -> Option<bool> {
        let corrects: Vec<_> = self.dimm_modes.iter().map(|mode| corrects(mode)).collect();
        if corrects.is_empty() || corrects.contains(&Some(false)) {
            Some(false)
        } else if corrects.contains(&None) {
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

/// Parses what [`READ_FILES`] prints.
fn parse(output: &str) -> Result<Edac> {
    let mut edac = Edac::default();
    for line in output.lines() {
        let context = || format!("unexpected EDAC line {line:?}");
        let (path, value) = line.split_once(' ').ok_or_else(|| anyhow!(context()))?;
        let count = || value.parse::<u64>().with_context(context);
        match path.rsplit_once('/').map(|(_, name)| name) {
            Some("dimm_edac_mode") => edac.dimm_modes.push(value.to_string()),
            Some("ce_count") => edac.corrected_errors += count()?,
            Some("ue_count") => edac.uncorrected_errors += count()?,
            _ => bail!(context()),
        }
    }
    Ok(edac)
}

/// What the EDAC memory controllers say about the RAM of the machine at the
/// other end of `session`.
pub fn read(session: &mut Session) -> Result<Edac> {
    parse(&session.run_ok(READ_FILES, QUICK)?)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn rejects_garbage() {
        for output in ["SECDED", "/sys/devices/system/edac/mc/mc0/size_mb 4096", "/sys/devices/system/edac/mc/mc0/ce_count", "/sys/devices/system/edac/mc/mc0/ue_count -1"] {
            assert!(parse(output).is_err(), "{output:?}");
        }
    }

    #[test]
    fn knows_whether_ecc_is_on() {
        assert_eq!(modes(&[]).ecc(), Some(false));
        assert_eq!(modes(&["SECDED", "S8ECD8ED"]).ecc(), Some(true));
        assert_eq!(modes(&["SECDED", "None"]).ecc(), Some(false));
        assert_eq!(modes(&["PARITY"]).ecc(), Some(false));
        assert_eq!(modes(&["Unknown", "SECDED"]).ecc(), None);
        assert_eq!(modes(&["Unknown", "None"]).ecc(), Some(false));
        assert_eq!(modes(&["Reserved"]).ecc(), None);
    }
}
