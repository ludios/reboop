//! Taking a remote machine down.

use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Result, bail, ensure};
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

/// The units that trigger systemd unit `unit` (its TriggeredBy=, such as
/// socket, timer and path units) and that could start it now: those that
/// aren't inactive or failed.
fn active_triggers(session: &mut Session, unit: &str) -> Result<Vec<String>> {
    let triggers = session.run_ok(&format!("systemctl show --property=TriggeredBy --value -- {}", shell_quote(unit)), QUICK)?;
    let triggers: Vec<&str> = triggers.split_whitespace().collect();
    if triggers.is_empty() {
        return Ok(Vec::new());
    }
    let words: Vec<String> = triggers.iter().map(|trigger| shell_quote(trigger)).collect();
    // This exits with 0 only if one of them is active, but prints the state
    // of each, in order, either way.
    let output = session.run(&format!("systemctl is-active -- {}", words.join(" ")), QUICK)?;
    let states = output.stdout_text();
    let states: Vec<&str> = states.lines().collect();
    ensure!(
        states.len() == triggers.len(),
        "systemctl is-active {} exited with status {} and printed {states:?}: {}",
        triggers.join(" "),
        output.status,
        output.stderr_text()
    );
    let active = triggers.iter().zip(states).filter(|(_, state)| !matches!(*state, "inactive" | "failed"));
    Ok(active.map(|(trigger, _)| trigger.to_string()).collect())
}

/// Stops systemd unit `unit` (e.g. "postgresql.service"), along with the
/// active units that trigger it (see [`active_triggers`]), waiting up to
/// `timeout` for them all to stop.  It stops the triggers because a socket
/// that's still listening would start `unit` again once a client connects,
/// and that start cancels the stop if `unit` is still stopping.  systemctl
/// stops them all in one transaction (with systemd 262 or later; before
/// that, in one transaction per unit), and in that transaction, a socket,
/// timer or path unit whose own stop is pending doesn't start `unit`.
///
/// Returns how `unit` stopped (or that the machine has no such unit), and
/// the triggering units that it stopped too.
pub fn stop_unit(session: &mut Session, unit: &str, timeout: Duration) -> Result<(Stopped, Vec<String>)> {
    let triggers = active_triggers(session, unit)?;
    let mut units: Vec<&str> = triggers.iter().map(String::as_str).collect();
    units.push(unit);
    let words: Vec<String> = units.iter().map(|name| shell_quote(name)).collect();
    let output = session.run(&format!("systemctl stop -- {}", words.join(" ")), timeout)?;
    match output.status {
        0 => {}
        // "program is not installed", in systemctl's LSB-style exit codes.
        // That's `unit`, as the triggers are active, and so loaded.
        5 => {
            ensure!(triggers.is_empty(), "systemctl stop says there's no {unit}, though {} would start it", triggers.join(", "));
            return Ok((Stopped::NotLoaded, Vec::new()));
        }
        status => bail!("systemctl stop {} exited with status {status}: {}", units.join(" "), output.stderr_text()),
    }
    let result = session.run_ok(&format!("systemctl show --property=Result --value -- {}", shell_quote(unit)), QUICK)?;
    let stopped = match result.trim_end() {
        "success" => Stopped::Cleanly,
        other => Stopped::Uncleanly(other.to_string()),
    };
    Ok((stopped, triggers))
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
    let Some(output) = session.run_or_disconnect(command, Duration::from_secs(60))? else {
        info!("the connection closed after asking to {}, as expected", down.verb());
        return Ok(());
    };
    ensure!(output.status == 0, "{command} exited with status {}: {}", output.status, output.stderr_text());
    Ok(())
}
