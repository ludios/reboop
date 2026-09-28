// Model-output: Claude Opus 5.5

//! `reboop set-luks-password`: asks for a machine's LUKS password, makes sure
//! it's right, and keeps it encrypted in [`config::passwords_dir`].

use crate::config;
use crate::deadline::Deadline;
use crate::initrd::{self, check_password};
use crate::passwords;
use crate::ssh::{OPEN_TIMEOUT, QUICK, Ssh};
use anyhow::{Context, Result, ensure};
use std::io::{self, IsTerminal};
use std::process::{Command, Stdio};

/// Asks for a password with systemd-ask-password, which shows the question
/// `prompt` on the terminal and reads the answer the same way the initrd
/// does: shown as bullets, or not at all after Tab.
fn ask(prompt: &str) -> Result<String> {
    // Otherwise systemd-ask-password would ask password agents instead.
    ensure!(io::stdin().is_terminal(), "can't ask for a password without a terminal");
    let output = Command::new("systemd-ask-password")
        // No time limit, and no newline after the password.
        .args(["--timeout=0", "-n", prompt])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context("failed to run systemd-ask-password")?;
    ensure!(output.status.success(), "systemd-ask-password failed ({})", output.status);
    String::from_utf8(output.stdout).context("the password isn't UTF-8")
}

/// Asks for the LUKS password of the configured machine `hostname`, and
/// saves it encrypted with the machine's luks_signing_key.  If `test`, first
/// checks over SSH that the password opens the LUKS device beneath / on the
/// machine; otherwise asks twice to catch typos.
pub fn set(hostname: &str, test: bool) -> Result<()> {
    let machines = config::load(&config::config_dir()?)?;
    let machine = config::find(&machines, hostname)?;

    let password = ask(&format!("LUKS password for {hostname}:"))?;
    check_password(&password)?;
    if test {
        let deadline = Deadline::after(OPEN_TIMEOUT + QUICK);
        let (device, opens) = initrd::test_root_password(&Ssh::default(), &machine.target(), &password, deadline)?;
        ensure!(opens, "the password doesn't open {device} beneath / on {hostname}");
        println!("The password opens {device} beneath / on {hostname}");
    } else {
        ensure!(ask(&format!("LUKS password for {hostname} (again):"))? == password, "the passwords don't match");
    }

    let dir = config::passwords_dir()?;
    passwords::save(&dir, hostname, &password, &machine.luks_signing_key)?;
    println!("Saved the LUKS password for {hostname} in {}", passwords::password_file(&dir, hostname)?.display());
    Ok(())
}
