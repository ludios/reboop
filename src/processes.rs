//! Finding processes that make rebooting a bad idea.

use crate::ssh::{QUICK, Session};
use anyhow::{Context, Result, anyhow};
use serde::{Serialize, Serializer};
use std::fmt;

/// A process, as listed by ps(1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
    /// Copying or moving files, or writing or copying a disk image, which
    /// can leave a partial copy if interrupted
    Copy,
    /// Interrupting a receive leaves a partial subvolume behind.
    BtrfsSendReceive,
    /// Anything from opening a device to reencrypting it
    Cryptsetup,
    /// Checking, repairing, making, converting, tuning, resizing or
    /// partitioning a filesystem or disk, recovering files from a broken
    /// filesystem, or testing a disk's blocks.  Interrupting some of these
    /// can leave a filesystem or partition table broken.
    DiskTool,
}

impl fmt::Display for Activity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Activity::Nix => "nix",
            Activity::SwitchToConfiguration => "switch-to-configuration",
            Activity::Copy => "copy",
            Activity::BtrfsSendReceive => "btrfs-send-receive",
            Activity::Cryptsetup => "cryptsetup",
            Activity::DiskTool => "disk-tool",
        })
    }
}

/// As its name, which is also what [`fmt::Display`] shows.
impl Serialize for Activity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
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

/// Programs that copy or move files, or write or copy disk images.  (mv
/// copies when rename(2) can't move a file: to another filesystem, or on
/// btrfs, to another subvolume.  The receiving end of a modern scp isn't
/// here: it's sftp-server, which also serves sshfs mounts for as long as
/// they're mounted.)
const COPY_PROGRAMS: &[&str] = &["rsync", "cp", "mv", "scp", "dd", "ddrescue", "pv"];

/// Programs that check, repair, make, convert, tune, resize or partition a
/// filesystem or disk, or test a disk's blocks, besides the fsck.* and
/// mkfs.* ones (see [`is_disk_tool`])
const DISK_TOOLS: &[&str] = &[
    "fsck", "e2fsck", "btrfsck", "xfs_repair", "btrfs-convert", "btrfstune",
    "mkfs", "mke2fs", "resize2fs", "badblocks",
    "fdisk", "cfdisk", "sfdisk", "gdisk", "sgdisk", "parted",
];

/// Whether the program `name` (from [`program_name`]) is one of the
/// [`DISK_TOOLS`], or an fsck.* or mkfs.* one
fn is_disk_tool(name: &str) -> bool {
    DISK_TOOLS.contains(&name) || name.starts_with("fsck.") || name.starts_with("mkfs.")
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
            let pid  = field();
            let ppid = field();
            let user = field();
            Ok(Process { pid: pid.parse()?, ppid: ppid.parse()?, user: user.to_string(), args: rest.to_string() })
        })
        .collect::<Result<_>>()
        .with_context(|| anyhow!("unexpected ps output: {output:?}"))
}

/// Lists all processes on the remote machine.
pub fn list(session: &mut Session) -> Result<Vec<Process>> {
    parse_ps(&session.run_ok("ps -e -ww -o pid=,ppid=,user:64=,args=", QUICK)?)
}

/// The name of the program at `path`, normalized so that a nixpkgs
/// wrapper's ".foo-wrapped" is "foo".
fn program_name(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = name.strip_prefix('.').unwrap_or(name);
    let name = name.strip_suffix("-wrapped").unwrap_or(name);
    // e.g. "sshd:" from "sshd: root [priv]"
    name.strip_suffix(':').unwrap_or(name).to_string()
}

/// Whether `word` is a cluster of short options, like "-c" or "-ec", that
/// includes -c, with which an interpreter runs a command string rather
/// than a script.
fn is_command_string_option(word: &str) -> bool {
    word.strip_prefix('-').is_some_and(|options| options.contains('c') && options.bytes().all(|b| b.is_ascii_lowercase()))
}

/// The names of the program(s) a command line runs: that of its first word,
/// and for an interpreter like bash or python, also that of its script.
/// (Not with a command string given with -c: the interpreter runs each
/// program in it as a process of its own, or execs the program, which
/// replaces the interpreter's command line.)
fn program_names(args: &str) -> Vec<String> {
    let mut words = args.split_whitespace();
    let Some(first) = words.next() else { return vec![] };
    let mut names = vec![program_name(first)];
    let interpreter = matches!(names[0].as_str(), "sh" | "bash" | "dash" | "zsh" | "perl") || names[0].starts_with("python");
    if interpreter {
        for word in words {
            if is_command_string_option(word) {
                break;
            }
            if !word.starts_with('-') {
                names.push(program_name(word));
                break;
            }
        }
    }
    names
}

/// btrfs's top-level commands, as of btrfs-progs 7.3 (leaving out reflink,
/// which only an experimental build has)
const BTRFS_COMMANDS: &[&str] = &[
    "subvolume", "filesystem", "balance", "device", "scrub", "check", "rescue", "restore", "inspect-internal",
    "property", "send", "receive", "quota", "qgroup", "replace", "help", "version",
];

/// The top-level command (e.g. "send") that a btrfs command line runs, if
/// any.  Like btrfs-progs, this takes any unambiguous prefix of a command to
/// mean that command.
fn btrfs_command(args: &str) -> Option<&'static str> {
    let mut words = args.split_whitespace().skip(1);
    while let Some(word) = words.next() {
        match word {
            // A global option that takes a value, maybe abbreviated, as
            // getopt_long allows
            _ if word.len() > 2 && ["--format", "--log", "--param"].iter().any(|option| option.starts_with(word)) => {
                words.next();
            }
            _ if word.starts_with('-') => {}
            prefix => {
                let mut commands = BTRFS_COMMANDS.iter().filter(|command| command.starts_with(prefix));
                let command = commands.next().copied();
                // btrfs refuses an ambiguous prefix.
                return if commands.next().is_none() { command } else { None };
            }
        }
    }
    None
}

/// Whether an rclone command line transfers files, with copy, sync, move or
/// another of `TRANSFERS`, rather than running until stopped, as mount or
/// serve does.  rclone takes options before and after its subcommand, some
/// with a value, so the subcommand is taken to be the first word that's
/// one of these, which an option's value seldom is.  (A path after the
/// subcommand can be, as in `rclone serve http copy`.)
fn is_rclone_transfer(args: &str) -> bool {
    const TRANSFERS: &[&str] = &["copy", "copyto", "copyurl", "sync", "move", "moveto", "bisync", "cat", "rcat", "archive"];
    const LASTING: &[&str] = &["mount", "cmount", "nfsmount", "serve", "rcd"];
    let subcommand = args.split_whitespace().skip(1).find(|word| TRANSFERS.contains(word) || LASTING.contains(word));
    subcommand.is_some_and(|word| TRANSFERS.contains(&word))
}

/// What `process` is doing that a reboot would interrupt, if anything.
pub fn activity(process: &Process) -> Option<Activity> {
    // Builders run as the nixbld users.
    if process.user.starts_with("nixbld") {
        return Some(Activity::Nix);
    }
    let names = program_names(&process.args);
    let is = |name: &str| names.iter().any(|n| n == name);
    let is_any = |programs: &[&str]| names.iter().any(|n| programs.contains(&n.as_str()));
    let btrfs = if is("btrfs") { btrfs_command(&process.args) } else { None };
    if is_any(NIX_PROGRAMS) {
        Some(Activity::Nix)
    } else if is("nix-daemon") && process.args.split_whitespace().any(|arg| arg == "--stdio") {
        // Serving a remote build or copy over ssh.
        Some(Activity::Nix)
    } else if is("switch-to-configuration") {
        Some(Activity::SwitchToConfiguration)
    } else if is_any(COPY_PROGRAMS) || (is("rclone") && is_rclone_transfer(&process.args)) {
        Some(Activity::Copy)
    } else if matches!(btrfs, Some("send" | "receive")) {
        Some(Activity::BtrfsSendReceive)
    } else if is("cryptsetup") || is("cryptsetup-reencrypt") {
        Some(Activity::Cryptsetup)
    } else if names.iter().any(|n| is_disk_tool(n)) || matches!(btrfs, Some("check" | "rescue" | "restore")) {
        Some(Activity::DiskTool)
    } else {
        None
    }
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
    fn resolves_btrfs_commands() {
        // Each full name means just that command (none is a prefix of another).
        for command in BTRFS_COMMANDS {
            assert_eq!(btrfs_command(&format!("btrfs {command} x")), Some(*command));
        }
        assert_eq!(btrfs_command("btrfs -q c /dev/sdb"), Some("check"));
        assert_eq!(btrfs_command("btrfs --log debug resc zero-log /dev/sdb"), Some("rescue"));
        assert_eq!(btrfs_command("btrfs --param foo=bar check /dev/sdb"), Some("check"));
        assert_eq!(btrfs_command("btrfs --lo debug --form text receive /backup"), Some("receive"));
        assert_eq!(btrfs_command("btrfs res /dev/sdb /mnt"), None);
        assert_eq!(btrfs_command("btrfs frobnicate"), None);
        assert_eq!(btrfs_command("btrfs --version"), None);
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
        assert_eq!(activity_of("at", "tmux new -s work"), None);
        for args in [
            "rsync --server -logDtpre.iLsfxCIvu . /backup/",
            "cp -a /data /backup",
            "/run/current-system/sw/bin/mv /mnt/a/big /mnt/b/",
            "scp -t /var/tmp/",
            "dd if=nixos.iso of=/dev/sdb bs=4M status=progress",
            "ddrescue /dev/sdb sdb.img sdb.map",
            "pv nixos.iso",
            "rclone copy /data b2:backup --transfers 8",
            "rclone rcat remote:backup.tar",
            "rclone cat remote:disk.img",
            "/nix/store/abc-rclone-1.71.2/bin/rclone --config /etc/rclone.conf sync -P /data remote:data",
        ] {
            assert_eq!(activity_of("root", args), Some(Activity::Copy), "{args}");
        }
        for args in [
            "btrfs send -p /snap/a /snap/b",
            "/nix/store/abc-btrfs-progs-6.17/bin/btrfs -q receive /backup",
            "btrfs receive -f stream /x",
            "btrfs --log info receive /backup",
            "btrfs --format text send /snapshot",
            "btrfs --format=json se /snapshot",
            "btrfs rec /backup",
        ] {
            assert_eq!(activity_of("root", args), Some(Activity::BtrfsSendReceive), "{args}");
        }
        for args in ["cryptsetup reencrypt /dev/sda2", "/nix/store/abc-cryptsetup-2.8.1/bin/cryptsetup luksOpen /dev/sdb x", "cryptsetup-reencrypt /dev/sdc"] {
            assert_eq!(activity_of("root", args), Some(Activity::Cryptsetup), "{args}");
        }
        for args in [
            "/nix/store/abc-util-linux-2.41.2-bin/bin/fsck -a /dev/sdb1",
            "fsck.ext4 -f /dev/sdb1",
            "mkfs.btrfs -d raid1 /dev/sdb /dev/sdc",
            "resize2fs /dev/sdb1 100G",
            "btrfs check --repair /dev/sdb",
            "btrfs rescue zero-log /dev/sdb",
            "btrfs rest /dev/sdb /mnt/rescued",
            "badblocks -wsv /dev/sdb",
            "xfs_repair /dev/sdb1",
            "btrfstune --convert-to-block-group-tree /dev/sdb",
            "sgdisk --zap-all /dev/sdb",
            "parted /dev/sdb resizepart 2 100%",
        ] {
            assert_eq!(activity_of("root", args), Some(Activity::DiskTool), "{args}");
        }
        for args in [
            "man tmux",
            "less /var/log/rsync.log",
            "man cp",
            "rclone mount remote: /mnt/remote --vfs-cache-mode full",
            "rclone mount remote: sync",
            "rclone serve http copy",
            "rclone serve webdav remote: --exclude sync",
            "[kworker/0:1-events]",
            "sshd: root [priv]",
            "",
            "btrfs subvolume snapshot /a /send",
            "btrfs scrub status /",
            "btrfs",
            "btrfs --log receive",
            "btrfs s /snapshot",
            "btrfs re /backup",
            "systemd-cryptsetup attach data /dev/sdb",
            // A shell that's done copying but lives on
            "bash -c cp /etc/template /etc/x.conf && nginx -g daemon off;",
            "/nix/store/abc-bash-5.3p9/bin/sh -ec dd if=/dev/zero of=/var/x count=1; sleep 600",
            "systemd-fsck /dev/sdb1",
            "btrfs filesystem resize max /",
        ] {
            assert_eq!(activity_of("root", args), None, "{args}");
        }
    }
}
