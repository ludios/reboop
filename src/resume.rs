//! Bringing back what was in a machine's tmux servers (see [`crate::tmux`])
//! after a reboot, or after `reboop tmux stop`: the captures that reboop
//! saves on the machine, and recreating what one of them describes.

use crate::bounce::Printer;
use crate::config::{self, Machine};
use crate::deadline::Deadline;
use crate::facts;
use crate::human::Style::Red;
use crate::ssh::{OPEN_TIMEOUT, QUICK, Session, Ssh, shell_quote};
use crate::tmux::{self, CommandPatterns, Pane, Server, TmuxSession, Verdict, Window, count, location};
use anyhow::{Context, Result, bail, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::thread::sleep;
use std::time::Duration;

/// Where captures are saved on a machine: one file per capture, named by
/// when it was taken and what for (see [`Capture::file_name`]).
pub const DIR: &str = "/var/lib/reboop/tmux";

/// A link in [`DIR`] to the capture taken right before reboop took the
/// machine's tmux servers down, until they're brought back.
const PENDING: &str = "/var/lib/reboop/tmux/pending";

/// How many captures are kept in [`DIR`]; older ones are deleted when a
/// capture is saved.
const KEEP: usize = 200;

/// How long mounts in progress get to finish before the servers are
/// brought back.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

const MOUNT_INTERVAL: Duration = Duration::from_secs(2);

/// How long a tmux server started as its user gets to answer at its socket.
const SERVER_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a pane's shell gets to reach its prompt.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a server gets to exit once asked to.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// The tmux buffer that commands are pasted through.
const BUFFER: &str = "reboop-resume";

/// What a capture was taken for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Purpose {
    /// A look at what's running, by `reboop check` or `reboop tmux list`
    Check,
    /// A reboot by `reboop bounce`
    Reboot,
    /// A shutdown by `reboop stop`
    Shutdown,
    /// `reboop tmux stop`
    TmuxStop,
}

impl fmt::Display for Purpose {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Purpose::Check => "check",
            Purpose::Reboot => "reboot",
            Purpose::Shutdown => "shutdown",
            Purpose::TmuxStop => "tmux-stop",
        })
    }
}

/// What was in a machine's tmux servers at some point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    pub captured_at: Timestamp,
    /// The boot it was taken in: see [`facts::boot_id`].
    pub boot_id: String,
    pub purpose: Purpose,
    pub servers: Vec<Server>,
}

impl Capture {
    /// A capture of `servers` taken now, in boot `boot_id`, for `purpose`.
    pub fn new(purpose: Purpose, boot_id: &str, servers: Vec<Server>) -> Capture {
        Capture { captured_at: Timestamp::now(), boot_id: boot_id.into(), purpose, servers }
    }

    /// Its file name in [`DIR`], like "20261107T063228Z.reboot.json", which
    /// sorts by time.
    pub fn file_name(&self) -> String {
        format!("{}.{}.json", self.captured_at.strftime("%Y%m%dT%H%M%SZ"), self.purpose)
    }

    /// Every pane in every server that could be listed, with its server
    /// and location (see [`tmux::location`]).
    fn panes(&self) -> impl Iterator<Item = (&Server, String, &Pane)> {
        self.servers.iter().flat_map(|server| server.panes().map(move |(location, pane)| (server, location, pane)))
    }

    /// What would be run again, one line per pane for people, given the
    /// commands that `patterns` allow: "at's work:0.1 in /data: rsync ...".
    pub fn resumable(&self, patterns: &CommandPatterns) -> Vec<String> {
        self.panes()
            .filter_map(|(server, location, pane)| match tmux::verdict(pane, patterns) {
                Verdict::Resume(command) => Some(format!("{}'s {location} in {}: {command}", server.user, pane.cwd)),
                Verdict::Idle | Verdict::Block(_) => None,
            })
            .collect()
    }

    /// "2 servers with 3 sessions and 8 panes", or "no tmux servers"
    pub fn counts(&self) -> String {
        if self.servers.is_empty() {
            return "no tmux servers".into();
        }
        let sessions = self.servers.iter().map(|server| server.sessions.len()).sum();
        let panes = self.panes().count();
        format!("{} with {} and {}", count(self.servers.len(), "tmux server"), count(sessions, "session"), count(panes, "pane"))
    }
}

/// Saves `capture` on the machine, in [`DIR`] under its file name, for root
/// only, deleting all but the latest [`KEEP`] captures there; and if
/// `pending`, points [`PENDING`] at it.  Returns the file's path.
pub fn save(session: &mut Session, capture: &Capture, pending: bool) -> Result<String> {
    let name = capture.file_name();
    let json = serde_json::to_string_pretty(capture)?;
    let link = if pending { format!("ln -sfn {name} {PENDING}\n") } else { String::new() };
    let script = format!(
        "set -e\numask 077\nmkdir -p {DIR}\nprintf %s {} >{DIR}/{name}.tmp\nmv {DIR}/{name}.tmp {DIR}/{name}\n{link}ls -1 {DIR}/*.json | head -n -{KEEP} | xargs -r rm -f",
        shell_quote(&json)
    );
    session.run_ok(&script, QUICK).context("failed to save the tmux capture")?;
    Ok(format!("{DIR}/{name}"))
}

/// The path in [`DIR`] of a saved capture given by its name there, or by its
/// path.
pub fn saved_path(name_or_path: &str) -> String {
    if name_or_path.starts_with('/') { name_or_path.to_string() } else { format!("{DIR}/{name_or_path}") }
}

/// Reads the capture saved at `path` on the machine.
pub fn load(session: &mut Session, path: &str) -> Result<Capture> {
    let json = session.run_ok(&format!("cat {}", shell_quote(path)), QUICK).with_context(|| format!("failed to read {path}"))?;
    serde_json::from_str(&json).with_context(|| format!("{path} isn't a capture"))
}

/// The capture that [`PENDING`] points to, with its path, if there is one.
pub fn pending(session: &mut Session) -> Result<Option<(String, Capture)>> {
    let output = session.run(&format!("test -L {PENDING} && readlink {PENDING}"), QUICK)?;
    if output.status != 0 {
        return Ok(None);
    }
    let path = saved_path(output.stdout_text().trim_end());
    Ok(Some((path.clone(), load(session, &path)?)))
}

/// Removes [`PENDING`], keeping the capture it pointed to.
pub fn forget_pending(session: &mut Session) -> Result<()> {
    session.run_ok(&format!("rm -f {PENDING}"), QUICK).map(drop)
}

/// A tmux command that failed, as opposed to a session that did.
#[derive(Debug)]
struct TmuxFailed(String);

impl fmt::Display for TmuxFailed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TmuxFailed {}

/// Whether `error` is a tmux command's failure, which is a problem with
/// what it was asked to do, rather than the session's, which ends things.
fn is_tmux_failure(error: &anyhow::Error) -> bool {
    error.downcast_ref::<TmuxFailed>().is_some()
}

/// Runs `tmux ARGS` against the server at `socket`, returning its output.
/// A failure of tmux's is a [`TmuxFailed`].
fn run_tmux(session: &mut Session, socket: &str, args: &str) -> Result<String> {
    let output = session.run(&format!("tmux -S {} {args}", shell_quote(socket)), QUICK)?;
    if output.status != 0 {
        return Err(TmuxFailed(format!("tmux {} failed: {}", args.split_whitespace().next().unwrap_or_default(), output.stderr_text())).into());
    }
    Ok(output.stdout_text())
}

/// `value` as a tmux argument that tmux expands as a format, like a `-c`
/// directory or a window name, with its "#"s kept.
fn format_quote(value: &str) -> String {
    shell_quote(&value.replace('#', "##"))
}

/// Waits (up to [`MOUNT_TIMEOUT`]) until no mount or mount.* process is
/// running, so that filesystems a script is mounting in parallel are there.
/// Returns whether that happened.
fn wait_for_mounts(session: &mut Session, printer: &mut Printer) -> Result<bool> {
    let deadline = Deadline::after(MOUNT_TIMEOUT);
    loop {
        let comms = session.run_ok("ps -e -o comm=", QUICK)?;
        let mounting = comms.lines().map(str::trim).filter(|comm| *comm == "mount" || comm.starts_with("mount.")).count();
        if mounting == 0 {
            return Ok(true);
        }
        if deadline.has_passed() {
            return Ok(false);
        }
        printer.progress(&format!("Waiting for {} to finish before bringing tmux back", count(mounting, "mount process")));
        sleep(MOUNT_INTERVAL);
    }
}

/// Starts `server`'s user's tmux server at its socket, with its first
/// session, `first`, of `width` by `height`, whose first window starts in
/// `cwd`: inside a logind session of the user's (as a login would give
/// them), in a transient unit that lasts as long as the server does.
fn start_server(session: &mut Session, server: &Server, first: &TmuxSession, width: u32, height: u32, cwd: &str) -> Result<()> {
    let socket_dir = server.socket.rsplit_once('/').map_or(".", |(dir, _)| dir);
    let script = format!(
        "umask 077 && mkdir -p {} && exec tmux -S {} new-session -d -s {} -x {width} -y {height} -c {}",
        shell_quote(socket_dir),
        shell_quote(&server.socket),
        shell_quote(&first.name),
        format_quote(cwd)
    );
    let unit = format!("reboop-tmux-$(systemd-escape -- {})-$(date +%s%N)", shell_quote(&server.user));
    let description = format!("reboop: {}'s tmux server at {}", server.user, server.socket);
    let start = format!(
        "unit={unit} && systemd-run --quiet --unit=\"$unit\" --uid={} -p PAMName=login -p ExitType=cgroup -p CollectMode=inactive-or-failed --description={} /bin/sh -c {} || exit 1
i=0; until tmux -S {} has-session -t {} 2>/dev/null; do i=$((i + 1)); [ $i -ge {} ] && {{ systemctl status --no-pager \"$unit\" 2>&1; exit 1; }}; sleep 0.2; done",
        shell_quote(&server.user),
        shell_quote(&description),
        shell_quote(&script),
        shell_quote(&server.socket),
        shell_quote(&format!("={}", first.name)),
        SERVER_TIMEOUT.as_millis() / 200
    );
    let output = session.run(&start, SERVER_TIMEOUT + QUICK)?;
    ensure!(output.status == 0, "{}'s tmux server at {} didn't start: {}\n{}", server.user, server.socket, output.stderr_text(), output.stdout_text().trim_end());
    Ok(())
}

/// Recreates `window` in the session `name` at `socket`: as the window
/// that's already there, `first` (its id, index and first pane's id, as the
/// session was created with it), or as a new one.  Returns its pane ids, in
/// the panes' order.
fn create_window(session: &mut Session, socket: &str, name: &str, window: &Window, first: Option<(&str, u32, &str)>) -> Result<Vec<String>> {
    let [first_pane, more @ ..] = &window.panes[..] else { bail!("the window has no panes") };
    let (window_id, mut pane_ids) = match first {
        Some((id, index, pane_id)) => {
            if index != window.index {
                run_tmux(session, socket, &format!("move-window -s {id} -t {}", shell_quote(&format!("={name}:{}", window.index))))?;
            }
            if let Some(window_name) = &window.name {
                run_tmux(session, socket, &format!("rename-window -t {id} {}", format_quote(window_name)))?;
            }
            (id.to_string(), vec![pane_id.to_string()])
        }
        None => {
            let named = window.name.as_ref().map_or(String::new(), |window_name| format!(" -n {}", format_quote(window_name)));
            let target = shell_quote(&format!("={name}:{}", window.index));
            let ids = run_tmux(session, socket, &format!("new-window -d -t {target} -c {}{named} -P -F '#{{window_id}} #{{pane_id}}'", format_quote(&first_pane.cwd)))?;
            let (window_id, pane_id) = ids.trim_end().split_once(' ').ok_or_else(|| TmuxFailed(format!("new-window printed {ids:?}")))?;
            (window_id.to_string(), vec![pane_id.to_string()])
        }
    };
    // Each pane after the first is split off below the one before it, at
    // full width and 2 rows (as many as fit), for the layout to size.
    for pane in more {
        let previous = pane_ids.last().unwrap().clone();
        let id = run_tmux(session, socket, &format!("split-window -d -f -v -l 2 -t {previous} -c {} -P -F '#{{pane_id}}'", format_quote(&pane.cwd)))?;
        pane_ids.push(id.trim_end().to_string());
    }
    run_tmux(session, socket, &format!("select-layout -t {window_id} {}", shell_quote(&window.layout)))?;
    if let Some(active) = window.panes.iter().position(|pane| pane.active) {
        run_tmux(session, socket, &format!("select-pane -t {}", pane_ids[active]))?;
    }
    if window.active {
        run_tmux(session, socket, &format!("select-window -t {window_id}"))?;
    }
    Ok(pane_ids)
}

/// Why a pane's command can't be run again.
enum NotReady {
    /// Its shell didn't reach its prompt within [`PROMPT_TIMEOUT`].
    NoPrompt,
    /// Its shell is in this directory, not the pane's.
    Elsewhere(String),
    /// The pane's directory is on the filesystem mounted here, not where it
    /// was.
    OtherMount(String),
}

/// Waits (up to [`PROMPT_TIMEOUT`]) for the shell in pane `pane_id` to be at
/// its prompt, and checks that it's in `pane`'s directory, which is on the
/// filesystem mounted where it was.
fn wait_for_prompt(session: &mut Session, socket: &str, pane_id: &str, pane: &Pane) -> Result<Result<(), NotReady>> {
    let script = format!(
        r#"set -- $(tmux -S {} display -p -t {pane_id} -F '#{{pane_pid}} #{{pane_tty}}' 2>/dev/null)
pid=$1 tty=$2
[ -n "$tty" ] || exit 1
i=0; until stty -F "$tty" -a 2>/dev/null | grep -q -- '-icanon'; do i=$((i + 1)); [ $i -ge {} ] && exit 2; sleep 0.1; done
cwd=$(readlink /proc/$pid/cwd) || cwd=
[ "$cwd" = {cwd} ] || {{ printf '%s' "$cwd"; exit 3; }}
m=$(findmnt -T {cwd} -n -o TARGET 2>/dev/null | head -n1)
[ "$m" = {} ] || {{ printf '%s' "$m"; exit 4; }}"#,
        shell_quote(socket),
        PROMPT_TIMEOUT.as_millis() / 100,
        shell_quote(&pane.mountpoint),
        cwd = shell_quote(&pane.cwd)
    );
    let output = session.run(&script, PROMPT_TIMEOUT + QUICK)?;
    Ok(match output.status {
        0 => Ok(()),
        1 => bail!("pane {pane_id} is gone"),
        2 => Err(NotReady::NoPrompt),
        3 => Err(NotReady::Elsewhere(output.stdout_text())),
        4 => Err(NotReady::OtherMount(output.stdout_text())),
        status => bail!("checking on pane {pane_id} exited with status {status}: {}", output.stderr_text()),
    })
}

/// Runs the commands of the panes of `window` (which has pane ids
/// `pane_ids`) again in them, where `patterns` allow, telling the user.
/// Returns the problems: commands that couldn't be run.
fn run_commands(session: &mut Session, server: &Server, name: &str, window: &Window, pane_ids: &[String], patterns: &CommandPatterns, printer: &mut Printer) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for (pane, pane_id) in window.panes.iter().zip(pane_ids) {
        let where_ = format!("{}'s {} in {}", server.user, location(name, window.index, pane.index), pane.cwd);
        let command = match tmux::verdict(pane, patterns) {
            Verdict::Idle => continue,
            Verdict::Block(why) => {
                printer.line(&format!("Left {where_} idle: {why}"));
                continue;
            }
            Verdict::Resume(command) => command,
        };
        let not_run = |why: String| format!("{where_}: {command} wasn't run again: {why}");
        match wait_for_prompt(session, &server.socket, pane_id, pane)? {
            Ok(()) => {}
            Err(NotReady::NoPrompt) => {
                problems.push(not_run(format!("its shell didn't reach its prompt within {}s", PROMPT_TIMEOUT.as_secs())));
                continue;
            }
            Err(NotReady::Elsewhere(cwd)) => {
                problems.push(not_run(format!("its shell is in {cwd:?} (does {} exist?)", pane.cwd)));
                continue;
            }
            Err(NotReady::OtherMount(mountpoint)) => {
                problems.push(not_run(format!("{} is on the filesystem mounted at {mountpoint:?}, not {:?} as before", pane.cwd, pane.mountpoint)));
                continue;
            }
        }
        let run = match &pane.running {
            // Pasted as typed (with bracketed paste, so a line break doesn't
            // run what's before it), then Enter.
            tmux::Running::Typed { command } => {
                let socket = shell_quote(&server.socket);
                format!(
                    "tmux -S {socket} set-buffer -b {BUFFER} -- {} && tmux -S {socket} paste-buffer -p -d -b {BUFFER} -t {pane_id} && tmux -S {socket} send-keys -t {pane_id} Enter",
                    shell_quote(command)
                )
            }
            // Through tmux's own parser, which takes the quoting back
            tmux::Running::Started { command } => format!("printf '%s\\n' {} | tmux -S {} source-file -", shell_quote(&format!("respawn-pane -k -t {pane_id} {command}")), shell_quote(&server.socket)),
            _ => unreachable!("only typed and started commands are resumable"),
        };
        let output = session.run(&run, QUICK)?;
        if output.status == 0 {
            printer.line(&format!("Resumed {where_}: {command}"));
        } else {
            problems.push(not_run(format!("tmux failed: {}", output.stderr_text())));
        }
    }
    Ok(problems)
}

/// Brings back `server`'s sessions (into the server at its socket if one's
/// running, else into one started for it), running their commands again
/// where `patterns` allow, telling the user.  Returns the problems.
fn restore_server(session: &mut Session, server: &Server, patterns: &CommandPatterns, printer: &mut Printer) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let socket = &server.socket;
    // The sessions already there, if a server's running at the socket
    let mut existing = match run_tmux(session, socket, "list-sessions -F '#{session_name}'") {
        Ok(names) => Some(names.lines().map(String::from).collect::<Vec<_>>()),
        Err(error) if is_tmux_failure(&error) => None,
        Err(error) => return Err(error),
    };
    for tmux_session in &server.sessions {
        let [first, ..] = &tmux_session.windows[..] else { bail!("session {} has no windows", tmux_session.name) };
        let [first_pane, ..] = &first.panes[..] else { bail!("window {}:{} has no panes", tmux_session.name, first.index) };
        if existing.as_ref().is_some_and(|names| names.contains(&tmux_session.name)) {
            problems.push(format!("{}'s tmux at {socket} already has a session called {}, which was left alone", server.user, tmux_session.name));
            continue;
        }
        // Big enough for every window's panes to be split off before the
        // layouts size them
        let width  = tmux_session.windows.iter().map(|window| window.width).max().unwrap_or(80);
        let height = tmux_session.windows.iter().map(|window| window.height).max().unwrap_or(24);
        let created = match &mut existing {
            None => {
                start_server(session, server, tmux_session, width, height, &first_pane.cwd)?;
                existing = Some(vec![]);
                run_tmux(session, socket, &format!("display -p -t {} -F '#{{window_id}} #{{window_index}} #{{pane_id}}'", shell_quote(&format!("={}:^", tmux_session.name))))?
            }
            Some(_) => run_tmux(
                session,
                socket,
                &format!("new-session -d -s {} -x {width} -y {height} -c {} -P -F '#{{window_id}} #{{window_index}} #{{pane_id}}'", shell_quote(&tmux_session.name), format_quote(&first_pane.cwd)),
            )?,
        };
        existing.as_mut().unwrap().push(tmux_session.name.clone());
        let [window_id, index, pane_id] = created.split_whitespace().collect::<Vec<_>>()[..] else { bail!("unexpected tmux output {created:?}") };
        let index: u32 = index.parse().with_context(|| format!("unexpected tmux output {created:?}"))?;
        for (i, window) in tmux_session.windows.iter().enumerate() {
            let first = (i == 0).then_some((window_id, index, pane_id));
            let pane_ids = match create_window(session, socket, &tmux_session.name, window, first) {
                Ok(pane_ids) => pane_ids,
                Err(error) if is_tmux_failure(&error) => {
                    problems.push(format!("{}'s {}:{} couldn't be recreated: {error:#}", server.user, tmux_session.name, window.index));
                    continue;
                }
                Err(error) => return Err(error),
            };
            problems.extend(run_commands(session, server, &tmux_session.name, window, &pane_ids, patterns, printer)?);
        }
    }
    Ok(problems)
}

/// Brings back what `capture` describes, running the commands that
/// `patterns` allow again, once mounts in progress are done, telling the
/// user.  Returns the problems (none if all is well).
pub fn restore(session: &mut Session, capture: &Capture, patterns: &CommandPatterns, printer: &mut Printer) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    if capture.servers.is_empty() {
        printer.line("There were no tmux servers to bring back");
        return Ok(problems);
    }
    if !wait_for_mounts(session, printer)? {
        let problem = format!("mount processes were still running after {}s; bringing tmux back anyway", MOUNT_TIMEOUT.as_secs());
        printer.styled_line(Red, &problem);
        problems.push(problem);
    }
    for server in &capture.servers {
        if let Some(error) = &server.error {
            problems.push(format!("{}'s tmux server at {} couldn't be listed when captured ({error}), so there's nothing to bring back", server.user, server.socket));
            continue;
        }
        printer.line(&format!("Bringing back {}'s tmux server at {}: {}", server.user, server.socket, server.counts()));
        problems.extend(restore_server(session, server, patterns, printer)?);
    }
    Ok(problems)
}

/// Brings back what the capture pending on the machine (see [`PENDING`])
/// describes, if one is: see [`restore`], which `machine`'s
/// resumable_commands are for.  Not if the capture is from this very boot
/// and was taken for a reboot or shutdown: then the machine didn't go down
/// after all, and what it describes is still running.  Either way, the
/// capture is no longer pending.  Returns the problems.
pub fn restore_pending(session: &mut Session, machine: &Machine, printer: &mut Printer) -> Result<Vec<String>> {
    let Some((path, capture)) = pending(session)? else { return Ok(vec![]) };
    // First, so that a restore that's cut short isn't repeated on top of
    // itself; the capture stays in DIR for `reboop tmux start --saved`.
    forget_pending(session)?;
    if capture.purpose != Purpose::TmuxStop && capture.boot_id == facts::boot_id(session)? {
        printer.line(&format!("{path} is from this boot, so what it describes is still running; not bringing it back"));
        return Ok(vec![]);
    }
    printer.line(&format!("Bringing back what was in tmux, from {path}"));
    restore(session, &capture, &machine.resumable_commands, printer)
}

/// Kills every server in `capture`, waiting for each to exit.
fn stop_servers(session: &mut Session, capture: &Capture, printer: &mut Printer) -> Result<()> {
    for server in &capture.servers {
        let script = format!(
            "tmux -S {} kill-server; i=0; while [ -d /proc/{} ]; do i=$((i + 1)); [ $i -ge {} ] && exit 1; sleep 0.1; done",
            shell_quote(&server.socket),
            server.pid,
            STOP_TIMEOUT.as_millis() / 100
        );
        let output = session.run(&script, STOP_TIMEOUT + QUICK)?;
        ensure!(output.status == 0, "{}'s tmux server at {} (pid {}) didn't exit within {}s", server.user, server.socket, server.pid, STOP_TIMEOUT.as_secs());
        printer.line(&format!("Stopped {}'s tmux server at {}", server.user, server.socket));
    }
    Ok(())
}

/// Opens a session to `machine`, making sure it's the machine.
fn open(ssh: &Ssh, machine: &Machine) -> Result<Session> {
    let mut session = Session::open(ssh, &machine.target(), OPEN_TIMEOUT)?;
    facts::ensure_hostname(&mut session, &machine.hostname)?;
    Ok(session)
}

/// Lists what's in `machine`'s tmux servers (or in the capture `saved` on
/// it, if given: see [`saved_path`]), telling the user: see
/// [`tmux::lines`].  A look at the servers is saved on the machine.
pub fn list(ssh: &Ssh, machine: &Machine, saved: Option<&str>, printer: &mut Printer) -> Result<()> {
    let mut session = open(ssh, machine)?;
    let capture = match saved {
        Some(saved) => load(&mut session, &saved_path(saved))?,
        None => {
            let capture = Capture::new(Purpose::Check, &facts::boot_id(&mut session)?, tmux::capture(&mut session)?);
            save(&mut session, &capture, false)?;
            capture
        }
    };
    if capture.servers.is_empty() {
        printer.line(&format!("{} has no tmux servers", machine.hostname));
    }
    for server in &capture.servers {
        printer.line(&tmux::lines(server, machine.tmux, &machine.resumable_commands).join("\n"));
    }
    Ok(())
}

/// Stops `machine`'s tmux servers if every pane's command is resumable,
/// saving what's in them first for `start` to bring back, telling the user.
/// Returns the reasons not to, if it wasn't okay (none if the servers were
/// stopped).
pub fn stop(ssh: &Ssh, machine: &Machine, printer: &mut Printer) -> Result<Vec<String>> {
    let hostname = &machine.hostname;
    let mut session = open(ssh, machine)?;
    let capture = Capture::new(Purpose::TmuxStop, &facts::boot_id(&mut session)?, tmux::capture(&mut session)?);
    // By the patterns alone: this is the user's own decision to stop tmux.
    let blockers = tmux::blockers(config::TmuxPolicy::Resume, &machine.resumable_commands, &capture.servers);
    if !blockers.is_empty() {
        printer.styled_line(Red, &format!("Not stopping tmux on {hostname}:\n    {}", blockers.join("\n    ")));
        return Ok(blockers);
    }
    if capture.servers.is_empty() {
        printer.line(&format!("{hostname} has no tmux servers to stop"));
        return Ok(vec![]);
    }
    let path = save(&mut session, &capture, true)?;
    printer.line(&format!("Saved what's in {hostname}'s tmux to {path}, for `reboop tmux start {hostname}` to bring back: {}", capture.counts()));
    let resumable = capture.resumable(&machine.resumable_commands);
    if !resumable.is_empty() {
        printer.line(&format!("Commands to run again then:\n    {}", resumable.join("\n    ")));
    }
    stop_servers(&mut session, &capture, printer)?;
    Ok(vec![])
}

/// Brings back what's pending on `machine` (see [`restore_pending`]), or the
/// capture `saved` on it if given (see [`saved_path`]), telling the user.
/// Returns the problems.  Fails if nothing is pending and nothing is given.
pub fn start(ssh: &Ssh, machine: &Machine, saved: Option<&str>, printer: &mut Printer) -> Result<Vec<String>> {
    let hostname = &machine.hostname;
    let mut session = open(ssh, machine)?;
    let problems = match saved {
        Some(saved) => {
            let path = saved_path(saved);
            let capture = load(&mut session, &path)?;
            printer.line(&format!("Bringing back what was in tmux, from {path}"));
            restore(&mut session, &capture, &machine.resumable_commands, printer)?
        }
        None => {
            ensure!(
                pending(&mut session)?.is_some(),
                "no tmux capture is pending on {hostname}; `reboop tmux start {hostname} --saved FILE` brings back one of those in {DIR}"
            );
            restore_pending(&mut session, machine, printer)?
        }
    };
    if problems.is_empty() {
        printer.line(&format!("tmux is back on {hostname}, and all is well"));
    } else {
        printer.styled_line(Red, &format!("tmux is back on {hostname}, but:\n    {}", problems.join("\n    ")));
    }
    Ok(problems)
}

/// Runs `command` on the configured machine `hostname`, in color if `color`.
fn with_machine<T>(hostname: &str, color: bool, command: impl FnOnce(&Ssh, &Machine, &mut Printer) -> Result<T>) -> Result<T> {
    let machines = config::load(&config::config_dir()?)?;
    let machine = config::find(&machines, hostname)?;
    crate::bounce::with_stdout_printer(color, |printer| command(&Ssh::default(), machine, printer))
}

/// `reboop tmux list`: see [`list`].  Returns the exit status: 0.
pub fn run_list(hostname: &str, saved: Option<&str>, color: bool) -> Result<u8> {
    with_machine(hostname, color, |ssh, machine, printer| list(ssh, machine, saved, printer))?;
    Ok(0)
}

/// `reboop tmux stop`: see [`stop`].  Returns the exit status: 0 if the
/// servers were stopped, or 2 if it wasn't okay to.
pub fn run_stop(hostname: &str, color: bool) -> Result<u8> {
    let blockers = with_machine(hostname, color, stop)?;
    Ok(if blockers.is_empty() { 0 } else { 2 })
}

/// `reboop tmux start`: see [`start`].  Returns the exit status: 0 if all is
/// well, or 1 if there were problems.
pub fn run_start(hostname: &str, saved: Option<&str>, color: bool) -> Result<u8> {
    let problems = with_machine(hostname, color, |ssh, machine, printer| start(ssh, machine, saved, printer))?;
    Ok(if problems.is_empty() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::tests::{patterns, test_server};

    #[test]
    fn names_files_by_time_and_purpose() {
        let mut capture = Capture::new(Purpose::Reboot, "4f6f3fbb", vec![]);
        capture.captured_at = "2026-11-07T06:32:28.5Z".parse().unwrap();
        assert_eq!(capture.file_name(), "20261107T063228Z.reboot.json");
        capture.purpose = Purpose::TmuxStop;
        assert_eq!(capture.file_name(), "20261107T063228Z.tmux-stop.json");
        assert_eq!(saved_path("20261107T063228Z.check.json"), "/var/lib/reboop/tmux/20261107T063228Z.check.json");
        assert_eq!(saved_path("/tmp/x.json"), "/tmp/x.json");
    }

    #[test]
    fn captures_round_trip_as_json() {
        let capture = Capture::new(Purpose::Shutdown, "4f6f3fbb", vec![test_server()]);
        let json = serde_json::to_string_pretty(&capture).unwrap();
        assert!(json.contains("\"purpose\": \"shutdown\""), "{json}");
        assert_eq!(serde_json::from_str::<Capture>(&json).unwrap(), capture);
        assert_eq!(capture.counts(), "1 tmux server with 1 session and 3 panes");
        assert_eq!(Capture::new(Purpose::Check, "x", vec![]).counts(), "no tmux servers");
        assert_eq!(capture.resumable(&patterns(&["rsync .*", "htop"])), ["at's work:0.1 in /data: rsync -a /data/ /backup/", "at's work:3.0 in /home/at: htop"]);
    }

    #[test]
    fn quotes_formats() {
        assert_eq!(format_quote("/a/b#c"), "'/a/b##c'");
        assert_eq!(format_quote("it's"), r"'it'\''s'");
    }
}
