//! Bringing back what was in a machine's tmux servers (see [`crate::tmux`])
//! after a reboot, or after `reboop tmux stop`: the captures that reboop
//! saves on the machine, and recreating what one of them describes.

use crate::bounce::{Printer, indented_list};
use crate::config::{self, Machine, TmuxPolicy};
use crate::deadline::Deadline;
use crate::facts;
use crate::human::Style::Red;
use crate::ssh::{OPEN_TIMEOUT, QUICK, Session, Ssh, shell_quote};
use crate::tmux::{self, CommandPatterns, Pane, Running, Server, TmuxSession, Verdict, Window, count, location};
use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::thread::sleep;
use std::time::Duration;

/// Where captures are saved on a machine: one file per capture, named by
/// when it was taken and what for (see [`Capture::file_name`]).
pub const DIR: &str = "/var/lib/reboop/tmux";

/// A link in [`DIR`] to the capture taken right before reboop took the
/// machine's tmux servers down, until bringing them back is tried.
const PENDING: &str = "/var/lib/reboop/tmux/pending";

/// How many captures are kept in [`DIR`]; older ones are deleted when a
/// capture is saved, except the one that's pending.
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

/// The width and height of a session as it's recreated: room for 200 panes,
/// split off one below another, before each window's layout sizes them.
/// (A client resizes it on attaching anyway.)
const STAGING_SIZE: u32 = 400;

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

    /// Whether it was taken for a reboot or shutdown during the boot
    /// `boot_id`: then if that's the current boot, the machine didn't go
    /// down after all.
    pub fn outlived(&self, boot_id: &str) -> bool {
        self.purpose != Purpose::TmuxStop && self.boot_id == boot_id
    }

    /// Its file name in [`DIR`], like "20261107T063228.512Z.reboot.json",
    /// which sorts by time.
    pub fn file_name(&self) -> String {
        let millis = self.captured_at.as_millisecond().rem_euclid(1000);
        format!("{}.{millis:03}Z.{}.json", self.captured_at.strftime("%Y%m%dT%H%M%S"), self.purpose)
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

    /// "2 tmux servers with 3 sessions and 8 panes", or "no tmux servers"
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
/// only, deleting all but the latest [`KEEP`] captures there (and the one
/// that's pending); and if `pending`, points [`PENDING`] at it.  Returns the
/// file's path.
pub fn save(session: &mut Session, capture: &Capture, pending: bool) -> Result<String> {
    let name = capture.file_name();
    let json = serde_json::to_string_pretty(capture)?;
    let link = if pending { format!("ln -sfn {name} {PENDING}\n") } else { String::new() };
    // Written whole and synced before it's named, so that a crash leaves
    // either the capture or nothing
    let script = format!(
        "set -e\numask 077\nmkdir -p {DIR}\ntmp=$(mktemp {DIR}/.capture.XXXXXX)\ntrap 'rm -f \"$tmp\"' EXIT\nprintf %s {} >\"$tmp\"\nsync \"$tmp\"\nmv \"$tmp\" {DIR}/{name}\n{link}sync {DIR}\n\
         keep=$(readlink {PENDING} 2>/dev/null || true)\nls -1 {DIR}/*.json | head -n -{KEEP} | grep -vxF \"{DIR}/$keep\" | xargs -r rm -f",
        shell_quote(&json)
    );
    session.run_ok(&script, QUICK).context("failed to save the tmux capture")?;
    Ok(format!("{DIR}/{name}"))
}

/// The path of a saved capture given as an absolute path, or by its name in
/// [`DIR`].
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

/// Removes [`PENDING`] for good (syncing [`DIR`]), keeping the capture it
/// pointed to.
pub fn forget_pending(session: &mut Session) -> Result<()> {
    session.run_ok(&format!("rm -f {PENDING} && sync {DIR}"), QUICK).map(drop)
}

/// Something about bringing a server back failed on the machine, as opposed
/// to the session failing: a problem to report, not a reason to stop.
#[derive(Debug)]
struct Failed(String);

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Failed {}

/// Whether `error` is a [`Failed`].
fn is_failure(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Failed>().is_some()
}

/// tmux, run with no environment, so that none of this session's reaches
/// what it makes: a session would get some (like SSH_CONNECTION), and a
/// pane made through a client that isn't attached gets the client's PATH.
/// (Sessions are made with -E too, since update-environment would take the
/// missing DISPLAY and such as reasons to unset the server's in them.)
const BARE_TMUX: &str = "env -i \"$(command -v tmux)\"";

/// Runs `tmux ARGS` against the server at `socket` (as [`BARE_TMUX`]),
/// returning its output.  A failure of tmux's is a [`Failed`].
fn run_tmux(session: &mut Session, socket: &str, args: &str) -> Result<String> {
    let output = session.run(&format!("{BARE_TMUX} -S {} {args}", shell_quote(socket)), QUICK)?;
    if output.status != 0 {
        return Err(Failed(format!("tmux {} failed: {}", args.split_whitespace().next().unwrap_or_default(), output.stderr_text())).into());
    }
    Ok(output.stdout_text())
}

/// `value` as a tmux argument that tmux expands as a format, like a `-c`
/// directory or a session or window name, with its "#"s kept.
fn format_quote(value: &str) -> String {
    shell_quote(&value.replace('#', "##"))
}

/// Waits (up to [`MOUNT_TIMEOUT`]) until no `mount` process is running, so
/// that filesystems a script is mounting in parallel are there.  (Just
/// mount itself, which waits for the mount.* helper it runs; some helpers
/// stay around as the filesystem's daemon.)  Returns whether that happened.
fn wait_for_mounts(session: &mut Session, printer: &mut Printer) -> Result<bool> {
    let deadline = Deadline::after(MOUNT_TIMEOUT);
    loop {
        let comms = session.run_ok("ps -e -o comm=", QUICK)?;
        let mounting = comms.lines().filter(|comm| comm.trim() == "mount").count();
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
/// session, `first`, whose first window starts in `cwd`: from a transient
/// unit that logs the user in through PAM, which puts the server in a
/// logind session scope of the user's, as a login would, where it stays
/// once the unit is done, like a tmux left behind by logging out.
fn start_server(session: &mut Session, server: &Server, first: &TmuxSession, cwd: &str) -> Result<()> {
    let socket_dir = server.socket.rsplit_once('/').map_or(".", |(dir, _)| dir);
    let script = format!(
        "umask 077 && mkdir -p {} && exec tmux -S {} new-session -d -s {} -x {STAGING_SIZE} -y {STAGING_SIZE} -c {}",
        shell_quote(socket_dir),
        shell_quote(&server.socket),
        format_quote(&first.name),
        format_quote(cwd)
    );
    let unit = format!("reboop-tmux-$(systemd-escape -- {})-$(date +%s%N)", shell_quote(&server.user));
    let description = format!("reboop: {}'s tmux server at {}", server.user, server.socket);
    // systemd-run would otherwise expand $VARIABLEs in the command line
    let start = format!(
        "unit={unit} && systemd-run --quiet --unit=\"$unit\" --uid={} -p PAMName=login --expand-environment=no --description={} /bin/sh -c {} || exit 1
i=0; until tmux -S {} has-session -t {} 2>/dev/null; do i=$((i + 1)); [ $i -ge {} ] && {{ systemctl status --no-pager \"$unit\" 2>&1; exit 1; }}; sleep 0.2; done",
        shell_quote(&server.user),
        shell_quote(&description),
        shell_quote(&script),
        shell_quote(&server.socket),
        shell_quote(&format!("={}", first.name)),
        SERVER_TIMEOUT.as_millis() / 200
    );
    let output = session.run(&start, SERVER_TIMEOUT + QUICK)?;
    if output.status != 0 {
        return Err(Failed(format!("the server didn't start: {}\n{}", output.stderr_text(), output.stdout_text().trim_end())).into());
    }
    Ok(())
}

/// Recreates `window` in the session `name` at `socket`: as the window
/// that's already there, `first` (its id, index and first pane's id, as the
/// session was created with it), or as a new one.  Returns its pane ids, in
/// the panes' order.
fn create_window(session: &mut Session, socket: &str, name: &str, window: &Window, first: Option<(&str, u32, &str)>) -> Result<Vec<String>> {
    let [first_pane, more @ ..] = &window.panes[..] else { return Err(Failed("the window has no panes".into()).into()) };
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
            let (window_id, pane_id) = ids.trim_end().split_once(' ').ok_or_else(|| Failed(format!("new-window printed {ids:?}")))?;
            (window_id.to_string(), vec![pane_id.to_string()])
        }
    };
    // Each pane after the first is split off below the one before it, at
    // full width and 2 rows, for the layout to size.
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

/// Why a pane isn't as it should be.
enum NotReady {
    /// It's gone already, as when its shell exited at once.
    Gone,
    /// Its shell didn't reach its prompt within [`PROMPT_TIMEOUT`].
    NoPrompt,
    /// Its process is in this directory, not the pane's.
    Elsewhere(String),
    /// The pane's directory is on the filesystem mounted here, not where it
    /// was.
    OtherMount(String),
}

/// Checks that the process in pane `pane_id` is in `pane`'s directory (by
/// its real path, as the pane's may go through a symlink), and that the
/// directory is on the filesystem mounted where it was.  If `prompt`, first
/// waits (up to [`PROMPT_TIMEOUT`]) for the pane's shell to be at its prompt:
/// in the foreground (its own process group, not a child's, is the
/// terminal's foreground one), reading in non-canonical mode, as a line
/// editor does.
fn check_pane(session: &mut Session, socket: &str, pane_id: &str, pane: &Pane, prompt: bool) -> Result<Result<(), NotReady>> {
    let script = format!(
        r#"set -- $(tmux -S {socket} display -p -t {pane_id} -F '#{{pane_pid}} #{{pane_tty}}' 2>/dev/null)
pid=$1 tty=$2
[ -n "$tty" ] || exit 1
if [ {} = 1 ]; then
    i=0
    until [ "$(ps -o tpgid= -p $pid | tr -d ' ')" = "$pid" ] && stty -F "$tty" -a 2>/dev/null | grep -q -- '-icanon'; do
        i=$((i + 1)); [ $i -ge {} ] && exit 2; sleep 0.1
    done
fi
cwd=$(readlink /proc/$pid/cwd) || cwd=
[ "$cwd" = "$(readlink -f -- {cwd})" ] || {{ printf '%s' "$cwd"; exit 3; }}
m=$(findmnt -T {cwd} -n -o TARGET 2>/dev/null | head -n1)
[ "$m" = {} ] || {{ printf '%s' "$m"; exit 4; }}"#,
        u8::from(prompt),
        PROMPT_TIMEOUT.as_millis() / 100,
        shell_quote(&pane.mountpoint),
        socket = shell_quote(socket),
        cwd = shell_quote(&pane.cwd)
    );
    let output = session.run(&script, PROMPT_TIMEOUT + QUICK)?;
    Ok(match output.status {
        0 => Ok(()),
        1 => Err(NotReady::Gone),
        2 => Err(NotReady::NoPrompt),
        3 => Err(NotReady::Elsewhere(output.stdout_text())),
        4 => Err(NotReady::OtherMount(output.stdout_text())),
        status => return Err(Failed(format!("checking on pane {pane_id} exited with status {status}: {}", output.stderr_text())).into()),
    })
}

/// A server being brought back.
struct Restoring<'a> {
    server: &'a Server,
    /// The commands that may be run again
    patterns: &'a CommandPatterns,
    /// The ids of the panes whose commands run again already (a window
    /// linked into several sessions comes back once per session)
    resumed: HashSet<String>,
}

/// Checks the panes of `window` (which has pane ids `pane_ids`) in session
/// `name` of the server being brought back, and runs their commands again
/// in them where allowed, telling the user.  Returns the problems: panes
/// that aren't where they should be, and commands that couldn't be run.
fn restore_panes(session: &mut Session, restoring: &mut Restoring, name: &str, window: &Window, pane_ids: &[String], printer: &mut Printer) -> Result<Vec<String>> {
    let server = restoring.server;
    let mut problems = Vec::new();
    for (pane, pane_id) in window.panes.iter().zip(pane_ids) {
        let verdict = tmux::verdict(pane, restoring.patterns);
        let where_ = format!("{}'s {} in {}", server.user, location(name, window.index, pane.index), pane.cwd);
        let command = match &verdict {
            Verdict::Resume(command) => Some(command.clone()),
            Verdict::Idle | Verdict::Block(_) => None,
        };
        let problem = |why: String| match &command {
            Some(command) => format!("{where_}: {command} wasn't run again: {why}"),
            None => format!("{where_}: {why}"),
        };
        let typed = matches!(pane.running, Running::Typed { .. });
        match check_pane(session, &server.socket, pane_id, pane, command.is_some() && typed)? {
            Ok(()) => {}
            Err(NotReady::Gone) => {
                problems.push(problem("the pane was gone right after it was made".into()));
                continue;
            }
            Err(NotReady::NoPrompt) => {
                problems.push(problem(format!("its shell didn't reach its prompt within {}s", PROMPT_TIMEOUT.as_secs())));
                continue;
            }
            Err(NotReady::Elsewhere(cwd)) => {
                problems.push(problem(format!("its shell is in {cwd:?} (does {} exist?)", pane.cwd)));
                continue;
            }
            Err(NotReady::OtherMount(mountpoint)) => {
                problems.push(problem(format!("{} is on the filesystem mounted at {mountpoint:?}, not {:?} as before", pane.cwd, pane.mountpoint)));
                continue;
            }
        }
        let Some(command) = &command else {
            if let Verdict::Block(why) = verdict {
                printer.line(&format!("Left {where_} idle: {why}"));
            }
            continue;
        };
        if !restoring.resumed.insert(pane.id.clone()) {
            printer.line(&format!("Left {where_} idle: it's the same pane as one brought back already"));
            continue;
        }
        let socket = shell_quote(&server.socket);
        let run = match &pane.running {
            // Pasted as typed (with bracketed paste, so a line break doesn't
            // run what's before it), then Enter
            Running::Typed { command } => format!(
                "tmux -S {socket} set-buffer -b {BUFFER} -- {} && tmux -S {socket} paste-buffer -p -d -b {BUFFER} -t {pane_id} && tmux -S {socket} send-keys -t {pane_id} Enter",
                shell_quote(command)
            ),
            // Through tmux's own parser, which takes the quoting back
            Running::Started { command } => format!("printf '%s\\n' {} | {BARE_TMUX} -S {socket} source-file -", shell_quote(&format!("respawn-pane -k -t {pane_id} {command}"))),
            _ => unreachable!("only typed and started commands are resumable"),
        };
        let output = session.run(&run, QUICK)?;
        if output.status == 0 {
            printer.line(&format!("Resumed {where_}: {command}"));
        } else {
            problems.push(problem(format!("tmux failed: {}", output.stderr_text())));
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
        Err(error) if is_failure(&error) => None,
        Err(error) => return Err(error),
    };
    // The first session brought back of each session group, whose windows
    // the group's others share
    let mut groups: HashMap<&str, &str> = HashMap::new();
    let mut restoring = Restoring { server, patterns, resumed: HashSet::new() };
    for tmux_session in &server.sessions {
        let name = &tmux_session.name;
        let not_recreated = |error: &anyhow::Error| format!("{}'s session {name} couldn't be recreated: {error:#}", server.user);
        if existing.as_ref().is_some_and(|names| names.contains(name)) {
            problems.push(format!("{}'s tmux at {socket} already has a session called {name}, which was left alone", server.user));
            // Its group's other sessions share its windows, still.
            if let Some(group) = &tmux_session.group {
                groups.entry(group).or_insert(name);
            }
            continue;
        }
        let Some(first_window) = tmux_session.windows.first() else {
            problems.push(not_recreated(&anyhow!("it has no windows")));
            continue;
        };
        let Some(first_pane) = first_window.panes.first() else {
            problems.push(not_recreated(&anyhow!("its window {} has no panes", first_window.index)));
            continue;
        };
        if let Some(first) = tmux_session.group.as_deref().and_then(|group| groups.get(group)) {
            // The group's windows are shared, but each session has its own
            // active one.
            let active = tmux_session.windows.iter().find(|window| window.active);
            let select = active.map_or(String::new(), |window| format!(" \\; select-window -t {}", shell_quote(&format!("={name}:{}", window.index))));
            let joined = format!("new-session -d -E -s {} -t {} -c {}{select}", format_quote(name), shell_quote(&format!("={first}")), format_quote(&first_pane.cwd));
            match run_tmux(session, socket, &joined) {
                Ok(_) => existing.get_or_insert_with(Vec::new).push(name.clone()),
                Err(error) if is_failure(&error) => problems.push(not_recreated(&error)),
                Err(error) => return Err(error),
            }
            continue;
        }
        let created = match &existing {
            // A server that doesn't start fails the whole server's restore,
            // rather than being tried again for each session.
            None => {
                start_server(session, server, tmux_session, &first_pane.cwd)?;
                run_tmux(session, socket, &format!("display -p -t {} -F '#{{window_id}} #{{window_index}} #{{pane_id}}'", shell_quote(&format!("={name}:^"))))
            }
            Some(_) => run_tmux(
                session,
                socket,
                &format!("new-session -d -E -s {} -x {STAGING_SIZE} -y {STAGING_SIZE} -c {} -P -F '#{{window_id}} #{{window_index}} #{{pane_id}}'", format_quote(name), format_quote(&first_pane.cwd)),
            ),
        };
        let created = match created {
            Ok(created) => created,
            Err(error) if is_failure(&error) => {
                problems.push(not_recreated(&error));
                continue;
            }
            Err(error) => return Err(error),
        };
        existing.get_or_insert_with(Vec::new).push(name.clone());
        if let Some(group) = &tmux_session.group {
            groups.insert(group, name);
        }
        let unexpected = || Failed(format!("unexpected tmux output {created:?}"));
        let [window_id, index, pane_id] = created.split_whitespace().collect::<Vec<_>>()[..] else { return Err(unexpected().into()) };
        let index: u32 = index.parse().map_err(|_| unexpected())?;
        for (i, window) in tmux_session.windows.iter().enumerate() {
            let first = (i == 0).then_some((window_id, index, pane_id));
            let pane_ids = match create_window(session, socket, name, window, first) {
                Ok(pane_ids) => pane_ids,
                Err(error) if is_failure(&error) => {
                    problems.push(format!("{}'s {name}:{} couldn't be recreated: {error:#}", server.user, window.index));
                    continue;
                }
                Err(error) => return Err(error),
            };
            problems.extend(restore_panes(session, &mut restoring, name, window, &pane_ids, printer)?);
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
            problems.push(format!("{} couldn't be listed when captured ({error}), so there's nothing to bring back", server.name()));
            continue;
        }
        printer.line(&format!("Bringing back {}'s tmux server at {}: {}", server.user, server.socket, server.counts()));
        match restore_server(session, server, patterns, printer) {
            Ok(found) => problems.extend(found),
            Err(error) if is_failure(&error) => {
                let problem = format!("{} couldn't be brought back: {error:#}", server.name());
                printer.styled_line(Red, &problem);
                problems.push(problem);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(problems)
}

/// Brings back what the capture pending on the machine (see [`PENDING`])
/// describes, if one is: see [`restore`], which `machine`'s
/// resumable_commands are for.  Fails if the capture is from this very
/// boot and was taken for a reboot or shutdown: then the machine didn't go
/// down after all, and what it describes is still running.  Either way, the
/// capture is no longer pending.  Returns the problems, or `None` if
/// nothing is pending.
pub fn restore_pending(session: &mut Session, machine: &Machine, printer: &mut Printer) -> Result<Option<Vec<String>>> {
    let Some((path, capture)) = pending(session)? else { return Ok(None) };
    // First, so that a restore that's cut short isn't repeated on top of
    // itself; the capture stays in DIR for `reboop tmux start --saved`.
    forget_pending(session)?;
    if capture.outlived(&facts::boot_id(session)?) {
        bail!("{path} is from this boot, so what it describes is still running; not bringing it back");
    }
    printer.line(&format!("Bringing back what was in tmux, from {path}"));
    Ok(Some(restore(session, &capture, &machine.resumable_commands, printer)?))
}

/// Kills every server in `capture`, waiting for each to exit.  (That hangs
/// up on the panes' processes, as closing a terminal does; one that ignores
/// SIGHUP keeps running.)
fn stop_servers(session: &mut Session, capture: &Capture, printer: &mut Printer) -> Result<()> {
    for server in &capture.servers {
        let script = format!(
            "tmux -S {} kill-server; i=0; while [ -d /proc/{} ]; do i=$((i + 1)); [ $i -ge {} ] && exit 1; sleep 0.1; done",
            shell_quote(&server.socket),
            server.pid,
            STOP_TIMEOUT.as_millis() / 100
        );
        let output = session.run(&script, STOP_TIMEOUT + QUICK)?;
        if output.status != 0 {
            bail!("{} (pid {}) didn't exit within {}s", server.name(), server.pid, STOP_TIMEOUT.as_secs());
        }
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
/// [`tmux::listing`].  A look at the servers is saved on the machine.
pub fn list(ssh: &Ssh, machine: &Machine, saved: Option<&str>, printer: &mut Printer) -> Result<()> {
    let mut session = open(ssh, machine)?;
    let capture = match saved {
        Some(saved) => {
            let path = saved_path(saved);
            let capture = load(&mut session, &path)?;
            printer.line(&format!("{path}, saved at {} for a {}, has {}:", capture.captured_at.strftime("%Y-%m-%d %H:%M:%S UTC"), capture.purpose, capture.counts()));
            capture
        }
        None => {
            let capture = Capture::new(Purpose::Check, &facts::boot_id(&mut session)?, tmux::capture(&mut session)?);
            save(&mut session, &capture, false)?;
            if capture.servers.is_empty() {
                printer.line(&format!("{} has no tmux servers", machine.hostname));
            }
            capture
        }
    };
    for text in tmux::listing(&capture.servers, machine.tmux, &machine.resumable_commands, printer.color) {
        printer.line(&text);
    }
    Ok(())
}

/// Stops `machine`'s tmux servers if every pane's command is resumable (by
/// its resumable_commands, whatever its tmux setting) and every server
/// could be listed, saving what's in them first for `start` to bring back,
/// telling the user.  Returns the reasons not to, if it wasn't okay (none if
/// the servers were stopped).
pub fn stop(ssh: &Ssh, machine: &Machine, printer: &mut Printer) -> Result<Vec<String>> {
    let hostname = &machine.hostname;
    let mut session = open(ssh, machine)?;
    let capture = Capture::new(Purpose::TmuxStop, &facts::boot_id(&mut session)?, tmux::capture(&mut session)?);
    let blockers = tmux::blockers(TmuxPolicy::Resume, &machine.resumable_commands, &capture.servers);
    if !blockers.is_empty() {
        printer.styled_line(Red, &indented_list(&format!("Not stopping tmux on {hostname}:"), &blockers));
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
        printer.line(&indented_list("Commands to run again then:", &resumable));
    }
    stop_servers(&mut session, &capture, printer)?;
    Ok(vec![])
}

/// Brings back what's pending on `machine` (see [`restore_pending`]), or the
/// capture `saved` on it if given (see [`saved_path`]), telling the user.
/// Returns the problems.  Fails if nothing is brought back for want of a
/// capture.
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
        None => restore_pending(&mut session, machine, printer)?
            .ok_or_else(|| anyhow!("nothing is pending on {hostname} to bring back; `reboop tmux start {hostname} --saved FILE` brings back one of the captures in {DIR}"))?,
    };
    if problems.is_empty() {
        printer.line(&format!("tmux is back on {hostname}, and all is well"));
    } else {
        printer.styled_line(Red, &indented_list(&format!("tmux is back on {hostname}, but:"), &problems));
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
        capture.captured_at = "2026-11-07T06:32:28.5126Z".parse().unwrap();
        assert_eq!(capture.file_name(), "20261107T063228.512Z.reboot.json");
        capture.purpose = Purpose::TmuxStop;
        assert_eq!(capture.file_name(), "20261107T063228.512Z.tmux-stop.json");
        assert_eq!(saved_path("20261107T063228.512Z.check.json"), "/var/lib/reboop/tmux/20261107T063228.512Z.check.json");
        assert_eq!(saved_path("/tmp/x.json"), "/tmp/x.json");
    }

    #[test]
    fn tells_whether_a_reboot_or_shutdown_happened_after_a_capture() {
        assert!(Capture::new(Purpose::Reboot, "4f6f3fbb", vec![]).outlived("4f6f3fbb"));
        assert!(Capture::new(Purpose::Shutdown, "4f6f3fbb", vec![]).outlived("4f6f3fbb"));
        assert!(!Capture::new(Purpose::Reboot, "4f6f3fbb", vec![]).outlived("9d1c2e07"));
        // `reboop tmux stop` doesn't take the machine down
        assert!(!Capture::new(Purpose::TmuxStop, "4f6f3fbb", vec![]).outlived("4f6f3fbb"));
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
