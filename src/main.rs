// Model-output: Claude Opus 5.5
// Model-output: Claude Fable 5.1

use anstream::AutoStream;
use anstream::stream::RawStream;
use clap::{ColorChoice, Parser};
use mimalloc::MiMalloc;
use reboop::{bounce, check, luks_password};
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
    /// Show whether machines are okay to reboot, and the facts behind that.
    /// Exits 0 if all are, 2 if any isn't, or 1 if any couldn't be checked.
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
    /// from the initrd if it has one, show how it came back, and scrub its
    /// btrfs filesystems.
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
    /// from the initrd if it's waiting there, show how it came back, and
    /// scrub its btrfs filesystems.
    /// Exits 0 if it came back fine, or 1 if it came back with problems or
    /// something failed.
    #[clap(name = "catch")]
    Catch {
        /// A machine from machines.jsonl
        hostname: String,
        #[clap(flatten)]
        color: ColorOption,
    },
    /// Shut down a machine if it's okay to (see check), and wait for it to
    /// stop accepting SSH.
    /// Exits 0 if it went down fine, 2 if it wasn't okay to shut down, or 1
    /// if it went down with problems or something failed.
    #[clap(name = "stop")]
    Stop {
        /// A machine from machines.jsonl
        hostname: String,
        #[clap(flatten)]
        color: ColorOption,
    },
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
    }
}
