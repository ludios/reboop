// Model-output: Claude Opus 5.5

//! Child processes whose output can be read as it arrives, without ever
//! blocking past a deadline.

use crate::deadline::Deadline;
use anyhow::{Context, Result, anyhow, bail};
use std::io::{ErrorKind, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How much of a child's stderr to keep; the rest is dropped.
const STDERR_LIMIT: usize = 64 * 1024;

/// Reads from `pipe` into `buffer`, returning how many bytes were read, or
/// `None` at EOF or on an error (other than being interrupted by a signal).
fn read_chunk(pipe: &mut impl Read, buffer: &mut [u8]) -> Option<usize> {
    loop {
        match pipe.read(buffer) {
            Ok(0) => return None,
            Ok(n) => return Some(n),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// A running child process with piped stdin, stdout and stderr.  Background
/// threads read its stdout (delivered in chunks) and collect its stderr.  The
/// process is killed when this is dropped.
pub struct ChildProcess {
    child: Child,
    program: String,
    /// Shared with the threads that write to it.
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    stdout: Receiver<Vec<u8>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Closed (never sent to) when stderr reaches EOF.
    stderr_closed: Receiver<()>,
}

impl ChildProcess {
    pub fn spawn(mut command: Command) -> Result<ChildProcess> {
        let program = command.get_program().to_string_lossy().into_owned();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run {program}"))?;

        let mut stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = vec![0; 64 * 1024];
            // Ends on EOF or a read error, either of which closes the channel.
            while let Some(n) = read_chunk(&mut stdout, &mut buffer) {
                if sender.send(buffer[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        let mut stderr_pipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_writer = Arc::clone(&stderr);
        let (closed_sender, stderr_closed) = mpsc::channel::<()>();
        thread::spawn(move || {
            let _closed_sender = closed_sender;
            let mut buffer = vec![0; 4096];
            while let Some(n) = read_chunk(&mut stderr_pipe, &mut buffer) {
                let mut collected = stderr_writer.lock().unwrap();
                let room = STDERR_LIMIT.saturating_sub(collected.len());
                collected.extend_from_slice(&buffer[..n.min(room)]);
            }
        });

        let stdin = child.stdin.take().map(|stdin| Arc::new(Mutex::new(stdin)));
        Ok(ChildProcess { stdin, child, program, stdout: receiver, stderr, stderr_closed })
    }

    /// Waits for the next chunk of stdout.  Returns `None` once stdout is
    /// closed, and an error if nothing arrives before `deadline`.
    pub fn read_stdout(&mut self, deadline: Deadline) -> Result<Option<Vec<u8>>> {
        match self.stdout.recv_timeout(deadline.remaining()) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(RecvTimeoutError::Disconnected) => Ok(None),
            Err(RecvTimeoutError::Timeout) => bail!("timed out waiting for output from {}", self.program),
        }
    }

    /// Writes `data` to the process's stdin, failing if that takes past
    /// `deadline` (because the process isn't reading).
    pub fn write_stdin(&mut self, data: &[u8], deadline: Deadline) -> Result<()> {
        let stdin = Arc::clone(self.stdin.as_ref().ok_or_else(|| anyhow!("stdin of {} is closed", self.program))?);
        let data = data.to_vec();
        let (sender, receiver) = mpsc::channel();
        // The thread may block until the process dies, if it never reads.
        thread::spawn(move || {
            let mut stdin = stdin.lock().unwrap();
            let _ = sender.send(stdin.write_all(&data).and_then(|()| stdin.flush()));
        });
        match receiver.recv_timeout(deadline.remaining()) {
            Ok(result) => result.with_context(|| format!("failed to write to {}", self.program)),
            Err(_) => bail!("timed out writing to {}", self.program),
        }
    }

    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// What the process has written to stderr so far, lossily decoded.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).trim_end().to_string()
    }

    /// Waits for the process to exit, killing it if it's still running at
    /// `deadline`.
    pub fn wait(&mut self, deadline: Deadline) -> Result<ExitStatus> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if deadline.has_passed() {
                self.kill();
                bail!("timed out waiting for {} to exit", self.program);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the process to exit and then briefly for the rest of its
    /// stderr, returning both.
    pub fn finish(&mut self, deadline: Deadline) -> Result<(ExitStatus, String)> {
        let status = self.wait(deadline)?;
        // A grandchild could keep stderr open, so don't wait long.
        let _ = self.stderr_closed.recv_timeout(Duration::from_secs(1).min(deadline.remaining()));
        Ok((status, self.stderr()))
    }

    pub fn kill(&mut self) {
        // Errors mean it already exited, which is what we want anyway.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Runs `command` to completion with `stdin` as its input, killing it if it
/// hasn't finished by `deadline`.
pub fn run(command: Command, stdin: &[u8], deadline: Deadline) -> Result<Output> {
    let mut child = ChildProcess::spawn(command)?;
    // A process may exit without reading its input, e.g. over a bad
    // argument; its status and stderr tell why.
    if let Err(error) = child.write_stdin(stdin, deadline) {
        if !error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == ErrorKind::BrokenPipe) {
            return Err(error);
        }
    }
    child.close_stdin();
    let mut stdout = Vec::new();
    while let Some(chunk) = child.read_stdout(deadline)? {
        stdout.extend_from_slice(&chunk);
    }
    let (status, stderr) = child.finish(deadline)?;
    Ok(Output { status, stdout, stderr: stderr.into_bytes() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn run_collects_output() {
        let output = run(sh("cat; echo err >&2; exit 3"), b"hello", Deadline::after(Duration::from_secs(10))).unwrap();
        assert_eq!(output.stdout, b"hello");
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn run_tolerates_a_child_that_exits_without_reading() {
        let output = run(sh("echo bad argument >&2; exit 3"), &vec![0; 1 << 20], Deadline::after(Duration::from_secs(5))).unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stderr, b"bad argument");
    }

    #[test]
    fn run_kills_a_child_that_doesnt_read_at_deadline() {
        let started = std::time::Instant::now();
        let input = vec![0; 1 << 20];
        let error = run(sh("sleep 10"), &input, Deadline::after(Duration::from_millis(200))).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn run_kills_at_deadline() {
        let started = std::time::Instant::now();
        let error = run(sh("sleep 10"), b"", Deadline::after(Duration::from_millis(200))).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
