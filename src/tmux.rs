//! The tmux servers on a machine and what's in them: each server's
//! sessions, windows and panes, with every pane's directory and what it's
//! running: a command line typed into its shell, as zsh-spy recorded it; or
//! the command tmux started it with; or nothing (an idle shell); or
//! something that can't be told.  A bounce can bring all of that back after
//! the reboot (see [`crate::resume`]), as far as the machine's
//! `resumable_commands` allow; what they don't allow keeps the machine from
//! going down.

use crate::config::TmuxPolicy;
use crate::human::{self, Align, Cell, Style};
use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use regex::{Regex, RegexSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// How much of a command a [`Verdict`] shows.
const COMMAND_SHOWN: usize = 100;

/// How much of the end of a zsh-spy archive is read: plenty for the last
/// command line typed, which is what it's for.  (A background job whose
/// start is further back shows up anyway, as a child of the shell.)
const ARCHIVE_TAIL: usize = 64 << 10;

/// Regular expressions for the commands that may be resumed.  Each has to
/// match a whole command line, like `grep -x`, so that "rsync .*" can't
/// match "rm -rf x && rsync ...".
#[derive(Clone, Debug)]
pub struct CommandPatterns {
    /// As given
    patterns: Vec<String>,
    /// The same, each anchored to the whole command
    set: RegexSet,
}

impl PartialEq for CommandPatterns {
    fn eq(&self, other: &CommandPatterns) -> bool {
        self.patterns == other.patterns
    }
}

impl CommandPatterns {
    /// Compiles `patterns`, failing on one that isn't a regular expression
    /// (in the syntax of Rust's regex crate).
    pub fn new(patterns: Vec<String>) -> Result<CommandPatterns> {
        let anchored: Vec<String> = patterns.iter().map(|pattern| format!("^(?:{pattern})$")).collect();
        // One at a time first, so that an error names its pattern
        for (pattern, anchored) in patterns.iter().zip(&anchored) {
            Regex::new(anchored).with_context(|| format!("{pattern:?} in resumable_commands isn't a regular expression"))?;
        }
        Ok(CommandPatterns { patterns, set: RegexSet::new(&anchored)? })
    }

    /// Whether one of the patterns matches all of `command`.
    pub fn matches(&self, command: &str) -> bool {
        self.set.is_match(command)
    }
}

/// What a pane is running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Running {
    /// Its shell is waiting at its prompt, with nothing running.
    Idle,
    /// Its shell is running a command line that was typed into it, as
    /// zsh-spy recorded it.
    Typed { command: String },
    /// The command tmux started it with in place of a shell, as tmux shows
    /// it, quoted for tmux's own command parser: e.g. `"sleep 300"` for a
    /// pane started with `tmux new-session 'sleep 300'`.
    Started { command: String },
    /// Jobs its shell put in the background (with &) are still running, as
    /// zsh-spy recorded what they ran: `None` for one whose line
    /// HIST_IGNORE_SPACE hid.  (Whatever the shell runs in the foreground.)
    Background { jobs: Vec<Option<String>> },
    /// It's running `program`, but what was typed to start it isn't known:
    /// its shell has no zsh-spy archive, or the line was hidden from it
    /// (HIST_IGNORE_SPACE), or the program replaced the shell, or the
    /// shell has a child process that the archive doesn't account for, like
    /// a job that was disowned.
    Unknown { program: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub index: u32,
    /// tmux's id for it, like "%3"; the same pane shows up in each session
    /// that a window is linked into.
    pub id: String,
    pub active: bool,
    /// The directory a typed command was typed in, as typed (so perhaps
    /// through a symlink); the directory a started command was started in;
    /// or else the pane's current directory
    pub cwd: String,
    /// Where the filesystem that `cwd` is on is mounted, or "" if that
    /// couldn't be found (as when `cwd` has been deleted)
    pub mountpoint: String,
    pub running: Running,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub index: u32,
    /// Its name, if set by hand or by a command; `None` if tmux names it
    /// after what it runs (automatic-rename)
    pub name: Option<String>,
    pub active: bool,
    /// Its layout as tmux describes it, size included, which
    /// `select-layout` takes back
    pub layout: String,
    pub panes: Vec<Pane>,
}

/// A tmux session.  (Not to be confused with an SSH [`Session`].)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxSession {
    pub name: String,
    /// Its session group, if it's in one: sessions in a group share their
    /// windows.
    pub group: Option<String>,
    pub windows: Vec<Window>,
}

/// A tmux server, and what's in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    /// Whose it is: the user name, or the numeric uid if the user has no
    /// name
    pub user: String,
    /// The path of its socket, or "" if that isn't known (see
    /// [`capture`])
    pub socket: String,
    pub pid: u32,
    /// Empty if there's an `error`
    pub sessions: Vec<TmuxSession>,
    /// Why its sessions couldn't be listed, if they couldn't: e.g. the
    /// machine's tmux is another version than the server's
    pub error: Option<String>,
}

/// Where a pane is, as a tmux target: "work:1.0" for pane 0 of window 1 of
/// session work.
pub fn location(session: &str, window: u32, pane: u32) -> String {
    format!("{session}:{window}.{pane}")
}

impl Server {
    /// "at's server at /tmp/tmux-1000/default", or "at's server (pid 1234)"
    /// if its socket isn't known
    pub fn name(&self) -> String {
        if self.socket.is_empty() { format!("{}'s server (pid {})", self.user, self.pid) } else { format!("{}'s server at {}", self.user, self.socket) }
    }

    /// Every pane in the server, with its location (see [`location`]).
    pub fn panes(&self) -> impl Iterator<Item = (String, &Pane)> {
        self.sessions.iter().flat_map(|session| {
            session.windows.iter().flat_map(move |window| window.panes.iter().map(move |pane| (location(&session.name, window.index, pane.index), pane)))
        })
    }

    /// "2 sessions and 5 panes"
    pub fn counts(&self) -> String {
        format!("{} and {}", count(self.sessions.len(), "session"), count(self.panes().count(), "pane"))
    }
}

/// "1 pane" or "3 panes"
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// Parses `ss -xlpH` (the listening unix sockets, with their processes)
/// into the socket path and pid of each tmux server.
fn parse_ss(output: &str) -> Result<Vec<(String, u32)>> {
    let mut servers = Vec::new();
    for line in output.lines() {
        let Some((socket, users)) = line.split_once(" users:(") else { continue };
        let Some((_, after_pid)) = users.split_once(r#"("tmux: server",pid="#) else { continue };
        let context = || format!("unexpected ss line {line:?}");
        // Netid State Recv-Q Send-Q Local-Address Port Peer-Address Port,
        // the address being the path, perhaps with spaces in it
        let fields: Vec<&str> = socket.split_whitespace().collect();
        ensure!(fields.len() >= 8 && fields[0] == "u_str", context());
        let pid = after_pid.split(',').next().unwrap_or_default().parse().with_context(context)?;
        servers.push((fields[4..fields.len() - 3].join(" "), pid));
    }
    Ok(servers)
}

/// Separates the fields of the lines that [`capture_server`] has
/// `tmux list-panes` print: a control character that no name or path has.
/// (One that does makes the server unlistable.)
const FIELD_SEPARATOR: char = '\x1f';

/// Those fields, as tmux calls them.
const PANE_FIELDS: [&str; 16] = [
    "session_name",
    "session_group",
    "window_index",
    "window_name",
    "automatic-rename",
    "window_active",
    "window_layout",
    "pane_index",
    "pane_id",
    "pane_active",
    "pane_dead",
    "pane_pid",
    "pane_current_path",
    "pane_start_path",
    "pane_current_command",
    "pane_start_command",
];

/// A line about a pane from `tmux list-panes`, as [`PANE_FIELDS`] has it
/// print them.
struct PaneLine {
    session: String,
    /// "" if the session isn't in a group
    group: String,
    window_index: u32,
    window_name: String,
    automatic_rename: bool,
    window_active: bool,
    layout: String,
    pane_index: u32,
    pane_id: String,
    pane_active: bool,
    /// Whether the pane's process has exited (remain-on-exit keeps such a
    /// pane); then there's no `pid`.
    dead: bool,
    /// The pane's process: its shell, or its start command's; 0 if dead
    pid: u32,
    /// The pane's current directory
    path: String,
    /// The directory the pane was started in
    start_path: String,
    /// The name of the program in the foreground, like "zsh" or "rsync"
    current_command: String,
    /// The command tmux started the pane with, if any (else a shell)
    start_command: String,
}

fn parse_pane_line(line: &str) -> Result<PaneLine> {
    let context = || format!("unexpected tmux list-panes line {line:?}");
    let fields: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
    let [session, group, window_index, window_name, automatic_rename, window_active, layout, pane_index, pane_id, pane_active, dead, pid, path, start_path, current_command, start_command] =
        fields[..]
    else {
        bail!(context());
    };
    let number = |field: &str| field.parse::<u32>().with_context(context);
    let flag = |field: &str| match field {
        "1" => Ok(true),
        "0" => Ok(false),
        _ => Err(anyhow!(context())),
    };
    let dead = flag(dead)?;
    Ok(PaneLine {
        session: session.into(),
        group: group.into(),
        window_index: number(window_index)?,
        window_name: window_name.into(),
        automatic_rename: flag(automatic_rename)?,
        window_active: flag(window_active)?,
        layout: layout.into(),
        pane_index: number(pane_index)?,
        pane_id: pane_id.into(),
        pane_active: flag(pane_active)?,
        dead,
        pid: if dead { 0 } else { number(pid)? },
        path: path.into(),
        start_path: start_path.into(),
        current_command: current_command.into(),
        start_command: start_command.into(),
    })
}

/// A record in a zsh-spy archive, of the kinds that tell what the shell is
/// running.  (The rest are `Other`.)
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record {
    CommandStart {
        id: String,
        shell_pid: u32,
        cwd: String,
        /// None if the line was hidden (HIST_IGNORE_SPACE)
        command: Option<String>,
    },
    CommandEnd {
        id: String,
    },
    AsyncStart {
        /// The id of the command that started the job
        id: String,
        job: u64,
        /// None if the command's line was hidden
        job_text: Option<String>,
    },
    AsyncEnd {
        id: String,
        job: u64,
    },
    AsyncLost {
        id: String,
        job: u64,
    },
    #[serde(other)]
    Other,
}

/// Parses the records in `tail`, the end of an archive.  Its first line
/// may be cut short, so that one is skipped if it doesn't parse; any other
/// line that doesn't is an error, rather than a command that goes unseen.
fn parse_records(tail: &str) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    for (index, line) in tail.lines().enumerate() {
        let parsed = serde_json::from_str(line);
        if index == 0 && parsed.is_err() {
            continue;
        }
        records.push(parsed.with_context(|| format!("couldn't parse a zsh-spy record: {}", human::truncate(line, COMMAND_SHOWN)))?);
    }
    Ok(records)
}

/// What the background jobs that `records` show started, but not ended or
/// lost, ran.  (By command id and job number, in whatever order the records
/// came, since zsh-spy's traps can write them out of order.)
fn running_jobs(records: &[Record]) -> Vec<Option<String>> {
    // (started, ended, what it ran) by (command id, job number)
    let mut jobs: BTreeMap<(&str, u64), (bool, bool, Option<String>)> = BTreeMap::new();
    for record in records {
        match record {
            Record::AsyncStart { id, job, job_text } => {
                let entry = jobs.entry((id, *job)).or_default();
                entry.0 = true;
                entry.2 = job_text.clone();
            }
            Record::AsyncEnd { id, job } | Record::AsyncLost { id, job } => jobs.entry((id, *job)).or_default().1 = true,
            _ => {}
        }
    }
    jobs.into_values().filter(|(started, ended, _)| *started && !ended).map(|(_, _, text)| text).collect()
}

/// What the shell with pid `pid` is running, by the `records` at the end of
/// its archive, and the directory to run it in again: where a typed command
/// was typed, or else `path`, the pane's current directory.  tmux says the
/// pane's foreground program is `program`.
fn shell_running(records: &[Record], pid: u32, program: &str, path: &str) -> Result<(Running, String)> {
    let jobs = running_jobs(records);
    if !jobs.is_empty() {
        return Ok((Running::Background { jobs }, path.into()));
    }
    let Some(start) = records.iter().rposition(|record| matches!(record, Record::CommandStart { .. })) else {
        return Ok((Running::Idle, path.into()));
    };
    let Record::CommandStart { id, shell_pid, cwd, command } = &records[start] else { unreachable!() };
    ensure!(shell_pid == &pid, "the archive that pid {pid} holds open has records of pid {shell_pid}");
    let ended = records[start + 1..].iter().any(|record| matches!(record, Record::CommandEnd { id: ended } if ended == id));
    if ended {
        return Ok((Running::Idle, path.into()));
    }
    Ok(match command {
        Some(command) => (Running::Typed { command: command.clone() }, cwd.clone()),
        None => (Running::Unknown { program: program.into() }, path.into()),
    })
}

/// Programs that are shells, so a pane running one with no children and no
/// zsh-spy archive is taken to be idle.
const SHELLS: &[&str] = &["zsh", "bash", "sh", "dash", "fish", "ksh"];

/// What's known about a pane's process besides what tmux says.
struct ProcessFacts {
    /// The names of its child processes
    children: Vec<String>,
    /// The end of its zsh-spy archive, if it's a shell that has one
    archive: Option<String>,
}

/// What the pane that `line` is about is running, given the `facts` about
/// its process (none for a dead pane), and the directory to run it in again
/// (see [`Pane::cwd`]).
fn pane_running(line: &PaneLine, facts: Option<&ProcessFacts>) -> Result<(Running, String)> {
    if line.dead {
        return Ok((Running::Idle, if line.path.is_empty() { line.start_path.clone() } else { line.path.clone() }));
    }
    let facts = facts.ok_or_else(|| anyhow!("no facts about pid {}", line.pid))?;
    // A child the archive doesn't account for, like a disowned job, means
    // the shell isn't idle after all.
    let unknown = || Running::Unknown { program: facts.children.first().cloned().unwrap_or_else(|| line.current_command.clone()) };
    if let Some(tail) = &facts.archive {
        let (running, cwd) = shell_running(&parse_records(tail)?, line.pid, &line.current_command, &line.path)?;
        return Ok(if running == Running::Idle && !facts.children.is_empty() { (unknown(), line.path.clone()) } else { (running, cwd) });
    }
    if !line.start_command.is_empty() {
        return Ok((Running::Started { command: line.start_command.clone() }, line.start_path.clone()));
    }
    if SHELLS.contains(&line.current_command.as_str()) && facts.children.is_empty() {
        return Ok((Running::Idle, line.path.clone()));
    }
    Ok((unknown(), line.path.clone()))
}

/// The [`ProcessFacts`] about each live pane's process among `lines`, by
/// pid: its children's names, and the end of the zsh-spy archive
/// (hist.*.jsonl) that it holds open, if any.
fn process_facts(session: &mut Session, lines: &[PaneLine]) -> Result<HashMap<u32, ProcessFacts>> {
    // Once each: a window linked into several sessions lists its panes once per session.
    let pids: BTreeSet<u32> = lines.iter().filter(|line| !line.dead).map(|line| line.pid).collect();
    let mut facts = HashMap::new();
    if pids.is_empty() {
        return Ok(facts);
    }
    // A line per pid: the pid, the base64 of its children's names (one per
    // line), and the base64 of the archive's tail, or "-" without one
    let script = format!(
        r#"for p in {}; do
    f=$(for l in /proc/$p/fd/*; do readlink "$l"; done 2>/dev/null | grep -m1 '/hist\..*\.jsonl$')
    printf '%s %s %s\n' "$p" "$(ps -o comm= --ppid "$p" | base64 -w0)" "$(if [ -n "$f" ]; then tail -c {ARCHIVE_TAIL} -- "$f" | base64 -w0; else echo -; fi)"
done"#,
        pids.iter().map(u32::to_string).collect::<Vec<_>>().join(" ")
    );
    let output = session.run_ok(&script, QUICK)?;
    for line in output.lines() {
        let context = || format!("unexpected process facts line {:?}", human::truncate(line, COMMAND_SHOWN));
        let [pid, children, archive] = line.splitn(3, ' ').collect::<Vec<_>>()[..] else { bail!(context()) };
        let pid: u32 = pid.parse().with_context(context)?;
        let children = String::from_utf8_lossy(&BASE64.decode(children).with_context(context)?).lines().map(String::from).collect();
        let mut tail = None;
        if archive != "-" {
            let bytes = BASE64.decode(archive).with_context(context)?;
            tail = Some(String::from_utf8(bytes).with_context(|| format!("the zsh-spy archive of pid {pid} isn't UTF-8"))?);
        }
        facts.insert(pid, ProcessFacts { children, archive: tail });
    }
    ensure!(facts.len() == pids.len(), "expected facts about {} processes, got {}", pids.len(), facts.len());
    Ok(facts)
}

/// Where the filesystem that each of `paths` is on is mounted: "" where that
/// can't be found, as for a deleted directory.
fn mountpoints(session: &mut Session, paths: &[&str]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut script = String::from("m() { printf '%s\\n' \"$(findmnt -T \"$1\" -n -o TARGET 2>/dev/null | head -n1)\"; }\n");
    for path in paths {
        script.push_str(&format!("m {}\n", shell_quote(path)));
    }
    let output = session.run_ok(&script, QUICK)?;
    let found: Vec<String> = output.lines().map(String::from).collect();
    ensure!(found.len() == paths.len(), "expected {} mountpoints, got {}: {output:?}", paths.len(), found.len());
    Ok(found)
}

/// Puts the panes together into sessions and windows, given each pane's
/// line, what it's running with the directory for that, and the directory's
/// mountpoint.
fn assemble(lines: Vec<PaneLine>, runnings: Vec<(Running, String)>, mountpoints: Vec<String>) -> Vec<TmuxSession> {
    let mut sessions: Vec<TmuxSession> = Vec::new();
    for (line, ((running, cwd), mountpoint)) in lines.into_iter().zip(runnings.into_iter().zip(mountpoints)) {
        let session = match sessions.iter().position(|session| session.name == line.session) {
            Some(index) => &mut sessions[index],
            None => {
                let group = Some(line.group.clone()).filter(|group| !group.is_empty());
                sessions.push(TmuxSession { name: line.session.clone(), group, windows: vec![] });
                sessions.last_mut().unwrap()
            }
        };
        let window = match session.windows.iter().position(|window| window.index == line.window_index) {
            Some(index) => &mut session.windows[index],
            None => {
                session.windows.push(Window {
                    index: line.window_index,
                    name: if line.automatic_rename { None } else { Some(line.window_name.clone()) },
                    active: line.window_active,
                    layout: line.layout.clone(),
                    panes: vec![],
                });
                session.windows.last_mut().unwrap()
            }
        };
        window.panes.push(Pane { index: line.pane_index, id: line.pane_id.clone(), active: line.pane_active, cwd, mountpoint, running });
    }
    sessions
}

/// The sessions in a server, given its `listing` by `tmux list-panes -a`
/// (see [`PANE_FIELDS`]).
fn list_sessions(session: &mut Session, listing: &str) -> Result<Vec<TmuxSession>> {
    let lines: Vec<PaneLine> = listing.lines().map(parse_pane_line).collect::<Result<_>>()?;
    let facts = process_facts(session, &lines)?;
    let runnings: Vec<(Running, String)> = lines.iter().map(|line| pane_running(line, facts.get(&line.pid))).collect::<Result<_>>()?;
    let cwds: Vec<&str> = runnings.iter().map(|(_, cwd)| cwd.as_str()).collect();
    let mountpoints = mountpoints(session, &cwds)?;
    Ok(assemble(lines, runnings, mountpoints))
}

/// Lists what's in the tmux server with pid `pid` at `socket`, or returns
/// `None` if it's gone.  Whatever keeps its sessions from being listed is
/// its `error` instead.
fn capture_server(session: &mut Session, socket: String, pid: u32) -> Result<Option<Server>> {
    let format: Vec<String> = PANE_FIELDS.iter().map(|field| format!("#{{{field}}}")).collect();
    // The user, then the listing; or what's wrong, with status 3
    let script = format!(
        r#"test -d /proc/{pid} || {{ echo gone; exit 0; }}
ids=$(stat -c '%U %u' /proc/{pid}) || exit 1
set -- $ids
[ "$1" = UNKNOWN ] && set -- "$2"
printf '%s\n' "$1"
answering=$(tmux -S {socket} display -p '#{{pid}}') || exit 2
[ "$answering" = {pid} ] || {{ echo "another server (pid $answering) answers at its socket"; exit 3; }}
exec tmux -S {socket} list-panes -a -F {}"#,
        shell_quote(&format.join(&FIELD_SEPARATOR.to_string())),
        socket = shell_quote(&socket)
    );
    let output = session.run(&script, QUICK)?;
    let stdout = output.stdout_text();
    if output.status == 0 && stdout == "gone\n" {
        return Ok(None);
    }
    let Some((user, rest)) = stdout.split_once('\n') else {
        bail!("couldn't tell whose tmux server pid {pid} is: {}", output.stderr_text());
    };
    let mut server = Server { user: user.into(), socket, pid, sessions: vec![], error: None };
    let listed = match output.status {
        0 => list_sessions(session, rest),
        3 => Err(anyhow!("{}", rest.trim_end())),
        status => Err(anyhow!("tmux exited with status {status}: {}", output.stderr_text())),
    };
    match listed {
        Ok(sessions) => server.sessions = sessions,
        Err(error) => server.error = Some(format!("{error:#}")),
    }
    Ok(Some(server))
}

/// Lists every tmux server on the machine (any user's, at any socket) and
/// what's in it.  A server whose socket `ss` doesn't show, as when it's in
/// another network namespace (a container's), is listed with an `error` and
/// no `socket`.
pub fn capture(session: &mut Session) -> Result<Vec<Server>> {
    let mut servers = Vec::new();
    for (socket, pid) in parse_ss(&session.run_ok("ss -xlpH", QUICK)?)? {
        if let Some(server) = capture_server(session, socket, pid)? {
            servers.push(server);
        }
    }
    for line in session.run_ok("ps -e -o pid=,user:64=,comm=", QUICK)?.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(user)) = (fields.next(), fields.next()) else { continue };
        if fields.collect::<Vec<_>>().join(" ") != "tmux: server" {
            continue;
        }
        let pid: u32 = pid.parse().with_context(|| format!("unexpected ps line {line:?}"))?;
        if servers.iter().all(|server| server.pid != pid) {
            let error = Some("ss doesn't list its socket (is it in another network namespace, like a container's?)".into());
            servers.push(Server { user: user.into(), socket: String::new(), pid, sessions: vec![], error });
        }
    }
    Ok(servers)
}

/// A command on one line, cut to [`COMMAND_SHOWN`] characters, for people.
fn summary(command: &str) -> String {
    human::truncate(&command.split_whitespace().collect::<Vec<_>>().join(" "), COMMAND_SHOWN)
}

/// What's to become of a pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Its shell is idle, so it comes back idle.
    Idle,
    /// Its command is resumable, so it's to be run again: the command, in
    /// short.
    Resume(String),
    /// It can't be resumed, for this reason, so the machine mustn't go down.
    Block(String),
}

/// What's to become of `pane`, given the commands that `patterns` allow.
pub fn verdict(pane: &Pane, patterns: &CommandPatterns) -> Verdict {
    match &pane.running {
        Running::Idle => Verdict::Idle,
        Running::Typed { command } | Running::Started { command } => {
            if pane.mountpoint.is_empty() {
                Verdict::Block(format!("{} (its directory isn't there)", summary(command)))
            } else if patterns.matches(command) {
                Verdict::Resume(summary(command))
            } else {
                Verdict::Block(format!("{} (doesn't match resumable_commands)", summary(command)))
            }
        }
        Running::Background { jobs } => {
            let texts: Vec<String> = jobs.iter().map(|job| job.as_deref().map_or("one whose line was hidden".into(), summary)).collect();
            Verdict::Block(format!("{} still running: {}", if jobs.len() == 1 { "a background job is" } else { "background jobs are" }, texts.join("; ")))
        }
        Running::Unknown { program } => Verdict::Block(format!("running {program}, but what was typed to start it is unknown")),
    }
}

/// The reasons the tmux `servers` keep the machine from going down, one line
/// each for people: with `policy` block, every server; with resume, every
/// pane whose command can't be resumed (see [`verdict`]); and either way,
/// every server that couldn't be listed.
pub fn blockers(policy: TmuxPolicy, patterns: &CommandPatterns, servers: &[Server]) -> Vec<String> {
    let mut blockers = Vec::new();
    for server in servers {
        if let Some(error) = &server.error {
            blockers.push(format!("tmux: couldn't list {}: {error}", server.name()));
            continue;
        }
        match policy {
            TmuxPolicy::Block => blockers.push(format!("tmux: {} has {} (tmux is \"block\")", server.name(), server.counts())),
            TmuxPolicy::Resume => {
                for (location, pane) in server.panes() {
                    if let Verdict::Block(why) = verdict(pane, patterns) {
                        blockers.push(format!("tmux: {}'s {location} in {}: {why}", server.user, pane.cwd));
                    }
                }
            }
        }
    }
    blockers
}

/// The row about `pane`, at `location`, in [`listing`]'s table, with
/// `patterns` (the machine's resumable_commands) deciding its fate.
fn row(location: String, pane: &Pane, patterns: &CommandPatterns) -> Vec<Cell> {
    let (fate, text) = match verdict(pane, patterns) {
        Verdict::Idle            => (Style::Dim.cell("idle"),          String::new()),
        Verdict::Resume(command) => (Style::Green.cell("resumable"),   command),
        Verdict::Block(why)      => (Style::Red.cell("not resumable"), why),
    };
    vec![fate.aligned(Align::Right), Style::Plain.cell(location), Style::Plain.cell(pane.cwd.clone()), Style::Plain.cell(text)]
}

/// The line about `server`, for people: with its sessions and panes
/// counted (and saying that it blocks a reboot, if `policy` is block), or
/// why it couldn't be listed.  What blocks a reboot is red if `color`.
fn heading(server: &Server, policy: TmuxPolicy, color: bool) -> String {
    let red = |text: &str| if color { Style::Red.paint(text) } else { text.to_string() };
    let at = if server.socket.is_empty() { String::new() } else { format!(" at {}", server.socket) };
    let name = format!("{}'s tmux server{at} (pid {})", server.user, server.pid);
    if let Some(error) = &server.error {
        return format!("{name}: {}", red(&format!("couldn't be listed: {error}")));
    }
    let mut text = format!("{name}: {}", server.counts());
    if policy == TmuxPolicy::Block {
        text.push_str(&format!(" {}", red("(tmux is \"block\", so the server blocks a reboot)")));
    }
    text
}

/// What's in `servers`, for people: a text per server, of a [`heading`]
/// line, then a line per pane, in a table whose columns line up across all
/// the servers.  A pane's row has "idle", "resumable" or "not resumable"
/// (as [`verdict`] tells, given the commands that `patterns` allow), flush
/// right; the pane's location; the pane's directory; and either the
/// command to run again, why the pane can't be resumed, or (for an idle
/// pane) nothing.  If `color`, the three fates are dim, green and red
/// respectively, and what makes a server block a reboot is red.
pub fn listing(servers: &[Server], policy: TmuxPolicy, patterns: &CommandPatterns, color: bool) -> Vec<String> {
    let rows: Vec<Vec<Cell>> = servers.iter().flat_map(|server| server.panes().map(move |(location, pane)| row(location, pane, patterns))).collect();
    let mut lines = human::table(&rows, color).into_iter();
    let texts: Vec<String> = servers
        .iter()
        .map(|server| {
            let mut text = heading(server, policy, color);
            for line in lines.by_ref().take(server.panes().count()) {
                text.push('\n');
                text.push_str(&line);
            }
            text
        })
        .collect();
    assert!(lines.next().is_none(), "the table has more lines than the servers have panes");
    texts
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn patterns(patterns: &[&str]) -> CommandPatterns {
        CommandPatterns::new(patterns.iter().map(|&pattern| pattern.into()).collect()).unwrap()
    }

    fn pane(index: u32, cwd: &str, running: Running) -> Pane {
        Pane { index, id: format!("%{index}"), active: index == 0, cwd: cwd.into(), mountpoint: "/".into(), running }
    }

    /// A server with a session "work" of two windows: one with an idle pane
    /// and a pane running `rsync`, and one with a pane running `htop`.
    pub(crate) fn test_server() -> Server {
        let window = |index, name: Option<&str>, panes| Window {
            index,
            name: name.map(String::from),
            active: index == 0,
            layout: r#"{"V":2,"L":{"t":"p","w":120,"h":40,"x":0,"y":0,"a":true,"i":0,"I":"%0"}}"#.into(),
            panes,
        };
        let idle = pane(0, "/home/at", Running::Idle);
        let rsync = pane(1, "/data", Running::Typed { command: "rsync -a /data/ /backup/".into() });
        let htop = pane(0, "/home/at", Running::Typed { command: "htop".into() });
        Server {
            user: "at".into(),
            socket: "/tmp/tmux-1000/default".into(),
            pid: 1234,
            sessions: vec![TmuxSession { name: "work".into(), group: None, windows: vec![window(0, None, vec![idle, rsync]), window(3, Some("top"), vec![htop])] }],
            error: None,
        }
    }

    /// The fields of a list-panes line about a zsh pane with pid 4242 in
    /// /pane, in window 0 of session work, with the given program in the
    /// foreground and start command.
    fn line(current_command: &str, start_command: &str) -> PaneLine {
        let fields = ["work", "", "0", "zsh", "1", "1", "{}", "0", "%7", "1", "0", "4242", "/pane", "/start", current_command, start_command];
        parse_pane_line(&fields.join("\x1f")).unwrap()
    }

    fn facts(children: &[&str], archive: Option<&str>) -> ProcessFacts {
        ProcessFacts { children: children.iter().map(|&child| child.into()).collect(), archive: archive.map(String::from) }
    }

    #[test]
    fn patterns_match_whole_commands() {
        let patterns = patterns(&["rsync .*", "htop", "(?s)yt-dlp .*"]);
        assert!(patterns.matches("rsync -a /a /b"));
        assert!(patterns.matches("htop"));
        assert!(!patterns.matches("rm -rf x && rsync -a /a /b"));
        assert!(!patterns.matches("htop -d 10"));
        assert!(!patterns.matches("rsync -a /a \\\n /b"));
        assert!(patterns.matches("yt-dlp \\\n  https://example.com"));
        assert!(CommandPatterns::new(vec!["(".into()]).is_err());
        assert!(!CommandPatterns::new(vec![]).unwrap().matches("htop"));
    }

    #[test]
    fn parses_ss() {
        let output = "u_str LISTEN 0      4096                        /run/systemd/journal/stdout 12345 * 0 users:((\"systemd-journal\",pid=1,fd=42),(\"systemd\",pid=1,fd=37))\n\
                      u_str LISTEN 0      128                           /tmp/tmux-1000/default 55065616 * 0 users:((\"tmux: server\",pid=1769201,fd=6))\n\
                      u_str LISTEN 0      128                           /tmp/with space/sock 55065617 * 0 users:((\"tmux: server\",pid=99,fd=6))\n\
                      u_dgr UNCONN 0      0                          /run/systemd/notify 12346 * 0 users:((\"systemd\",pid=1,fd=12))\n";
        assert_eq!(parse_ss(output).unwrap(), [("/tmp/tmux-1000/default".to_string(), 1769201), ("/tmp/with space/sock".to_string(), 99)]);
        assert_eq!(parse_ss("").unwrap(), []);
        assert!(parse_ss("u_str LISTEN 0 128 /tmp/sock users:((\"tmux: server\",pid=99,fd=6))\n").is_err());
    }

    #[test]
    fn parses_pane_lines() {
        let fields = ["work", "g", "0", "zsh", "1", "1", "{\"V\":2}", "1", "%3", "0", "0", "4242", "/data", "/home", "rsync", ""];
        let line = parse_pane_line(&fields.join("\x1f")).unwrap();
        assert_eq!((line.session.as_str(), line.group.as_str(), line.window_index, line.window_name.as_str(), line.automatic_rename, line.window_active), ("work", "g", 0, "zsh", true, true));
        assert_eq!((line.layout.as_str(), line.pane_index, line.pane_id.as_str(), line.pane_active, line.dead), ("{\"V\":2}", 1, "%3", false, false));
        assert_eq!((line.pid, line.path.as_str(), line.start_path.as_str(), line.current_command.as_str(), line.start_command.as_str()), (4242, "/data", "/home", "rsync", ""));
        // A dead pane has no pid
        let mut dead = fields;
        dead[10] = "1";
        dead[11] = "";
        assert_eq!((parse_pane_line(&dead.join("\x1f")).unwrap().dead, parse_pane_line(&dead.join("\x1f")).unwrap().pid), (true, 0));
        assert!(parse_pane_line(&fields[..15].join("\x1f")).is_err());
        assert!(parse_pane_line(&fields.join("\x1f").replace("4242", "x")).is_err());
    }

    /// Records of a shell (pid 4242) that ran `ls`, then `sleep 9 &` (job 1,
    /// still running) and `sleep 1 &` (job 2, done).
    fn records_after_jobs() -> String {
        [
            r#"{"type":"session_start","shell_pid":4242}"#,
            r#"{"type":"command_start","id":"s.1","shell_pid":4242,"cwd":"/home/at","command":"ls","hist_hidden":false}"#,
            r#"{"type":"command_end","id":"s.1","status":0}"#,
            r#"{"type":"command_start","id":"s.2","shell_pid":4242,"cwd":"/tmp","command":"sleep 9 &; sleep 1 &","hist_hidden":false}"#,
            r#"{"type":"async_start","id":"s.2","job":1,"job_text":"sleep 9"}"#,
            r#"{"type":"async_start","id":"s.2","job":2,"job_text":"sleep 1"}"#,
            r#"{"type":"command_end","id":"s.2","status":0}"#,
            r#"{"type":"async_end","id":"s.2","job":2,"status":0}"#,
        ]
        .join("\n")
    }

    #[test]
    fn reads_what_a_shell_is_running() {
        let running = |tail: &str| shell_running(&parse_records(tail).unwrap(), 4242, "zsh", "/pane").unwrap();
        // A cut-short first line, then a command still running
        let typed = "status\":0}\n{\"type\":\"command_start\",\"id\":\"s.1\",\"shell_pid\":4242,\"cwd\":\"/data\",\"command\":\"rsync -a /a /b\"}\n";
        assert_eq!(running(typed), (Running::Typed { command: "rsync -a /a /b".into() }, "/data".into()));
        // One typed with a leading space
        let hidden = r#"{"type":"command_start","id":"s.1","shell_pid":4242,"cwd":"/data","hist_hidden":true,"command":null}"#;
        assert_eq!(running(hidden), (Running::Unknown { program: "zsh".into() }, "/pane".into()));
        // Nothing typed yet
        assert_eq!(running(r#"{"type":"session_start","shell_pid":4242}"#), (Running::Idle, "/pane".into()));
        assert_eq!(running(""), (Running::Idle, "/pane".into()));
        // A background job left running, with its end written before its start
        let after_jobs = records_after_jobs();
        let job = |jobs: Vec<Option<String>>| (Running::Background { jobs }, "/pane".to_string());
        assert_eq!(running(&after_jobs), job(vec![Some("sleep 9".into())]));
        let reordered = after_jobs.replace("\n{\"type\":\"async_end\",\"id\":\"s.2\",\"job\":2,\"status\":0}", "").replacen('\n', "\n{\"type\":\"async_end\",\"id\":\"s.2\",\"job\":2,\"status\":0}\n", 1);
        assert_eq!(running(&reordered), job(vec![Some("sleep 9".into())]));
        // Even while a command runs in the foreground
        let foreground = format!("{after_jobs}\n{{\"type\":\"command_start\",\"id\":\"s.3\",\"shell_pid\":4242,\"cwd\":\"/tmp\",\"command\":\"htop\"}}");
        assert_eq!(running(&foreground), job(vec![Some("sleep 9".into())]));
        // Lost (disowned), then nothing left
        let lost = format!("{after_jobs}\n{{\"type\":\"async_lost\",\"id\":\"s.2\",\"job\":1,\"reason\":\"missing_from_job_table\"}}");
        assert_eq!(running(&lost), (Running::Idle, "/pane".into()));
        // A hidden job
        let hidden_job = format!("{after_jobs}\n{{\"type\":\"command_start\",\"id\":\"s.3\",\"shell_pid\":4242,\"cwd\":\"/tmp\",\"command\":null}}\n{{\"type\":\"async_start\",\"id\":\"s.3\",\"job\":1,\"job_text\":null}}\n{{\"type\":\"command_end\",\"id\":\"s.3\"}}");
        assert_eq!(running(&hidden_job), job(vec![Some("sleep 9".into()), None]));
        // The wrong shell's archive
        let other = r#"{"type":"command_start","id":"s.1","shell_pid":1,"cwd":"/","command":"x"}"#;
        assert!(shell_running(&parse_records(other).unwrap(), 4242, "zsh", "/pane").is_err());
        // A record of a known kind that doesn't parse, after the first line
        assert!(parse_records("{\"type\":\"session_start\"}\n{\"type\":\"command_start\",\"id\":\"s.1\"}").is_err());
        assert!(parse_records("{\"type\":\"command_start\",\"id\":\"s.1\"}\n{\"type\":\"whatever\"}").is_ok());
    }

    #[test]
    fn tells_what_panes_run() {
        let running = |line: &PaneLine, facts: Option<&ProcessFacts>| pane_running(line, facts).unwrap();
        let archive = r#"{"type":"command_start","id":"s.1","shell_pid":4242,"cwd":"/data","command":"htop"}"#;
        let done = format!("{archive}\n{{\"type\":\"command_end\",\"id\":\"s.1\"}}");
        assert_eq!(running(&line("htop", ""), Some(&facts(&["htop"], Some(archive)))), (Running::Typed { command: "htop".into() }, "/data".into()));
        assert_eq!(running(&line("zsh", ""), Some(&facts(&[], Some(&done)))), (Running::Idle, "/pane".into()));
        // The archive says idle, but the shell has a child: a disowned job, say
        assert_eq!(running(&line("zsh", ""), Some(&facts(&["sleep"], Some(&done)))), (Running::Unknown { program: "sleep".into() }, "/pane".into()));
        // A shell started by a start command (default-command, or `tmux new zsh`) goes by its archive
        assert_eq!(running(&line("htop", "zsh"), Some(&facts(&["htop"], Some(archive)))), (Running::Typed { command: "htop".into() }, "/data".into()));
        // Started with a command, which started in /start
        assert_eq!(running(&line("sleep", "\"sleep 300\""), Some(&facts(&[], None))), (Running::Started { command: "\"sleep 300\"".into() }, "/start".into()));
        // Shells without an archive: idle with no children, else unknown
        assert_eq!(running(&line("bash", ""), Some(&facts(&[], None))), (Running::Idle, "/pane".into()));
        assert_eq!(running(&line("bash", ""), Some(&facts(&["python3"], None))), (Running::Unknown { program: "python3".into() }, "/pane".into()));
        assert_eq!(running(&line("htop", ""), Some(&facts(&[], None))), (Running::Unknown { program: "htop".into() }, "/pane".into()));
        assert!(pane_running(&line("zsh", ""), None).is_err());
        // A dead pane
        let fields = ["work", "", "0", "zsh", "1", "1", "{}", "0", "%7", "1", "1", "", "", "/start", "", "\"sleep 1\""];
        assert_eq!(running(&parse_pane_line(&fields.join("\x1f")).unwrap(), None), (Running::Idle, "/start".into()));
    }

    #[test]
    fn assembles_sessions() {
        let line = |session: &str, group: &str, window: &str, name: &str, automatic: &str, pane: &str, pid: &str| {
            let fields = [session, group, window, name, automatic, "0", "{}", pane, "%1", "0", "0", pid, "/", "/", "zsh", ""];
            parse_pane_line(&fields.join("\x1f")).unwrap()
        };
        let lines = vec![line("a", "", "0", "zsh", "1", "0", "1"), line("a", "", "0", "zsh", "1", "1", "2"), line("a", "", "2", "top", "0", "0", "3"), line("b", "g", "0", "zsh", "1", "0", "4")];
        let runnings: Vec<_> = (1..=4).map(|n| (Running::Idle, format!("/{n}"))).collect();
        let sessions = assemble(lines, runnings, vec!["/".into(); 4]);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].windows.len(), 2);
        assert_eq!(sessions[0].windows[0].panes.iter().map(|pane| pane.cwd.as_str()).collect::<Vec<_>>(), ["/1", "/2"]);
        assert_eq!((sessions[0].windows[0].name.as_deref(), sessions[0].windows[1].name.as_deref()), (None, Some("top")));
        assert_eq!((sessions[0].group.as_deref(), sessions[1].group.as_deref()), (None, Some("g")));
        assert_eq!((sessions[1].name.as_str(), sessions[1].windows[0].panes[0].cwd.as_str(), sessions[1].windows[0].panes[0].id.as_str()), ("b", "/4", "%1"));
    }

    #[test]
    fn judges_panes() {
        let patterns = patterns(&["rsync .*"]);
        let server = test_server();
        let verdicts: Vec<_> = server.panes().map(|(location, pane)| (location, verdict(pane, &patterns))).collect();
        assert_eq!(
            verdicts,
            [
                ("work:0.0".to_string(), Verdict::Idle),
                ("work:0.1".to_string(), Verdict::Resume("rsync -a /data/ /backup/".into())),
                ("work:3.0".to_string(), Verdict::Block("htop (doesn't match resumable_commands)".into())),
            ]
        );
        let long = pane(0, "/", Running::Started { command: format!("\"sleep {}\"", "9".repeat(200)) });
        let Verdict::Block(why) = verdict(&long, &patterns) else { panic!() };
        assert!(why.starts_with("\"sleep 999") && why.ends_with("… (doesn't match resumable_commands)") && why.len() < 150, "{why}");
        let multiline = pane(0, "/", Running::Typed { command: "rsync -a \\\n  /a \\\n  /b".into() });
        assert_eq!(verdict(&multiline, &super::tests::patterns(&["(?s)rsync .*"])), Verdict::Resume("rsync -a \\ /a \\ /b".into()));
        let gone = Pane { mountpoint: String::new(), ..pane(0, "/gone", Running::Typed { command: "rsync -a /a /b".into() }) };
        assert_eq!(verdict(&gone, &patterns), Verdict::Block("rsync -a /a /b (its directory isn't there)".into()));
        let jobs = pane(0, "/", Running::Background { jobs: vec![Some("sleep 9".into()), None] });
        assert_eq!(verdict(&jobs, &patterns), Verdict::Block("background jobs are still running: sleep 9; one whose line was hidden".into()));
        let job = pane(0, "/", Running::Background { jobs: vec![Some("sleep 9".into())] });
        assert_eq!(verdict(&job, &patterns), Verdict::Block("a background job is still running: sleep 9".into()));
        let unknown = pane(0, "/", Running::Unknown { program: "sudo".into() });
        assert_eq!(verdict(&unknown, &patterns), Verdict::Block("running sudo, but what was typed to start it is unknown".into()));
    }

    #[test]
    fn finds_blockers_by_policy() {
        let patterns = patterns(&["rsync .*"]);
        let server = test_server();
        assert_eq!(blockers(TmuxPolicy::Block, &patterns, std::slice::from_ref(&server)), ["tmux: at's server at /tmp/tmux-1000/default has 1 session and 3 panes (tmux is \"block\")"]);
        assert_eq!(blockers(TmuxPolicy::Resume, &patterns, std::slice::from_ref(&server)), ["tmux: at's work:3.0 in /home/at: htop (doesn't match resumable_commands)"]);
        assert_eq!(blockers(TmuxPolicy::Resume, &super::tests::patterns(&["rsync .*", "htop"]), std::slice::from_ref(&server)), Vec::<String>::new());
        let unlisted = Server { sessions: vec![], error: Some("protocol version mismatch (client 8, server 7)".into()), ..server };
        let expected = ["tmux: couldn't list at's server at /tmp/tmux-1000/default: protocol version mismatch (client 8, server 7)"];
        assert_eq!(blockers(TmuxPolicy::Resume, &patterns, std::slice::from_ref(&unlisted)), expected);
        assert_eq!(blockers(TmuxPolicy::Block, &patterns, std::slice::from_ref(&unlisted)), expected);
        let unseen = Server { socket: String::new(), error: Some("ss doesn't list its socket".into()), ..unlisted };
        assert_eq!(blockers(TmuxPolicy::Block, &patterns, std::slice::from_ref(&unseen)), ["tmux: couldn't list at's server (pid 1234): ss doesn't list its socket"]);
        assert_eq!(blockers(TmuxPolicy::Block, &patterns, &[]), Vec::<String>::new());
    }

    #[test]
    fn describes_servers() {
        let patterns = patterns(&["rsync .*"]);
        let server = test_server();
        let at = "at's tmux server at /tmp/tmux-1000/default (pid 1234)";
        assert_eq!(
            listing(std::slice::from_ref(&server), TmuxPolicy::Resume, &patterns, false),
            [[
                &format!("{at}: 1 session and 3 panes"),
                "         idle  work:0.0  /home/at",
                "    resumable  work:0.1  /data     rsync -a /data/ /backup/",
                "not resumable  work:3.0  /home/at  htop (doesn't match resumable_commands)",
            ]
            .join("\n")]
        );
        let colored = listing(std::slice::from_ref(&server), TmuxPolicy::Resume, &patterns, true).concat();
        assert!(colored.contains("\n         \x1b[2midle\x1b[0m  work:0.0") && colored.contains("\n    \x1b[32mresumable\x1b[0m  ") && colored.contains("\n\x1b[31mnot resumable\x1b[0m  "), "{colored}");
        assert!(listing(std::slice::from_ref(&server), TmuxPolicy::Block, &patterns, false)[0].starts_with(&format!("{at}: 1 session and 3 panes (tmux is \"block\", so the server blocks a reboot)\n")));
        assert!(listing(std::slice::from_ref(&server), TmuxPolicy::Block, &patterns, true)[0].starts_with(&format!("{at}: 1 session and 3 panes \x1b[31m(tmux is \"block\", so the server blocks a reboot)\x1b[0m\n")));

        // The columns line up across servers, even one with only an idle
        // pane; a server that couldn't be listed, or has no panes, has just
        // its line.
        let mut root = Server { user: "root".into(), socket: "/tmp/tmux-0/default".into(), pid: 1, ..server.clone() };
        root.sessions[0].windows.truncate(1);
        root.sessions[0].windows[0].panes.truncate(1);
        let empty = Server { sessions: vec![], ..server.clone() };
        let unlisted = Server { sessions: vec![], error: Some("no such file".into()), ..server.clone() };
        assert_eq!(
            listing(&[unlisted, root, empty, server], TmuxPolicy::Resume, &patterns, false),
            [
                format!("{at}: couldn't be listed: no such file"),
                "root's tmux server at /tmp/tmux-0/default (pid 1): 1 session and 1 pane\n         idle  work:0.0  /home/at".into(),
                format!("{at}: 0 sessions and 0 panes"),
                listing(&[test_server()], TmuxPolicy::Resume, &patterns, false)[0].clone(),
            ]
        );
        let unlisted = Server { sessions: vec![], error: Some("no such file".into()), ..test_server() };
        assert_eq!(listing(&[unlisted], TmuxPolicy::Resume, &patterns, true), [format!("{at}: \x1b[31mcouldn't be listed: no such file\x1b[0m")]);
    }

    #[test]
    fn serializes_running() {
        let json = |running: Running| serde_json::to_value(running).unwrap();
        assert_eq!(json(Running::Idle), serde_json::json!({"state": "idle"}));
        assert_eq!(json(Running::Typed { command: "htop".into() }), serde_json::json!({"state": "typed", "command": "htop"}));
        assert_eq!(json(Running::Background { jobs: vec![None] }), serde_json::json!({"state": "background", "jobs": [null]}));
        let server: Server = serde_json::from_value(serde_json::to_value(test_server()).unwrap()).unwrap();
        assert_eq!(server, test_server());
    }
}
