// Model-output: Claude Opus 5.5

//! Logging in to machines as root with the ssh binary (so the user's ssh
//! config, agent and known_hosts all apply), and running commands there.

use crate::child::ChildProcess;
use crate::deadline::{Deadline, Permanent, retry};
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use std::fmt;
use std::process::Command;
use std::time::Duration;

/// A timeout for commands that should finish right away.
pub const QUICK: Duration = Duration::from_secs(30);

/// How long to let [`Session::open`] take, which is long enough to
/// authenticate with a key that needs a touch.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(60);

/// Quotes `s` as a single word for sh(1).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A remote command that has /bin/sh run `script`.  Base64 keeps the script
/// intact through root's login shell, whatever that is.
pub fn sh_c(script: &str) -> String {
    format!(r#"exec /bin/sh -c "$(printf %s '{}' | base64 -d)""#, BASE64.encode(script))
}

/// Whether ssh's `stderr` shows a failure that trying again won't fix: the
/// server's host key isn't the known one.  (Not "Permission denied", which
/// can also mean the user missed touching their key.)
pub fn is_permanent_failure(stderr: &str) -> bool {
    ["Host key verification failed", "REMOTE HOST IDENTIFICATION HAS CHANGED"]
        .iter()
        .any(|message| stderr.contains(message))
}

/// An SSH server to log in to as root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// What ssh calls the machine, which picks the `Host` stanzas of the
    /// user's ssh config that apply.
    pub name: String,
    /// Where to connect, whatever the ssh config says.  known_hosts entries
    /// are looked up by this, as when a `Host` stanza sets HostName.
    pub address: String,
    pub port: u16,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "root@{} ({}) port {}", self.name, self.address, self.port)
    }
}

/// How to run ssh(1).
#[derive(Clone, Debug, Default)]
pub struct Ssh {
    /// Arguments for every ssh invocation, placed before the destination;
    /// e.g. `-F some_config`.
    pub extra_args: Vec<String>,
}

impl Ssh {
    /// An ssh command that logs in to `target` and has root's login shell
    /// run `remote_command`, with extra ssh `options`.
    ///
    /// The options set here keep a dead connection from hanging around:
    /// keepalives notice a network that disappeared without closing the
    /// connection (as when an initrd switches root), and ControlPath=none
    /// keeps a ControlMaster in the user's config from reusing a connection
    /// that died with the previous boot.
    pub fn command(&self, target: &Target, options: &[&str], remote_command: &str) -> Command {
        let mut command = Command::new("ssh");
        for option in ["BatchMode=yes", "ConnectTimeout=15", "ServerAliveInterval=5", "ServerAliveCountMax=3", "ControlPath=none"] {
            command.args(["-o", option]);
        }
        command
            .args(options)
            .args(&self.extra_args)
            .args(["-o", &format!("HostName={}", target.address)])
            .args(["-p", &target.port.to_string()])
            .arg(format!("root@{}", target.name))
            .arg("--")
            .arg(remote_command);
        command
    }
}

/// What a command run in a [`Session`] did.
#[derive(Clone, Debug)]
pub struct CommandOutput {
    /// The exit status, or 128 + the signal number if a signal killed it.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim_end().to_string()
    }
}

/// Line printed by [`SESSION_SHELL`] once it's ready for commands.
const SESSION_READY: &[u8] = b"reboop-session-ready";

/// The remote end of a [`Session`], run by /bin/sh.  It reads one command per
/// line, base64-encoded, runs it with /bin/sh (stdin from /dev/null so it
/// can't eat the commands that follow), and answers with a line of
/// "STATUS BASE64_STDOUT BASE64_STDERR".  Each command gets its own output
/// files, since a background process it leaves behind may write to them
/// later.
const SESSION_SHELL: &str = r#"
t=$(mktemp -d) || exit 1
trap 'rm -rf "$t"' EXIT
# Exit through the EXIT trap even when a signal ends us, e.g. SIGPIPE when a
# command outlives the connection.
trap 'exit 1' HUP PIPE TERM
echo reboop-session-ready
n=0
while IFS= read -r c; do
    n=$((n + 1))
    printf %s "$c" | base64 -d >"$t/$n.sh" || exit 1
    /bin/sh "$t/$n.sh" </dev/null >"$t/$n.out" 2>"$t/$n.err"
    s=$?
    printf '%s %s %s\n' "$s" "$(base64 -w0 <"$t/$n.out")" "$(base64 -w0 <"$t/$n.err")"
    rm -f "$t/$n.sh" "$t/$n.out" "$t/$n.err"
done
"#;

/// Parses a line of [`SESSION_SHELL`]'s output about a command.
fn parse_response(line: &[u8]) -> Result<CommandOutput> {
    let text = std::str::from_utf8(line)?;
    let [status, stdout, stderr] = text.split(' ').collect::<Vec<_>>()[..] else {
        bail!("malformed response from the remote shell: {text:?}");
    };
    Ok(CommandOutput {
        status: status.parse().with_context(|| format!("malformed status in {text:?}"))?,
        stdout: BASE64.decode(stdout)?,
        stderr: BASE64.decode(stderr)?,
    })
}

/// A root shell on a remote machine that runs commands one at a time over a
/// single SSH connection.  (Some users' keys need a touch per connection.)
///
/// Commands are sh(1) scripts that don't depend on root's login shell.  A
/// command that times out breaks the session, since it may still be running.
pub struct Session {
    ssh: ChildProcess,
    target: Target,
    /// Output received but not yet consumed as lines.
    pending: Vec<u8>,
    /// How much of `pending` is known to have no newline.
    scanned: usize,
    broken: bool,
}

impl Session {
    /// Returns the next line of output without its newline, or `None` if ssh
    /// closed its stdout.
    fn read_line(&mut self, deadline: Deadline) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(offset) = self.pending[self.scanned..].iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=self.scanned + offset).collect();
                line.pop();
                self.scanned = 0;
                return Ok(Some(line));
            }
            self.scanned = self.pending.len();
            match self.ssh.read_stdout(deadline)? {
                Some(chunk) => self.pending.extend_from_slice(&chunk),
                None => return Ok(None),
            }
        }
    }

    /// Logs in to `target` and waits until it's ready to run commands.
    /// Failures that trying again won't fix are [`Permanent`].
    pub fn open(ssh: &Ssh, target: &Target, timeout: Duration) -> Result<Session> {
        let deadline = Deadline::after(timeout);
        let child = ChildProcess::spawn(ssh.command(target, &["-T"], &sh_c(SESSION_SHELL)))?;
        let mut session = Session { ssh: child, target: target.clone(), pending: Vec::new(), scanned: 0, broken: false };
        loop {
            // Skip anything that root's shell startup files print.
            match session.read_line(deadline).with_context(|| format!("failed to open a session to {target}"))? {
                Some(line) if line == SESSION_READY => return Ok(session),
                Some(_) => {}
                None => {
                    let stderr = session.ssh.finish(deadline).map(|(_, stderr)| stderr).unwrap_or_default();
                    let message = format!("failed to open a session to {target}: {stderr}");
                    if is_permanent_failure(&stderr) {
                        return Err(Permanent(message).into());
                    }
                    bail!(message);
                }
            }
        }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Sends `script` and waits for its result, which is `None` if the
    /// connection closed first.
    fn exchange(&mut self, script: &str, deadline: Deadline) -> Result<Option<CommandOutput>> {
        self.ssh.write_stdin(format!("{}\n", BASE64.encode(script)).as_bytes(), deadline)?;
        match self.read_line(deadline)? {
            Some(line) => Ok(Some(parse_response(&line)?)),
            None => {
                // So that ssh's stderr is complete
                let _ = self.ssh.finish(deadline);
                Ok(None)
            }
        }
    }

    /// Like [`Session::run`], but for a command that may end the connection,
    /// like a reboot: returns `None` if the connection closed after the
    /// command was sent but before its result came back.
    pub fn run_or_disconnect(&mut self, script: &str, timeout: Duration) -> Result<Option<CommandOutput>> {
        ensure!(!self.broken, "the session to {} broke earlier", self.target);
        let result = self.exchange(script, Deadline::after(timeout));
        if !matches!(result, Ok(Some(_))) {
            self.broken = true;
            self.ssh.kill();
        }
        result.with_context(|| format!("failed to run {script:?} on {}", self.target))
    }

    /// Runs `script` with /bin/sh on the remote machine.
    pub fn run(&mut self, script: &str, timeout: Duration) -> Result<CommandOutput> {
        match self.run_or_disconnect(script, timeout)? {
            Some(output) => Ok(output),
            None => bail!("the connection to {} closed while running {script:?}: {}", self.target, self.ssh.stderr()),
        }
    }

    /// Like [`Session::run`], but fails unless the command exits 0, and
    /// returns its stdout, lossily decoded.
    pub fn run_ok(&mut self, script: &str, timeout: Duration) -> Result<String> {
        let output = self.run(script, timeout)?;
        ensure!(
            output.status == 0,
            "{script:?} on {} exited with status {}: {}",
            self.target,
            output.status,
            output.stderr_text()
        );
        Ok(output.stdout_text())
    }
}

/// Tries to open a session to `target` once per `interval` until one
/// succeeds, `deadline` passes, or a failure is [`Permanent`].
pub fn wait_for_session(ssh: &Ssh, target: &Target, interval: Duration, deadline: Deadline) -> Result<Session> {
    retry(deadline, interval, |deadline| Session::open(ssh, target, deadline.at_most(OPEN_TIMEOUT).remaining()))
        .with_context(|| format!("couldn't open a session to {target}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_survives_sh() {
        let tricky = r#"it's a "test" with $HOME, `backticks`, \ and newline
"#;
        let output = std::process::Command::new("sh").arg("-c").arg(format!("printf %s {}", shell_quote(tricky))).output().unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), tricky);
    }

    #[test]
    fn command_dials_the_address_under_the_name() {
        let target = Target { name: "one".into(), address: "10.0.0.1".into(), port: 904 };
        let command = Ssh { extra_args: vec!["-F".into(), "config".into()] }.command(&target, &["-T"], "true");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_str().unwrap()).collect();
        let tail = ["-T", "-F", "config", "-o", "HostName=10.0.0.1", "-p", "904", "root@one", "--", "true"];
        assert_eq!(args[args.len() - tail.len()..], tail);
    }

    #[test]
    fn parses_responses() {
        let output = parse_response(b"3 aGk= ").unwrap();
        assert_eq!((output.status, &output.stdout[..], &output.stderr[..]), (3, &b"hi"[..], &b""[..]));
        assert!(parse_response(b"0 aGk=").is_err());
        assert!(parse_response(b"x  ").is_err());
    }

    #[test]
    fn session_shell_protocol() {
        // Run the remote end locally to check the protocol itself.
        let mut child = std::process::Command::new("sh")
            .args(["-c", SESSION_SHELL])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        let scripts = [
            "printf 'a\\nb'; echo oops >&2; exit 7",
            "cat; echo done",
            "",
            // A background process that writes after its command is done
            "(sleep 0.2; echo a late write, longer than the next output; echo late >&2) &",
            "sleep 0.5; echo on time",
        ];
        for script in scripts {
            writeln!(stdin, "{}", BASE64.encode(script)).unwrap();
        }
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0].as_bytes(), SESSION_READY);
        let first = parse_response(lines[1].as_bytes()).unwrap();
        assert_eq!((first.status, first.stdout_text().as_str(), first.stderr_text().as_str()), (7, "a\nb", "oops"));
        let second = parse_response(lines[2].as_bytes()).unwrap();
        assert_eq!((second.status, second.stdout_text().as_str()), (0, "done\n"));
        let third = parse_response(lines[3].as_bytes()).unwrap();
        assert_eq!((third.status, third.stdout.len()), (0, 0));
        let fifth = parse_response(lines[5].as_bytes()).unwrap();
        assert_eq!((fifth.stdout_text().as_str(), fifth.stderr.len()), ("on time\n", 0));
    }
}
