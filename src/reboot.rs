// Model-output: Claude Opus 5.5
// Model-output: Claude Fable 5.1

//! Taking a remote machine down.

use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Result, bail};
use std::time::Duration;
use tracing::info;

/// What [`stop_unit`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The unit is stopped, and systemd's result for it is "success".
    Cleanly,
    /// The unit is stopped, but systemd's result for it is this instead of
    /// "success": e.g. "timeout" if systemd had to kill it, or whatever made
    /// it fail before.  (systemctl stop succeeds either way.)
    Uncleanly(String),
    /// There's no such unit on the machine.
    NotLoaded,
}

/// Stops systemd unit `unit` (e.g. "postgresql.service"), waiting up to
/// `timeout` for it to stop.
pub fn stop_unit(session: &mut Session, unit: &str, timeout: Duration) -> Result<Stopped> {
    let unit_word = shell_quote(unit);
    let output = session.run(&format!("systemctl stop -- {unit_word}"), timeout)?;
    match output.status {
        0 => {}
        // "program is not installed", in systemctl's LSB-style exit codes
        5 => return Ok(Stopped::NotLoaded),
        status => bail!("systemctl stop {unit} exited with status {status}: {}", output.stderr_text()),
    }
    let result = session.run_ok(&format!("systemctl show --property=Result --value -- {unit_word}"), QUICK)?;
    Ok(match result.trim_end() {
        "success" => Stopped::Cleanly,
        other => Stopped::Uncleanly(other.to_string()),
    })
}

/// A way to take a machine down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Down {
    Reboot,
    /// Shut down and powered off
    Shutdown,
}

impl Down {
    /// For people, as in "okay to reboot" or "okay to shut down".
    pub fn verb(self) -> &'static str {
        match self {
            Down::Reboot   => "reboot",
            Down::Shutdown => "shut down",
        }
    }

    /// For people, as in "Not rebooting" or "Not shutting down".
    pub fn gerund(self) -> &'static str {
        match self {
            Down::Reboot   => "rebooting",
            Down::Shutdown => "shutting down",
        }
    }

    /// The systemctl command that asks for it.
    fn command(self) -> &'static str {
        match self {
            Down::Reboot   => "systemctl reboot",
            Down::Shutdown => "systemctl poweroff",
        }
    }
}

/// Asks systemd to take the machine `down`.  The connection often closes
/// before systemctl can report back, which is fine, so success here doesn't
/// prove the machine is going down: see afterwards (for a reboot, compare
/// boot IDs).  Fails if the request can't have reached the machine, or
/// systemctl failed or hung.
pub fn ask(mut session: Session, down: Down) -> Result<()> {
    let command = down.command();
    match session.run_or_disconnect(command, Duration::from_secs(60))? {
        Some(output) if output.status == 0 => Ok(()),
        Some(output) => bail!("{command} exited with status {}: {}", output.status, output.stderr_text()),
        None => {
            info!("the connection closed after asking to {}, as expected", down.verb());
            Ok(())
        }
    }
}
