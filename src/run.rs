use std::io::{self, Write};
use std::time::SystemTime;

use crate::cli::Args;
use crate::clock::{Clock, rfc3339};
use crate::diff::diff;
use crate::envelope::{self, EXIT_ERROR, Envelope, ErrorKind, Outcome, Summary};
use crate::exec::Exec;
use crate::forge::{Backend, Failure, Snapshot, Target};
use crate::snapshot_file::{Entry, OpenError, SnapshotFile};
use crate::target::{self, Locator};

/// A usage error found after argument parsing, such as an unreadable snapshot file.
#[derive(Debug)]
pub struct UsageError(pub String);

/// Watches the targets until one wakes, times out or fails; prints, saves and returns the exit code.
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
    let targets = match Locator::new(exec, call_deadline).resolve(backend, &args.targets) {
        Ok(targets) => targets,
        Err((given, error)) => return report_unresolved(stdout, given, error),
    };
    let mut tracked: Vec<_> = targets
        .into_iter()
        .map(|target| Tracked {
            evaluated: false,
            polled: None,
            stored: target::baseline(&mut file, &target),
            target,
        })
        .collect();
    let mut write_failed = false;
    let mut envelope = loop {
        let started = clock.now();
        match poll_round(&mut tracked, backend, clock, &mut file, call_deadline) {
            Ok(Some(ended)) => break ended,
            Ok(None) => {}
            Err(error) => {
                write_failed = true;
                let message = format!("{snapshot_path}: {error}");
                break local_failure(ErrorKind::SnapshotIo, message);
            }
        }
        if !wait(clock, started + args.interval, deadline) {
            break Envelope::ended(Outcome::TimedOut);
        }
    };
    envelope.targets = tracked
        .iter()
        .filter_map(|tracked| {
            let snapshot = tracked.polled.as_ref()?;
            Some(Summary::new(&tracked.target.url, snapshot))
        })
        .collect();
    if envelope.write(stdout).is_err() {
        return Ok(EXIT_ERROR);
    }
    // The envelope has already reported the write that failed.
    if write_failed {
        return Ok(envelope.exit_code());
    }
    let woke = envelope.outcome == Outcome::Woke;
    let entries: Vec<_> = tracked
        .into_iter()
        .filter(|tracked| tracked.evaluated || !woke)
        .filter_map(|tracked| Some(entry(&tracked.target, tracked.polled?)))
        .collect();
    if !entries.is_empty()
        && let Err(error) = file.save(entries)
    {
        let _ = writeln!(stderr, "babysit: could not save {snapshot_path}: {error}");
        return Ok(EXIT_ERROR);
    }
    Ok(envelope.exit_code())
}

/// A target through the run: the baseline the file held for it, then its last evaluated poll.
struct Tracked {
    /// Whether the latest round polled it successfully.
    evaluated: bool,
    polled: Option<Snapshot>,
    stored: Option<Snapshot>,
    target: Target,
}

/// Polls each target once, saving a first baseline at once; returns a wake, else the round's failure.
fn poll_round(
    tracked: &mut [Tracked],
    backend: &impl Backend,
    clock: &impl Clock,
    file: &mut SnapshotFile,
    call_deadline: Option<SystemTime>,
) -> io::Result<Option<Envelope>> {
    let mut events = Vec::new();
    let mut failed = None;
    for tracked in tracked.iter_mut() {
        tracked.evaluated = false;
    }
    for tracked in tracked {
        let poll = match backend.poll(&tracked.target, call_deadline) {
            Ok(poll) => poll,
            // Every later call would be killed at once by the passed deadline.
            Err(Failure::TimedOut) => {
                failed = Some(Envelope::ended(Outcome::TimedOut));
                break;
            }
            Err(failure) => {
                failed.get_or_insert(Envelope::from_failure(failure, &tracked.target.url));
                continue;
            }
        };
        tracked.evaluated = true;
        let Some(baseline) = tracked.polled.as_ref().or(tracked.stored.as_ref()) else {
            let first = entry(&tracked.target, poll.clone());
            tracked.polled = Some(poll);
            file.save([first])?;
            continue;
        };
        let (seen, next) = diff(baseline, poll);
        tracked.polled = Some(next);
        if seen.is_empty() {
            continue;
        }
        let observed_at = rfc3339(clock.now());
        events.extend(seen.into_iter().map(|event| envelope::Event {
            seen: event,
            target: tracked.target.url.clone(),
            observed_at: observed_at.clone(),
        }));
    }
    if events.is_empty() {
        return Ok(failed);
    }
    Ok(Some(Envelope {
        events,
        ..Envelope::ended(Outcome::Woke)
    }))
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
    report(stdout, &local_failure(kind, message))
}

fn local_failure(kind: ErrorKind, message: String) -> Envelope {
    Envelope::failed(envelope::Error {
        kind,
        forge: None,
        target: None,
        message,
    })
}

fn report_unresolved(
    stdout: &mut impl Write,
    given: &str,
    error: target::Error,
) -> Result<u8, UsageError> {
    let envelope = match error {
        target::Error::Fatal {
            forge,
            kind,
            message,
        } => Envelope::failed(envelope::Error {
            kind,
            forge,
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
    use crate::forge::{Poll, poll};
    use crate::reference::{self, Reference};

    const NODE_ID: &str = "PR_1";
    const OTHER: &str = "https://github.com/o/r/pull/2";
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

    type Step = Box<dyn FnOnce(&Target) -> Result<Poll, Failure>>;

    struct FakeBackend {
        polls: RefCell<VecDeque<Step>>,
    }

    impl Backend for FakeBackend {
        fn resolve(&self, reference: &Reference, _: Option<SystemTime>) -> Result<Target, Failure> {
            let Reference {
                host,
                number,
                owner,
                repo,
                ..
            } = reference;
            Ok(Target {
                forge_id: format!("PR_{number}"),
                host: host.clone(),
                url: format!("https://{host}/{owner}/{repo}/pull/{number}"),
            })
        }

        fn poll(&self, target: &Target, _: Option<SystemTime>) -> Result<Poll, Failure> {
            let step = self.polls.borrow_mut().pop_front();
            step.expect("unexpected poll")(target)
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

    fn returns(url: &'static str, poll: Poll) -> Step {
        step(url, Ok(poll))
    }

    fn fails(url: &'static str, failure: Failure) -> Step {
        step(url, Err(failure))
    }

    fn step(url: &'static str, result: Result<Poll, Failure>) -> Step {
        Box::new(move |target| {
            assert_eq!(target.url, url);
            result
        })
    }

    fn auth_failure() -> Failure {
        Failure::Fatal {
            forge: Forge::GitHub,
            kind: ErrorKind::Auth,
            message: "gh auth login".to_owned(),
        }
    }

    fn args(targets: &[&str], snapshot: &Path, timeout: Option<u64>) -> Args {
        Args {
            targets: targets
                .iter()
                .map(|target| reference::parse_name(target).unwrap())
                .collect(),
            snapshot: snapshot.to_owned(),
            interval: Duration::from_secs(30),
            timeout: timeout.map(Duration::from_secs),
        }
    }

    fn seed(snapshot: &Path, url: &str, forge_id: &str, ids: &[&str]) {
        let mut file = SnapshotFile::open(snapshot).unwrap();
        file.save([Entry {
            forge_id: forge_id.to_owned(),
            url: url.to_owned(),
            snapshot: poll(ids),
        }])
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

    fn summary(url: &str) -> Value {
        json!({
            "target": url,
            "state": "open",
            "head": "abc123",
            "colour": null,
            "merge": null,
            "unresolved_threads": 0,
        })
    }

    fn timed_out(targets: &[Value]) -> Value {
        json!({ "outcome": "timed_out", "events": [], "targets": targets, "error": null })
    }

    #[test]
    fn a_first_run_with_timeout_zero_saves_the_baseline_and_times_out() {
        let sandbox = Sandbox::new("first-run");

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), Some(0)),
            vec![returns(URL, poll(&["1"]))],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[summary(URL)]));
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
                                "edited_at": null,
                                "editor": null,
                                "id": "1",
                                "review_id": null,
                                "url": format!("{URL}#issuecomment-1"),
                            }],
                            "head": "abc123",
                            "reviews": [],
                            "state": "open",
                            "threads": [],
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
        let check = Box::new(move |_: &Target| {
            assert_eq!(
                saved(&snapshot)["targets"][URL]["snapshot"],
                json!(poll(&["1"]))
            );
            Err(Failure::TimedOut)
        });

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), Some(600)),
            vec![returns(URL, poll(&["1"])), check],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[summary(URL)]));
    }

    #[test]
    fn a_later_run_wakes_on_a_new_comment_then_saves() {
        let sandbox = Sandbox::new("wake");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![returns(URL, poll(&["1"])), returns(URL, poll(&["1", "2"]))],
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
                "targets": [summary(URL)],
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
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let once = args(&[URL], &sandbox.snapshot(), Some(0));

        let (quiet, _) = watch(&once, vec![returns(URL, poll(&["1"]))]);
        let (woke, envelope) = watch(&once, vec![returns(URL, poll(&["1", "2"]))]);

        assert_eq!(quiet, 3);
        assert_eq!(woke, 0);
        assert_eq!(envelope["events"][0]["comment_id"], "2");
    }

    #[test]
    fn the_timeout_ends_the_run_at_the_deadline() {
        let sandbox = Sandbox::new("timeout");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), Some(45)),
            vec![returns(URL, poll(&["1"])), returns(URL, poll(&["1"]))],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[summary(URL)]));
    }

    #[test]
    fn a_call_killed_at_the_deadline_times_out_leaving_the_baseline() {
        let sandbox = Sandbox::new("killed");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let before = saved(&sandbox.snapshot());

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), Some(600)),
            vec![fails(URL, Failure::TimedOut)],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[]));
        assert_eq!(saved(&sandbox.snapshot()), before);
    }

    #[test]
    fn a_fatal_failure_reports_the_error() {
        let sandbox = Sandbox::new("fatal");
        let failure = fails(URL, auth_failure());

        let (code, envelope) = watch(&args(&[URL], &sandbox.snapshot(), None), vec![failure]);

        assert_eq!(code, 1);
        assert_eq!(
            envelope["error"],
            json!({ "kind": "auth", "forge": "github", "target": URL, "message": "gh auth login" }),
        );
    }

    #[test]
    fn a_remote_host_claimed_by_both_clis_is_auth_naming_the_target_as_given() {
        let sandbox = Sandbox::new("claimed-by-both");
        let args = args(&["#1"], &sandbox.snapshot(), None);
        let exec = Script::new([
            (
                "git remote -v".to_owned(),
                Ok(Output::exited(
                    0,
                    "origin\thttps://forge.example.com/o/r.git (fetch)\n\
                     origin\thttps://forge.example.com/o/r.git (push)\n",
                )),
            ),
            (
                "gh auth token --hostname forge.example.com".to_owned(),
                Ok(Output::exited(0, "gho_x\n")),
            ),
            (
                "glab config get token --host forge.example.com".to_owned(),
                Ok(Output::exited(0, "glpat-x\n")),
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
        assert_eq!(envelope["error"]["kind"], "auth");
        assert_eq!(envelope["error"]["forge"], Value::Null);
        assert_eq!(envelope["error"]["target"], "#1");
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap()
                .contains("forge.example.com"),
        );
        assert!(!sandbox.snapshot().exists());
    }

    #[test]
    fn a_duration_past_the_clocks_reach_is_a_usage_error_before_any_write() {
        let sandbox = Sandbox::new("overflow");
        let mut too_long = args(&[URL], &sandbox.snapshot(), Some(u64::MAX));

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

        let (code, envelope) = watch(&args(&[URL], &sandbox.snapshot(), None), vec![]);

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

            let (code, _) = watch_into(
                &args(&[URL], &sandbox.snapshot(), None),
                vec![],
                &mut Vec::new(),
            );

            assert!(code.is_err(), "{contents:?}");
            assert_eq!(fs::read(sandbox.snapshot()).unwrap(), contents);
        }
    }

    #[test]
    fn a_failed_stdout_write_exits_1_without_saving() {
        let sandbox = Sandbox::new("stdout");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let before = saved(&sandbox.snapshot());

        let (code, stderr) = watch_into(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![returns(URL, poll(&["1", "2"]))],
            &mut FailingWriter,
        );

        assert_eq!(code.unwrap(), 1);
        assert_eq!(stderr, "");
        assert_eq!(saved(&sandbox.snapshot()), before);
    }

    #[test]
    fn a_save_failing_after_a_printed_wake_exits_1_with_one_stderr_line() {
        let sandbox = Sandbox::new("save");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        fs::set_permissions(&sandbox.dir, fs::Permissions::from_mode(0o555)).unwrap();
        let mut stdout = Vec::new();

        let (code, stderr) = watch_into(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![returns(URL, poll(&["1", "2"]))],
            &mut stdout,
        );

        let envelope: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(code.unwrap(), 1);
        assert_eq!(envelope["outcome"], "woke");
        assert_eq!(stderr.lines().count(), 1);
        assert!(stderr.starts_with("babysit: could not save "), "{stderr}");
    }

    #[test]
    fn several_targets_report_in_invocation_order_once_each() {
        let sandbox = Sandbox::new("several");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        seed(&sandbox.snapshot(), OTHER, "PR_2", &["1"]);
        let args = args(
            &[OTHER, URL, "http://github.com/o/r/pull/2"],
            &sandbox.snapshot(),
            None,
        );

        let (code, envelope) = watch(
            &args,
            vec![
                returns(OTHER, poll(&["1", "3"])),
                returns(URL, poll(&["1", "2"])),
            ],
        );

        assert_eq!(code, 0);
        let events: Vec<_> = envelope["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| {
                (
                    event["target"].as_str().unwrap(),
                    event["comment_id"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(events, [(OTHER, "3"), (URL, "2")]);
        assert_eq!(envelope["targets"], json!([summary(OTHER), summary(URL)]));
        let saved = saved(&sandbox.snapshot());
        assert_eq!(
            saved["targets"][OTHER]["snapshot"],
            json!(poll(&["1", "3"]))
        );
        assert_eq!(saved["targets"][URL]["snapshot"], json!(poll(&["1", "2"])));
    }

    #[test]
    fn a_failed_poll_leaves_its_entry_while_another_target_wakes_and_saves() {
        let sandbox = Sandbox::new("partial");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1", "2"]);
        seed(&sandbox.snapshot(), OTHER, "PR_2", &["1"]);
        let before = saved(&sandbox.snapshot());

        let (code, envelope) = watch(
            &args(&[URL, OTHER], &sandbox.snapshot(), None),
            vec![
                // A deletion fires nothing but changes the snapshot.
                returns(URL, poll(&["1"])),
                returns(OTHER, poll(&["1"])),
                fails(URL, auth_failure()),
                returns(OTHER, poll(&["1", "2"])),
            ],
        );

        assert_eq!(code, 0);
        assert_eq!(envelope["outcome"], "woke");
        assert_eq!(envelope["events"].as_array().unwrap().len(), 1);
        assert_eq!(envelope["events"][0]["target"], OTHER);
        assert_eq!(envelope["error"], Value::Null);
        assert_eq!(envelope["targets"], json!([summary(URL), summary(OTHER)]));
        let saved = saved(&sandbox.snapshot());
        assert_eq!(saved["targets"][URL], before["targets"][URL]);
        assert_eq!(
            saved["targets"][OTHER]["snapshot"],
            json!(poll(&["1", "2"]))
        );
    }

    #[test]
    fn a_failed_poll_without_a_wake_ends_the_run_once_the_round_finishes() {
        let sandbox = Sandbox::new("round");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let (code, envelope) = watch(
            &args(&[URL, OTHER], &sandbox.snapshot(), None),
            vec![fails(URL, auth_failure()), returns(OTHER, poll(&["5"]))],
        );

        assert_eq!(code, 1);
        assert_eq!(envelope["error"]["target"], URL);
        assert_eq!(envelope["targets"], json!([summary(OTHER)]));
        assert_eq!(
            saved(&sandbox.snapshot())["targets"][OTHER]["snapshot"],
            json!(poll(&["5"]))
        );
    }

    #[test]
    fn a_deadline_killing_a_later_poll_times_out_over_an_earlier_failure() {
        let sandbox = Sandbox::new("timeout-over-error");

        let (code, envelope) = watch(
            &args(&[URL, OTHER], &sandbox.snapshot(), Some(600)),
            vec![fails(URL, auth_failure()), fails(OTHER, Failure::TimedOut)],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[]));
    }

    #[test]
    fn a_reused_path_is_replaced_by_a_fresh_baseline() {
        let sandbox = Sandbox::new("reused");
        seed(&sandbox.snapshot(), URL, "PR_deleted", &["1"]);

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), Some(0)),
            vec![returns(URL, poll(&["1", "2"]))],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[summary(URL)]));
        let saved = saved(&sandbox.snapshot());
        assert_eq!(saved["targets"][URL]["forge_id"], NODE_ID);
        assert_eq!(saved["targets"][URL]["snapshot"], json!(poll(&["1", "2"])));
    }

    #[test]
    fn a_renamed_target_is_rekeyed_and_diffed() {
        let sandbox = Sandbox::new("renamed");
        let old_url = "https://github.com/o/old/pull/1";
        seed(&sandbox.snapshot(), old_url, NODE_ID, &["1"]);

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![returns(URL, poll(&["1", "2"]))],
        );

        assert_eq!(code, 0);
        assert_eq!(envelope["events"][0]["comment_id"], "2");
        let saved = saved(&sandbox.snapshot());
        assert_eq!(saved["targets"].as_object().unwrap().len(), 1);
        assert_eq!(saved["targets"][URL]["url"], URL);
        assert_eq!(saved["targets"][URL]["snapshot"], json!(poll(&["1", "2"])));
    }

    #[test]
    fn the_same_forge_id_on_another_host_is_no_rename() {
        let sandbox = Sandbox::new("other-host");
        let enterprise = "https://ghe.example.com/o/r/pull/1";
        seed(&sandbox.snapshot(), enterprise, NODE_ID, &["1"]);
        let before = saved(&sandbox.snapshot());

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), Some(0)),
            vec![returns(URL, poll(&["1", "2"]))],
        );

        assert_eq!(code, 3);
        assert_eq!(envelope, timed_out(&[summary(URL)]));
        let saved = saved(&sandbox.snapshot());
        assert_eq!(saved["targets"][enterprise], before["targets"][enterprise]);
        assert_eq!(saved["targets"][URL]["snapshot"], json!(poll(&["1", "2"])));
    }

    #[test]
    fn a_failed_baseline_save_still_summarises_the_evaluated_targets() {
        let sandbox = Sandbox::new("baseline-save");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        fs::set_permissions(&sandbox.dir, fs::Permissions::from_mode(0o555)).unwrap();

        let (code, envelope) = watch(
            &args(&[URL, OTHER], &sandbox.snapshot(), None),
            vec![returns(URL, poll(&["1"])), returns(OTHER, poll(&["5"]))],
        );

        assert_eq!(code, 1);
        assert_eq!(envelope["error"]["kind"], "snapshot_io");
        assert_eq!(envelope["targets"], json!([summary(URL), summary(OTHER)]));
    }

    #[test]
    fn a_new_target_is_baselined_and_entries_outside_the_run_are_untouched() {
        let sandbox = Sandbox::new("new-target");
        let elsewhere = "https://github.com/o/r/pull/9";
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        seed(&sandbox.snapshot(), elsewhere, "PR_9", &["7"]);
        let before = saved(&sandbox.snapshot());

        let (code, envelope) = watch(
            &args(&[URL, OTHER], &sandbox.snapshot(), None),
            vec![
                returns(URL, poll(&["1", "2"])),
                returns(OTHER, poll(&["5"])),
            ],
        );

        assert_eq!(code, 0);
        assert_eq!(envelope["events"].as_array().unwrap().len(), 1);
        assert_eq!(envelope["events"][0]["target"], URL);
        let saved = saved(&sandbox.snapshot());
        assert_eq!(saved["targets"][elsewhere], before["targets"][elsewhere]);
        assert_eq!(saved["targets"][OTHER]["snapshot"], json!(poll(&["5"])));
    }
}
