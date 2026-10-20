use std::io::Write;
use std::time::SystemTime;

use crate::cli::Args;
use crate::clock::{Clock, rfc3339};
use crate::diff::diff;
use crate::envelope::{self, EXIT_ERROR, Envelope, ErrorKind, Outcome, Summary};
use crate::exec::Exec;
use crate::forge::{Backend, Snapshot, Target};
use crate::snapshot_file::{Entry, OpenError, SnapshotFile};
use crate::target::{self, Locator};

/// A usage error found after argument parsing, such as an unreadable snapshot file.
#[derive(Debug)]
pub struct UsageError(pub String);

/// Watches the target until it wakes, times out or fails; prints, saves and returns the exit code.
pub fn run(
    args: &Args,
    backend: &impl Backend,
    exec: &impl Exec,
    clock: &impl Clock,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<u8, UsageError> {
    let now = clock.now();
    let reach = |flag: &str, duration| {
        now.checked_add(duration).ok_or_else(|| {
            UsageError(format!(
                "{flag} reaches past the latest time the clock can hold"
            ))
        })
    };
    reach("--interval", args.interval)?;
    let deadline = args
        .timeout
        .map(|timeout| reach("--timeout", timeout))
        .transpose()?;
    // `--timeout 0` polls once, so its already-passed deadline must not kill that poll.
    let call_deadline = args
        .timeout
        .filter(|timeout| !timeout.is_zero())
        .and(deadline);
    let snapshot_path = args.snapshot.display();
    let mut file = match SnapshotFile::open(&args.snapshot) {
        Ok(file) => file,
        Err(OpenError::Invalid(message)) => return Err(UsageError(message)),
        Err(OpenError::LockHeld) => {
            let message = format!("{snapshot_path} is locked by another babysit run");
            return Ok(fail_locally(stdout, ErrorKind::LockHeld, message));
        }
        Err(OpenError::Io(error)) => {
            let message = format!("{snapshot_path}: {error}");
            return Ok(fail_locally(stdout, ErrorKind::SnapshotIo, message));
        }
    };
    let given = args.target.given();
    let reference = match Locator::new(exec, call_deadline).locate(&args.target) {
        Ok(reference) => reference,
        Err(error) => return report_unlocated(stdout, given, error),
    };
    let target = match backend.resolve(&reference, call_deadline) {
        Ok(target) => target,
        Err(failure) => {
            return Ok(report(stdout, &Envelope::from_failure(failure, given)));
        }
    };
    let stored = file.entry(&target.url).map(|entry| entry.snapshot.clone());
    let mut polled: Option<Snapshot> = None;
    let mut envelope = loop {
        let started = clock.now();
        let poll = match backend.poll(&target, call_deadline) {
            Ok(poll) => poll,
            Err(failure) => break Envelope::from_failure(failure, &target.url),
        };
        match polled.as_ref().or(stored.as_ref()) {
            None => {
                if let Err(error) = file.save(entry(&target, poll.clone())) {
                    let message = format!("{snapshot_path}: {error}");
                    return Ok(fail_locally(stdout, ErrorKind::SnapshotIo, message));
                }
                polled = Some(poll);
            }
            Some(baseline) => {
                let (events, next) = diff(baseline, poll);
                polled = Some(next);
                if !events.is_empty() {
                    let observed_at = rfc3339(clock.now());
                    let events = events.into_iter().map(|event| envelope::Event {
                        seen: event,
                        target: target.url.clone(),
                        observed_at: observed_at.clone(),
                    });
                    break Envelope {
                        events: events.collect(),
                        ..Envelope::ended(Outcome::Woke)
                    };
                }
            }
        }
        if !wait(clock, started + args.interval, deadline) {
            break Envelope::ended(Outcome::TimedOut);
        }
    };
    envelope.targets = polled
        .iter()
        .map(|snapshot| Summary::new(&target.url, snapshot))
        .collect();
    if envelope.write(stdout).is_err() {
        return Ok(EXIT_ERROR);
    }
    if let Some(snapshot) = polled
        && let Err(error) = file.save(entry(&target, snapshot))
    {
        let _ = writeln!(stderr, "babysit: could not save {snapshot_path}: {error}");
        return Ok(EXIT_ERROR);
    }
    Ok(envelope.exit_code())
}

/// Sleeps until the next poll is due, returning `false` once the deadline has passed instead.
fn wait(clock: &impl Clock, next_poll: SystemTime, deadline: Option<SystemTime>) -> bool {
    clock.sleep_until(deadline.map_or(next_poll, |deadline| deadline.min(next_poll)));
    deadline.is_none_or(|deadline| clock.now() < deadline)
}

fn entry(target: &Target, snapshot: Snapshot) -> Entry {
    Entry {
        forge_id: target.forge_id.clone(),
        url: target.url.clone(),
        snapshot,
    }
}

fn fail_locally(stdout: &mut impl Write, kind: ErrorKind, message: String) -> u8 {
    let error = envelope::Error {
        kind,
        forge: None,
        target: None,
        message,
    };
    report(stdout, &Envelope::failed(error))
}

fn report_unlocated(
    stdout: &mut impl Write,
    given: &str,
    error: target::Error,
) -> Result<u8, UsageError> {
    let envelope = match error {
        target::Error::Fatal { kind, message } => Envelope::failed(envelope::Error {
            kind,
            forge: None,
            target: Some(given.to_owned()),
            message,
        }),
        target::Error::TimedOut => Envelope::ended(Outcome::TimedOut),
        target::Error::Usage(message) => return Err(UsageError(message)),
    };
    Ok(report(stdout, &envelope))
}

fn report(stdout: &mut impl Write, envelope: &Envelope) -> u8 {
    match envelope.write(stdout) {
        Ok(()) => envelope.exit_code(),
        Err(_) => EXIT_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::fs::{self, File};
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::time::{Duration, UNIX_EPOCH};

    use serde_json::{Value, json};

    use super::*;
    use crate::envelope::Forge;
    use crate::exec::{Output, Script};
    use crate::forge::{Failure, Poll, poll};
    use crate::reference::{self, Reference};

    const NODE_ID: &str = "PR_1";
    const STARTED: u64 = 1_792_523_957;
    const URL: &str = "https://github.com/o/r/pull/1";

    struct FakeClock(Cell<SystemTime>);

    impl Clock for FakeClock {
        fn now(&self) -> SystemTime {
            self.0.get()
        }

        fn sleep_until(&self, until: SystemTime) {
            self.0.set(self.0.get().max(until));
        }
    }

    type Step = Box<dyn FnOnce() -> Result<Poll, Failure>>;

    struct FakeBackend {
        polls: RefCell<VecDeque<Step>>,
    }

    impl Backend for FakeBackend {
        fn resolve(&self, _: &Reference, _: Option<SystemTime>) -> Result<Target, Failure> {
            Ok(Target {
                forge_id: NODE_ID.to_owned(),
                host: "github.com".to_owned(),
                url: URL.to_owned(),
            })
        }

        fn poll(&self, _: &Target, _: Option<SystemTime>) -> Result<Poll, Failure> {
            let step = self.polls.borrow_mut().pop_front();
            step.expect("unexpected poll")()
        }
    }

    impl Drop for FakeBackend {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                assert!(self.polls.borrow().is_empty(), "polls left in the script");
            }
        }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Sandbox {
        dir: PathBuf,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("babysit-{}-{name}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn snapshot(&self) -> PathBuf {
            self.dir.join("snapshot.json")
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn returns(poll: Poll) -> Step {
        Box::new(move || Ok(poll))
    }

    fn args(snapshot: &Path, timeout: Option<u64>) -> Args {
        Args {
            target: reference::parse_name(URL).unwrap(),
            snapshot: snapshot.to_owned(),
            interval: Duration::from_secs(30),
            timeout: timeout.map(Duration::from_secs),
        }
    }

    fn seed(snapshot: &Path, ids: &[&str]) {
        let mut file = SnapshotFile::open(snapshot).unwrap();
        file.save(Entry {
            forge_id: NODE_ID.to_owned(),
            url: URL.to_owned(),
            snapshot: poll(ids),
        })
        .unwrap();
    }

    fn watch_into(
        args: &Args,
        polls: Vec<Step>,
        stdout: &mut impl Write,
    ) -> (Result<u8, UsageError>, String) {
        let clock = FakeClock(Cell::new(UNIX_EPOCH + Duration::from_secs(STARTED)));
        let backend = FakeBackend {
            polls: RefCell::new(polls.into()),
        };
        let mut stderr = Vec::new();
        let code = run(
            args,
            &backend,
            &Script::new([]),
            &clock,
            stdout,
            &mut stderr,
        );
        (code, String::from_utf8(stderr).unwrap())
    }

    fn watch(args: &Args, polls: Vec<Step>) -> (u8, Value) {
        let mut stdout = Vec::new();
        let (code, stderr) = watch_into(args, polls, &mut stdout);
        assert_eq!(stderr, "");
        (code.unwrap(), serde_json::from_slice(&stdout).unwrap())
    }

    fn saved(snapshot: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(snapshot).unwrap()).unwrap()
    }

    fn summary() -> Value {
        json!({
            "target": URL,
            "state": "open",
            "head": "abc123",
            "colour": null,
            "merge": null,
            "unresolved_threads": null,
        })
    }

    fn timed_out(targets: &[Value]) -> Value {
        json!({ "outcome": "timed_out", "events": [], "targets": targets, "error": null })
    }

    #[test]
    fn a_first_run_with_timeout_zero_saves_the_baseline_and_times_out() {
        let sandbox = Sandbox::new("first-run");

        let (code, envelope) = watch(
            &args(&sandbox.snapshot(), Some(0)),
            vec![returns(poll(&["1"]))],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[summary()]));
        assert_eq!(
            saved(&sandbox.snapshot()),
            json!({
                "version": 1,
                "targets": {
                    URL: {
                        "forge_id": NODE_ID,
                        "url": URL,
                        "snapshot": {
                            "comments": [{
                                "actor": { "bot": null, "login": "octocat" },
                                "at": "2026-10-20T19:00:00Z",
                                "body": "comment 1",
                                "id": "1",
                                "url": format!("{URL}#issuecomment-1"),
                            }],
                            "head": "abc123",
                            "state": "open",
                        },
                    },
                },
            }),
        );
    }

    #[test]
    fn a_first_run_saves_its_baseline_before_polling_again() {
        let sandbox = Sandbox::new("baseline-at-once");
        let snapshot = sandbox.snapshot();
        let check = Box::new(move || {
            assert_eq!(
                saved(&snapshot)["targets"][URL]["snapshot"],
                json!(poll(&["1"]))
            );
            Err(Failure::TimedOut)
        });

        let (code, envelope) = watch(
            &args(&sandbox.snapshot(), Some(600)),
            vec![returns(poll(&["1"])), check],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[summary()]));
    }

    #[test]
    fn a_later_run_wakes_on_a_new_comment_then_saves() {
        let sandbox = Sandbox::new("wake");
        seed(&sandbox.snapshot(), &["1"]);

        let (code, envelope) = watch(
            &args(&sandbox.snapshot(), None),
            vec![returns(poll(&["1"])), returns(poll(&["1", "2"]))],
        );

        assert_eq!(code, 0);
        assert_eq!(
            envelope,
            json!({
                "outcome": "woke",
                "events": [{
                    "kind": "comment.created",
                    "comment_id": "2",
                    "body": "comment 2",
                    "url": format!("{URL}#issuecomment-2"),
                    "at": "2026-10-20T19:00:00Z",
                    "actor": { "bot": null, "login": "octocat" },
                    "target": URL,
                    "observed_at": "2026-10-20T19:19:47Z",
                }],
                "targets": [summary()],
                "error": null,
            }),
        );
        assert_eq!(
            saved(&sandbox.snapshot())["targets"][URL]["snapshot"],
            json!(poll(&["1", "2"]))
        );
    }

    #[test]
    fn timeout_zero_against_a_baseline_polls_once() {
        let sandbox = Sandbox::new("timeout-zero");
        seed(&sandbox.snapshot(), &["1"]);
        let once = args(&sandbox.snapshot(), Some(0));

        let (quiet, _) = watch(&once, vec![returns(poll(&["1"]))]);
        let (woke, envelope) = watch(&once, vec![returns(poll(&["1", "2"]))]);

        assert_eq!(quiet, 3);
        assert_eq!(woke, 0);
        assert_eq!(envelope["events"][0]["comment_id"], "2");
    }

    #[test]
    fn the_timeout_ends_the_run_at_the_deadline() {
        let sandbox = Sandbox::new("timeout");
        seed(&sandbox.snapshot(), &["1"]);

        let (code, envelope) = watch(
            &args(&sandbox.snapshot(), Some(45)),
            vec![returns(poll(&["1"])), returns(poll(&["1"]))],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[summary()]));
    }

    #[test]
    fn a_call_killed_at_the_deadline_times_out_leaving_the_baseline() {
        let sandbox = Sandbox::new("killed");
        seed(&sandbox.snapshot(), &["1"]);
        let before = saved(&sandbox.snapshot());

        let (code, envelope) = watch(
            &args(&sandbox.snapshot(), Some(600)),
            vec![Box::new(|| Err(Failure::TimedOut))],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[]));
        assert_eq!(saved(&sandbox.snapshot()), before);
    }

    #[test]
    fn a_fatal_failure_reports_the_error() {
        let sandbox = Sandbox::new("fatal");
        let failure = Box::new(|| {
            Err(Failure::Fatal {
                forge: Forge::GitHub,
                kind: ErrorKind::Auth,
                message: "gh auth login".to_owned(),
            })
        });

        let (code, envelope) = watch(&args(&sandbox.snapshot(), None), vec![failure]);

        assert_eq!(code, 1);
        assert_eq!(
            envelope["error"],
            json!({ "kind": "auth", "forge": "github", "target": URL, "message": "gh auth login" }),
        );
    }

    #[test]
    fn an_unclaimed_host_is_auth_naming_the_target_as_given() {
        let sandbox = Sandbox::new("unclaimed");
        let given = "https://forge.example.com/o/r/pull/1";
        let args = Args {
            target: reference::parse_name(given).unwrap(),
            ..args(&sandbox.snapshot(), None)
        };
        let exec = Script::new([
            (
                "gh auth token --hostname forge.example.com".to_owned(),
                Ok(Output::exited(1, "")),
            ),
            (
                "glab config get token --host forge.example.com".to_owned(),
                Ok(Output::exited(1, "")),
            ),
        ]);
        let clock = FakeClock(Cell::new(UNIX_EPOCH + Duration::from_secs(STARTED)));
        let backend = FakeBackend {
            polls: RefCell::default(),
        };
        let mut stdout = Vec::new();

        let code = run(&args, &backend, &exec, &clock, &mut stdout, &mut Vec::new());

        let envelope: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(code.unwrap(), 1);
        assert_eq!(
            envelope["error"],
            json!({
                "kind": "auth",
                "forge": null,
                "target": given,
                "message": "neither gh nor glab holds a token for forge.example.com",
            }),
        );
        assert!(!sandbox.snapshot().exists());
    }

    #[test]
    fn a_duration_past_the_clocks_reach_is_a_usage_error_before_any_write() {
        let sandbox = Sandbox::new("overflow");
        let mut too_long = args(&sandbox.snapshot(), Some(u64::MAX));

        let (timeout, _) = watch_into(&too_long, vec![], &mut Vec::new());
        too_long.timeout = None;
        too_long.interval = Duration::from_secs(u64::MAX);
        let (interval, _) = watch_into(&too_long, vec![], &mut Vec::new());

        assert!(timeout.is_err());
        assert!(interval.is_err());
        assert_eq!(fs::read_dir(&sandbox.dir).unwrap().count(), 0);
    }

    #[test]
    fn a_held_lock_is_lock_held() {
        let sandbox = Sandbox::new("lock");
        let lock = File::create(sandbox.dir.join("snapshot.json.lock")).unwrap();
        lock.try_lock().unwrap();

        let (code, envelope) = watch(&args(&sandbox.snapshot(), None), vec![]);

        assert_eq!(code, 1);
        assert_eq!(envelope["outcome"], "error");
        assert_eq!(envelope["error"]["kind"], "lock_held");
    }

    #[test]
    fn an_unreadable_or_other_version_snapshot_file_is_a_usage_error() {
        let sandbox = Sandbox::new("usage");
        for contents in [
            &b"not json"[..],
            br#"{"version": 2, "targets": {}}"#,
            b"\xff",
        ] {
            fs::write(sandbox.snapshot(), contents).unwrap();

            let (code, _) = watch_into(&args(&sandbox.snapshot(), None), vec![], &mut Vec::new());

            assert!(code.is_err(), "{contents:?}");
            assert_eq!(fs::read(sandbox.snapshot()).unwrap(), contents);
        }
    }

    #[test]
    fn a_failed_stdout_write_exits_1_without_saving() {
        let sandbox = Sandbox::new("stdout");
        seed(&sandbox.snapshot(), &["1"]);
        let before = saved(&sandbox.snapshot());

        let (code, stderr) = watch_into(
            &args(&sandbox.snapshot(), None),
            vec![returns(poll(&["1", "2"]))],
            &mut FailingWriter,
        );

        assert_eq!(code.unwrap(), 1);
        assert_eq!(stderr, "");
        assert_eq!(saved(&sandbox.snapshot()), before);
    }

    #[test]
    fn a_save_failing_after_a_printed_wake_exits_1_with_one_stderr_line() {
        let sandbox = Sandbox::new("save");
        seed(&sandbox.snapshot(), &["1"]);
        fs::set_permissions(&sandbox.dir, fs::Permissions::from_mode(0o555)).unwrap();
        let mut stdout = Vec::new();

        let (code, stderr) = watch_into(
            &args(&sandbox.snapshot(), None),
            vec![returns(poll(&["1", "2"]))],
            &mut stdout,
        );

        let envelope: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(code.unwrap(), 1);
        assert_eq!(envelope["outcome"], "woke");
        assert_eq!(stderr.lines().count(), 1);
        assert!(stderr.starts_with("babysit: could not save "), "{stderr}");
    }
}
