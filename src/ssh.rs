// Model-output: Claude Opus 5.5

//! Logging in to machines as root with the ssh binary (so the user's ssh
//! config, agent and known_hosts all apply), and running commands there.

use crate::child::ChildProcess;
use crate::deadline::{Deadline, retry};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use std::fmt;
use std::process::Command;
use std::time::Duration;

/// How to run ssh(1).
#[derive(Clone, Debug, Default)]
pub struct Ssh {
    /// Arguments for every ssh invocation, placed before the destination;
    /// e.g. `-F some_config`.
    pub extra_args: Vec<String>,
}

/// An SSH server to log in to as root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "root@{} port {}", self.host, self.port)
    }
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
            .arg("-p")
            .arg(target.port.to_string())
            .arg(format!("root@{}", target.host))
            .arg("--")
            .arg(remote_command);
        command
    }
}

/// Quotes `s` as a single word for sh(1).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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
/// "STATUS BASE64_STDOUT BASE64_STDERR".
const SESSION_SHELL: &str = r#"
t=$(mktemp -d) || exit 1
trap 'rm -rf "$t"' EXIT
echo reboop-session-ready
while IFS= read -r c; do
    printf %s "$c" | base64 -d >"$t/c" || exit 1
    /bin/sh "$t/c" </dev/null >"$t/o" 2>"$t/e"
    s=$?
    printf '%s %s %s\n' "$s" "$(base64 -w0 <"$t/o")" "$(base64 -w0 <"$t/e")"
done
"#;

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
    broken: bool,
}

impl Session {
    /// Logs in to `target` and waits until it's ready to run commands.
    pub fn open(ssh: &Ssh, target: &Target, timeout: Duration) -> Result<Session> {
        let deadline = Deadline::after(timeout);
        // Base64 keeps the script intact through root's login shell, whatever
        // that is.
        let remote_command = format!(r#"exec /bin/sh -c "$(printf %s '{}' | base64 -d)""#, BASE64.encode(SESSION_SHELL));
        let child = ChildProcess::spawn(ssh.command(target, &["-T"], &remote_command))?;
        let mut session = Session { ssh: child, target: target.clone(), pending: Vec::new(), broken: false };
        loop {
            // Skip anything that root's shell startup files print.
            match session.read_line(deadline).with_context(|| format!("failed to open a session to {target}"))? {
                Some(line) if line == SESSION_READY => return Ok(session),
                Some(_) => {}
                None => {
                    let stderr = session.ssh.finish(deadline).map(|(_, stderr)| stderr).unwrap_or_default();
                    bail!("failed to open a session to {target}: {stderr}");
                }
            }
        }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Returns the next line of output without its newline, or `None` if ssh
    /// closed its stdout.
    fn read_line(&mut self, deadline: Deadline) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(newline) = self.pending.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=newline).collect();
                line.pop();
                return Ok(Some(line));
            }
            match self.ssh.read_stdout(deadline)? {
                Some(chunk) => self.pending.extend_from_slice(&chunk),
                None => return Ok(None),
            }
        }
    }

    /// Runs `script` with /bin/sh on the remote machine.
    pub fn run(&mut self, script: &str, timeout: Duration) -> Result<CommandOutput> {
        ensure!(!self.broken, "the session to {} broke earlier", self.target);
        let result = self.run_inner(script, Deadline::after(timeout));
        if result.is_err() {
            self.broken = true;
            self.ssh.kill();
        }
        result.with_context(|| format!("failed to run {script:?} on {}", self.target))
    }

    fn run_inner(&mut self, script: &str, deadline: Deadline) -> Result<CommandOutput> {
        self.ssh.write_stdin(format!("{}\n", BASE64.encode(script)).as_bytes())?;
        let Some(line) = self.read_line(deadline)? else {
            let stderr = self.ssh.finish(deadline).map(|(_, stderr)| stderr).unwrap_or_default();
            bail!("the connection closed: {stderr}");
        };
        parse_response(&line)
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

/// Tries to open a session to `target` once per `interval` (each attempt
/// allowed up to `interval` too) until one succeeds or `deadline` passes.
pub fn wait_for_session(ssh: &Ssh, target: &Target, interval: Duration, deadline: Deadline) -> Result<Session> {
    retry(deadline, interval, |deadline| Session::open(ssh, target, deadline.at_most(interval).remaining()))
        .with_context(|| format!("couldn't open a session to {target}"))
}

/// Parses a store path printed by a command, checking that it is one.
pub fn store_path(output: &str) -> Result<String> {
    let path = output.trim_end_matches('\n');
    let name = path.strip_prefix("/nix/store/").ok_or_else(|| anyhow!("not a store path: {path:?}"))?;
    ensure!(!name.is_empty() && !name.contains('/'), "not a store path: {path:?}");
    Ok(path.to_string())
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
        for script in ["printf 'a\\nb'; echo oops >&2; exit 7", "cat; echo done", ""] {
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
    }

    #[test]
    fn store_paths() {
        assert_eq!(store_path("/nix/store/abc-foo\n").unwrap(), "/nix/store/abc-foo");
        assert!(store_path("/nix/store/abc-foo/bin").is_err());
        assert!(store_path("/run/current-system").is_err());
    }
}
