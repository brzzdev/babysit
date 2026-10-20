use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

pub const CALL_LIMIT: Duration = Duration::from_secs(60);
const WAIT_STEP: Duration = Duration::from_millis(10);

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    TimedOut(Limit),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Limit {
    CallLimit,
    Deadline,
}

/// Runs an external command to completion, killing it at the run's wall-clock `deadline` or once it
/// has run for the call limit, whichever comes first.
pub trait Exec {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        deadline: Option<SystemTime>,
    ) -> Result<Output, Error>;
}

pub struct SystemExec {
    call_limit: Duration,
}

impl Default for SystemExec {
    fn default() -> Self {
        Self {
            call_limit: CALL_LIMIT,
        }
    }
}

impl Exec for SystemExec {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        deadline: Option<SystemTime>,
    ) -> Result<Output, Error> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(Error::Io)?;
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let call_limit = Instant::now() + self.call_limit;
        loop {
            if let Some(status) = child.try_wait().map_err(Error::Io)? {
                return Ok(Output {
                    code: status.code(),
                    stdout: collect(stdout),
                    stderr: collect(stderr),
                });
            }
            let limit = if deadline.is_some_and(|deadline| SystemTime::now() >= deadline) {
                Limit::Deadline
            } else if Instant::now() >= call_limit {
                Limit::CallLimit
            } else {
                thread::sleep(WAIT_STEP);
                continue;
            };
            // The drain threads are left to finish on their own: a grandchild may still hold the pipes.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::TimedOut(limit));
        }
    }
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

fn collect(drained: JoinHandle<Vec<u8>>) -> String {
    String::from_utf8_lossy(&drained.join().unwrap_or_default()).into_owned()
}

/// A fake `Exec` that answers an ordered script of calls, asserting each call's command line.
#[cfg(test)]
pub struct Script {
    calls: std::cell::RefCell<std::collections::VecDeque<(String, Result<Output, Error>)>>,
}

#[cfg(test)]
impl Script {
    pub fn new(calls: impl IntoIterator<Item = (String, Result<Output, Error>)>) -> Self {
        Self {
            calls: std::cell::RefCell::new(calls.into_iter().collect()),
        }
    }
}

#[cfg(test)]
impl Exec for Script {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        _deadline: Option<SystemTime>,
    ) -> Result<Output, Error> {
        let command_line = [&[program], args].concat().join(" ");
        let (expected, result) = self
            .calls
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected call: {command_line}"));
        assert_eq!(command_line, expected);
        result
    }
}

#[cfg(test)]
impl Drop for Script {
    fn drop(&mut self) {
        if !thread::panicking() {
            assert!(self.calls.borrow().is_empty(), "calls left in the script");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOD_BYTES: usize = 1_000_000;

    fn sh(exec: &SystemExec, script: &str, deadline: Option<SystemTime>) -> Result<Output, Error> {
        exec.run("sh", &["-c", script], deadline)
    }

    #[test]
    fn returns_exit_code_and_both_streams() {
        let output = sh(
            &SystemExec::default(),
            "echo out; echo err >&2; exit 3",
            None,
        )
        .unwrap();

        assert_eq!(output.code, Some(3));
        assert_eq!(output.stdout, "out\n");
        assert_eq!(output.stderr, "err\n");
    }

    #[test]
    fn drains_a_child_flooding_both_streams() {
        let script =
            format!("head -c {FLOOD_BYTES} /dev/zero; head -c {FLOOD_BYTES} /dev/zero >&2");

        let output = sh(&SystemExec::default(), &script, None).unwrap();

        assert_eq!(output.code, Some(0));
        assert_eq!(output.stdout.len(), FLOOD_BYTES);
        assert_eq!(output.stderr.len(), FLOOD_BYTES);
    }

    #[test]
    fn kills_a_call_at_the_run_deadline() {
        let started = Instant::now();
        let deadline = SystemTime::now() + Duration::from_millis(200);

        let result = sh(&SystemExec::default(), "exec sleep 30", Some(deadline));

        assert!(matches!(result, Err(Error::TimedOut(Limit::Deadline))));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn kills_a_call_past_the_call_limit() {
        let exec = SystemExec {
            call_limit: Duration::from_millis(200),
        };
        let started = Instant::now();

        let result = sh(&exec, "exec sleep 30", None);

        assert!(matches!(result, Err(Error::TimedOut(Limit::CallLimit))));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn reports_a_missing_program_as_not_found() {
        let result = SystemExec::default().run("babysit-no-such-program", &[], None);

        assert!(matches!(result, Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound));
    }
}
