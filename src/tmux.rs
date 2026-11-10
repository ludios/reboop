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
use std::sync::LazyLock;

/// How much of a command a [`Verdict`] shows.
const COMMAND_SHOWN: usize = 100;

/// An awk program that prints the records of a zsh-spy archive that tell
/// what its shell runs, in the archive's order:
/// - every record from the last command_start on (a builtin that has run
///   for a while may have had records of many background jobs come after
///   it);
/// - of the records before the last command_start, the async_start,
///   async_end and async_lost of every job that hasn't ended (one that has
///   more async_starts than async_ends and async_losts in the archive, as
///   zsh can give a job the number of one that ended during the same line),
///   and the command_start and command_end of the line that started that
///   job, however far back they are;
/// - of the records before the last command_start, those about a job whose
///   async_end or async_lost was written before its async_start, if that
///   async_start comes after the last command_start.
///
/// Its arguments are `ARCHIVE pass=2 ARCHIVE`: it reads the archive twice,
/// so as to hold no more than the keys of the jobs that haven't ended.
const PICK_RECORDS: &str = r#"# A record's first "id" and "job": its line's id and its job number.  (A
# command's text can't hold them, as JSON escapes its quotes.)
function id(r) { match(r, /"id":"[^"]*"/); return substr(r, RSTART + 6, RLENGTH - 7) }
function job(r) { match(r, /"job":[0-9]+/); return substr(r, RSTART + 6, RLENGTH - 6) }
# Pass 1: n[k] is how many more async_starts than ends job k has had so
# far (kept only while it isn't 0), and late[k] where an async_start of job
# k came after an end that it made up for.
pass != 2 {
    if (/"type":"command_start"/) {
        last = FNR
    } else if (/"type":"async_start"/) {
        k = id($0) SUBSEP job($0)
        if (++n[k] == 0) {
            delete n[k]
            late[k] = FNR
        }
    } else if (/"type":"async_(end|lost)"/) {
        k = id($0) SUBSEP job($0)
        if (--n[k] == 0) delete n[k]
    }
    next
}
# Pass 2 (where looking up a key that isn't there would add it)
!ready {
    ready = 1
    for (k in n) if (n[k] > 0) {
        split(k, part, SUBSEP)
        lines[part[1]] = 1
    }
}
last && FNR >= last { print; next }
/"type":"command_(start|end)"/ { if (id($0) in lines) print; next }
/"type":"async_(start|end|lost)"/ {
    k = id($0) SUBSEP job($0)
    if ((k in n && n[k] > 0) || (k in late && late[k] > last)) print
}"#;

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
    /// zsh-spy recorded it, perhaps one that was stopped with Ctrl-Z and
    /// brought back with fg (see [`shell_running`]).
    Typed { command: String },
    /// The command tmux started it with in place of a shell, as tmux shows
    /// it, quoted for tmux's own command parser: e.g. `"sleep 300"` for a
    /// pane started with `tmux new-session 'sleep 300'`.
    Started { command: String },
    /// Jobs of its shell haven't ended, whether they run in the background
    /// (put there with &), are stopped (as with Ctrl-Z), or were brought
    /// back into the foreground: what each ran, as zsh-spy recorded it, or
    /// `None` for one whose line HIST_IGNORE_SPACE hid.  (Whatever the shell
    /// runs in the foreground, except for a job that counts as `Typed`: see
    /// [`shell_running`].)
    Background { jobs: Vec<Option<String>> },
    /// It's running `program`, but what was typed to start it isn't known:
    /// its shell has no zsh-spy archive, or the line was hidden from it
    /// (HIST_IGNORE_SPACE), or the line is nothing but fg, or the program
    /// replaced the shell, or the shell has a child process that the archive
    /// doesn't account for, like a job that was disowned.
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
    /// Its process's umask, which its panes inherit, in octal as
    /// /proc/PID/status shows it, like "0022"; `None` if that isn't known
    /// (as in a capture that was saved before reboop recorded it)
    #[serde(default)]
    pub umask: Option<String>,
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
/// `tmux list-panes` print: a control character, which names and paths
/// hardly ever have.  (One that has it makes the server unlistable.)
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
    CommandStart(LineStart),
    CommandEnd {
        id: String,
        /// The line's exit status
        status: Option<i64>,
        /// The jobs that the line started, by their numbers in zsh's job
        /// table, each a JSON number (or, for a key of zsh's that isn't a
        /// number, a JSON string)
        async_jobs: Option<Vec<serde_json::Value>>,
    },
    AsyncStart {
        /// The id of the command that started the job
        id: String,
        job: u64,
        /// zsh's state of the job when zsh-spy found it, like "running" or
        /// "suspended"
        job_state: Option<String>,
        /// The job's pids, comma-separated, in the order zsh started them
        job_pids: Option<String>,
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

/// A command_start record: a line that the shell began to run.
#[derive(Debug, Deserialize)]
struct LineStart {
    id: String,
    shell_pid: u32,
    /// The directory it was typed in
    cwd: String,
    /// None if the line was hidden (HIST_IGNORE_SPACE)
    command: Option<String>,
    /// The line with its aliases expanded, or None if it was hidden
    command_expanded: Option<String>,
}

impl LineStart {
    /// Whether the line does nothing but bring a job back into the
    /// foreground (see [`is_bare_fg`]), both as typed and with its aliases
    /// expanded.  (A function named fg isn't seen through.)
    fn only_fg(&self) -> bool {
        [&self.command, &self.command_expanded].iter().all(|command| command.as_deref().is_some_and(is_bare_fg))
    }
}

/// Parses the records in `picked`, the lines of an archive that
/// [`PICK_RECORDS`] printed.  A line that doesn't parse is an error, rather
/// than a command that goes unseen.
fn parse_records(picked: &str) -> Result<Vec<Record>> {
    picked.lines()
        .map(|line| serde_json::from_str(line).with_context(|| format!("couldn't parse a zsh-spy record: {}", human::truncate(line, COMMAND_SHOWN))))
        .collect()
}

/// A job of a shell that its archive shows started, but not ended or lost.
struct Job<'a> {
    /// The id of the line that started it
    id: &'a str,
    /// Its number in the shell's job table
    number: u64,
    /// Whether zsh-spy found it stopped
    suspended: bool,
    /// Its first process, which leads its process group, if known
    leader: Option<i32>,
    /// What it ran, as zsh shows it, or None if its line was hidden
    text: Option<String>,
}

/// The jobs that `records` show started, but not ended or lost: by command
/// id and job number, those with more async_starts than async_ends and
/// async_losts, in whatever order the records came, since zsh-spy's traps
/// can write them out of order, and zsh can give a job the number of one
/// that ended during the same line.
fn unended_jobs(records: &[Record]) -> Vec<Job<'_>> {
    // (how many more starts than ends, the job of the last start) by
    // (command id, job number)
    let mut jobs: BTreeMap<(&str, u64), (i64, Option<Job>)> = BTreeMap::new();
    for record in records {
        match record {
            Record::AsyncStart { id, job, job_state, job_pids, job_text } => {
                let suspended = job_state.as_deref() == Some("suspended");
                let leader = job_pids.as_deref().and_then(|pids| pids.split(',').next()?.parse().ok());
                let entry = jobs.entry((id, *job)).or_default();
                entry.0 += 1;
                entry.1 = Some(Job { id, number: *job, suspended, leader, text: job_text.clone() });
            }
            Record::AsyncEnd { id, job } | Record::AsyncLost { id, job } => jobs.entry((id, *job)).or_default().0 -= 1,
            _ => {}
        }
    }
    jobs.into_values().filter(|(starts, _)| *starts > 0).filter_map(|(_, job)| job).collect()
}

/// Whether `command`, a line for zsh, is nothing but `fg`, `fg JOB` or
/// `%JOB` (with spaces or tabs around them), where JOB is made of letters,
/// digits and `_%+?.-`, as `1`, `+`, `sleep` or `?600` are, or is left out.
fn is_bare_fg(command: &str) -> bool {
    static BARE_FG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[ \t]*(fg([ \t]+[\w%+?.-]+)?|%[\w%+?.-]*)[ \t]*$").unwrap());
    BARE_FG.is_match(command)
}

/// The statuses that zsh gives a line when its foreground job is stopped:
/// 128 plus the number of SIGSTOP or of SIGTSTP (Ctrl-Z's), as on x86 and
/// ARM Linux.
const STOPPED: [i64; 2] = [128 + 19, 128 + 20];

/// Whether `job` was stopped in the foreground of the line that started it,
/// and that line started no other job: zsh-spy found the job stopped, and
/// the line's command_end among `records` has a status that zsh gives a
/// stopped line and lists only that job.  A job that was put in the
/// background with & doesn't pass: zsh-spy finds it running, or its line
/// ends with the status of what ran after it, as with `cat &`, which
/// reading the terminal stops.
fn stopped_alone(records: &[Record], job: &Job) -> bool {
    job.suspended
        && records.iter().any(|record| {
            matches!(record, Record::CommandEnd { id, status: Some(status), async_jobs: Some(jobs) }
                if id == job.id && STOPPED.contains(status) && *jobs == [serde_json::json!(job.number)])
        })
}

/// What a shell runs while it runs the line that `line` began, and the
/// directory to run it in again: the line as typed, where it was typed; or
/// else, if the line isn't known, is hidden, or is nothing but fg (and so
/// can't run again), something unknown, as `program` in `path`.
fn line_running(line: Option<&LineStart>, program: &str, path: &str) -> (Running, String) {
    if let Some(line) = line
        && let Some(command) = &line.command
        && !is_bare_fg(command)
    {
        return (Running::Typed { command: command.clone() }, line.cwd.clone());
    }
    (Running::Unknown { program: program.into() }, path.into())
}

/// What the shell with pid `pid` is running, by the `records` that
/// [`PICK_RECORDS`] picked from its archive, and the directory to run it in
/// again: where a typed command was typed, or else `path`, the pane's
/// current directory.  tmux says the pane's foreground program is
/// `program`; `waited_on` is the process group of the shell's children that
/// has its terminal, if any.
///
/// A job that hasn't ended makes it [`Running::Background`], unless the
/// shell brought the job back into the foreground (with a line that's
/// nothing but `fg` or the like: see [`LineStart::only_fg`]) after it was
/// stopped there (see [`stopped_alone`]), and it's the shell's only job:
/// then what runs is the line that started the job (see [`line_running`]).
fn shell_running(records: &[Record], pid: u32, waited_on: Option<i32>, program: &str, path: &str) -> Result<(Running, String)> {
    // The line it runs: its last one, unless that has ended
    let mut running = None;
    let last = records.iter().enumerate().rev().find_map(|(at, record)| if let Record::CommandStart(line) = record { Some((at, line)) } else { None });
    if let Some((at, line)) = last {
        ensure!(line.shell_pid == pid, "the archive that pid {pid} holds open has records of pid {}", line.shell_pid);
        if !records[at + 1..].iter().any(|record| matches!(record, Record::CommandEnd { id, .. } if *id == line.id)) {
            running = Some(line);
        }
    }

    let jobs = unended_jobs(records);
    if let [job] = jobs.as_slice()
        && job.leader.is_some()
        && job.leader == waited_on
        && running.is_some_and(LineStart::only_fg)
        && stopped_alone(records, job)
    {
        let started = records.iter().find_map(|record| if let Record::CommandStart(line) = record && line.id == job.id { Some(line) } else { None });
        return Ok(line_running(started, program, path));
    }
    if !jobs.is_empty() {
        return Ok((Running::Background { jobs: jobs.into_iter().map(|job| job.text).collect() }, path.into()));
    }
    Ok(running.map_or_else(|| (Running::Idle, path.into()), |line| line_running(Some(line), program, path)))
}

/// Programs that are shells, so a pane running one with no children and no
/// zsh-spy archive is taken to be idle.
const SHELLS: &[&str] = &["zsh", "bash", "sh", "dash", "fish", "ksh"];

/// What's known about a pane's process besides what tmux says.
struct ProcessFacts {
    /// The names of its child processes
    children: Vec<String>,
    /// The names of those that aren't in the foreground process group of
    /// its terminal, as a typed command is and a background job isn't
    background: Vec<String>,
    /// The foreground process group of its terminal, if that's of its
    /// children: of a typed command, or of a job brought back with fg
    waited_on: Option<i32>,
    /// The records of its zsh-spy archive that [`PICK_RECORDS`] prints, if
    /// it's a shell that has one
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
    let unknown = || Running::Unknown { program: facts.children.first().cloned().unwrap_or_else(|| line.current_command.clone()) };
    if let Some(picked) = &facts.archive {
        let (running, cwd) = shell_running(&parse_records(picked)?, line.pid, facts.waited_on, &line.current_command, &line.path)?;
        // A child that the archive doesn't account for, like a disowned
        // job, means that more is running than the archive says.  (Also
        // what's left of a job that zsh split when it was stopped, as with
        // `sleep 5 | { sleep 60; echo x; }`, while it's brought back.)
        let unaccounted = match running {
            Running::Idle => facts.children.first(),
            Running::Typed { .. } => facts.background.first(),
            _ => None,
        };
        if let Some(program) = unaccounted {
            return Ok((Running::Unknown { program: program.clone() }, line.path.clone()));
        }
        return Ok((running, cwd));
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
/// pid: its children, its terminal's foreground process group if that's
/// theirs, and the records that [`PICK_RECORDS`] picks from the zsh-spy
/// archive (hist.*.jsonl) that it holds open, if any, read through its fd
/// (so that one in another mount namespace is found).
fn process_facts(session: &mut Session, lines: &[PaneLine]) -> Result<HashMap<u32, ProcessFacts>> {
    // Once each: a window linked into several sessions lists its panes once per session.
    let pids: BTreeSet<u32> = lines.iter().filter(|line| !line.dead).map(|line| line.pid).collect();
    let mut facts = HashMap::new();
    if pids.is_empty() {
        return Ok(facts);
    }
    // All at once, so that a child's process group and its parent's
    // terminal's foreground one are from the same moment
    let mut foregrounds: HashMap<u32, i32> = HashMap::new();
    let mut children: HashMap<u32, Vec<(i32, String)>> = HashMap::new();
    for line in session.run_ok("ps -e -o pid=,ppid=,pgid=,tpgid=,comm=", QUICK)?.lines() {
        let context = || format!("unexpected ps line {line:?}");
        let fields: Vec<&str> = line.split_whitespace().collect();
        ensure!(fields.len() >= 5, context());
        let pid: u32 = fields[0].parse().with_context(context)?;
        let ppid: u32 = fields[1].parse().with_context(context)?;
        let group: i32 = fields[2].parse().with_context(context)?;
        // -1 without a terminal
        let foreground: i32 = fields[3].parse().with_context(context)?;
        if pids.contains(&pid) {
            foregrounds.insert(pid, foreground);
        }
        if pids.contains(&ppid) {
            children.entry(ppid).or_default().push((group, fields[4..].join(" ")));
        }
    }
    // A line per pid: the pid, and the base64 of its archive's records that
    // matter, or "-" without an archive
    let script = format!(
        r#"for p in {}; do
    a=-
    for l in /proc/$p/fd/*; do
        case $(readlink "$l" 2>/dev/null) in
        */hist.*.jsonl)
            t=$(awk {} "$l" pass=2 "$l") || exit 1
            a=$(printf %s "$t" | base64 -w0)
            break;;
        esac
    done
    printf '%s %s\n' "$p" "$a"
done"#,
        pids.iter().map(u32::to_string).collect::<Vec<_>>().join(" "),
        shell_quote(PICK_RECORDS)
    );
    let output = session.run_ok(&script, QUICK)?;
    for line in output.lines() {
        let context = || format!("unexpected process facts line {:?}", human::truncate(line, COMMAND_SHOWN));
        let Some((pid, archive)) = line.split_once(' ') else { bail!(context()) };
        let pid: u32 = pid.parse().with_context(context)?;
        let foreground = foregrounds.get(&pid).copied().unwrap_or(-1);
        let mut facts_about = ProcessFacts { children: vec![], background: vec![], waited_on: None, archive: None };
        for (group, name) in children.remove(&pid).unwrap_or_default() {
            if group == foreground {
                facts_about.waited_on = Some(group);
            } else {
                facts_about.background.push(name.clone());
            }
            facts_about.children.push(name);
        }
        if archive != "-" {
            let bytes = BASE64.decode(archive).with_context(context)?;
            facts_about.archive = Some(String::from_utf8(bytes).with_context(|| format!("the zsh-spy archive of pid {pid} isn't UTF-8"))?);
        }
        facts.insert(pid, facts_about);
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

/// Whether `umask` is a umask as /proc/PID/status shows one: 1 to 4 octal
/// digits.
pub fn is_umask(umask: &str) -> bool {
    (1..=4).contains(&umask.len()) && umask.bytes().all(|byte| (b'0'..=b'7').contains(&byte))
}

/// Lists what's in the tmux server with pid `pid` at `socket`, or returns
/// `None` if it's gone.  Whatever keeps its sessions from being listed is
/// its `error` instead.
fn capture_server(session: &mut Session, socket: String, pid: u32) -> Result<Option<Server>> {
    let format: Vec<String> = PANE_FIELDS.iter().map(|field| format!("#{{{field}}}")).collect();
    // A line with the user and the server's umask (nothing, if the kernel
    // doesn't show it), then the listing; or what's wrong, with status 3
    let script = format!(
        r#"ids=$(stat -c '%U %u' /proc/{pid} 2>/dev/null) || {{ echo gone; exit 0; }}
set -- $ids
[ "$1" = UNKNOWN ] && set -- "$2"
printf '%s %s\n' "$1" "$(sed -n 's/^Umask:[[:space:]]*//p' /proc/{pid}/status 2>/dev/null)"
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
    let Some((owner, rest)) = stdout.split_once('\n') else {
        bail!("couldn't tell whose tmux server pid {pid} is: {}", output.stderr_text());
    };
    let (user, umask) = owner.split_once(' ').ok_or_else(|| anyhow!("unexpected line about the tmux server pid {pid}: {owner:?}"))?;
    ensure!(umask.is_empty() || is_umask(umask), "unexpected umask of the tmux server pid {pid}: {umask:?}");
    let umask = (!umask.is_empty()).then(|| umask.to_string());
    let mut server = Server { user: user.into(), socket, pid, umask, sessions: vec![], error: None };
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
        let Some(pid) = fields.next() else { continue };
        let Some(user) = fields.next() else { continue };
        if fields.collect::<Vec<_>>().join(" ") != "tmux: server" {
            continue;
        }
        let pid: u32 = pid.parse().with_context(|| format!("unexpected ps line {line:?}"))?;
        if servers.iter().all(|server| server.pid != pid) {
            let error = Some("ss doesn't list its socket (is it in another network namespace, like a container's?)".into());
            servers.push(Server { user: user.into(), socket: String::new(), pid, umask: None, sessions: vec![], error });
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
            Verdict::Block(format!("its shell has {}: {}", if jobs.len() == 1 { "a job that hasn't ended" } else { "jobs that haven't ended" }, texts.join("; ")))
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

/// The rows about `server`'s panes in [`listing`]'s table (none if the
/// server couldn't be listed), with `patterns` (the machine's
/// resumable_commands) deciding each pane's fate.
fn pane_rows(server: &Server, patterns: &CommandPatterns) -> Vec<Vec<Cell>> {
    if server.error.is_some() {
        return vec![];
    }
    let mut rows = Vec::new();
    for (location, pane) in server.panes() {
        let (fate, text) = match verdict(pane, patterns) {
            Verdict::Idle            => (Style::Dim.cell("idle"),          String::new()),
            Verdict::Resume(command) => (Style::Green.cell("resumable"),   command),
            Verdict::Block(why)      => (Style::Red.cell("not resumable"), why),
        };
        rows.push(vec![fate.aligned(Align::Right), Style::Plain.cell(location), Style::Plain.cell(pane.cwd.clone()), Style::Plain.cell(text)]);
    }
    rows
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

/// What's in `servers`, for people: a text per server, of a line about the
/// server, then a line per pane of the server (none if the server couldn't
/// be listed), in a table whose columns line up across all the servers.
///
/// A server's line has its sessions and panes counted (and says that the
/// server blocks a reboot, if `policy` is block), or why the server
/// couldn't be listed.  A pane's row has "idle", "resumable" or "not
/// resumable" (as [`verdict`] tells, given the commands that `patterns`
/// allow), flush right; the pane's location; the pane's directory; and
/// either the command to run again, why the pane can't be resumed, or (for
/// an idle pane) nothing.  If `color`, the three fates are dim, green and
/// red respectively, and a server's line has in red what it says about the
/// server blocking a reboot or not being listed.
pub fn listing(servers: &[Server], policy: TmuxPolicy, patterns: &CommandPatterns, color: bool) -> Vec<String> {
    let rows: Vec<Vec<Vec<Cell>>> = servers.iter().map(|server| pane_rows(server, patterns)).collect();
    let mut lines = human::table(&rows.concat(), color).into_iter();
    servers
        .iter()
        .zip(&rows)
        .map(|(server, rows)| {
            let mut text = heading(server, policy, color);
            for line in lines.by_ref().take(rows.len()) {
                text.push('\n');
                text.push_str(&line);
            }
            text
        })
        .collect()
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
        // Another pane than the idle one, with the same index in its window
        let htop = Pane { id: "%2".into(), ..pane(0, "/home/at", Running::Typed { command: "htop".into() }) };
        Server {
            user: "at".into(),
            socket: "/tmp/tmux-1000/default".into(),
            pid: 1234,
            umask: Some("0027".into()),
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

    /// Facts about a process with children in the `foreground` process
    /// group (whose number doesn't matter here) and in `background` ones.
    fn facts(foreground: &[&str], background: &[&str], archive: Option<&str>) -> ProcessFacts {
        ProcessFacts {
            children: foreground.iter().chain(background).map(|&child| child.into()).collect(),
            background: background.iter().map(|&child| child.into()).collect(),
            waited_on: None,
            archive: archive.map(String::from),
        }
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
        let running = |picked: &str| shell_running(&parse_records(picked).unwrap(), 4242, None, "zsh", "/pane").unwrap();
        // A command still running
        let typed = "{\"type\":\"command_start\",\"id\":\"s.1\",\"shell_pid\":4242,\"cwd\":\"/data\",\"command\":\"rsync -a /a /b\"}\n";
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
        assert!(shell_running(&parse_records(other).unwrap(), 4242, None, "zsh", "/pane").is_err());
        // A record of a known kind that doesn't parse, or a line that isn't
        // a record, even the first
        assert!(parse_records("{\"type\":\"session_start\"}\n{\"type\":\"command_start\",\"id\":\"s.1\"}").is_err());
        assert!(parse_records("\"type\":\"session_end\"}\n{\"type\":\"whatever\"}").is_err());
        assert!(parse_records("{\"type\":\"whatever\"}").is_ok());
    }

    #[test]
    fn takes_a_job_brought_back_with_fg_for_its_line() {
        // `sleep 600 | cat` (pids 500 and 501) typed in /data, stopped with
        // Ctrl-Z
        let stopped = [
            r#"{"type":"command_start","id":"s.1","shell_pid":4242,"cwd":"/data","command":"sleep 600 | cat"}"#,
            r#"{"type":"async_start","id":"s.1","job":1,"job_state":"suspended","job_pids":"500,501","job_text":"sleep 600 | cat"}"#,
            r#"{"type":"command_end","id":"s.1","status":148,"async_jobs":[1]}"#,
        ]
        .join("\n");
        // ... then brought back by `line`, typed in /tmp, which is
        // `expanded` with its aliases expanded
        let after_expanded = |stopped: &str, line: &str, expanded: &str| {
            let start = serde_json::json!({"type": "command_start", "id": "s.2", "shell_pid": 4242, "cwd": "/tmp", "command": line, "command_expanded": expanded});
            format!("{stopped}\n{start}")
        };
        let after = |stopped: &str, line: &str| after_expanded(stopped, line, line);
        let running = |picked: &str, waited_on| shell_running(&parse_records(picked).unwrap(), 4242, waited_on, "sleep", "/pane").unwrap();
        let typed = (Running::Typed { command: "sleep 600 | cat".into() }, "/data".to_string());
        let job = (Running::Background { jobs: vec![Some("sleep 600 | cat".into())] }, "/pane".to_string());
        for line in ["fg", "fg %1", "fg sleep", " %  ", "%sleep", "%?600"] {
            assert_eq!(running(&after(&stopped, line), Some(500)), typed, "{line}");
        }
        assert_eq!(running(&after(&stopped.replace("148", "147"), "fg"), Some(500)), typed);
        // Not while another group, or the shell, has the terminal
        assert_eq!(running(&after(&stopped, "fg"), Some(501)), job);
        assert_eq!(running(&after(&stopped, "fg"), None), job);
        // Not when the line that brought it back does more, or might, or
        // doesn't bring it back
        for line in ["fg; make install", "fg && make install", "fg\nmake", "fg %1\nmake", "fg %1 %2", "f", "wait %1", "fg $(x)"] {
            assert_eq!(running(&after(&stopped, line), Some(500)), job, "{line}");
        }
        assert_eq!(running(&after(&stopped, " fg").replace("\" fg\"", "null"), Some(500)), job);
        assert_eq!(running(&after_expanded(&stopped, "fg", "fg\necho x"), Some(500)), job);
        // Not once that line has ended
        assert_eq!(running(&format!("{}\n{{\"type\":\"command_end\",\"id\":\"s.2\",\"status\":0}}", after(&stopped, "fg")), Some(500)), job);
        // Not a job put in the background with &, or one whose line went
        // on after Ctrl-Z, or started another job too
        assert_eq!(running(&after(&stopped.replace("148", "0"), "fg"), Some(500)), job);
        assert_eq!(running(&after(&stopped.replace("\"suspended\"", "\"running\""), "fg"), Some(500)), job);
        assert_eq!(running(&after(&stopped.replace("[1]", "[1,2]"), "fg"), Some(500)), job);
        // Not with another job that hasn't ended
        let two = stopped.replace(
            "\n{\"type\":\"command_end\"",
            "\n{\"type\":\"async_start\",\"id\":\"s.1\",\"job\":2,\"job_pids\":\"600\",\"job_text\":\"sleep 9\"}\n{\"type\":\"command_end\"",
        );
        assert_eq!(running(&after(&two, "fg"), Some(500)), (Running::Background { jobs: vec![Some("sleep 600 | cat".into()), Some("sleep 9".into())] }, "/pane".into()));
        // Its line hidden, or not among the records
        let hidden = stopped.replace("\"sleep 600 | cat\"", "null");
        assert_eq!(running(&after(&hidden, "fg"), Some(500)), (Running::Unknown { program: "sleep".into() }, "/pane".into()));
        let unseen = stopped.split_once('\n').unwrap().1;
        assert_eq!(running(&after(unseen, "fg"), Some(500)), (Running::Unknown { program: "sleep".into() }, "/pane".into()));
        // A line that's nothing but fg can't run again, whether it's the one
        // that brought a job back or, as zsh-spy saw it, started the job
        assert_eq!(running(&after("", "fg").trim_start(), Some(500)), (Running::Unknown { program: "sleep".into() }, "/pane".into()));
        let refound = stopped.replace("\"sleep 600 | cat\"}", "\"fg\"}");
        assert_eq!(running(&after(&refound, "fg"), Some(500)), (Running::Unknown { program: "sleep".into() }, "/pane".into()));
        // A child of the shell outside the job's group (here, left by a job
        // that zsh split) is a process that the archive doesn't account for
        let facts = ProcessFacts { waited_on: Some(500), ..facts(&["sleep", "cat"], &["zsh"], Some(&after(&stopped, "fg"))) };
        assert_eq!(pane_running(&line("sleep", ""), Some(&facts)).unwrap(), (Running::Unknown { program: "zsh".into() }, "/pane".into()));
    }

    /// The lines that [`PICK_RECORDS`], run by this machine's awk, prints
    /// for an archive of `records`.
    fn picked(records: &[String]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("hist.s.jsonl");
        std::fs::write(&archive, records.iter().map(|record| format!("{record}\n")).collect::<String>()).unwrap();
        let output = std::process::Command::new("awk").arg(PICK_RECORDS).arg(&archive).arg("pass=2").arg(&archive).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().lines().map(String::from).collect()
    }

    #[test]
    fn picks_the_records_that_tell_what_a_shell_runs() {
        let start = |id: &str, command: &str| format!(r#"{{"type":"command_start","id":"{id}","shell_pid":4242,"cwd":"/","command":"{command}"}}"#);
        let end = |id: &str| format!(r#"{{"type":"command_end","id":"{id}","status":0}}"#);
        let job = |kind: &str, id: &str, number: u32| format!(r#"{{"type":"async_{kind}","id":"{id}","job":{number}}}"#);
        let mut archive = vec![r#"{"type":"session_start","shell_pid":4242}"#.to_string()];
        // Job 1 of s.1, still running, then more than 100 records
        let old = [start("s.1", "sleep 9 &"), job("start", "s.1", 1), end("s.1")];
        archive.extend(old.clone());
        for n in 0..60 {
            archive.extend([start(&format!("f.{n}"), "true"), end(&format!("f.{n}"))]);
        }
        // Job 1 of s.2, which ended, then another job 1, still running; and
        // job 2, lost
        let reused = [
            start("s.2", "sleep 1 & sleep 2; jobs; sleep 8 & sleep 7 &"),
            job("start", "s.2", 1),
            job("end", "s.2", 1),
            job("start", "s.2", 1),
            job("start", "s.2", 2),
            end("s.2"),
            job("lost", "s.2", 2),
        ];
        archive.extend(reused.clone());
        // A line whose text looks like the keys of job 1 of s.1
        archive.push(r#"{"type":"command_start","id":"s.3","shell_pid":4242,"cwd":"/","command":"echo \"id\":\"s.1\",\"job\":1"}"#.into());
        archive.push(end("s.3"));
        // The end of job 1 of s.4, written before its start, which comes
        // after the last command_start
        let early = job("end", "s.4", 1);
        let last = [start("s.4", "sleep 3 & sleep 4 & make"), job("start", "s.4", 1), job("start", "s.4", 2)];
        archive.push(early.clone());
        archive.extend(last.clone());

        let mut expected = old.to_vec();
        expected.extend_from_slice(&reused[..4]);
        expected.push(reused[5].clone());
        expected.push(early);
        expected.extend_from_slice(&last);
        let picked_records = picked(&archive);
        assert_eq!(picked_records, expected);
        let records = parse_records(&picked_records.join("\n")).unwrap();
        assert_eq!(unended_jobs(&records).iter().map(|job| (job.id, job.number)).collect::<Vec<_>>(), [("s.1", 1), ("s.2", 1), ("s.4", 2)]);
        // Nothing to pick without a command line
        assert_eq!(picked(&archive[..1]), Vec::<String>::new());
        assert_eq!(picked(&[]), Vec::<String>::new());
        // Only the last line's own records when no job is left
        assert_eq!(picked(&archive[4..10]), archive[8..10]);
    }

    #[test]
    fn tells_what_panes_run() {
        let running = |line: &PaneLine, facts: Option<&ProcessFacts>| pane_running(line, facts).unwrap();
        let archive = r#"{"type":"command_start","id":"s.1","shell_pid":4242,"cwd":"/data","command":"htop"}"#;
        let done = format!("{archive}\n{{\"type\":\"command_end\",\"id\":\"s.1\"}}");
        assert_eq!(running(&line("htop", ""), Some(&facts(&["htop"], &[], Some(archive)))), (Running::Typed { command: "htop".into() }, "/data".into()));
        assert_eq!(running(&line("zsh", ""), Some(&facts(&[], &[], Some(&done)))), (Running::Idle, "/pane".into()));
        // The archive says idle, or that a command runs in the foreground,
        // but the shell has a child in the background: a disowned job, say
        assert_eq!(running(&line("zsh", ""), Some(&facts(&[], &["sleep"], Some(&done)))), (Running::Unknown { program: "sleep".into() }, "/pane".into()));
        assert_eq!(running(&line("htop", ""), Some(&facts(&["htop"], &["sleep"], Some(archive)))), (Running::Unknown { program: "sleep".into() }, "/pane".into()));
        // A shell started by a start command (default-command, or `tmux new zsh`) goes by its archive
        assert_eq!(running(&line("htop", "zsh"), Some(&facts(&["htop"], &[], Some(archive)))), (Running::Typed { command: "htop".into() }, "/data".into()));
        // Started with a command, which started in /start
        assert_eq!(running(&line("sleep", "\"sleep 300\""), Some(&facts(&[], &[], None))), (Running::Started { command: "\"sleep 300\"".into() }, "/start".into()));
        // Shells without an archive: idle with no children, else unknown
        assert_eq!(running(&line("bash", ""), Some(&facts(&[], &[], None))), (Running::Idle, "/pane".into()));
        assert_eq!(running(&line("bash", ""), Some(&facts(&["python3"], &[], None))), (Running::Unknown { program: "python3".into() }, "/pane".into()));
        assert_eq!(running(&line("htop", ""), Some(&facts(&[], &[], None))), (Running::Unknown { program: "htop".into() }, "/pane".into()));
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
        assert_eq!(verdict(&jobs, &patterns), Verdict::Block("its shell has jobs that haven't ended: sleep 9; one whose line was hidden".into()));
        let job = pane(0, "/", Running::Background { jobs: vec![Some("sleep 9".into())] });
        assert_eq!(verdict(&job, &patterns), Verdict::Block("its shell has a job that hasn't ended: sleep 9".into()));
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
        let at_listing = [
            &format!("{at}: 1 session and 3 panes"),
            "         idle  work:0.0  /home/at",
            "    resumable  work:0.1  /data     rsync -a /data/ /backup/",
            "not resumable  work:3.0  /home/at  htop (doesn't match resumable_commands)",
        ]
        .join("\n");
        assert_eq!(listing(std::slice::from_ref(&server), TmuxPolicy::Resume, &patterns, false), [at_listing.clone()]);
        let colored = listing(std::slice::from_ref(&server), TmuxPolicy::Resume, &patterns, true).concat();
        assert!(colored.contains("\n         \x1b[2midle\x1b[0m  work:0.0") && colored.contains("\n    \x1b[32mresumable\x1b[0m  ") && colored.contains("\n\x1b[31mnot resumable\x1b[0m  "), "{colored}");
        assert!(listing(std::slice::from_ref(&server), TmuxPolicy::Block, &patterns, false)[0].starts_with(&format!("{at}: 1 session and 3 panes (tmux is \"block\", so the server blocks a reboot)\n")));
        assert!(listing(std::slice::from_ref(&server), TmuxPolicy::Block, &patterns, true)[0].starts_with(&format!("{at}: 1 session and 3 panes \x1b[31m(tmux is \"block\", so the server blocks a reboot)\x1b[0m\n")));
        // A server that couldn't be listed has just its line, even with
        // sessions (as a capture could, if edited).
        let unlisted = Server { error: Some("no such file".into()), ..server.clone() };
        assert_eq!(listing(std::slice::from_ref(&unlisted), TmuxPolicy::Resume, &patterns, true), [format!("{at}: \x1b[31mcouldn't be listed: no such file\x1b[0m")]);

        // The columns line up across servers: root's pane's location and
        // directory are padded as wide as at's, and its "resumable" as wide
        // as at's "not resumable".
        let mut root = Server { user: "root".into(), socket: "/tmp/tmux-0/default".into(), pid: 1, ..server.clone() };
        root.sessions[0].name = "s".into();
        root.sessions[0].windows.truncate(1);
        root.sessions[0].windows[0].panes = vec![pane(0, "/root", Running::Typed { command: "rsync -a /x/ /y/".into() })];
        let empty = Server { sessions: vec![], ..server.clone() };
        assert_eq!(
            listing(&[unlisted, root, empty, server], TmuxPolicy::Resume, &patterns, false),
            [
                format!("{at}: couldn't be listed: no such file"),
                "root's tmux server at /tmp/tmux-0/default (pid 1): 1 session and 1 pane\n    resumable  s:0.0     /root     rsync -a /x/ /y/".into(),
                format!("{at}: 0 sessions and 0 panes"),
                at_listing,
            ]
        );
    }

    #[test]
    fn serializes_running() {
        let json = |running: Running| serde_json::to_value(running).unwrap();
        assert_eq!(json(Running::Idle), serde_json::json!({"state": "idle"}));
        assert_eq!(json(Running::Typed { command: "htop".into() }), serde_json::json!({"state": "typed", "command": "htop"}));
        assert_eq!(json(Running::Background { jobs: vec![None] }), serde_json::json!({"state": "background", "jobs": [null]}));
        let mut json_server = serde_json::to_value(test_server()).unwrap();
        let server: Server = serde_json::from_value(json_server.clone()).unwrap();
        assert_eq!(server, test_server());
        // A capture saved before the umask was recorded
        json_server.as_object_mut().unwrap().remove("umask");
        assert_eq!(serde_json::from_value::<Server>(json_server).unwrap(), Server { umask: None, ..test_server() });
        assert!(is_umask("0022") && is_umask("777") && is_umask("0"));
        assert!(!is_umask("") && !is_umask("00022") && !is_umask("0o22") && !is_umask("8") && !is_umask("22; x"));
    }
}
