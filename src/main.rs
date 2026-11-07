use anstream::AutoStream;
use anstream::stream::RawStream;
use clap::{ColorChoice, Parser};
use mimalloc::MiMalloc;
use reboop::{bounce, check, luks_password, resume};
use std::process::ExitCode;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// --color, for commands whose output has colors.
#[derive(clap::Args, Debug)]
struct ColorOption {
    /// When to color the output (auto: on a terminal, subject to NO_COLOR,
    /// CLICOLOR, and CLICOLOR_FORCE)
    #[clap(long, value_enum, value_name = "WHEN", default_value_t = ColorChoice::Auto, overrides_with = "color")]
    color: ColorChoice,
}

#[derive(Parser, Debug)]
#[clap(name = "reboop", version)]
/// Carefully reboots NixOS machines
enum ReboopCommand {
    /// Show whether machines are okay to reboot, why they need one (if they
    /// do), whether a nixos-rebuild would change them, and the facts behind
    /// that.
    /// Exits 0 if all are okay to reboot, 2 if any isn't, or 1 if any
    /// couldn't be checked.
    #[clap(name = "check")]
    Check {
        /// Machines from machines.jsonl (default: all of them)
        hostnames: Vec<String>,
        /// Print JSON instead of a table
        #[clap(long)]
        json: bool,
        #[clap(flatten)]
        color: ColorOption,
    },
    /// Reboot a machine if it's okay to (see check), unlock its LUKS device
    /// from the initrd if it has one, show how it came back, bring back
    /// what was in its tmux servers (see tmux), and scrub its btrfs
    /// filesystems, unless a scrub found them clean lately.
    /// Exits 0 if it came back fine, 2 if it wasn't okay to reboot, or 1 if
    /// it came back with problems or something failed.
    #[clap(name = "bounce")]
    Bounce {
        /// A machine from machines.jsonl
        hostname: String,
        #[clap(flatten)]
        color: ColorOption,
    },
    /// Do what bounce does after the reboot, for a machine that was rebooted
    /// some other way, or whose bounce was cut short: unlock its LUKS device
    /// from the initrd if it's waiting there, show how it came back, bring
    /// back what was in its tmux servers if reboop took them down (see
    /// tmux), and scrub its btrfs filesystems, unless a scrub found them
    /// clean lately.
    /// Exits 0 if it came back fine, or 1 if it came back with problems or
    /// something failed.
    #[clap(name = "catch")]
    Catch {
        /// A machine from machines.jsonl
        hostname: String,
        #[clap(flatten)]
        color: ColorOption,
    },
    /// Shut down a machine if it's okay to (see check), saving what's in
    /// its tmux servers for catch to bring back (see tmux), and wait for it
    /// to stop accepting SSH.
    /// Exits 0 if it went down fine, 2 if it wasn't okay to shut down, or 1
    /// if it went down with problems or something failed.
    #[clap(name = "stop")]
    Stop {
        /// A machine from machines.jsonl
        hostname: String,
        #[clap(flatten)]
        color: ColorOption,
    },
    /// Show what's in a machine's tmux servers, stop them to bring them
    /// back later, or bring them back; see `reboop tmux --help`.
    #[clap(name = "tmux", subcommand)]
    Tmux(TmuxCommand),
    /// Ask for a machine's LUKS password, check over SSH that it opens the
    /// LUKS device beneath /, and store it encrypted with the machine's
    /// luks_signing_key for unlocking the machine after a reboot
    #[clap(name = "set-luks-password")]
    SetLuksPassword {
        /// A machine from machines.jsonl
        hostname: String,
        /// Don't check the password on the machine; ask for it twice instead
        #[clap(long)]
        no_test_passphrase: bool,
    },
    /// Print a machine's stored LUKS password
    #[clap(name = "get-luks-password")]
    GetLuksPassword {
        /// A machine from machines.jsonl
        hostname: String,
    },
}

/// What's in a machine's tmux servers: every session, window and pane, with
/// the pane's directory and what it's running (a command typed into its
/// shell, which zsh-spy recorded, or one tmux started it with).  A bounce
/// saves that right before the reboot and brings it back afterwards,
/// running again the commands that the machine's resumable_commands allow,
/// if its tmux setting is "resume".
#[derive(clap::Subcommand, Debug)]
enum TmuxCommand {
    /// List what's in a machine's tmux servers: each pane's directory and
    /// what it's running (shortened to a line), and whether that's
    /// resumable by the machine's resumable_commands.  (Saved on the
    /// machine like every look at the servers: see --saved.)
    /// Exits 0, or 1 if that failed.
    #[clap(name = "list")]
    List {
        /// A machine from machines.jsonl
        hostname: String,
        /// Show a capture saved on the machine instead: a file in
        /// /var/lib/reboop/tmux, by its name there or its absolute path
        #[clap(long, value_name = "FILE")]
        saved: Option<String>,
        #[clap(flatten)]
        color: ColorOption,
    },
    /// Save what's in a machine's tmux servers and kill them, for `reboop
    /// tmux start` to bring back (e.g. around upgrading tmux).  Refuses if
    /// a pane runs something that isn't resumable or a server can't be
    /// listed, whatever the machine's tmux setting.
    /// Exits 0 if the servers were stopped, 2 if it wasn't okay to, or 1 if
    /// something failed.
    #[clap(name = "stop")]
    Stop {
        /// A machine from machines.jsonl
        hostname: String,
        #[clap(flatten)]
        color: ColorOption,
    },
    /// Bring back what was in a machine's tmux servers when `reboop tmux
    /// stop` stopped them, or when reboop rebooted or shut down the machine
    /// but didn't get to bring them back: recreate the sessions, windows and
    /// panes, and run the commands that are resumable again.
    /// Exits 0 if all is well, or 1 if something couldn't be brought back
    /// or something failed.
    #[clap(name = "start")]
    Start {
        /// A machine from machines.jsonl
        hostname: String,
        /// Bring back a capture saved on the machine instead: a file in
        /// /var/lib/reboop/tmux, by its name there or its absolute path
        #[clap(long, value_name = "FILE")]
        saved: Option<String>,
        #[clap(flatten)]
        color: ColorOption,
    },
}

/// Whether to style what's written to `stream`, given a choice like --color's.
/// Auto means only on a terminal, and by the environment variables that
/// clap's own messages follow: NO_COLOR, CLICOLOR, CLICOLOR_FORCE, and TERM.
fn styles(stream: &impl RawStream, color: ColorChoice) -> bool {
    match color {
        ColorChoice::Auto   => AutoStream::choice(stream) != anstream::ColorChoice::Never,
        ColorChoice::Always => true,
        ColorChoice::Never  => false,
    }
}

fn main() -> ExitCode {
    // Directives in RUST_LOG that don't parse are reported and skipped.
    let env_filter = EnvFilter::builder().with_default_directive(LevelFilter::WARN.into()).from_env_lossy();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(styles(&std::io::stderr(), ColorChoice::Auto))
        .with_env_filter(env_filter)
        .init();

    // clap exits 2 on usage errors, but that means "not okay to reboot".
    let command = match ReboopCommand::try_parse() {
        Ok(command) => command,
        Err(error) => {
            let _ = error.print();
            return if error.use_stderr() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
        }
    };
    let result = match command {
        ReboopCommand::Check { hostnames, json, color: ColorOption { color } } => check::run(&hostnames, json, styles(&std::io::stdout(), color)),
        ReboopCommand::Bounce { hostname, color: ColorOption { color } } => bounce::run(&hostname, styles(&std::io::stdout(), color)),
        ReboopCommand::Catch { hostname, color: ColorOption { color } } => bounce::run_catch(&hostname, styles(&std::io::stdout(), color)),
        ReboopCommand::Stop { hostname, color: ColorOption { color } } => bounce::run_stop(&hostname, styles(&std::io::stdout(), color)),
        ReboopCommand::Tmux(TmuxCommand::List { hostname, saved, color: ColorOption { color } }) => resume::run_list(&hostname, saved.as_deref(), styles(&std::io::stdout(), color)),
        ReboopCommand::Tmux(TmuxCommand::Stop { hostname, color: ColorOption { color } }) => resume::run_stop(&hostname, styles(&std::io::stdout(), color)),
        ReboopCommand::Tmux(TmuxCommand::Start { hostname, saved, color: ColorOption { color } }) => resume::run_start(&hostname, saved.as_deref(), styles(&std::io::stdout(), color)),
        ReboopCommand::SetLuksPassword { hostname, no_test_passphrase } => luks_password::set(&hostname, !no_test_passphrase).map(|()| 0),
        ReboopCommand::GetLuksPassword { hostname } => luks_password::get(&hostname).map(|()| 0),
    };
    match result {
        Ok(status) => ExitCode::from(status),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn parses_arguments() {
        ReboopCommand::command().debug_assert();
        let color = |args: &[&str]| match ReboopCommand::try_parse_from([&["reboop", "check"], args].concat()).unwrap() {
            ReboopCommand::Check { color: ColorOption { color }, .. } => color,
            command => panic!("parsed as {command:?}"),
        };
        assert_eq!(color(&["one"]), ColorChoice::Auto);
        assert_eq!(color(&["--color", "never", "one"]), ColorChoice::Never);
        assert_eq!(color(&["--color=always", "--color=never"]), ColorChoice::Never);
        assert!(ReboopCommand::try_parse_from(["reboop", "check", "--color", "one"]).is_err());

        match ReboopCommand::try_parse_from(["reboop", "bounce", "--color=never", "one"]).unwrap() {
            ReboopCommand::Bounce { hostname, color: ColorOption { color } } => assert_eq!((hostname.as_str(), color), ("one", ColorChoice::Never)),
            command => panic!("parsed as {command:?}"),
        }
        assert!(ReboopCommand::try_parse_from(["reboop", "bounce", "one", "two"]).is_err());

        match ReboopCommand::try_parse_from(["reboop", "tmux", "start", "one", "--saved", "20261107T063228Z.check.json"]).unwrap() {
            ReboopCommand::Tmux(TmuxCommand::Start { hostname, saved, .. }) => assert_eq!((hostname.as_str(), saved.as_deref()), ("one", Some("20261107T063228Z.check.json"))),
            command => panic!("parsed as {command:?}"),
        }
        assert!(matches!(ReboopCommand::try_parse_from(["reboop", "tmux", "stop", "one"]).unwrap(), ReboopCommand::Tmux(TmuxCommand::Stop { .. })));
        assert!(ReboopCommand::try_parse_from(["reboop", "tmux", "stop", "one", "--saved", "x"]).is_err());
        assert!(ReboopCommand::try_parse_from(["reboop", "tmux"]).is_err());
    }
}
