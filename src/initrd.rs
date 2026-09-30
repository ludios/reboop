// Model-output: Claude Opus 5.5

//! Finding the LUKS devices a machine's initrd will ask to unlock, testing
//! the password beforehand, and unlocking them over SSH to the systemd initrd.

use crate::child::{self, ChildProcess};
use crate::deadline::{Deadline, Permanent};
use crate::ssh::{QUICK, Session, Ssh, Target, is_permanent_failure, sh_c};
use anyhow::{Result, anyhow, bail};
use std::fmt;
use std::time::Duration;
use tracing::info;

/// The end of systemd's passphrase prompts.  systemd stops echoing input
/// right after printing it.
const PROMPT_END: &str = "(press TAB for no echo)";

/// How long to wait after answering a prompt for either the connection to
/// close (the initrd switching to the real root) or the prompt to come back.
const SETTLE: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum UnlockError {
    /// ssh exited before the remote end printed anything, e.g. because the
    /// initrd's sshd isn't up yet.
    Unreachable(String),
    /// A prompt came back after we answered it.  systemd-cryptsetup gives up
    /// after 3 wrong passphrases, so we don't try again.
    WrongPassword { prompt: String },
    Other(anyhow::Error),
}

impl fmt::Display for UnlockError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UnlockError::Unreachable(stderr) => write!(f, "couldn't reach the initrd: {stderr}"),
            UnlockError::WrongPassword { prompt } => write!(f, "the password was rejected; the initrd asked again: {prompt:?}"),
            UnlockError::Other(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for UnlockError {}

impl From<anyhow::Error> for UnlockError {
    fn from(error: anyhow::Error) -> Self {
        UnlockError::Other(error)
    }
}

/// Checks that `password` will get through a terminal intact.
pub fn check_password(password: &str) -> Result<()> {
    if password.is_empty() {
        bail!("the password is empty");
    }
    if password.chars().any(char::is_control) {
        bail!("the password contains control characters, which a terminal would interpret");
    }
    Ok(())
}

/// Sets $root to the device mounted at /, and the positional parameters to
/// the LUKS devices beneath it, each once (lsblk lists a device once per
/// path to it).  Every step is its own command so that under `set -e`, a
/// failure can't pass for a lack of LUKS devices.
const FIND_LUKS_DEVICES: &str = r#"
set -euf
root=$(findmnt -nvo SOURCE /)
tree=$(lsblk -rsnpo PATH,FSTYPE "$root")
devices=$(printf '%s\n' "$tree" | awk '$2 == "crypto_LUKS" && !seen[$1]++ { print $1 }')
set -- $devices
"#;

/// The LUKS devices beneath / on the machine at the other end of `session`,
/// which are what its initrd will ask to unlock.
pub fn luks_devices(session: &mut Session) -> Result<Vec<String>> {
    let script = format!(r#"{FIND_LUKS_DEVICES} for device; do echo "$device"; done"#);
    Ok(session.run_ok(&script, QUICK)?.lines().map(str::to_string).collect())
}

/// After [`FIND_LUKS_DEVICES`], checks that there's one LUKS device, prints
/// "reboop-luks-device DEVICE", and tests whether the password on stdin
/// opens it.  (cryptsetup would try PIN-less tokens, like a TPM2's, before
/// the password.)
const TEST_LUKS_PASSWORD: &str = r#"
if [ $# -ne 1 ]; then
    echo "expected one LUKS device beneath / ($root), found $#: $*" >&2
    exit 1
fi
echo "reboop-luks-device $1"
exec cryptsetup luksOpen --test-passphrase --disable-external-tokens --key-file=- "$1"
"#;

/// Tests whether `password` opens the LUKS device beneath / on the booted
/// machine at `target`, which is the device its initrd asks to unlock.
/// Returns the device's path and whether the password opens it.
///
/// Only connects to hosts whose key is already in known_hosts, whatever the
/// user's ssh config says, since we're sending a secret.
pub fn test_luks_password(ssh: &Ssh, target: &Target, password: &str, deadline: Deadline) -> Result<(String, bool)> {
    check_password(password)?;
    let script = format!("{FIND_LUKS_DEVICES}{TEST_LUKS_PASSWORD}");
    let command = ssh.command(target, &["-T", "-o", "StrictHostKeyChecking=yes"], &sh_c(&script));
    let output = child::run(command, password.as_bytes(), deadline)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let device = stdout.lines().find_map(|line| line.strip_prefix("reboop-luks-device "));
    match (device, output.status.code()) {
        (Some(device), Some(0)) => Ok((device.into(), true)),
        // cryptsetup's "No permission (bad passphrase)"
        (Some(device), Some(2)) => Ok((device.into(), false)),
        _ => bail!(
            "failed to test the LUKS password on {target} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

/// Removes terminal escape sequences, which systemd uses for colors.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then a final byte in @..~
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: ends with BEL or ESC \
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The prompt at the end of `screen` (the text before [`PROMPT_END`] on the
/// last line), if the last line is a complete passphrase prompt.
fn find_prompt(screen: &[u8]) -> Option<String> {
    let text = strip_escapes(&String::from_utf8_lossy(screen));
    let last_line = text.rsplit('\n').next()?.trim();
    Some(last_line.strip_suffix(PROMPT_END)?.trim().to_string())
}

/// `screen` as text, for error messages, without `password` in case the
/// terminal echoed it.
fn redact(screen: &[u8], password: &str) -> String {
    String::from_utf8_lossy(screen).replace(password, "<password>")
}

/// Connects to the initrd's sshd at `target`, runs systemd's password agent
/// there, and answers each passphrase prompt with `password`.
///
/// Returns the prompts it answered once the connection closes, or once
/// [`SETTLE`] (or `deadline`) passes after the last answer with no new
/// prompt.  Fails if a prompt comes back, since that means `password` is
/// wrong.
///
/// Only connects to hosts whose key is already in known_hosts, whatever the
/// user's ssh config says, since we're sending a secret.
pub fn unlock(ssh: &Ssh, target: &Target, password: &str, deadline: Deadline) -> Result<Vec<String>, UnlockError> {
    check_password(password)?;
    // --watch rather than the default --query, which exits if the initrd
    // hasn't asked for a passphrase yet.
    let command = ssh.command(
        target,
        &["-tt", "-o", "EscapeChar=none", "-o", "StrictHostKeyChecking=yes"],
        "systemd-tty-ask-password-agent --watch",
    );
    let mut child = ChildProcess::spawn(command)?;
    // Output since the last answer
    let mut screen = Vec::new();
    let mut received_output = false;
    let mut answered: Vec<String> = Vec::new();
    let mut settled = None;

    loop {
        let wait_until = settled.map_or(deadline, |settled: Deadline| settled.min(deadline));
        match child.read_stdout(wait_until) {
            Ok(Some(chunk)) => {
                received_output = true;
                screen.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) if !answered.is_empty() => {
                info!("no new prompt since the last answer; assuming the initrd is done with us");
                return Ok(answered);
            }
            Err(error) => {
                let screen = redact(&screen, password);
                return Err(error.context(format!("waiting for a passphrase prompt from {target}; output so far: {screen:?}")).into());
            }
        }
        if let Some(prompt) = find_prompt(&screen) {
            if answered.contains(&prompt) {
                return Err(UnlockError::WrongPassword { prompt });
            }
            info!("answering {prompt:?}");
            child.write_stdin(format!("{password}\n").as_bytes(), deadline)?;
            answered.push(prompt);
            screen.clear();
            settled = Some(Deadline::after(SETTLE));
        }
    }

    let stderr = child.finish(deadline).map(|(_, stderr)| stderr).unwrap_or_default();
    if !answered.is_empty() {
        Ok(answered)
    } else if !received_output && is_permanent_failure(&stderr) {
        Err(anyhow::Error::new(Permanent(format!("couldn't log in to the initrd at {target}: {stderr}"))).into())
    } else if !received_output {
        Err(UnlockError::Unreachable(stderr))
    } else {
        let screen = redact(&screen, password);
        Err(anyhow!("the connection to {target} closed before any passphrase prompt; output: {screen:?} {stderr}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "\x1b[0;1;39m🔐 Please enter passphrase for disk root: \x1b[0;38;5;245m(press TAB for no echo) \x1b[0m";

    #[test]
    fn finds_complete_prompts_only() {
        assert_eq!(find_prompt(PROMPT.as_bytes()).as_deref(), Some("🔐 Please enter passphrase for disk root:"));
        let partial = PROMPT.split("(press").next().unwrap();
        assert_eq!(find_prompt(partial.as_bytes()), None);
        assert_eq!(find_prompt(b""), None);
        // A prompt that was already answered, with the echoed asterisks
        let answered = format!("{PROMPT}\x08 \x08\x08 \x08*****\r\n");
        assert_eq!(find_prompt(answered.as_bytes()), None);
        let asked_again = format!("{answered}{PROMPT}");
        assert_eq!(find_prompt(asked_again.as_bytes()).as_deref(), Some("🔐 Please enter passphrase for disk root:"));
    }

    #[test]
    fn strips_escapes() {
        assert_eq!(strip_escapes("a\x1b[1;2mb\x1b]0;title\x07c\x1b]8;;x\x1b\\d"), "abcd");
    }

    #[test]
    fn checks_passwords() {
        assert!(check_password("correct horse ~. battery").is_ok());
        assert!(check_password("").is_err());
        assert!(check_password("two\nlines").is_err());
        assert!(check_password("ctrl\x03c").is_err());
    }
}
