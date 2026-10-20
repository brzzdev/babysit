use std::io::Write;
use std::time::{Duration, SystemTime};

use crate::cli::Args;
use crate::clock::{Clock, rfc3339};
use crate::diff::diff;
use crate::envelope::{self, Envelope, ErrorKind, Outcome, Summary};
use crate::forge::{Backend, Snapshot, Target};
use crate::snapshot_file::{Entry, OpenError, SnapshotFile};

/// A usage error found after argument parsing, such as an unreadable snapshot file.
#[derive(Debug)]
pub struct UsageError(pub String);

/// Watches the target until it wakes, times out or fails, printing the envelope and saving what it
/// saw. Returns the exit code.
pub fn run(
    args: &Args,
    backend: &impl Backend,
    clock: &impl Clock,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<u8, UsageError> {
    let deadline = args.timeout.map(|timeout| clock.now() + timeout);
    let call_deadline = deadline.filter(|_| args.timeout != Some(Duration::ZERO));
    let mut file = match SnapshotFile::open(&args.snapshot) {
        Ok(file) => file,
        Err(OpenError::Invalid(message)) => return Err(UsageError(message)),
        Err(OpenError::LockHeld(lock_path)) => {
            let message = format!("{} is held by another babysit run", lock_path.display());
            return Ok(report(
                stdout,
                &Envelope::failed(local_error(ErrorKind::LockHeld, message)),
            ));
        }
        Err(OpenError::Io(error)) => {
            let message = format!("{}: {error}", args.snapshot.display());
            return Ok(report(
                stdout,
                &Envelope::failed(local_error(ErrorKind::SnapshotIo, message)),
            ));
        }
    };
    let target = match backend.resolve(&args.target, call_deadline) {
        Ok(target) => target,
        Err(failure) => {
            return Ok(report(
                stdout,
                &Envelope::from_failure(failure, &args.target.given),
            ));
        }
    };
    let mut snapshot = file.entry(&target.url).map(|entry| entry.snapshot.clone());
    let mut evaluated = false;
    let mut envelope = loop {
        let started = clock.now();
        let poll = match backend.poll(&target, call_deadline) {
            Ok(poll) => poll,
            Err(failure) => break Envelope::from_failure(failure, &target.url),
        };
        evaluated = true;
        match &snapshot {
            None => {
                if let Err(error) = file.save(entry(&target, poll.clone())) {
                    let message = format!("{}: {error}", file.path().display());
                    let error = local_error(ErrorKind::SnapshotIo, message);
                    return Ok(report(stdout, &Envelope::failed(error)));
                }
                snapshot = Some(poll);
            }
            Some(baseline) => {
                let (events, next) = diff(baseline, poll);
                snapshot = Some(next);
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
    let saved = snapshot.filter(|_| evaluated);
    envelope.targets = saved
        .iter()
        .map(|snapshot| Summary::new(&target.url, snapshot))
        .collect();
    if envelope.write(stdout).is_err() {
        return Ok(1);
    }
    if let Some(snapshot) = saved
        && let Err(error) = file.save(entry(&target, snapshot))
    {
        let _ = writeln!(
            stderr,
            "babysit: could not save {}: {error}",
            file.path().display()
        );
        return Ok(1);
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

fn local_error(kind: ErrorKind, message: String) -> envelope::Error {
    envelope::Error {
        kind,
        forge: None,
        target: None,
        message,
    }
}

fn report(stdout: &mut impl Write, envelope: &Envelope) -> u8 {
    match envelope.write(stdout) {
        Ok(()) => envelope.exit_code(),
        Err(_) => 1,
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
    use std::sync::{MutexGuard, PoisonError};
    use std::time::UNIX_EPOCH;

    use serde_json::{Value, json};

    use super::*;
    use crate::envelope::Forge;
    use crate::exec::SPAWN_GUARD;
    use crate::forge::{Account, Comment, Failure, Poll, TargetState};
    use crate::reference::{self, Reference};

    const URL: &str = "https://github.com/o/r/pull/1";
    const NODE_ID: &str = "PR_1";
    const STARTED: u64 = 1_792_523_957;

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

    /// A fresh directory under the system temp dir, removed on drop.
    struct Sandbox {
        dir: PathBuf,
        _spawn_guard: MutexGuard<'static, ()>,
    }

    impl Sandbox {
        fn new(name: &str) -> Self {
            let spawn_guard = SPAWN_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
            let dir = std::env::temp_dir().join(format!("babysit-{}-{name}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                dir,
                _spawn_guard: spawn_guard,
            }
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

    fn comment(id: &str) -> Comment {
        Comment {
            actor: Some(Account {
                bot: None,
                login: "octocat".to_owned(),
            }),
            at: "2026-10-20T19:00:00Z".to_owned(),
            body: format!("comment {id}"),
            id: id.to_owned(),
            url: format!("{URL}#issuecomment-{id}"),
        }
    }

    fn poll(ids: &[&str]) -> Poll {
        Poll {
            comments: ids.iter().map(|id| comment(id)).collect(),
            head: "abc123".to_owned(),
            state: TargetState::Open,
        }
    }

    fn returns(poll: Poll) -> Step {
        Box::new(move || Ok(poll))
    }

    fn args(snapshot: &Path, timeout: Option<u64>) -> Args {
        Args {
            target: reference::parse(URL).unwrap(),
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
        let code = run(args, &backend, &clock, stdout, &mut stderr);
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
        for contents in ["not json", r#"{"version": 2, "targets": {}}"#] {
            fs::write(sandbox.snapshot(), contents).unwrap();

            let (code, _) = watch_into(&args(&sandbox.snapshot(), None), vec![], &mut Vec::new());

            assert!(code.is_err(), "{contents}");
            assert_eq!(fs::read_to_string(sandbox.snapshot()).unwrap(), contents);
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
