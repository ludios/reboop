// Model-output: Claude Opus 5.5

//! Taking a remote machine down.

use crate::ssh::{Session, shell_quote};
use anyhow::{Result, bail};
use std::time::Duration;
use tracing::info;

/// What [`stop_unit`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    Stopped,
    /// There's no such unit on the machine.
    NotLoaded,
}

/// Stops systemd unit `unit` (e.g. "postgresql.service"), waiting up to
/// `timeout` for it to stop.
pub fn stop_unit(session: &mut Session, unit: &str, timeout: Duration) -> Result<Stopped> {
    let output = session.run(&format!("systemctl stop -- {}", shell_quote(unit)), timeout)?;
    match output.status {
        0 => Ok(Stopped::Stopped),
        // "program is not installed", in systemctl's LSB-style exit codes
        5 => Ok(Stopped::NotLoaded),
        status => bail!("systemctl stop {unit} exited with status {status}: {}", output.stderr_text()),
    }
}

/// Asks systemd to reboot the machine.  The connection often closes before
/// systemctl can report back, which is fine, so success here doesn't prove
/// the machine is rebooting; compare boot IDs afterwards.  Fails if the
/// request can't have reached the machine, or systemctl failed or hung.
pub fn reboot(mut session: Session) -> Result<()> {
    match session.run_or_disconnect("systemctl reboot", Duration::from_secs(60))? {
        Some(output) if output.status == 0 => Ok(()),
        Some(output) => bail!("systemctl reboot exited with status {}: {}", output.status, output.stderr_text()),
        None => {
            info!("the connection closed after asking for a reboot, as expected");
            Ok(())
        }
    }
}
