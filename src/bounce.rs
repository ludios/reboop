// Model-output: Claude Opus 5.5
// Model-output: Claude Fable 5.1

//! `reboop bounce`: reboots a machine that's okay to reboot, unlocks its
//! LUKS device from the initrd if it has one, shows how it came back, and
//! scrubs its btrfs filesystems; `reboop catch`, which does what comes
//! after the reboot, for a machine that was rebooted some other way; and
//! `reboop stop`, which shuts down a machine that's okay to shut down.

use crate::boot::DefaultBoot;
use crate::btrfs::{self, ScrubState, ScrubStatus};
use crate::config::{self, Machine};
use crate::deadline::{Deadline, Permanent, retry};
use crate::facts::{self, Systems};
use crate::human::{self, Style::{self, Bold, Dim, Plain, Red}};
use crate::initrd::{self, UnlockError};
use crate::passwords;
use crate::preflight::{self, Facts};
use crate::reboot::{self, Down, Stopped};
use crate::ssh::{OPEN_TIMEOUT, QUICK, Session, Ssh, is_unreachable};
use anyhow::{Context, Result, anyhow, bail, ensure};
use jiff::Zoned;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::net::Ipv4Addr;
use std::thread::sleep;
use std::time::Duration;
use tracing::debug;

/// How long each of a machine's stop_services gets to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often to try reaching a machine that's rebooting or shutting down.
const RETRY_INTERVAL: Duration = Duration::from_secs(15);

/// How long a machine gets from being asked to reboot until it accepts SSH
/// again: to shut down, get through its firmware, wait at the initrd, and
/// boot.
const RETURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How long systemd gets to finish starting up once the machine accepts SSH.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long a machine gets from being asked to shut down until it stops
/// accepting SSH.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often to check on a scrub.
const SCRUB_INTERVAL: Duration = Duration::from_secs(2);

/// How long a scrub may take: days more than a big disk needs.
const SCRUB_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Tells the user how a bounce is going.  Write errors are ignored, since a
/// closed stdout is no reason to leave a machine half-bounced.
pub struct Printer<'a> {
    out: &'a mut dyn Write,
    /// Whether to show problems in red, values in bold, and timestamps
    /// dimmed.
    color: bool,
    /// If progress is shown by rewriting the last line (which takes a
    /// terminal), gives the terminal's width, which can change.
    columns: Option<fn() -> usize>,
    /// If lines start with a timestamp, gives the time of day, like
    /// "12:03:16".
    clock: Option<fn() -> String>,
    /// How many characters of progress are on the last line.
    progress_chars: usize,
}

impl<'a> Printer<'a> {
    pub fn new(out: &'a mut dyn Write, color: bool, columns: Option<fn() -> usize>, clock: Option<fn() -> String>) -> Printer<'a> {
        Printer { out, color, columns, clock, progress_chars: 0 }
    }

    /// `text` in `style`, if in color.
    fn paint(&self, style: Style, text: &str) -> String {
        if self.color { style.paint(text) } else { text.to_string() }
    }

    /// `value` in bold, if in color, to stand out in a plain line.  Not for
    /// other lines, since the reset at the end would also end a red line's
    /// color, and would throw off progress's count of characters.
    fn bold(&self, value: impl fmt::Display) -> String {
        self.paint(Bold, &value.to_string())
    }

    /// Prints `text` in `style` and a newline, in place of any progress on
    /// the last line.  If timestamped, the first line of `text` comes after
    /// the time, and the rest after as many spaces.
    fn styled_line(&mut self, style: Style, text: &str) {
        if self.progress_chars > 0 {
            let _ = write!(self.out, "\r{}\r", " ".repeat(self.progress_chars));
            self.progress_chars = 0;
        }
        let text = match self.clock {
            Some(clock) => {
                let time = clock();
                let text = text.replace('\n', &format!("\n{}", " ".repeat(time.chars().count() + 1)));
                format!("{} {}", self.paint(Dim, &time), self.paint(style, &text))
            }
            None => self.paint(style, text),
        };
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
    /// rewriting, after the time if timestamped and there's room.  Runs of
    /// whitespace, like line breaks, become single spaces.
    fn progress(&mut self, text: &str) {
        let Some(columns) = self.columns else { return };
        // \r only goes back to the start of the last row, so the line has to
        // fit in one, with room for the cursor.
        let width = columns().saturating_sub(1).max(1);
        // The time and a space, if that leaves room for some of the text
        let time = self.clock.map(|clock| clock()).filter(|time| time.chars().count() + 1 < width);
        let time_chars = time.as_ref().map_or(0, |time| time.chars().count() + 1);
        let text = human::truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), width - time_chars);
        let chars = time_chars + text.chars().count();
        let time = time.map_or(String::new(), |time| format!("{} ", self.paint(Dim, &time)));
        let _ = write!(self.out, "\r{time}{text}{}", " ".repeat(self.progress_chars.saturating_sub(chars)));
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

/// How a bounce or stop ended, short of an error.
#[derive(Debug)]
pub enum Outcome {
    /// The machine wasn't okay to reboot or shut down, for these reasons, so
    /// it wasn't.  If that only turned out once its stop_services were
    /// stopped, they're left stopped.
    NotOkay(Vec<String>),
    /// The machine was rebooted or shut down, with these problems, which are
    /// none if all is well.
    Done(Vec<String>),
}

impl Outcome {
    /// The exit status for it: 0 if all is well, 1 if there were problems,
    /// or 2 if the machine wasn't okay to reboot or shut down.
    pub fn exit_status(&self) -> u8 {
        match self {
            Outcome::Done(problems) if problems.is_empty() => 0,
            Outcome::Done(_) => 1,
            Outcome::NotOkay(_) => 2,
        }
    }
}

/// Makes sure that `machine`'s initrd will be where bounce looks for it, given
/// what the boot loader will boot (`boots`): that it won't take a static
/// address (ip=) other than the machine's.
fn check_initrd_addresses(machine: &Machine, boots: &[DefaultBoot]) -> Result<()> {
    for boot in boots {
        let addresses = boot.initrd_addresses();
        let elsewhere = !addresses.is_empty() && !addresses.contains(&machine.ipv4);
        let addresses: Vec<_> = addresses.iter().map(Ipv4Addr::to_string).collect();
        let (loader, entry, ipv4) = (&boot.loader, &boot.entry, machine.ipv4);
        ensure!(!elsewhere, "{loader}'s default, {entry:?}, has the initrd take {} (ip=), not {ipv4}", addresses.join(" and "));
    }
    Ok(())
}

/// The password that `machine`'s initrd will ask for, if any: if `machine`
/// (at the other end of `session`) has a LUKS device beneath /, gets it from
/// `password`, and makes sure that it opens the device, and that the initrd
/// will be where bounce looks for it, given what the boot loader will boot
/// (`boots`).
fn password_for_initrd(
    ssh: &Ssh,
    machine: &Machine,
    session: &mut Session,
    boots: &[DefaultBoot],
    password: impl FnOnce() -> Result<String>,
    printer: &mut Printer,
) -> Result<Option<String>> {
    let hostname = &machine.hostname;
    if initrd::luks_devices(session)?.is_empty() {
        printer.line("There's no LUKS device beneath /, so there'll be nothing to unlock");
        return Ok(None);
    }
    check_initrd_addresses(machine, boots)?;
    let password = password().with_context(|| format!("couldn't get the stored LUKS password (`reboop set-luks-password {hostname}` sets it)"))?;
    let deadline = Deadline::after(OPEN_TIMEOUT + QUICK);
    let (device, opens) = initrd::test_luks_password(ssh, &machine.target(), &password, deadline)?;
    ensure!(opens, "the stored LUKS password doesn't open {device} (`reboop set-luks-password {hostname}` replaces it)");
    printer.line(&format!("The stored LUKS password opens {}", printer.bold(&device)));
    Ok(Some(password))
}

/// Stops `machine`'s stop_services in order, over `session`.  Fails, with the
/// machine still up, if systemctl fails to stop one.  Returns the services
/// that are stopped now (leaving out those the machine doesn't have), and
/// the problems: services that systemd doesn't say stopped cleanly.
fn stop_services(machine: &Machine, session: &mut Session, printer: &mut Printer) -> Result<(Vec<String>, Vec<String>)> {
    let mut stopped = Vec::new();
    let mut problems = Vec::new();
    for service in &machine.stop_services {
        match reboot::stop_unit(session, service, STOP_TIMEOUT)? {
            Stopped::Cleanly => printer.line(&format!("{} is stopped", printer.bold(service))),
            Stopped::Uncleanly(result) => {
                let problem = format!("{service} is stopped, but systemd's result for it is {result}, not success");
                printer.styled_line(Red, &problem);
                problems.push(problem);
            }
            Stopped::NotLoaded => {
                printer.line(&format!("There's no {} to stop", printer.bold(service)));
                continue;
            }
        }
        stopped.push(service.clone());
    }
    Ok((stopped, problems))
}

/// What a check of a machine found: what was wanted if it's okay to take
/// down, or else the reasons not to.
type Checked<T> = Result<T, Vec<String>>;

/// Opens a session to `machine` and checks whether it's okay to take
/// `down`, telling the user.  Returns the session and the facts if so, or
/// else the reasons not to.
fn check(ssh: &Ssh, machine: &Machine, down: Down, printer: &mut Printer) -> Result<Checked<(Session, Facts)>> {
    let hostname = &machine.hostname;
    printer.line(&format!("Checking whether {hostname} is okay to {}", down.verb()));
    let mut session = Session::open(ssh, &machine.target(), OPEN_TIMEOUT)?;
    let facts = preflight::gather(&mut session, hostname)?;
    let blockers = preflight::blockers(machine, &facts);
    if !blockers.is_empty() {
        printer.styled_line(Red, &indented_list(&format!("Not {} {hostname}:", down.gerund()), &blockers));
        return Ok(Err(blockers));
    }
    Ok(Ok((session, facts)))
}

/// Checks again whether `machine` (at the other end of `session`) is okay to
/// take down, as testing the password and stopping services take a while,
/// during which a timer, say, may have started an upgrade or scrub.
/// Stopping services changes the load and network traffic, so those don't
/// count.  If it's to be unlocked after a reboot (`luks`), also makes sure
/// that its initrd will still be where bounce looks for it.  Returns the
/// facts and the reasons not to (none if it's okay).
fn check_again(machine: &Machine, session: &mut Session, luks: bool) -> Result<(Facts, Vec<String>)> {
    let facts = preflight::gather(session, &machine.hostname)?;
    if luks {
        check_initrd_addresses(machine, &facts.boot)?;
    }
    let blockers = preflight::blockers_ignoring_load_and_network(machine, &facts);
    Ok((facts, blockers))
}

/// Stops `machine`'s stop_services over `session` (see [`stop_services`]),
/// and checks again whether it's okay to take `down` (see [`check_again`],
/// which `luks` is for), telling the user.  Returns the facts and the
/// problems so far (services that didn't stop cleanly) if so; or else the
/// reasons not to, leaving the stop_services stopped and saying so.
fn stop_and_check_again(machine: &Machine, session: &mut Session, down: Down, luks: bool, printer: &mut Printer) -> Result<Checked<(Facts, Vec<String>)>> {
    let hostname = &machine.hostname;
    let (stopped, problems) = stop_services(machine, session, printer)?;
    let still_stopped = |printer: &mut Printer| {
        if !stopped.is_empty() {
            printer.styled_line(Red, &indented_list(&format!("Still stopped on {hostname}:"), &stopped));
        }
    };
    printer.line(&format!("Checking again whether {hostname} is okay to {}", down.verb()));
    match check_again(machine, session, luks) {
        Ok((facts, blockers)) if blockers.is_empty() => Ok(Ok((facts, problems))),
        Ok((_, blockers)) => {
            printer.styled_line(Red, &indented_list(&format!("Not {} {hostname} after all:", down.gerund()), &blockers));
            still_stopped(printer);
            Ok(Err(blockers))
        }
        Err(error) => {
            still_stopped(printer);
            Err(error)
        }
    }
}

/// Shows on `printer`'s progress line why the last try at something
/// failed: the last non-blank line of the innermost message in `error`'s
/// chain that has one, since the line before says what it's trying.
fn show_last_try(printer: &mut Printer, error: &anyhow::Error) {
    let last_line = |message: String| message.lines().map(str::trim).rfind(|line| !line.is_empty()).map(str::to_string);
    let reason = error.chain().rev().find_map(|cause| last_line(cause.to_string())).unwrap_or_default();
    printer.progress(&format!("Latest try: {reason}"));
}

/// Where a machine coming back from a reboot was found.
enum Found {
    /// Back, at the other end of this session.
    Back(Session),
    /// At its initrd, which took the password at these prompts.
    Unlocked(Vec<String>),
}

/// Waits for `machine` to be back from a reboot: up (past its initrd), not
/// shutting down, and in a boot other than `old_boot_id` (if known).  Or,
/// given a `password`, for its initrd to take it, if the initrd comes first.
/// Tries once per [`RETRY_INTERVAL`] until `deadline`, passing each failed
/// try to `on_retry`.
fn wait_for_return(
    ssh: &Ssh,
    machine: &Machine,
    old_boot_id: Option<&str>,
    password: Option<&str>,
    deadline: Deadline,
    on_retry: impl FnMut(&anyhow::Error),
) -> Result<Found> {
    let (target, initrd_target) = (machine.target(), machine.initrd_target());
    let back = |deadline: Deadline| -> Result<Session> {
        let mut session = Session::open(ssh, &target, deadline.at_most(OPEN_TIMEOUT).remaining())?;
        if let Some(old_boot_id) = old_boot_id {
            ensure!(facts::boot_id(&mut session)? != old_boot_id, "it hasn't rebooted yet");
        }
        // As when ssh_port is initrd_ssh_port
        ensure!(!facts::in_initrd(&mut session)?, "it's at its initrd");
        ensure!(facts::system_state(&mut session)? != "stopping", "it's shutting down");
        Ok(session)
    };
    // Failures to unlock, other than an unreachable initrd, are returned
    // inside Ok so that retry doesn't try again.
    let attempt = |deadline: Deadline| -> Result<Result<Found, UnlockError>> {
        let error = match back(deadline) {
            Ok(session) => return Ok(Ok(Found::Back(session))),
            Err(error) => error,
        };
        let Some(password) = password else { return Err(error) };
        match initrd::unlock(ssh, &initrd_target, password, deadline) {
            Ok(prompts) => Ok(Ok(Found::Unlocked(prompts))),
            // Until it's unlocked, it's likelier to be at its initrd than up,
            // so it's the initrd's failure that's passed on (like its
            // refusing the user's key), unless ssh_port's is Permanent.
            Err(UnlockError::Unreachable(stderr)) if error.downcast_ref::<Permanent>().is_none() => {
                debug!("{target} isn't back: {error:#}");
                Err(anyhow!(stderr.trim_end().to_string()).context(format!("couldn't reach the initrd at {initrd_target}")))
            }
            Err(UnlockError::Unreachable(_)) => Err(error),
            Err(fatal) => Ok(Err(fatal)),
        }
    };
    let found = retry(deadline, RETRY_INTERVAL, attempt, on_retry).with_context(|| format!("{} didn't come back", machine.hostname))?;
    Ok(found?)
}

/// What's wrong with how a machine came back, given its `systems`, systemd's
/// `state` of it (from [`facts::wait_until_booted`]), and its
/// `failed_units`.  It should have booted its system profile.
fn postflight_problems(systems: &Systems, state: &str, failed_units: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    // "degraded" means there are failed units, which get their own problem.
    if state != "running" && state != "degraded" {
        problems.push(format!("systemd says the system is {state}"));
    }
    if !failed_units.is_empty() {
        problems.push(format!("failed units: {}", failed_units.join(", ")));
    }
    if systems.booted != systems.default {
        problems.push(format!("booted {}, not {}", systems.booted, systems.default));
    }
    if systems.running_kernel != systems.default_kernel {
        problems.push(format!("booted kernel {}, not {}", systems.running_kernel, systems.default_kernel));
    }
    problems
}

/// Waits for the machine at the other end of `session` to finish starting
/// up, shows how it came back, and returns its problems (see
/// [`postflight_problems`]).
fn postflight(session: &mut Session, printer: &mut Printer) -> Result<Vec<String>> {
    printer.line("Waiting for systemd to finish starting up");
    let state = facts::wait_until_booted(session, STARTUP_TIMEOUT)?;
    let failed_units = facts::failed_units(session)?;
    let after = facts::systems(session)?;
    let kernel_errors = facts::kernel_errors(session)?;

    printer.line(&format!("systemd says the system is {}", printer.bold(&state)));
    printer.line(&format!("Booted {} with kernel {}", printer.bold(&after.booted), printer.bold(&after.running_kernel)));
    if failed_units.is_empty() {
        printer.line("Failed units: none");
    } else {
        let units: Vec<_> = failed_units.iter().map(|unit| printer.bold(unit)).collect();
        printer.line(&format!("Failed units: {}", units.join(", ")));
    }
    if kernel_errors.trim().is_empty() {
        printer.line("Kernel errors: none");
    } else {
        printer.line(&indented_list("Kernel errors:", kernel_errors.trim_end().lines()));
    }
    Ok(postflight_problems(&after, &state, &failed_units))
}

/// Scrubs the btrfs filesystem mounted at `mountpoint`, or waits for the
/// scrub that's already running there, showing its progress.  Returns its
/// final status.
fn scrub(session: &mut Session, mountpoint: &str, printer: &mut Printer) -> Result<ScrubStatus> {
    // Such as one started by a timer that came due while the machine was down
    if btrfs::scrub_status(session, mountpoint)?.state == ScrubState::Running {
        printer.line(&format!("Waiting for the scrub that's already running on {}", printer.bold(mountpoint)));
    } else {
        printer.line(&format!("Scrubbing btrfs on {}", printer.bold(mountpoint)));
        btrfs::start_scrub(session, mountpoint, QUICK)?;
    }
    btrfs::wait_for_scrub(session, mountpoint, SCRUB_INTERVAL, Deadline::after(SCRUB_TIMEOUT), |status| {
        printer.progress(&format!("btrfs on {mountpoint}: {}", status.summary()));
    })
}

/// Waits for `machine` to come back from a reboot, out of boot `old_boot_id`
/// if that's known, unlocking its initrd with `password` (if any) if it
/// waits there; shows how it did; and scrubs its scrub_mounts.  Returns the
/// problems (none if all is well).
fn come_back(ssh: &Ssh, machine: &Machine, old_boot_id: Option<&str>, mut password: Option<&str>, printer: &mut Printer) -> Result<Vec<String>> {
    let deadline = Deadline::after(RETURN_TIMEOUT);
    let (target, initrd_target) = (machine.target(), machine.initrd_target());
    printer.line(&match password {
        Some(_) => format!("Waiting for {target}, or for the initrd at {initrd_target}"),
        None => format!("Waiting for {target}"),
    });
    // Unlocks the initrd at most once, since its asking again would mean
    // that the password didn't work.
    let mut session = loop {
        match wait_for_return(ssh, machine, old_boot_id, password, deadline, |error| show_last_try(printer, error))? {
            Found::Back(session) => break session,
            Found::Unlocked(prompts) => {
                for prompt in prompts {
                    printer.line(&format!("Answered {}", printer.bold(format!("{prompt:?}"))));
                }
                printer.line(&format!("Waiting for {target}"));
                password = None;
            }
        }
    };
    let mut problems = postflight(&mut session, printer)?;

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

/// Tells the user that `hostname` is `how` ("back" or "down"), and its
/// `problems` (if any).
fn show_done(hostname: &str, how: &str, problems: &[String], printer: &mut Printer) {
    if problems.is_empty() {
        printer.line(&format!("{hostname} is {how}, and all is well"));
    } else {
        printer.styled_line(Red, &indented_list(&format!("{hostname} is {how}, but:"), problems));
    }
}

/// Bounces `machine`, telling the user about it with `printer`: checks that
/// it's okay to reboot; if it has a LUKS device beneath /, gets the password
/// from `password` and tests it; stops its stop_services; checks again,
/// apart from load and network traffic (leaving the stop_services stopped if
/// it's no longer okay); reboots it; answers its initrd's password prompt;
/// waits for it to come back; shows how it did; and scrubs its
/// scrub_mounts.  Problems along the way are collected for the end.
///
/// Errors say whether they happened before or after asking the machine to
/// reboot; after, it may be down, e.g. waiting at its initrd.
pub fn bounce(ssh: &Ssh, machine: &Machine, password: impl FnOnce() -> Result<String>, printer: &mut Printer) -> Result<Outcome> {
    let hostname = &machine.hostname;
    let not_rebooted = || format!("didn't reboot {hostname}");
    let (mut session, facts) = match check(ssh, machine, Down::Reboot, printer).with_context(not_rebooted)? {
        Ok(okay) => okay,
        Err(blockers) => return Ok(Outcome::NotOkay(blockers)),
    };
    let systems = &facts.systems;
    printer.line(&format!("{hostname} is okay to reboot, into {} with kernel {}", printer.bold(&systems.default), printer.bold(&systems.default_kernel)));
    let password = password_for_initrd(ssh, machine, &mut session, &facts.boot, password, printer).with_context(not_rebooted)?;
    let (facts, mut problems) = match stop_and_check_again(machine, &mut session, Down::Reboot, password.is_some(), printer).with_context(not_rebooted)? {
        Ok(stopped) => stopped,
        Err(blockers) => return Ok(Outcome::NotOkay(blockers)),
    };
    printer.line(&format!("Asking {hostname} to reboot"));
    // Which may have worked even if it failed, e.g. by hanging
    reboot::ask(session, Down::Reboot).with_context(|| format!("failed to ask {hostname} to reboot (if it's rebooting anyway, `reboop catch {hostname}` takes it from there)"))?;
    // So that the first try doesn't log in to the old boot on its way down,
    // which costs a key touch for some.
    sleep(RETRY_INTERVAL);

    let came_back = come_back(ssh, machine, Some(&facts.boot_id), password.as_deref(), printer);
    problems.extend(came_back.with_context(|| format!("after asking {hostname} to reboot (`reboop catch {hostname}` takes it from there)"))?);
    show_done(hostname, "back", &problems, printer);
    Ok(Outcome::Done(problems))
}

/// Catches `machine` on its way back up from a reboot that wasn't a bounce,
/// or whose bounce was cut short: waits for it, unlocking its initrd with
/// `password` (if any) if it waits there; shows how it came back; and
/// scrubs its scrub_mounts.  If it's up, that's taken to be the boot it
/// came back in.  Returns the problems (none if all is well).
pub fn catch(ssh: &Ssh, machine: &Machine, password: Option<&str>, printer: &mut Printer) -> Result<Vec<String>> {
    let hostname = &machine.hostname;
    if password.is_none() {
        printer.line(&format!("There's no stored LUKS password for {hostname}, so its initrd won't be unlocked"));
    }
    let problems = come_back(ssh, machine, None, password, printer)?;
    show_done(hostname, "back", &problems, printer);
    Ok(problems)
}

/// Waits for `machine` to stop accepting SSH, as it does on its way down,
/// after being asked to shut down.  Tries once per [`RETRY_INTERVAL`] until
/// `deadline`, passing each try that found it up to `on_retry`.
fn wait_for_down(ssh: &Ssh, machine: &Machine, deadline: Deadline, on_retry: impl FnMut(&anyhow::Error)) -> Result<()> {
    let target = machine.target();
    let attempt = |deadline: Deadline| -> Result<()> {
        let mut session = match Session::open(ssh, &target, deadline.at_most(OPEN_TIMEOUT).remaining()) {
            Ok(session) => session,
            // Only ssh's finding no server there shows that it's down: not
            // a server's refusing us (as when a key isn't touched), nor our
            // giving up on ssh.
            Err(error) if is_unreachable(&error.root_cause().to_string()) => {
                debug!("{target} is down: {error:#}");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        match facts::system_state(&mut session)?.as_str() {
            "stopping" => bail!("it's shutting down"),
            state => bail!("it's up: systemd says the system is {state}"),
        }
    };
    retry(deadline, RETRY_INTERVAL, attempt, on_retry).with_context(|| format!("{} didn't go down", machine.hostname))
}

/// Shuts `machine` down, telling the user about it with `printer`: checks
/// that it's okay to shut down; stops its stop_services; checks again, apart
/// from load and network traffic (leaving the stop_services stopped if it's
/// no longer okay); powers it off; and waits for it to stop accepting SSH,
/// which is as much of its going down as can be seen.  Problems along the
/// way are collected for the end.
///
/// Errors say whether they happened before or after asking the machine to
/// shut down.
pub fn stop(ssh: &Ssh, machine: &Machine, printer: &mut Printer) -> Result<Outcome> {
    let hostname = &machine.hostname;
    let not_stopped = || format!("didn't shut down {hostname}");
    let (mut session, facts) = match check(ssh, machine, Down::Shutdown, printer).with_context(not_stopped)? {
        Ok(okay) => okay,
        Err(blockers) => return Ok(Outcome::NotOkay(blockers)),
    };
    let systems = &facts.systems;
    printer.line(&format!("{hostname} is okay to shut down, and will boot {} with kernel {} next time", printer.bold(&systems.default), printer.bold(&systems.default_kernel)));
    let (_, problems) = match stop_and_check_again(machine, &mut session, Down::Shutdown, false, printer).with_context(not_stopped)? {
        Ok(stopped) => stopped,
        Err(blockers) => return Ok(Outcome::NotOkay(blockers)),
    };
    printer.line(&format!("Asking {hostname} to shut down"));
    reboot::ask(session, Down::Shutdown).with_context(|| format!("failed to ask {hostname} to shut down"))?;
    // So that the first try doesn't log in to the machine on its way down,
    // which costs a key touch for some.
    sleep(RETRY_INTERVAL);

    printer.line(&format!("Waiting for {} to stop accepting SSH", machine.target()));
    wait_for_down(ssh, machine, Deadline::after(SHUTDOWN_TIMEOUT), |error| show_last_try(printer, error))
        .with_context(|| format!("after asking {hostname} to shut down"))?;
    show_done(hostname, "down", &problems, printer);
    Ok(Outcome::Done(problems))
}

/// The local time of day, like "12:03:16".
fn time_of_day() -> String {
    Zoned::now().strftime("%H:%M:%S").to_string()
}

/// The width of the terminal on stdout, or 80 columns if that's unknown.
fn terminal_columns() -> usize {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ fills in the winsize that its argument points to.
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 { size.ws_col.into() } else { 80 }
}

/// Calls `print` with a [`Printer`] to stdout that timestamps lines, is in
/// color if `color`, and shows progress if stdout is a terminal.
fn with_stdout_printer<T>(color: bool, print: impl FnOnce(&mut Printer) -> T) -> T {
    let mut stdout = io::stdout();
    let columns = stdout.is_terminal().then_some(terminal_columns as fn() -> usize);
    print(&mut Printer::new(&mut stdout, color, columns, Some(time_of_day)))
}

/// Bounces the configured machine `hostname` (see [`bounce`]), in color if
/// `color`.  Returns the exit status (see [`Outcome::exit_status`]).
pub fn run(hostname: &str, color: bool) -> Result<u8> {
    let machines = config::load(&config::config_dir()?)?;
    let machine = config::find(&machines, hostname)?;
    let password = || passwords::load(&config::passwords_dir()?, hostname, &passwords::master_key(&machine.luks_signing_key)?);
    Ok(with_stdout_printer(color, |printer| bounce(&Ssh::default(), machine, password, printer))?.exit_status())
}

/// Shuts down the configured machine `hostname` (see [`stop`]), in color if
/// `color`.  Returns the exit status (see [`Outcome::exit_status`]).
pub fn run_stop(hostname: &str, color: bool) -> Result<u8> {
    let machines = config::load(&config::config_dir()?)?;
    let machine = config::find(&machines, hostname)?;
    Ok(with_stdout_printer(color, |printer| stop(&Ssh::default(), machine, printer))?.exit_status())
}

/// Catches the configured machine `hostname` (see [`catch`]), with its
/// stored LUKS password if it has one, in color if `color`.  Returns the
/// exit status: 0 if it came back fine, or 1 if it came back with problems.
pub fn run_catch(hostname: &str, color: bool) -> Result<u8> {
    let machines = config::load(&config::config_dir()?)?;
    let machine = config::find(&machines, hostname)?;
    let dir = config::passwords_dir()?;
    let file = passwords::password_file(&dir, hostname)?;
    let load = || passwords::load(&dir, hostname, &passwords::master_key(&machine.luks_signing_key)?);
    let password = if file.try_exists().with_context(|| format!("failed to check for {}", file.display()))? {
        Some(load().context("couldn't get the stored LUKS password")?)
    } else {
        None
    };
    let problems = with_stdout_printer(color, |printer| catch(&Ssh::default(), machine, password.as_deref(), printer))?;
    Ok(if problems.is_empty() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preflight::idle_facts;

    fn printed(color: bool, columns: Option<fn() -> usize>, clock: Option<fn() -> String>, print: impl FnOnce(&mut Printer)) -> String {
        let mut out = Vec::new();
        print(&mut Printer::new(&mut out, color, columns, clock));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn prints_progress_over_itself() {
        let text = printed(false, Some(|| 80), None, |printer| {
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
        let text = printed(false, Some(|| 6), None, |printer| {
            printer.progress("1234567");
            printer.line("done");
        });
        assert_eq!(text, "\r1234…\r     \rdone\n");
    }

    #[test]
    fn prints_no_progress_to_a_file() {
        let text = printed(false, None, None, |printer| {
            printer.line("start");
            printer.progress("12345");
            printer.alarm("bad");
        });
        assert_eq!(text, "start\n!!!!!!!!!!!\n!!! bad !!!\n!!!!!!!!!!!\n");
    }

    #[test]
    fn styles_values_and_problems() {
        let text = printed(true, None, None, |printer| {
            printer.line(&format!("{} is fine", printer.bold("one")));
            printer.styled_line(Red, &indented_list("problems:", ["one", "two"]));
        });
        assert_eq!(text, "\x1b[1mone\x1b[0m is fine\n\x1b[31mproblems:\n    one\n    two\x1b[0m\n");
        assert_eq!(printed(false, None, None, |printer| printer.line(&printer.bold("plain"))), "plain\n");
    }

    #[test]
    fn prints_timestamps() {
        let text = printed(true, Some(|| 20), Some(|| "12:34:56".into()), |printer| {
            printer.styled_line(Red, &indented_list("problems:", ["one"]));
            printer.progress("two\nlines");
            printer.progress("more than fits in a row");
        });
        let time = "\x1b[2m12:34:56\x1b[0m";
        assert_eq!(text, format!("{time} \x1b[31mproblems:\n             one\x1b[0m\n\r{time} two lines\r{time} more than…\n"));
        // Without room for the time and some of the text, just the text
        let text = printed(true, Some(|| 10), Some(|| "12:34:56".into()), |printer| printer.progress("cut short"));
        assert_eq!(text, "\rcut short\n");
    }

    #[test]
    fn tells_the_time() {
        let time = time_of_day();
        let shape = time.char_indices().all(|(i, c)| if i % 3 == 2 { c == ':' } else { c.is_ascii_digit() });
        assert!(time.len() == 8 && shape, "{time}");
    }

    #[test]
    fn shows_why_the_last_try_failed() {
        let stderr = "Warning: Permanently added '10.0.0.5' (ED25519) to the list of known hosts.\r\nConnection refused\n";
        let text = printed(false, Some(|| 80), None, |printer| {
            show_last_try(printer, &anyhow::anyhow!(stderr).context("failed to open a session to one"));
            show_last_try(printer, &anyhow::anyhow!(" \n").context("failed to open a session to one"));
        });
        assert_eq!(text, "\rLatest try: Connection refused\rLatest try: failed to open a session to one\n");
    }

    #[test]
    fn finds_postflight_problems() {
        let systems = idle_facts().systems;
        assert_eq!(postflight_problems(&systems, "running", &[]), Vec::<String>::new());
        let booted_older = Systems { booted: "/nix/store/bbb-nixos-system-one-26.05".into(), running_kernel: "6.18.53".into(), ..systems.clone() };
        assert_eq!(
            postflight_problems(&booted_older, "degraded", &["a.service".into(), "b.service".into()]),
            [
                "failed units: a.service, b.service",
                "booted /nix/store/bbb-nixos-system-one-26.05, not /nix/store/aaa-nixos-system-one-26.05",
                "booted kernel 6.18.53, not 6.18.54",
            ]
        );
        assert_eq!(postflight_problems(&systems, "maintenance", &[]), ["systemd says the system is maintenance"]);
    }
}
