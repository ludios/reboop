// Model-output: Claude Opus 5.5

use clap::Parser;
use mimalloc::MiMalloc;
use reboop::{check, luks_password};
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

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

fn main() -> ExitCode {
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("warn"))
        .unwrap();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
        ReboopCommand::Check { hostnames, json } => check::run(&hostnames, json),
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
