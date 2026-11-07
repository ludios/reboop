//! The tmux servers on a machine and what's in them: each server's
//! sessions, windows and panes, with every pane's working directory and
//! what it's running, as zsh-spy recorded it being typed.  A bounce can
//! bring all of that back after the reboot (see [`crate::resume`]), as
//! far as the machine's `resumable_commands` allow; what they don't allow
//! keeps the machine from going down.

use crate::config::TmuxPolicy;
use crate::human;
use crate::ssh::{QUICK, Session, shell_quote};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use regex::{Regex, RegexSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// How much of a command a [`Verdict`] shows.
const COMMAND_SHOWN: usize = 100;

/// How much of the end of a zsh-spy archive is read: plenty for the last
/// command typed (nothing is written while a foreground command runs) and
/// the background jobs still running, unless a job was started more than
/// this many bytes of records ago, which then goes unseen.
const ARCHIVE_TAIL: usize = 1 << 20;

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
    /// Compiles `patterns`, failing on one that isn't a regular expression.
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

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

/// What a pane is running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Running {
    /// Its shell is waiting at its prompt.
    Idle,
    /// Its shell is running a command line that was typed into it, as
    /// zsh-spy recorded it.
    Typed { command: String },
    /// The command tmux started it with in place of a shell, as tmux shows
    /// it, quoted for tmux's own command parser: e.g. `"sleep 300"` for a
    /// pane started with `tmux new-session 'sleep 300'`.
    Started { command: String },
    /// Its shell is at its prompt, but jobs the shell put in the background
    /// (with &) are still running, as zsh-spy recorded what they ran: `None`
    /// for one whose line was typed with a leading space (HIST_IGNORE_SPACE).
    Background { jobs: Vec<Option<String>> },
    /// It's running `program` (as tmux names it), but what was typed to
    /// start it isn't known: its shell has no zsh-spy archive, or the line
    /// was typed with a leading space, or the program replaced the shell.
    Unknown { program: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub index: u32,
    pub active: bool,
    /// The directory a typed command was typed in, or else the pane's
    /// current directory
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
    pub width: u32,
    pub height: u32,
    /// Its layout as tmux describes it, which `select-layout` takes back
    pub layout: String,
    pub panes: Vec<Pane>,
}

/// A tmux session.  (Not to be confused with an SSH [`Session`].)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxSession {
    pub name: String,
    pub windows: Vec<Window>,
}

/// A tmux server, and what's in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    /// Whose it is: the user name, or the numeric uid if the user has no
    /// name
    pub user: String,
    /// The path of its socket
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
        // Netid State Recv-Q Send-Q Local-Address Port Peer-Address Port
        let fields: Vec<&str> = socket.split_whitespace().collect();
        ensure!(fields.len() == 8 && fields[0] == "u_str", context());
        let pid = after_pid.split(',').next().unwrap_or_default().parse().with_context(context)?;
        servers.push((fields[4].to_string(), pid));
    }
    Ok(servers)
}

/// Separates the fields of the lines that [`capture_server`] has
/// `tmux list-panes` print: a control character that no name or path has.
const FIELD_SEPARATOR: char = '\x1f';

/// Those fields, as tmux calls them.
const PANE_FIELDS: [&str; 14] = [
    "session_name",
    "window_index",
    "window_name",
    "automatic-rename",
    "window_active",
    "window_width",
    "window_height",
    "window_layout",
    "pane_index",
    "pane_active",
    "pane_pid",
    "pane_current_path",
    "pane_current_command",
    "pane_start_command",
];

/// A line about a pane from `tmux list-panes`, as [`PANE_FIELDS`] has it
/// print them.
struct PaneLine {
    session: String,
    window_index: u32,
    window_name: String,
    automatic_rename: bool,
    window_active: bool,
    width: u32,
    height: u32,
    layout: String,
    pane_index: u32,
    pane_active: bool,
    /// The pane's process: its shell, or its start command's
    pid: u32,
    /// The pane's current directory
    path: String,
    /// The name of the program in the foreground, like "zsh" or "rsync"
    current_command: String,
    /// The command tmux started the pane with, if any (else a shell)
    start_command: String,
}

fn parse_pane_line(line: &str) -> Result<PaneLine> {
    let context = || format!("unexpected tmux list-panes line {line:?}");
    let fields: Vec<&str> = line.split(FIELD_SEPARATOR).collect();
    let [session, window_index, window_name, automatic_rename, window_active, width, height, layout, pane_index, pane_active, pid, path, current_command, start_command] =
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
    Ok(PaneLine {
        session: session.into(),
        window_index: number(window_index)?,
        window_name: window_name.into(),
        automatic_rename: flag(automatic_rename)?,
        window_active: flag(window_active)?,
        width: number(width)?,
        height: number(height)?,
        layout: layout.into(),
        pane_index: number(pane_index)?,
        pane_active: flag(pane_active)?,
        pid: number(pid)?,
        path: path.into(),
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
        /// None if the line was typed with a leading space
        command: Option<String>,
    },
    CommandEnd {
        id: String,
    },
    AsyncStart {
        /// The id of the command that started the job
        id: String,
        job: u64,
        /// None if the command's line was typed with a leading space
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

/// Parses the records in `tail`, the end of an archive, skipping lines that
/// don't parse: the first, which may be cut short, and any with bytes that
/// aren't UTF-8, which zsh-spy says to skip.
fn parse_records(tail: &str) -> Vec<Record> {
    tail.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
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
    let Some(start) = records.iter().rposition(|record| matches!(record, Record::CommandStart { .. })) else {
        return Ok((Running::Idle, path.into()));
    };
    let Record::CommandStart { id, shell_pid, cwd, command } = &records[start] else { unreachable!() };
    ensure!(shell_pid == &pid, "the archive that pid {pid} holds open has records of pid {shell_pid}");
    let ended = records[start + 1..].iter().any(|record| matches!(record, Record::CommandEnd { id: ended } if ended == id));
    if !ended {
        return Ok(match command {
            Some(command) => (Running::Typed { command: command.clone() }, cwd.clone()),
            None => (Running::Unknown { program: program.into() }, path.into()),
        });
    }
    let jobs = running_jobs(records);
    if jobs.is_empty() { Ok((Running::Idle, path.into())) } else { Ok((Running::Background { jobs }, path.into())) }
}

/// Programs that are shells, so a pane running one without a zsh-spy
/// archive is taken to be idle.
const SHELLS: &[&str] = &["zsh", "bash", "sh", "dash", "fish", "ksh"];

/// What the pane that `line` is about is running, given the end of its
/// shell's zsh-spy archive (`None` if there's no shell or archive), and the
/// directory to run it in again (see [`shell_running`]).
fn pane_running(line: &PaneLine, archive: Option<&str>) -> Result<(Running, String)> {
    if !line.start_command.is_empty() {
        return Ok((Running::Started { command: line.start_command.clone() }, line.path.clone()));
    }
    match archive {
        Some(tail) => shell_running(&parse_records(tail), line.pid, &line.current_command, &line.path),
        None if SHELLS.contains(&line.current_command.as_str()) => Ok((Running::Idle, line.path.clone())),
        None => Ok((Running::Unknown { program: line.current_command.clone() }, line.path.clone())),
    }
}

/// Reads the end of the zsh-spy archive of each shell among the panes'
/// processes (those of `lines` that have no start command): the
/// hist.*.jsonl file that the shell holds open.  Returns the tail by pid,
/// `None` for a shell without one.
fn read_archives(session: &mut Session, lines: &[PaneLine]) -> Result<HashMap<u32, Option<String>>> {
    let pids: Vec<String> = lines.iter().filter(|line| line.start_command.is_empty()).map(|line| line.pid.to_string()).collect();
    let mut archives = HashMap::new();
    if pids.is_empty() {
        return Ok(archives);
    }
    // A line per pid: the pid, and the base64 of the tail if there's an archive
    let script = format!(
        r#"for p in {}; do
    f=$(for l in /proc/$p/fd/*; do readlink "$l"; done 2>/dev/null | grep -m1 '/hist\..*\.jsonl$')
    if [ -n "$f" ]; then printf '%s %s\n' "$p" "$(tail -c {ARCHIVE_TAIL} -- "$f" | base64 -w0)"; else printf '%s\n' "$p"; fi
done"#,
        pids.join(" ")
    );
    let output = session.run_ok(&script, QUICK)?;
    for line in output.lines() {
        let context = || format!("unexpected archive line {:?}", human::truncate(line, 100));
        let (pid, tail) = line.split_once(' ').map_or((line, None), |(pid, tail)| (pid, Some(tail)));
        let pid = pid.parse().with_context(context)?;
        let tail = tail.map(|tail| BASE64.decode(tail).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())).transpose().with_context(context)?;
        archives.insert(pid, tail);
    }
    ensure!(archives.len() == pids.len(), "expected {} archive lines, got {}", pids.len(), archives.len());
    Ok(archives)
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
                sessions.push(TmuxSession { name: line.session.clone(), windows: vec![] });
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
                    width: line.width,
                    height: line.height,
                    layout: line.layout.clone(),
                    panes: vec![],
                });
                session.windows.last_mut().unwrap()
            }
        };
        window.panes.push(Pane { index: line.pane_index, active: line.pane_active, cwd, mountpoint, running });
    }
    sessions
}

/// Lists what's in the tmux server with pid `pid` at `socket`, or returns
/// `None` if it's gone.  If its sessions can't be listed, the server has an
/// `error` instead.
fn capture_server(session: &mut Session, socket: String, pid: u32) -> Result<Option<Server>> {
    let format: Vec<String> = PANE_FIELDS.iter().map(|field| format!("#{{{field}}}")).collect();
    let script = format!(
        "test -d /proc/{pid} || {{ echo gone; exit 0; }}\nuser=$(stat -c %U /proc/{pid}) || exit 1\nprintf '%s\\n' \"$user\"\nexec tmux -S {} list-panes -a -F {}",
        shell_quote(&socket),
        shell_quote(&format.join(&FIELD_SEPARATOR.to_string()))
    );
    let output = session.run(&script, QUICK)?;
    let stdout = output.stdout_text();
    if output.status == 0 && stdout == "gone\n" {
        return Ok(None);
    }
    let Some((user, listing)) = stdout.split_once('\n') else {
        bail!("couldn't tell whose tmux server pid {pid} is: {}", output.stderr_text());
    };
    let mut server = Server { user: user.into(), socket, pid, sessions: vec![], error: None };
    if output.status != 0 {
        server.error = Some(format!("tmux list-panes exited with status {}: {}", output.status, output.stderr_text()));
        return Ok(Some(server));
    }
    let lines: Vec<PaneLine> = listing.lines().map(parse_pane_line).collect::<Result<_>>()?;
    let archives = read_archives(session, &lines)?;
    let runnings: Vec<(Running, String)> =
        lines.iter().map(|line| pane_running(line, archives.get(&line.pid).and_then(Option::as_deref))).collect::<Result<_>>()?;
    let cwds: Vec<&str> = runnings.iter().map(|(_, cwd)| cwd.as_str()).collect();
    let mountpoints = mountpoints(session, &cwds)?;
    server.sessions = assemble(lines, runnings, mountpoints);
    Ok(Some(server))
}

/// Lists every tmux server on the machine (any user's, at any socket) and
/// what's in it.
pub fn capture(session: &mut Session) -> Result<Vec<Server>> {
    let listing = session.run_ok("ss -xlpH", QUICK)?;
    let mut servers = Vec::new();
    for (socket, pid) in parse_ss(&listing)? {
        if let Some(server) = capture_server(session, socket, pid)? {
            servers.push(server);
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

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Verdict::Idle => f.write_str("idle"),
            Verdict::Resume(command) => write!(f, "{command} (resumable)"),
            Verdict::Block(why) => f.write_str(why),
        }
    }
}

/// What's to become of `pane`, given the commands that `patterns` allow.
pub fn verdict(pane: &Pane, patterns: &CommandPatterns) -> Verdict {
    match &pane.running {
        Running::Idle => Verdict::Idle,
        Running::Typed { command } | Running::Started { command } => {
            if patterns.matches(command) {
                Verdict::Resume(summary(command))
            } else {
                Verdict::Block(format!("{} (doesn't match resumable_commands)", summary(command)))
            }
        }
        Running::Background { jobs } => {
            let texts: Vec<String> = jobs.iter().map(|job| job.as_deref().map_or("one whose line was typed with a leading space".into(), summary)).collect();
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
            blockers.push(format!("tmux: couldn't list {}'s server at {}: {error}", server.user, server.socket));
            continue;
        }
        match policy {
            TmuxPolicy::Block => blockers.push(format!("tmux: {}'s server at {} has {} (tmux is \"block\")", server.user, server.socket, server.counts())),
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

/// What's in `server`, for people: a line about the server, then one per
/// pane (indented) with its location, directory and [`Verdict`].  With
/// `policy` block, the server's line says that it blocks.
pub fn lines(server: &Server, policy: TmuxPolicy, patterns: &CommandPatterns) -> Vec<String> {
    let mut lines = vec![format!("{}'s tmux server at {} (pid {})", server.user, server.socket, server.pid)];
    if let Some(error) = &server.error {
        lines[0].push_str(&format!(": couldn't be listed: {error}"));
        return lines;
    }
    lines[0].push_str(&format!(": {}", server.counts()));
    if policy == TmuxPolicy::Block {
        lines[0].push_str(" (tmux is \"block\", so the server blocks a reboot)");
    }
    for (location, pane) in server.panes() {
        lines.push(format!("    {location} in {}: {}", pane.cwd, verdict(pane, patterns)));
    }
    lines
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn patterns(patterns: &[&str]) -> CommandPatterns {
        CommandPatterns::new(patterns.iter().map(|&pattern| pattern.into()).collect()).unwrap()
    }

    fn pane(index: u32, cwd: &str, running: Running) -> Pane {
        Pane { index, active: index == 0, cwd: cwd.into(), mountpoint: "/".into(), running }
    }

    /// A server with a session "work" of two windows: one with an idle pane
    /// and a pane running `rsync`, and one with a pane running `htop`.
    pub(crate) fn test_server() -> Server {
        let window = |index, name: Option<&str>, panes| Window {
            index,
            name: name.map(String::from),
            active: index == 0,
            width: 120,
            height: 40,
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
            sessions: vec![TmuxSession { name: "work".into(), windows: vec![window(0, None, vec![idle, rsync]), window(3, Some("top"), vec![htop])] }],
            error: None,
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
        assert_eq!(super::tests::patterns(&["a", "b"]).patterns(), ["a", "b"]);
    }

    #[test]
    fn parses_ss() {
        let output = "u_str LISTEN 0      4096                        /run/systemd/journal/stdout 12345 * 0 users:((\"systemd-journal\",pid=1,fd=42),(\"systemd\",pid=1,fd=37))\n\
                      u_str LISTEN 0      128                           /tmp/tmux-1000/default 55065616 * 0 users:((\"tmux: server\",pid=1769201,fd=6))\n\
                      u_str LISTEN 0      128                           /tmp/tmux-0/other 55065617 * 0 users:((\"tmux: server\",pid=99,fd=6))\n\
                      u_dgr UNCONN 0      0                          /run/systemd/notify 12346 * 0 users:((\"systemd\",pid=1,fd=12))\n";
        assert_eq!(parse_ss(output).unwrap(), [("/tmp/tmux-1000/default".to_string(), 1769201), ("/tmp/tmux-0/other".to_string(), 99)]);
        assert_eq!(parse_ss("").unwrap(), []);
        assert!(parse_ss("u_str LISTEN 0 128 /tmp/with space/sock 1 * 0 users:((\"tmux: server\",pid=99,fd=6))\n").is_err());
    }

    #[test]
    fn parses_pane_lines() {
        let fields = ["work", "0", "zsh", "1", "1", "120", "40", "{\"V\":2}", "1", "0", "4242", "/data", "rsync", ""];
        let line = parse_pane_line(&fields.join("\x1f")).unwrap();
        assert_eq!((line.session.as_str(), line.window_index, line.window_name.as_str(), line.automatic_rename, line.window_active), ("work", 0, "zsh", true, true));
        assert_eq!((line.width, line.height, line.layout.as_str(), line.pane_index, line.pane_active), (120, 40, "{\"V\":2}", 1, false));
        assert_eq!((line.pid, line.path.as_str(), line.current_command.as_str(), line.start_command.as_str()), (4242, "/data", "rsync", ""));
        assert!(parse_pane_line(&fields[..13].join("\x1f")).is_err());
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
        let running = |tail: &str| shell_running(&parse_records(tail), 4242, "zsh", "/pane").unwrap();
        // A cut-short first line, then a command still running
        let typed = "status\":0}\n{\"type\":\"command_start\",\"id\":\"s.1\",\"shell_pid\":4242,\"cwd\":\"/data\",\"command\":\"rsync -a /a /b\"}\n{\"type\":\"async_start\",\"id\":\"s.1\",\"job\":1,\"job_text\":\"x\"}\n";
        assert_eq!(running(typed), (Running::Typed { command: "rsync -a /a /b".into() }, "/data".into()));
        // One typed with a leading space
        let hidden = r#"{"type":"command_start","id":"s.1","shell_pid":4242,"cwd":"/data","hist_hidden":true,"command":null}"#;
        assert_eq!(running(hidden), (Running::Unknown { program: "zsh".into() }, "/pane".into()));
        // Nothing typed yet
        assert_eq!(running(r#"{"type":"session_start","shell_pid":4242}"#), (Running::Idle, "/pane".into()));
        assert_eq!(running(""), (Running::Idle, "/pane".into()));
        // A background job left running, with its end written before its start
        let after_jobs = records_after_jobs();
        assert_eq!(running(&after_jobs), (Running::Background { jobs: vec![Some("sleep 9".into())] }, "/pane".into()));
        let reordered = after_jobs.replace("\n{\"type\":\"async_end\",\"id\":\"s.2\",\"job\":2,\"status\":0}", "").replacen('\n', "\n{\"type\":\"async_end\",\"id\":\"s.2\",\"job\":2,\"status\":0}\n", 1);
        assert_eq!(running(&reordered), (Running::Background { jobs: vec![Some("sleep 9".into())] }, "/pane".into()));
        // Lost (disowned), then nothing left
        let lost = format!("{after_jobs}\n{{\"type\":\"async_lost\",\"id\":\"s.2\",\"job\":1,\"reason\":\"missing_from_job_table\"}}");
        assert_eq!(running(&lost), (Running::Idle, "/pane".into()));
        // A hidden job
        let hidden_job = format!("{after_jobs}\n{{\"type\":\"command_start\",\"id\":\"s.3\",\"shell_pid\":4242,\"cwd\":\"/tmp\",\"command\":null}}\n{{\"type\":\"async_start\",\"id\":\"s.3\",\"job\":1,\"job_text\":null}}\n{{\"type\":\"command_end\",\"id\":\"s.3\"}}");
        assert_eq!(running(&hidden_job), (Running::Background { jobs: vec![Some("sleep 9".into()), None] }, "/pane".into()));
        // The wrong shell's archive
        let other = r#"{"type":"command_start","id":"s.1","shell_pid":1,"cwd":"/","command":"x"}"#;
        assert!(shell_running(&parse_records(other), 4242, "zsh", "/pane").is_err());
    }

    #[test]
    fn tells_what_panes_run() {
        let line = |current_command: &str, start_command: &str| {
            let fields = ["work", "0", "zsh", "1", "1", "120", "40", "{}", "0", "1", "4242", "/pane", current_command, start_command];
            parse_pane_line(&fields.join("\x1f")).unwrap()
        };
        let started = pane_running(&line("sleep", "\"sleep 300\""), None).unwrap();
        assert_eq!(started, (Running::Started { command: "\"sleep 300\"".into() }, "/pane".into()));
        assert_eq!(pane_running(&line("zsh", ""), None).unwrap(), (Running::Idle, "/pane".into()));
        assert_eq!(pane_running(&line("htop", ""), None).unwrap(), (Running::Unknown { program: "htop".into() }, "/pane".into()));
        let archive = r#"{"type":"command_start","id":"s.1","shell_pid":4242,"cwd":"/data","command":"htop"}"#;
        assert_eq!(pane_running(&line("htop", ""), Some(archive)).unwrap(), (Running::Typed { command: "htop".into() }, "/data".into()));
    }

    #[test]
    fn assembles_sessions() {
        let line = |session: &str, window: &str, name: &str, automatic: &str, pane: &str, pid: &str| {
            let fields = [session, window, name, automatic, "0", "80", "24", "{}", pane, "0", pid, "/", "zsh", ""];
            parse_pane_line(&fields.join("\x1f")).unwrap()
        };
        let lines = vec![line("a", "0", "zsh", "1", "0", "1"), line("a", "0", "zsh", "1", "1", "2"), line("a", "2", "top", "0", "0", "3"), line("b", "0", "zsh", "1", "0", "4")];
        let runnings: Vec<_> = (1..=4).map(|n| (Running::Idle, format!("/{n}"))).collect();
        let sessions = assemble(lines, runnings, vec!["/".into(); 4]);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].windows.len(), 2);
        assert_eq!(sessions[0].windows[0].panes.iter().map(|pane| pane.cwd.as_str()).collect::<Vec<_>>(), ["/1", "/2"]);
        assert_eq!((sessions[0].windows[0].name.as_deref(), sessions[0].windows[1].name.as_deref()), (None, Some("top")));
        assert_eq!((sessions[1].name.as_str(), sessions[1].windows[0].panes[0].cwd.as_str()), ("b", "/4"));
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
        let jobs = pane(0, "/", Running::Background { jobs: vec![Some("sleep 9".into()), None] });
        assert_eq!(verdict(&jobs, &patterns), Verdict::Block("background jobs are still running: sleep 9; one whose line was typed with a leading space".into()));
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
        assert_eq!(blockers(TmuxPolicy::Block, &patterns, &[unlisted]), expected);
        assert_eq!(blockers(TmuxPolicy::Block, &patterns, &[]), Vec::<String>::new());
    }

    #[test]
    fn describes_servers() {
        let patterns = patterns(&["rsync .*"]);
        let server = test_server();
        assert_eq!(
            lines(&server, TmuxPolicy::Resume, &patterns),
            [
                "at's tmux server at /tmp/tmux-1000/default (pid 1234): 1 session and 3 panes",
                "    work:0.0 in /home/at: idle",
                "    work:0.1 in /data: rsync -a /data/ /backup/ (resumable)",
                "    work:3.0 in /home/at: htop (doesn't match resumable_commands)",
            ]
        );
        assert_eq!(lines(&server, TmuxPolicy::Block, &patterns)[0], "at's tmux server at /tmp/tmux-1000/default (pid 1234): 1 session and 3 panes (tmux is \"block\", so the server blocks a reboot)");
        let unlisted = Server { sessions: vec![], error: Some("no such file".into()), ..server };
        assert_eq!(lines(&unlisted, TmuxPolicy::Resume, &patterns), ["at's tmux server at /tmp/tmux-1000/default (pid 1234): couldn't be listed: no such file"]);
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
