// Model-output: Claude Opus 5.5

//! Finding processes that make rebooting a bad idea.

use crate::ssh::Session;
use anyhow::{Context, Result, anyhow};
use std::time::Duration;

/// A process, as listed by ps(1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    /// The user name, or the numeric uid if the user has no name.
    pub user: String,
    /// The command line, with arguments joined by spaces.
    pub args: String,
}

/// Something going on that a reboot would interrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Activity {
    /// A Nix command, a remote build or copy being served, or a builder.
    Nix,
    SwitchToConfiguration,
    Tmux,
    Rsync,
}

/// Programs that are Nix clients.  (Not nix-daemon itself: it has a worker
/// process per client connection, and some services stay connected.)
const NIX_PROGRAMS: &[&str] = &[
    "nix",
    "nix-build",
    "nix-channel",
    "nix-collect-garbage",
    "nix-copy-closure",
    "nix-env",
    "nix-instantiate",
    "nix-prefetch-url",
    "nix-shell",
    "nix-store",
    "nixos-install",
    "nixos-rebuild",
];

/// Lists all processes on the remote machine.
pub fn list(session: &mut Session) -> Result<Vec<Process>> {
    parse_ps(&session.run_ok("ps -e -ww -o pid=,ppid=,user:64=,args=", Duration::from_secs(30))?)
}

fn parse_ps(output: &str) -> Result<Vec<Process>> {
    output
        .lines()
        .map(|line| {
            let mut rest = line.trim_start();
            let mut field = || {
                let (value, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                rest = after.trim_start();
                value
            };
            let (pid, ppid, user) = (field(), field(), field());
            Ok(Process { pid: pid.parse()?, ppid: ppid.parse()?, user: user.to_string(), args: rest.to_string() })
        })
        .collect::<Result<_>>()
        .with_context(|| anyhow!("unexpected ps output: {output:?}"))
}

/// What `process` is doing that a reboot would interrupt, if anything.
pub fn activity(process: &Process) -> Option<Activity> {
    // Builders run as the nixbld users.
    if process.user.starts_with("nixbld") {
        return Some(Activity::Nix);
    }
    let names = program_names(&process.args);
    let is = |name: &str| names.iter().any(|n| n == name);
    if names.iter().any(|n| NIX_PROGRAMS.contains(&n.as_str())) {
        Some(Activity::Nix)
    } else if is("nix-daemon") && process.args.split_whitespace().any(|arg| arg == "--stdio") {
        // Serving a remote build or copy over ssh.
        Some(Activity::Nix)
    } else if is("switch-to-configuration") {
        Some(Activity::SwitchToConfiguration)
    } else if is("tmux") {
        Some(Activity::Tmux)
    } else if is("rsync") {
        Some(Activity::Rsync)
    } else {
        None
    }
}

/// The names of the program(s) a command line runs: that of its first word,
/// and for an interpreter like bash or python, also that of its script.
/// Names are normalized so that a nixpkgs wrapper's ".foo-wrapped" is "foo".
fn program_names(args: &str) -> Vec<String> {
    let mut words = args.split_whitespace();
    let Some(first) = words.next() else { return vec![] };
    let mut names = vec![program_name(first)];
    let interpreter = matches!(names[0].as_str(), "sh" | "bash" | "dash" | "zsh" | "perl") || names[0].starts_with("python");
    if interpreter && let Some(script) = words.find(|word| !word.starts_with('-')) {
        names.push(program_name(script));
    }
    names
}

fn program_name(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = name.strip_prefix('.').unwrap_or(name);
    let name = name.strip_suffix("-wrapped").unwrap_or(name);
    // e.g. "sshd:" from "sshd: root [priv]"
    name.strip_suffix(':').unwrap_or(name).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(user: &str, args: &str) -> Process {
        Process { pid: 100, ppid: 1, user: user.into(), args: args.into() }
    }

    fn activity_of(user: &str, args: &str) -> Option<Activity> {
        activity(&process(user, args))
    }

    #[test]
    fn parses_ps_output() {
        let output = "      1       0 root                             /run/current-system/systemd/lib/systemd/systemd\n   \
                      2       0 root                             [kthreadd]\n 707120  644808 at          sshd-session: at [postauth]\n";
        let processes = parse_ps(output).unwrap();
        assert_eq!(processes.len(), 3);
        assert_eq!(processes[2], Process { pid: 707120, ppid: 644808, user: "at".into(), args: "sshd-session: at [postauth]".into() });
        assert!(parse_ps("x 1 root foo\n").is_err());
    }

    #[test]
    fn recognizes_nix() {
        for args in [
            "nix-build -A foo",
            "/nix/store/abc-nix-2.34.8/bin/nix build .#foo",
            "nix-store --serve --write",
            "nix-daemon --stdio",
            "/nix/store/abc-python3-3.13.8/bin/python3.13 /nix/store/abc-nixos-rebuild-ng-26.05/bin/.nixos-rebuild-wrapped switch",
            "/nix/store/abc-bash-5.3p9/bin/bash -e /run/current-system/sw/bin/nixos-rebuild boot",
        ] {
            assert_eq!(activity_of("root", args), Some(Activity::Nix), "{args}");
        }
        assert_eq!(activity_of("nixbld3", "/nix/store/abc-bash/bin/bash -e /nix/store/abc-default-builder.sh"), Some(Activity::Nix));
        // The daemon and its workers
        assert_eq!(activity_of("root", "nix-daemon --daemon"), None);
        assert_eq!(activity_of("root", "nix-daemon 774456"), None);
        // Other programs that happen to be named nix-something
        assert_eq!(activity_of("at", "nix-tree"), None);
    }

    #[test]
    fn recognizes_others() {
        let stc = "/nix/store/abc-nixos-system-foo/bin/switch-to-configuration switch";
        assert_eq!(activity_of("root", stc), Some(Activity::SwitchToConfiguration));
        let stc_wrapper = "/nix/store/abc-bash-5.3p9/bin/bash -e /nix/store/abc-nixos-system-foo/bin/switch-to-configuration boot";
        assert_eq!(activity_of("root", stc_wrapper), Some(Activity::SwitchToConfiguration));
        assert_eq!(activity_of("at", "tmux new -s work"), Some(Activity::Tmux));
        assert_eq!(activity_of("root", "rsync --server -logDtpre.iLsfxCIvu . /backup/"), Some(Activity::Rsync));
        for args in ["man tmux", "less /var/log/rsync.log", "[kworker/0:1-events]", "sshd: root [priv]", ""] {
            assert_eq!(activity_of("root", args), None, "{args}");
        }
    }
}
