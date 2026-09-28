// Model-output: Claude Opus 5.5

//! `reboop bounce`: reboots a machine that's okay to reboot, unlocks its
//! LUKS device from the initrd if it has one, shows how it came back, and
//! scrubs its btrfs filesystems.

use crate::btrfs::{self, ScrubState, ScrubStatus};
use crate::config::{self, Machine};
use crate::deadline::{Deadline, retry};
use crate::facts::{self, Systems};
use crate::human::{self, Style::{self, Plain, Red}};
use crate::initrd;
use crate::passwords;
use crate::preflight::{self, Facts};
use crate::reboot::{self, Stopped};
use crate::ssh::{OPEN_TIMEOUT, QUICK, Session, Ssh, Target};
use anyhow::{Context, Result, ensure};
use std::io::{self, IsTerminal, Write};
use std::thread::sleep;
use std::time::Duration;

/// How long each of a machine's stop_services gets to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often to try reaching a machine that's rebooting.
const RETRY_INTERVAL: Duration = Duration::from_secs(15);

/// How long a machine gets from being asked to reboot until it accepts SSH
/// again: to shut down, get through its firmware, wait at the initrd, and
/// boot.
const RETURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long systemd gets to finish starting up once the machine accepts SSH.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How often to check on a scrub.
const SCRUB_INTERVAL: Duration = Duration::from_secs(2);

/// How long a scrub may take: days more than a big disk needs.
const SCRUB_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Tells the user how a bounce is going.  Write errors are ignored, since a
/// closed stdout is no reason to leave a machine half-bounced.
pub struct Printer<'a> {
    out: &'a mut dyn Write,
    /// Whether to show problems in color.
    color: bool,
    /// If progress is shown by rewriting the last line (which takes a
    /// terminal), gives the terminal's width, which can change.
    columns: Option<fn() -> usize>,
    /// How many characters of progress are on the last line.
    progress_chars: usize,
}

impl<'a> Printer<'a> {
    pub fn new(out: &'a mut dyn Write, color: bool, columns: Option<fn() -> usize>) -> Printer<'a> {
        Printer { out, color, columns, progress_chars: 0 }
    }

    /// Prints `text` in `style` and a newline, in place of any progress on
    /// the last line.
    fn styled_line(&mut self, style: Style, text: &str) {
        if self.progress_chars > 0 {
            let _ = write!(self.out, "\r{}\r", " ".repeat(self.progress_chars));
            self.progress_chars = 0;
        }
        let text = if self.color { style.paint(text) } else { text.to_string() };
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    fn line(&mut self, text: &str) {
        self.styled_line(Plain, text);
    }

    /// Prints `text` so that it's hard to miss.
    fn alarm(&mut self, text: &str) {
        let bar = "!".repeat(text.chars().count() + 8);
        self.styled_line(Red, &format!("{bar}\n!!! {text} !!!\n{bar}"));
    }

    /// Shows `text` on the last line in place of the previous progress, if
    /// rewriting.
    fn progress(&mut self, text: &str) {
        let Some(columns) = self.columns else { return };
        // \r only goes back to the start of the last row, so the line has to
        // fit in one, with room for the cursor.
        let text = human::truncate(text, columns().saturating_sub(1).max(1));
        let chars = text.chars().count();
        let _ = write!(self.out, "\r{text}{}", " ".repeat(self.progress_chars.saturating_sub(chars)));
        let _ = self.out.flush();
        self.progress_chars = chars;
    }
}

impl Drop for Printer<'_> {
    /// Keeps the last progress, and ends its line so that whatever's printed
    /// next (like an error) starts on a line of its own.
    fn drop(&mut self) {
        if self.progress_chars > 0 {
            let _ = writeln!(self.out);
            let _ = self.out.flush();
        }
    }
}

/// `heading`, then each of `items` on an indented line.
fn indented_list(heading: &str, items: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let mut text = heading.to_string();
    for item in items {
        text.push_str("\n    ");
        text.push_str(item.as_ref());
    }
    text
}

/// How a bounce ended, short of an error.
#[derive(Debug)]
pub enum Outcome {
    /// The machine wasn't okay to reboot, for these reasons, so it wasn't
    /// rebooted.
    NotOkay(Vec<String>),
    /// The machine came back with these problems, which are none if all is
    /// well.
    Bounced(Vec<String>),
}

/// The password that `machine`'s initrd will ask for, if any: if `machine`
/// (at the other end of `session`) has a LUKS device beneath /, gets it from
/// `password`, and makes sure that it opens the device.
fn password_for_initrd(
    ssh: &Ssh,
    machine: &Machine,
    session: &mut Session,
    password: impl FnOnce() -> Result<String>,
    printer: &mut Printer,
) -> Result<Option<String>> {
    let hostname = &machine.hostname;
    if initrd::luks_devices(session)?.is_empty() {
        printer.line("There's no LUKS device beneath /, so there'll be nothing to unlock");
        return Ok(None);
    }
    let password = password().with_context(|| format!("couldn't get the stored LUKS password (`reboop set-luks-password {hostname}` sets it)"))?;
    let deadline = Deadline::after(OPEN_TIMEOUT + QUICK);
    let (device, opens) = initrd::test_luks_password(ssh, &machine.target(), &password, deadline)?;
    ensure!(opens, "the stored LUKS password doesn't open {device} (`reboop set-luks-password {hostname}` replaces it)");
    printer.line(&format!("The stored LUKS password opens {device}"));
    Ok(Some(password))
}

/// Stops `machine`'s stop_services in order, over `session`.  Fails, with the
/// machine still up, if systemctl fails to stop one.  Returns the problems:
/// services that systemd doesn't say stopped cleanly.
fn stop_services(machine: &Machine, session: &mut Session, printer: &mut Printer) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for service in &machine.stop_services {
        match reboot::stop_unit(session, service, STOP_TIMEOUT)? {
            Stopped::Cleanly => printer.line(&format!("{service} is stopped")),
            Stopped::Uncleanly(result) => {
                let problem = format!("{service} is stopped, but systemd's result for it is {result}, not success");
                printer.styled_line(Red, &problem);
                problems.push(problem);
            }
            Stopped::NotLoaded => printer.line(&format!("There's no {service} to stop")),
        }
    }
    Ok(problems)
}

/// Opens a session to `target` once it's up in a boot other than
/// `old_boot_id`, trying once per [`RETRY_INTERVAL`] until `deadline`.
fn wait_for_new_boot(ssh: &Ssh, target: &Target, old_boot_id: &str, deadline: Deadline) -> Result<Session> {
    retry(deadline, RETRY_INTERVAL, |deadline| {
        let mut session = Session::open(ssh, target, deadline.at_most(OPEN_TIMEOUT).remaining())?;
        ensure!(facts::boot_id(&mut session)? != old_boot_id, "{target} hasn't rebooted yet");
        Ok(session)
    })
    .with_context(|| format!("{target} didn't come back"))
}

/// What's wrong with how a machine came back, given the systems from
/// `before` it rebooted and `after`, systemd's `state` of it (from
/// [`facts::wait_until_booted`]), and its `failed_units`.
fn boot_problems(before: &Systems, after: &Systems, state: &str, failed_units: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    // "degraded" means there are failed units, which get their own problem.
    if state != "running" && state != "degraded" {
        problems.push(format!("systemd says the system is {state}"));
    }
    if !failed_units.is_empty() {
        problems.push(format!("failed units: {}", failed_units.join(", ")));
    }
    if after.booted != before.default {
        problems.push(format!("booted {}, not {}", after.booted, before.default));
    }
    if after.running_kernel != before.default_kernel {
        problems.push(format!("booted kernel {}, not {}", after.running_kernel, before.default_kernel));
    }
    problems
}

/// Waits for the machine at the other end of `session` to finish starting
/// up, shows how it came back, and returns its problems (see
/// [`boot_problems`]), given the systems from `before` it rebooted.
fn postflight(session: &mut Session, before: &Systems, printer: &mut Printer) -> Result<Vec<String>> {
    printer.line("Waiting for systemd to finish starting up");
    let state = facts::wait_until_booted(session, STARTUP_TIMEOUT)?;
    let failed_units = facts::failed_units(session)?;
    let after = facts::systems(session)?;
    let kernel_errors = facts::kernel_errors(session)?;

    printer.line(&format!("systemd says the system is {state}"));
    printer.line(&format!("Booted {} with kernel {}", after.booted, after.running_kernel));
    if failed_units.is_empty() {
        printer.line("Failed units: none");
    } else {
        printer.line(&format!("Failed units: {}", failed_units.join(", ")));
    }
    if kernel_errors.trim().is_empty() {
        printer.line("Kernel errors: none");
    } else {
        printer.line(&indented_list("Kernel errors:", kernel_errors.trim_end().lines()));
    }
    Ok(boot_problems(before, &after, &state, &failed_units))
}

/// Scrubs the btrfs filesystem mounted at `mountpoint`, or waits for the
/// scrub that's already running there, showing its progress.  Returns its
/// final status.
fn scrub(session: &mut Session, mountpoint: &str, printer: &mut Printer) -> Result<ScrubStatus> {
    // Such as one started by a timer that came due while the machine was down
    if btrfs::scrub_status(session, mountpoint)?.state == ScrubState::Running {
        printer.line(&format!("Waiting for the scrub that's already running on {mountpoint}"));
    } else {
        printer.line(&format!("Scrubbing btrfs on {mountpoint}"));
        btrfs::start_scrub(session, mountpoint, QUICK)?;
    }
    btrfs::wait_for_scrub(session, mountpoint, SCRUB_INTERVAL, Deadline::after(SCRUB_TIMEOUT), |status| {
        printer.progress(&format!("btrfs on {mountpoint}: {}", status.summary()));
    })
}

/// After asking `machine` to reboot, unlocks its initrd with `password` (if
/// any), waits for it to come back, shows how it did, and scrubs its
/// scrub_mounts.  `before` is what [`preflight::gather`] found before
/// the reboot.  Returns the problems (none if all is well).
fn come_back(ssh: &Ssh, machine: &Machine, before: &Facts, password: Option<&str>, printer: &mut Printer) -> Result<Vec<String>> {
    let deadline = Deadline::after(RETURN_TIMEOUT);
    if let Some(password) = password {
        let initrd_target = machine.initrd_target();
        printer.line(&format!("Waiting for the initrd at {initrd_target}"));
        for prompt in initrd::wait_and_unlock(ssh, &initrd_target, password, RETRY_INTERVAL, deadline)? {
            printer.line(&format!("Answered {prompt:?}"));
        }
    } else {
        // So that the first try doesn't log in to the old boot on its way
        // down, which costs a key touch for some.
        sleep(RETRY_INTERVAL);
    }
    let target = machine.target();
    printer.line(&format!("Waiting for {target}"));
    let mut session = wait_for_new_boot(ssh, &target, &before.boot_id, deadline)?;
    let mut problems = postflight(&mut session, &before.systems, printer)?;

    for mountpoint in &machine.scrub_mounts {
        let status = match scrub(&mut session, mountpoint, printer) {
            Ok(status) => status,
            Err(error) => {
                let problem = format!("btrfs on {mountpoint}: couldn't scrub: {error:#}");
                printer.styled_line(Red, &problem);
                problems.push(problem);
                continue;
            }
        };
        let summary = format!("btrfs on {mountpoint}: {}", status.summary());
        if status.errors.is_empty() {
            printer.line(&summary);
        } else {
            printer.alarm(&summary);
        }
        if !status.errors.is_empty() || status.state != ScrubState::Finished {
            problems.push(summary);
        }
    }
    Ok(problems)
}

/// Bounces `machine`, telling the user about it with `printer`: checks that
/// it's okay to reboot; if it has a LUKS device beneath /, gets the password
/// from `password` and tests it; stops its stop_services; reboots it; answers
/// its initrd's password prompt; waits for it to come back; shows how it
/// did; and scrubs its scrub_mounts.  Problems along the way are collected
/// for the end.
///
/// Errors say whether they happened before or after asking the machine to
/// reboot; after, it may be down, e.g. waiting at its initrd.
pub fn bounce(ssh: &Ssh, machine: &Machine, password: impl FnOnce() -> Result<String>, printer: &mut Printer) -> Result<Outcome> {
    let hostname = &machine.hostname;
    let not_rebooted = || format!("didn't reboot {hostname}");
    printer.line(&format!("Checking whether {hostname} is okay to reboot"));
    let mut session = Session::open(ssh, &machine.target(), OPEN_TIMEOUT).with_context(not_rebooted)?;
    let facts = preflight::gather(&mut session, hostname).with_context(not_rebooted)?;
    let blockers = preflight::blockers(machine, &facts);
    if !blockers.is_empty() {
        printer.styled_line(Red, &indented_list(&format!("Not rebooting {hostname}:"), &blockers));
        return Ok(Outcome::NotOkay(blockers));
    }
    let systems = &facts.systems;
    printer.line(&format!("{hostname} is okay to reboot, into {} with kernel {}", systems.default, systems.default_kernel));
    let password = password_for_initrd(ssh, machine, &mut session, password, printer).with_context(not_rebooted)?;
    let mut problems = stop_services(machine, &mut session, printer).with_context(not_rebooted)?;
    printer.line(&format!("Asking {hostname} to reboot"));
    // Which may have worked even if it failed, e.g. by hanging
    reboot::reboot(session).with_context(|| format!("failed to ask {hostname} to reboot"))?;

    let came_back = come_back(ssh, machine, &facts, password.as_deref(), printer);
    problems.extend(came_back.with_context(|| format!("after asking {hostname} to reboot"))?);
    if problems.is_empty() {
        printer.line(&format!("{hostname} is back, and all is well"));
    } else {
        printer.styled_line(Red, &indented_list(&format!("{hostname} is back, but:"), &problems));
    }
    Ok(Outcome::Bounced(problems))
}

/// The width of the terminal on stdout, or 80 columns if that's unknown.
fn terminal_columns() -> usize {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ fills in the winsize that its argument points to.
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 { size.ws_col.into() } else { 80 }
}

/// Bounces the configured machine `hostname` (see [`bounce`]), with
/// problems in color if `color`.  Returns the exit status: 0 if it came
/// back fine, 1 if it came back with problems, or 2 if it wasn't okay to
/// reboot.
pub fn run(hostname: &str, color: bool) -> Result<u8> {
    let machines = config::load(&config::config_dir()?)?;
    let machine = config::find(&machines, hostname)?;
    let password = || passwords::load(&config::passwords_dir()?, hostname, &passwords::master_key(&machine.luks_signing_key)?);
    let mut stdout = io::stdout();
    let columns = stdout.is_terminal().then_some(terminal_columns as fn() -> usize);
    let mut printer = Printer::new(&mut stdout, color, columns);
    Ok(match bounce(&Ssh::default(), machine, password, &mut printer)? {
        Outcome::Bounced(problems) if problems.is_empty() => 0,
        Outcome::Bounced(_) => 1,
        Outcome::NotOkay(_) => 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::idle_facts;

    fn printed(color: bool, columns: Option<fn() -> usize>, print: impl FnOnce(&mut Printer)) -> String {
        let mut out = Vec::new();
        print(&mut Printer::new(&mut out, color, columns));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn prints_progress_over_itself() {
        let text = printed(false, Some(|| 80), |printer| {
            printer.line("start");
            printer.progress("12345");
            printer.progress("123");
            printer.line("done");
            printer.progress("last");
        });
        assert_eq!(text, "start\n\r12345\r123  \r   \rdone\n\rlast\n");
    }

    #[test]
    fn fits_progress_in_a_row() {
        let text = printed(false, Some(|| 6), |printer| {
            printer.progress("1234567");
            printer.line("done");
        });
        assert_eq!(text, "\r1234…\r     \rdone\n");
    }

    #[test]
    fn prints_no_progress_to_a_file() {
        let text = printed(false, None, |printer| {
            printer.line("start");
            printer.progress("12345");
            printer.alarm("bad");
        });
        assert_eq!(text, "start\n!!!!!!!!!!!\n!!! bad !!!\n!!!!!!!!!!!\n");
    }

    #[test]
    fn colors_problems() {
        let text = printed(true, None, |printer| {
            printer.line("fine");
            printer.styled_line(Red, &indented_list("problems:", ["one", "two"]));
        });
        assert_eq!(text, "fine\n\x1b[31mproblems:\n    one\n    two\x1b[0m\n");
    }

    #[test]
    fn finds_boot_problems() {
        let before = idle_facts().systems;
        assert_eq!(boot_problems(&before, &before, "running", &[]), Vec::<String>::new());
        let after = Systems { booted: "/nix/store/bbb-nixos-system-one-26.05".into(), running_kernel: "6.18.53".into(), ..before.clone() };
        assert_eq!(
            boot_problems(&before, &after, "degraded", &["a.service".into(), "b.service".into()]),
            [
                "failed units: a.service, b.service",
                "booted /nix/store/bbb-nixos-system-one-26.05, not /nix/store/aaa-nixos-system-one-26.05",
                "booted kernel 6.18.53, not 6.18.54",
            ]
        );
        assert_eq!(boot_problems(&before, &before, "maintenance", &[]), ["systemd says the system is maintenance"]);
    }
}
