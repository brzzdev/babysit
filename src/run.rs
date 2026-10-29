use std::collections::HashMap;
use std::io::{self, Write};
use std::ops::ControlFlow;
use std::time::{Duration, SystemTime};

use crate::cli::Args;
use crate::clock::{Clock, rfc3339};
use crate::diff::diff;
use crate::envelope::{self, EXIT_ERROR, Envelope, ErrorKind, Forge, Outcome, Summary};
use crate::exec::Exec;
use crate::forge::{Backend, Failure, Hints, Snapshot, Target};
use crate::reference::Reference;
use crate::rule;
use crate::snapshot_file::{Entry, OpenError, SnapshotFile};
use crate::target::{self, Locator};

/// Consecutive failed polls of one target that end the run.
const GIVE_UP_AFTER: u32 = 10;
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);
const RATE_LIMIT_WAIT_MAX: Duration = Duration::from_mins(15);

pub struct Backends<'a> {
    pub github: &'a dyn Backend,
    pub gitlab: &'a dyn Backend,
    pub slack: &'a dyn Backend,
}

impl<'a> Backends<'a> {
    fn of(&self, forge: Forge) -> &'a dyn Backend {
        match forge {
            Forge::GitHub => self.github,
            Forge::GitLab => self.gitlab,
            Forge::Slack => self.slack,
        }
    }
}

/// A usage error found after argument parsing, such as an unreadable snapshot file.
#[derive(Debug)]
pub struct UsageError(pub String);

/// Watches the targets until one wakes, times out or fails; prints, saves and returns the exit code.
pub fn run(
    args: &Args,
    backends: &Backends,
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
    let schedule = Schedule::new(args, deadline);
    let call_deadline = schedule.call_deadline;
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
    let located = Locator::new(exec, call_deadline).locate_all(&args.targets);
    let references = match located {
        Ok(references) => references,
        Err((given, error)) => return report_unresolved(stdout, given, error),
    };
    let mut watch = Watch::new(args, references, schedule, now);
    let watched = watch.run(args, backends, clock, &mut file, stderr);
    let (mut envelope, write_failed) = match watched {
        Ok(envelope) => (envelope, false),
        Err(error) => {
            let message = format!("{snapshot_path}: {error}");
            (local_failure(ErrorKind::SnapshotIo, message), true)
        }
    };
    envelope.targets = watch
        .tracked
        .iter()
        .filter_map(|tracked| {
            let snapshot = tracked.polled.as_ref()?;
            Some(Summary::new(&tracked.target, snapshot))
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
    let entries: Vec<_> = watch
        .tracked
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

/// Every call the run still has to make: names to resolve, viewers to look up, targets to poll.
struct Watch<'a> {
    pending: Vec<Pending<'a>>,
    schedule: Schedule,
    /// In the order their names first occur.
    tracked: Vec<Tracked>,
    /// One per host when a rule names `@me`; a host's targets are polled once its viewer is known.
    viewers: Vec<Viewer>,
}

/// When a call may next be made, and how many in a row have failed.
struct Attempts {
    failures: u32,
    next: SystemTime,
}

impl Attempts {
    fn new(now: SystemTime) -> Self {
        Self {
            failures: 0,
            next: now,
        }
    }
}

/// A name not yet resolved to a target.
struct Pending<'a> {
    attempts: Attempts,
    given: &'a str,
    /// Where the name occurs among the targets given.
    order: usize,
    reference: Reference,
}

/// A target through the run: the baseline the file held for it, then its last evaluated poll.
struct Tracked {
    attempts: Attempts,
    /// Whether the latest round polled it successfully.
    evaluated: bool,
    /// Where its name first occurs among the targets given.
    order: usize,
    polled: Option<Snapshot>,
    stored: Option<Snapshot>,
    target: Target,
}

/// The viewer's account id on a host, looked up once.
struct Viewer {
    attempts: Attempts,
    /// The forge of the first name given on the host.
    forge: Forge,
    host: String,
    id: Option<String>,
    /// The first name given on the host.
    name: String,
}

/// A failed call: the kind of call, its host, and the name its failure reports.
struct Call<'a> {
    host: &'a str,
    kind: &'a str,
    name: &'a str,
}

/// What a failed call leaves the run to do.
enum Fallout {
    /// The deadline killed the call.
    Deadline,
    End(Envelope),
    Retry,
}

impl<'a> Watch<'a> {
    fn new(
        args: &Args,
        references: Vec<(&'a str, Reference)>,
        schedule: Schedule,
        now: SystemTime,
    ) -> Self {
        let mut viewers: Vec<Viewer> = Vec::new();
        if rule::names_me(args.on.iter().chain(&args.ignore)) {
            for (given, reference) in &references {
                if viewers.iter().all(|viewer| viewer.host != reference.host) {
                    viewers.push(Viewer {
                        attempts: Attempts::new(now),
                        forge: reference.forge,
                        host: reference.host.clone(),
                        id: None,
                        name: (*given).to_owned(),
                    });
                }
            }
        }
        let pending = references
            .into_iter()
            .enumerate()
            .map(|(order, (given, reference))| Pending {
                attempts: Attempts::new(now),
                given,
                order,
                reference,
            })
            .collect();
        Self {
            pending,
            schedule,
            tracked: Vec::new(),
            viewers,
        }
    }

    /// Runs round after round until one wakes or fails, or every call is due past the deadline.
    fn run(
        &mut self,
        args: &Args,
        backends: &Backends,
        clock: &impl Clock,
        file: &mut SnapshotFile,
        stderr: &mut impl Write,
    ) -> io::Result<Envelope> {
        loop {
            if let Some(ended) = self.round(args, backends, clock, file, stderr)? {
                return Ok(ended);
            }
            let next = self.next_call();
            if self
                .schedule
                .deadline
                .is_some_and(|deadline| next >= deadline)
            {
                return Ok(Envelope::ended(Outcome::TimedOut));
            }
            clock.sleep_until(next);
        }
    }

    fn next_call(&self) -> SystemTime {
        let schedule = &self.schedule;
        let resolves = self
            .pending
            .iter()
            .map(|pending| schedule.due(&pending.reference.host, pending.attempts.next));
        let lookups = self
            .viewers
            .iter()
            .filter(|viewer| viewer.id.is_none())
            .map(|viewer| schedule.due(&viewer.host, viewer.attempts.next));
        let polls = self
            .tracked
            .iter()
            .filter(|tracked| pollable(&self.viewers, &tracked.target.host))
            .map(|tracked| schedule.due(&tracked.target.host, tracked.attempts.next));
        resolves
            .chain(lookups)
            .chain(polls)
            .min()
            .expect("every run has a call left to make")
    }

    /// Makes each call that is due, saving a first baseline at once; returns the round's wake or
    /// failure.
    fn round(
        &mut self,
        args: &Args,
        backends: &Backends,
        clock: &impl Clock,
        file: &mut SnapshotFile,
        stderr: &mut impl Write,
    ) -> io::Result<Option<Envelope>> {
        let mut failed = None;
        if self
            .resolve_due(backends, clock, file, stderr, &mut failed)
            .is_break()
            || self
                .look_up_due(backends, clock, stderr, &mut failed)
                .is_break()
        {
            return Ok(Some(Envelope::ended(Outcome::TimedOut)));
        }
        let Self {
            schedule,
            tracked,
            viewers,
            ..
        } = self;
        let mut events = Vec::new();
        for tracked in tracked.iter_mut() {
            tracked.evaluated = false;
        }
        for tracked in tracked {
            let host = &tracked.target.host;
            if !pollable(viewers, host) || schedule.due(host, tracked.attempts.next) > clock.now() {
                continue;
            }
            let polled = backends
                .of(tracked.target.forge)
                .poll(&tracked.target, schedule.call_deadline);
            let now = clock.now();
            let poll = match polled {
                Ok((poll, hints)) => {
                    tracked.attempts.failures = 0;
                    tracked.attempts.next = now + schedule.spacing(hints);
                    if let Some(until) = schedule.polled(host, hints, now) {
                        notify_wait(stderr, host, until);
                    }
                    poll
                }
                Err(failure) => {
                    let call = Call {
                        host,
                        kind: "poll",
                        name: &tracked.target.url,
                    };
                    let fallout =
                        schedule.failed(&mut tracked.attempts, &call, failure, now, stderr);
                    // Every later call would be killed at once by the passed deadline.
                    if settle(fallout, &mut failed).is_break() {
                        failed = Some(Envelope::ended(Outcome::TimedOut));
                        break;
                    }
                    continue;
                }
            };
            tracked.evaluated = true;
            let Some(baseline) = tracked.polled.as_ref().or(tracked.stored.as_ref()) else {
                let poll = Snapshot::first(poll, now);
                let first = entry(&tracked.target, poll.clone());
                tracked.polled = Some(poll);
                file.save([first])?;
                continue;
            };
            let (mut seen, next) = diff(baseline, poll, now);
            tracked.polled = Some(next);
            let me = viewer_on(viewers, host).and_then(|viewer| viewer.id.as_deref());
            seen.retain(|event| rule::wakes(&args.on, &args.ignore, event, me));
            if seen.is_empty() {
                continue;
            }
            let observed_at = rfc3339(clock.now());
            events.extend(seen.into_iter().map(|mut seen| {
                let truncated = args.brief && seen.detail.body_mut().is_some_and(envelope::shorten);
                envelope::Event {
                    seen,
                    target: tracked.target.url.clone(),
                    observed_at: observed_at.clone(),
                    truncated,
                }
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

    /// Resolves each pending name that is due, tracking a new target in first-occurrence order;
    /// breaks when the deadline killed a call.
    fn resolve_due(
        &mut self,
        backends: &Backends,
        clock: &impl Clock,
        file: &mut SnapshotFile,
        stderr: &mut impl Write,
        failed: &mut Option<Envelope>,
    ) -> ControlFlow<()> {
        let mut index = 0;
        while let Some(pending) = self.pending.get_mut(index) {
            let host = &pending.reference.host;
            if self.schedule.due(host, pending.attempts.next) > clock.now() {
                index += 1;
                continue;
            }
            let resolved = backends
                .of(pending.reference.forge)
                .resolve(&pending.reference, self.schedule.call_deadline);
            let now = clock.now();
            let target = match resolved {
                Ok((target, hints)) => {
                    if let Some(until) = self.schedule.answered(host, hints, now) {
                        notify_wait(stderr, host, until);
                    }
                    target
                }
                Err(failure) => {
                    let call = Call {
                        host,
                        kind: "resolve",
                        name: pending.given,
                    };
                    let attempts = &mut pending.attempts;
                    settle(
                        self.schedule.failed(attempts, &call, failure, now, stderr),
                        failed,
                    )?;
                    index += 1;
                    continue;
                }
            };
            let pending = self.pending.remove(index);
            if let Err(failure) = target::check_host(pending.given, &pending.reference, &target) {
                failed.get_or_insert(Envelope::from_failure(failure, pending.given));
                continue;
            }
            self.track(file, target, pending.order, now);
        }
        ControlFlow::Continue(())
    }

    /// Looks up each host's viewer that is due; breaks when the deadline killed a call.
    fn look_up_due(
        &mut self,
        backends: &Backends,
        clock: &impl Clock,
        stderr: &mut impl Write,
        failed: &mut Option<Envelope>,
    ) -> ControlFlow<()> {
        for viewer in self.viewers.iter_mut().filter(|viewer| viewer.id.is_none()) {
            if self.schedule.due(&viewer.host, viewer.attempts.next) > clock.now() {
                continue;
            }
            let looked_up = backends
                .of(viewer.forge)
                .viewer(&viewer.host, self.schedule.call_deadline);
            let now = clock.now();
            match looked_up {
                Ok((id, hints)) => {
                    if let Some(until) = self.schedule.answered(&viewer.host, hints, now) {
                        notify_wait(stderr, &viewer.host, until);
                    }
                    viewer.id = Some(id);
                }
                Err(failure) => {
                    let resolved = self
                        .tracked
                        .iter()
                        .find(|tracked| tracked.target.host == viewer.host);
                    let call = Call {
                        host: &viewer.host,
                        kind: "viewer lookup",
                        name: resolved.map_or(&viewer.name, |tracked| &tracked.target.url),
                    };
                    let attempts = &mut viewer.attempts;
                    settle(
                        self.schedule.failed(attempts, &call, failure, now, stderr),
                        failed,
                    )?;
                }
            }
        }
        ControlFlow::Continue(())
    }

    /// Tracks `target` from the name at `order`, or moves an earlier-resolved one up to it.
    fn track(&mut self, file: &mut SnapshotFile, target: Target, order: usize, now: SystemTime) {
        match self
            .tracked
            .iter_mut()
            .find(|tracked| tracked.target.url == target.url)
        {
            Some(known) => known.order = known.order.min(order),
            None => self.tracked.push(Tracked {
                attempts: Attempts::new(now),
                evaluated: false,
                order,
                polled: None,
                stored: target::baseline(file, &target),
                target,
            }),
        }
        self.tracked.sort_by_key(|tracked| tracked.order);
    }
}

/// Records `fallout` in `failed`, breaking when the deadline killed the call.
fn settle(fallout: Fallout, failed: &mut Option<Envelope>) -> ControlFlow<()> {
    match fallout {
        Fallout::Deadline => return ControlFlow::Break(()),
        Fallout::End(envelope) => {
            failed.get_or_insert(envelope);
        }
        Fallout::Retry => {}
    }
    ControlFlow::Continue(())
}

/// Whether `host`'s targets may be polled: its viewer is known, or no rule needs one.
fn pollable(viewers: &[Viewer], host: &str) -> bool {
    viewer_on(viewers, host).is_none_or(|viewer| viewer.id.is_some())
}

fn viewer_on<'v>(viewers: &'v [Viewer], host: &str) -> Option<&'v Viewer> {
    viewers.iter().find(|viewer| viewer.host == host)
}

/// When a spent quota refills, if that is still to come.
fn refill(hints: Hints, now: SystemTime) -> Option<SystemTime> {
    hints.reset.filter(|reset| *reset > now)
}

/// When each host may next be called, and how a target's polls are spaced.
struct Schedule {
    call_deadline: Option<SystemTime>,
    deadline: Option<SystemTime>,
    give_up_after: u32,
    hosts: HashMap<String, Cooldown>,
    interval: Duration,
    /// `--timeout 0`: one round, its calls held for nothing.
    once: bool,
}

/// A host's rate-limit standing since its last successful poll.
#[derive(Default)]
struct Cooldown {
    /// Consecutive rate limits that named no wait.
    backoffs: u32,
    until: Option<SystemTime>,
}

impl Schedule {
    fn new(args: &Args, deadline: Option<SystemTime>) -> Self {
        let once = args.timeout.is_some_and(|timeout| timeout.is_zero());
        Self {
            // `--timeout 0` polls once, so its already-passed deadline must not kill that poll.
            call_deadline: deadline.filter(|_| !once),
            deadline,
            give_up_after: if once { 1 } else { GIVE_UP_AFTER },
            hosts: HashMap::new(),
            interval: args.interval,
            once,
        }
    }

    /// The later of `next` and the end of `host`'s cooldown.
    fn due(&self, host: &str, next: SystemTime) -> SystemTime {
        let cooldown = self.hosts.get(host).and_then(|cooldown| cooldown.until);
        cooldown.map_or(next, |until| until.max(next))
    }

    /// The interval, stretched to whatever the forge asked for.
    fn spacing(&self, hints: Hints) -> Duration {
        [hints.poll_interval, hints.retry_after]
            .into_iter()
            .flatten()
            .fold(self.interval, Duration::max)
    }

    /// Records a complete successful poll at `now`: the backoff restarts, and a spent quota holds
    /// the host until the refill it returns, unless `once`.
    fn polled(&mut self, host: &str, hints: Hints, now: SystemTime) -> Option<SystemTime> {
        self.hosts.entry(host.to_owned()).or_default().backoffs = 0;
        if self.once {
            return None;
        }
        self.pause(host, refill(hints, now))
    }

    /// Holds `host` after a successful startup call at `now` for its `retry-after` and spent
    /// quota, returning the quota's refill, unless `once`.
    fn answered(&mut self, host: &str, hints: Hints, now: SystemTime) -> Option<SystemTime> {
        if self.once {
            return None;
        }
        let reset = refill(hints, now);
        self.pause(host, hints.retry_after.map(|after| now + after).max(reset));
        reset
    }

    /// Sets `host`'s cooldown after a successful call to `until`, which it returns.
    fn pause(&mut self, host: &str, until: Option<SystemTime>) -> Option<SystemTime> {
        self.hosts.entry(host.to_owned()).or_default().until = until;
        until
    }

    /// Records `call` failing at `now`: a rate limit cools its host down, a transient failure is
    /// retried until the give-up count, and anything else ends the run.
    fn failed(
        &mut self,
        attempts: &mut Attempts,
        call: &Call,
        failure: Failure,
        now: SystemTime,
        stderr: &mut impl Write,
    ) -> Fallout {
        match failure {
            Failure::TimedOut => Fallout::Deadline,
            Failure::RateLimited { hints, .. } => {
                if let Some(until) = self.rate_limited(call.host, hints, now) {
                    notify_wait(stderr, call.host, until);
                }
                Fallout::Retry
            }
            Failure::Transient { hints, message, .. }
                if attempts.failures + 1 < self.give_up_after =>
            {
                attempts.failures += 1;
                let wait = self.spacing(hints);
                attempts.next = now + wait;
                notify_retry(stderr, call, wait, &message);
                Fallout::Retry
            }
            failure => Fallout::End(Envelope::from_failure(failure, call.name)),
        }
    }

    /// When the host may be called again: `retry-after`, the quota reset, or a doubling backoff;
    /// `once` holds nothing.
    fn rate_limited(&mut self, host: &str, hints: Hints, now: SystemTime) -> Option<SystemTime> {
        if self.once {
            return None;
        }
        let cooldown = self.hosts.entry(host.to_owned()).or_default();
        let named = hints
            .retry_after
            .map(|after| now + after)
            .or(refill(hints, now));
        let until = named.unwrap_or_else(|| {
            let doublings = 2u32.saturating_pow(cooldown.backoffs);
            cooldown.backoffs += 1;
            now + RATE_LIMIT_WAIT
                .saturating_mul(doublings)
                .min(RATE_LIMIT_WAIT_MAX)
        });
        cooldown.until = Some(until);
        Some(until)
    }
}

fn notify_retry(stderr: &mut impl Write, call: &Call, wait: Duration, reason: &str) {
    let Call { kind, name, .. } = call;
    let wait = wait.as_secs();
    let _ = writeln!(
        stderr,
        "babysit: {kind} failed for {name}, retrying in {wait}s: {reason}"
    );
}

fn notify_wait(stderr: &mut impl Write, host: &str, until: SystemTime) {
    let until = rfc3339(until);
    let _ = writeln!(
        stderr,
        "babysit: rate limited on {host}, waiting until {until}"
    );
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
    use crate::envelope::{BRIEF_BODY_CHARS, Forge};
    use crate::exec::{Output, Script};
    use crate::forge::{Account, Merge, Poll, TargetKind, TargetState, octocat, poll};
    use crate::reference::{self, Locus, Reference};

    const ENTERPRISE: &str = "https://ghe.example.com/o/r/pull/3";
    const NODE_ID: &str = "PR_1";
    const OTHER: &str = "https://github.com/o/r/pull/2";
    const STARTED: u64 = 1_792_523_957;
    const URL: &str = "https://github.com/o/r/pull/1";
    const VIEWER_ID: &str = "U_1";

    struct FakeClock(Cell<SystemTime>);

    impl Clock for FakeClock {
        fn now(&self) -> SystemTime {
            self.0.get()
        }

        fn sleep_until(&self, until: SystemTime) {
            self.0.set(self.0.get().max(until));
        }
    }

    type Step = Box<dyn FnOnce(&Target, &FakeClock) -> Result<(Poll, Hints), Failure>>;

    struct FakeBackend<'a> {
        clock: &'a FakeClock,
        /// Each call's target and its time in seconds since the run started.
        log: RefCell<Vec<(String, u64)>>,
        polls: RefCell<VecDeque<Step>>,
        /// What `resolve` answers, each `Ok` its success's hints, before succeeding without any.
        resolves: RefCell<VecDeque<Result<Hints, Failure>>>,
        /// What `viewer` answers, as `resolves` does.
        viewers: RefCell<VecDeque<Result<Hints, Failure>>>,
    }

    impl Backend for FakeBackend<'_> {
        fn resolve(
            &self,
            reference: &Reference,
            _: Option<SystemTime>,
        ) -> Result<(Target, Hints), Failure> {
            let hints = self
                .resolves
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(Hints::default()))?;
            let Reference {
                forge,
                host,
                locus: Locus::Numbered {
                    number, project, ..
                },
                ..
            } = reference
            else {
                unreachable!("the run tests name no Slack thread");
            };
            let (id, path) = match forge {
                Forge::GitHub => ("PR", "pull"),
                Forge::GitLab => ("MR", "-/merge_requests"),
                Forge::Slack => unreachable!("the run tests name no Slack thread"),
            };
            let target = Target {
                forge: *forge,
                forge_id: format!("{id}_{number}"),
                host: host.clone(),
                kind: TargetKind::ChangeRequest,
                url: format!("https://{host}/{project}/{path}/{number}"),
            };
            Ok((target, hints))
        }

        fn poll(&self, target: &Target, _: Option<SystemTime>) -> Result<(Poll, Hints), Failure> {
            self.log
                .borrow_mut()
                .push((target.url.clone(), elapsed(self.clock)));
            let step = self.polls.borrow_mut().pop_front();
            step.expect("unexpected poll")(target, self.clock)
        }

        fn viewer(&self, _: &str, _: Option<SystemTime>) -> Result<(String, Hints), Failure> {
            let hints = self
                .viewers
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(Hints::default()))?;
            Ok((VIEWER_ID.to_owned(), hints))
        }
    }

    impl Drop for FakeBackend<'_> {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                assert!(self.polls.borrow().is_empty(), "polls left in the script");
                assert!(
                    self.resolves.borrow().is_empty(),
                    "resolves left in the script"
                );
                assert!(
                    self.viewers.borrow().is_empty(),
                    "viewers left in the script"
                );
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

    fn start() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(STARTED)
    }

    fn at(seconds: u64) -> SystemTime {
        start() + Duration::from_secs(seconds)
    }

    fn elapsed(clock: &FakeClock) -> u64 {
        clock.now().duration_since(start()).unwrap().as_secs()
    }

    fn returns(url: &'static str, poll: Poll) -> Step {
        paced(url, poll, Hints::default())
    }

    fn paced(url: &'static str, poll: Poll, hints: Hints) -> Step {
        step(url, Ok((poll, hints)))
    }

    fn fails(url: &'static str, failure: Failure) -> Step {
        step(url, Err(failure))
    }

    /// A step that takes `seconds` of the fake clock before answering.
    fn slow(url: &'static str, seconds: u64, result: Result<(Poll, Hints), Failure>) -> Step {
        Box::new(move |target, clock| {
            clock.sleep_until(clock.now() + Duration::from_secs(seconds));
            step(url, result)(target, clock)
        })
    }

    fn step(url: &'static str, result: Result<(Poll, Hints), Failure>) -> Step {
        Box::new(move |target, _| {
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

    fn transient(reason: &str) -> Failure {
        Failure::Transient {
            forge: Forge::GitHub,
            hints: Hints::default(),
            message: reason.to_owned(),
        }
    }

    fn rate_limited(hints: Hints) -> Failure {
        Failure::RateLimited {
            forge: Forge::GitHub,
            hints,
        }
    }

    fn args(targets: &[&str], snapshot: &Path, timeout: Option<u64>) -> Args {
        Args {
            brief: false,
            ignore: Vec::new(),
            on: Vec::new(),
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
            snapshot: Snapshot::first(poll(ids), UNIX_EPOCH),
        }])
        .unwrap();
    }

    /// Polls are each target with its second since the start; `ended` is the run's last second.
    struct Ran {
        code: Result<u8, UsageError>,
        ended: u64,
        polls: Vec<(String, u64)>,
        stderr: String,
    }

    /// The answers of the calls before the first poll.
    #[derive(Default)]
    struct Startup {
        resolves: Vec<Result<Hints, Failure>>,
        viewers: Vec<Result<Hints, Failure>>,
    }

    fn run_into(args: &Args, startup: Startup, polls: Vec<Step>, stdout: &mut impl Write) -> Ran {
        let clock = FakeClock(Cell::new(start()));
        let backend = FakeBackend {
            clock: &clock,
            log: RefCell::default(),
            polls: RefCell::new(polls.into()),
            resolves: RefCell::new(startup.resolves.into()),
            viewers: RefCell::new(startup.viewers.into()),
        };
        let mut stderr = Vec::new();
        let code = run(
            args,
            &Backends {
                github: &backend,
                gitlab: &backend,
                slack: &backend,
            },
            &Script::new([]),
            &clock,
            stdout,
            &mut stderr,
        );
        Ran {
            code,
            ended: elapsed(&clock),
            polls: backend.log.take(),
            stderr: String::from_utf8(stderr).unwrap(),
        }
    }

    struct Traced {
        code: u8,
        ended: u64,
        envelope: Value,
        polls: Vec<(String, u64)>,
        stderr: String,
    }

    fn trace(args: &Args, polls: Vec<Step>) -> Traced {
        trace_starting(args, Startup::default(), polls)
    }

    fn trace_starting(args: &Args, startup: Startup, polls: Vec<Step>) -> Traced {
        let mut stdout = Vec::new();
        let ran = run_into(args, startup, polls, &mut stdout);
        Traced {
            code: ran.code.unwrap(),
            ended: ran.ended,
            envelope: serde_json::from_slice(&stdout).unwrap(),
            polls: ran.polls,
            stderr: ran.stderr,
        }
    }

    fn watch_into(
        args: &Args,
        polls: Vec<Step>,
        stdout: &mut impl Write,
    ) -> (Result<u8, UsageError>, String) {
        let ran = run_into(args, Startup::default(), polls, stdout);
        (ran.code, ran.stderr)
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
    fn each_target_is_resolved_and_polled_by_its_forges_backend() {
        let sandbox = Sandbox::new("routing");
        let merge_request = "https://gitlab.com/g/sub/p/-/merge_requests/4";
        let clock = FakeClock(Cell::new(start()));
        let backend = |polls: Vec<Step>| FakeBackend {
            clock: &clock,
            log: RefCell::default(),
            polls: RefCell::new(polls.into()),
            resolves: RefCell::default(),
            viewers: RefCell::default(),
        };
        let github = backend(vec![returns(URL, poll(&[]))]);
        let gitlab = backend(vec![returns(merge_request, poll(&[]))]);
        let backends = Backends {
            github: &github,
            gitlab: &gitlab,
            slack: &github,
        };

        let code = run(
            &args(&[merge_request, URL], &sandbox.snapshot(), Some(0)),
            &backends,
            &Script::new([]),
            &clock,
            &mut Vec::new(),
            &mut Vec::new(),
        );

        assert_eq!(code.unwrap(), 3);
        assert_eq!(github.log.take(), [(URL.to_owned(), 0)]);
        assert_eq!(gitlab.log.take(), [(merge_request.to_owned(), 0)]);
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
                                "reply_to": null,
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
        let check = Box::new(move |_: &Target, _: &FakeClock| {
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
    fn the_summary_carries_the_reported_mergeability() {
        let sandbox = Sandbox::new("merge-summary");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let conflicted = Poll {
            merge: Some(Merge::Conflicted),
            ..poll(&["1"])
        };

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), Some(0)),
            vec![returns(URL, conflicted)],
        );

        let mut expected = summary(URL);
        expected["merge"] = json!("conflicted");
        assert_eq!(code, 3);
        assert_eq!(envelope["targets"], json!([expected]));
    }

    #[test]
    fn a_target_that_is_not_open_summarises_no_mergeability() {
        let sandbox = Sandbox::new("merged-summary");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let conflicted = Poll {
            merge: Some(Merge::Conflicted),
            ..poll(&["1"])
        };
        let merged = Poll {
            state: TargetState::Merged,
            ..conflicted.clone()
        };

        let (code, envelope) = watch(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![returns(URL, conflicted), returns(URL, merged)],
        );

        let mut expected = summary(URL);
        expected["state"] = json!("merged");
        assert_eq!(code, 0);
        assert_eq!(envelope["targets"], json!([expected]));
        assert_eq!(
            saved(&sandbox.snapshot())["targets"][URL]["snapshot"]["reported"]["merge"],
            "conflicted"
        );
    }

    #[test]
    fn an_ignored_event_is_saved_as_seen_for_later_runs() {
        let sandbox = Sandbox::new("ignored");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let every_event = args(&[URL], &sandbox.snapshot(), Some(0));
        let ignoring = Args {
            ignore: vec![rule::parse("comment.created").unwrap()],
            ..args(&[URL], &sandbox.snapshot(), Some(0))
        };

        let (ignored, envelope) = watch(&ignoring, vec![returns(URL, poll(&["1", "2"]))]);
        let (later, _) = watch(&every_event, vec![returns(URL, poll(&["1", "2"]))]);

        assert_eq!(ignored, 3);
        assert_eq!(envelope, timed_out(&[summary(URL)]));
        assert_eq!(later, 3);
    }

    #[test]
    fn me_wakes_on_the_viewers_account_alone_whatever_its_login() {
        let sandbox = Sandbox::new("me");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let mut next = poll(&["1", "2", "3"]);
        next.comments[1].actor = Some(Account {
            id: Some(VIEWER_ID.to_owned()),
            ..octocat()
        });
        let mine = Args {
            on: vec![rule::parse("actor=@me").unwrap()],
            ..args(&[URL], &sandbox.snapshot(), Some(0))
        };

        let (code, envelope) = watch(&mine, vec![returns(URL, next)]);

        assert_eq!(code, 0);
        assert_eq!(envelope["events"].as_array().unwrap().len(), 1);
        assert_eq!(envelope["events"][0]["comment_id"], "2");
    }

    #[test]
    fn brief_rules_match_the_full_body_and_print_it_cut() {
        let sandbox = Sandbox::new("brief");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let long = format!("{}tail", "a".repeat(BRIEF_BODY_CHARS));
        let mut next = poll(&["1", "2", "3"]);
        next.comments[1].body = long.clone();
        let brief = Args {
            brief: true,
            on: vec![
                rule::parse(&format!("body^={long}")).unwrap(),
                rule::parse("body^=comment").unwrap(),
            ],
            ..args(&[URL], &sandbox.snapshot(), Some(0))
        };

        let (code, envelope) = watch(&brief, vec![returns(URL, next)]);

        assert_eq!(code, 0);
        let [cut, short] = &envelope["events"].as_array().unwrap()[..] else {
            panic!("expected two events");
        };
        assert_eq!(
            cut["body"],
            format!("{}…", "a".repeat(BRIEF_BODY_CHARS - 1))
        );
        assert_eq!(cut["truncated"], true);
        assert_eq!(short["body"], "comment 3");
        assert_eq!(short.get("truncated"), None);
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
        let clock = FakeClock(Cell::new(start()));
        let backend = FakeBackend {
            clock: &clock,
            log: RefCell::default(),
            polls: RefCell::default(),
            resolves: RefCell::default(),
            viewers: RefCell::default(),
        };
        let mut stdout = Vec::new();

        let code = run(
            &args,
            &Backends {
                github: &backend,
                gitlab: &backend,
                slack: &backend,
            },
            &exec,
            &clock,
            &mut stdout,
            &mut Vec::new(),
        );

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

    fn seconds(traced: &Traced) -> Vec<u64> {
        traced.polls.iter().map(|(_, second)| *second).collect()
    }

    fn woke(traced: &Traced) -> bool {
        traced.code == 0 && traced.envelope["outcome"] == "woke"
    }

    fn retry_after(seconds: u64) -> Hints {
        Hints {
            retry_after: Some(Duration::from_secs(seconds)),
            ..Hints::default()
        }
    }

    #[test]
    fn ten_consecutive_failed_polls_give_up_naming_the_target() {
        let sandbox = Sandbox::new("give-up");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let before = saved(&sandbox.snapshot());
        let failures = (1..=10)
            .map(|attempt| fails(URL, transient(&format!("HTTP 502: attempt {attempt}"))))
            .collect();

        let traced = trace(&args(&[URL], &sandbox.snapshot(), None), failures);

        assert_eq!(traced.code, 1);
        assert_eq!(
            traced.envelope["error"],
            json!({
                "kind": "poll_failed",
                "forge": "github",
                "target": URL,
                "message": "HTTP 502: attempt 10",
            }),
        );
        assert_eq!(
            seconds(&traced),
            [0, 30, 60, 90, 120, 150, 180, 210, 240, 270]
        );
        assert_eq!(traced.stderr.lines().count(), 9);
        assert_eq!(
            traced.stderr.lines().next().unwrap(),
            format!("babysit: poll failed for {URL}, retrying in 30s: HTTP 502: attempt 1"),
        );
        assert_eq!(saved(&sandbox.snapshot()), before);
    }

    #[test]
    fn a_successful_poll_resets_the_failure_count() {
        let sandbox = Sandbox::new("reset-count");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let streak = || (0..9).map(|_| fails(URL, transient("HTTP 502")));
        let polls = streak()
            .chain([returns(URL, poll(&["1"]))])
            .chain(streak())
            .chain([returns(URL, poll(&["1", "2"]))])
            .collect();

        let traced = trace(&args(&[URL], &sandbox.snapshot(), None), polls);

        assert!(woke(&traced), "{}", traced.envelope);
    }

    #[test]
    fn a_wait_starts_once_a_slow_poll_answers() {
        let sandbox = Sandbox::new("slow-poll");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let unavailable = Failure::Transient {
            forge: Forge::GitHub,
            hints: retry_after(60),
            message: "HTTP 503".to_owned(),
        };

        let traced = trace(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![
                slow(URL, 40, Err(unavailable)),
                returns(URL, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [0, 100]);
    }

    #[test]
    fn resolution_retries_a_transient_failure_and_waits_out_a_rate_limit() {
        let sandbox = Sandbox::new("startup-retry");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let startup = Startup {
            resolves: vec![
                Err(transient("HTTP 502")),
                Err(rate_limited(retry_after(100))),
            ],
            ..Startup::default()
        };

        let traced = trace_starting(
            &args(&[URL], &sandbox.snapshot(), None),
            startup,
            vec![returns(URL, poll(&["1", "2"]))],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [130]);
        assert_eq!(
            traced.stderr,
            format!(
                "babysit: resolve failed for {URL}, retrying in 30s: HTTP 502\n\
                 babysit: rate limited on github.com, waiting until 2026-10-20T19:21:27Z\n"
            ),
        );
    }

    #[test]
    fn ten_failed_resolutions_give_up_naming_the_target_as_given() {
        let sandbox = Sandbox::new("startup-give-up");
        let startup = Startup {
            resolves: (0..10).map(|_| Err(transient("HTTP 502"))).collect(),
            ..Startup::default()
        };

        let traced = trace_starting(&args(&[URL], &sandbox.snapshot(), None), startup, vec![]);

        assert_eq!(traced.code, 1);
        assert_eq!(traced.envelope["error"]["kind"], "poll_failed");
        assert_eq!(traced.envelope["error"]["target"], URL);
        assert_eq!(traced.ended, 270);
    }

    #[test]
    fn a_resolution_rate_limited_past_the_deadline_times_out() {
        let sandbox = Sandbox::new("startup-timeout");

        let startup = Startup {
            resolves: vec![Err(rate_limited(retry_after(100)))],
            ..Startup::default()
        };

        let traced = trace_starting(
            &args(&[URL], &sandbox.snapshot(), Some(60)),
            startup,
            vec![],
        );

        assert_eq!(traced.code, 3);
        assert_eq!(traced.envelope, timed_out(&[]));
        assert_eq!(traced.ended, 0);
    }

    #[test]
    fn other_hosts_resolve_and_poll_while_one_waits_out_a_startup_rate_limit() {
        let sandbox = Sandbox::new("startup-other-hosts");
        seed(&sandbox.snapshot(), ENTERPRISE, "PR_3", &["1"]);
        let startup = Startup {
            resolves: vec![Err(rate_limited(retry_after(3600))), Ok(Hints::default())],
            ..Startup::default()
        };

        let traced = trace_starting(
            &args(&[URL, ENTERPRISE], &sandbox.snapshot(), Some(60)),
            startup,
            vec![
                returns(ENTERPRISE, poll(&["1"])),
                returns(ENTERPRISE, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced), "{}", traced.envelope);
        assert_eq!(seconds(&traced), [0, 30]);
        assert_eq!(traced.envelope["targets"], json!([summary(ENTERPRISE)]));
    }

    #[test]
    fn a_name_resolving_late_to_a_tracked_url_moves_it_to_its_first_occurrence() {
        let sandbox = Sandbox::new("late-merge");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        seed(&sandbox.snapshot(), OTHER, "PR_2", &["1"]);
        let startup = Startup {
            resolves: vec![
                Err(transient("HTTP 502")),
                Ok(Hints::default()),
                Ok(Hints::default()),
                Ok(Hints::default()),
            ],
            ..Startup::default()
        };

        let traced = trace_starting(
            &args(
                &["http://github.com/o/r/pull/2", URL, OTHER],
                &sandbox.snapshot(),
                None,
            ),
            startup,
            vec![
                returns(URL, poll(&["1"])),
                returns(OTHER, poll(&["1"])),
                returns(OTHER, poll(&["1", "3"])),
                returns(URL, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        let targets: Vec<_> = traced.envelope["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["target"].as_str().unwrap())
            .collect();
        assert_eq!(targets, [OTHER, URL]);
        assert_eq!(
            traced.envelope["targets"],
            json!([summary(OTHER), summary(URL)])
        );
    }

    #[test]
    fn a_resolution_that_spends_the_quota_delays_the_first_poll() {
        let sandbox = Sandbox::new("startup-spent");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let startup = Startup {
            resolves: vec![Ok(Hints {
                reset: Some(at(300)),
                ..Hints::default()
            })],
            ..Startup::default()
        };

        let traced = trace_starting(
            &args(&[URL], &sandbox.snapshot(), None),
            startup,
            vec![returns(URL, poll(&["1", "2"]))],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [300]);
    }

    #[test]
    fn a_resolution_asking_for_a_retry_after_delays_the_first_poll() {
        let sandbox = Sandbox::new("startup-retry-after");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let startup = Startup {
            resolves: vec![Ok(retry_after(90))],
            ..Startup::default()
        };

        let traced = trace_starting(
            &args(&[URL], &sandbox.snapshot(), None),
            startup,
            vec![returns(URL, poll(&["1", "2"]))],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [90]);
        assert_eq!(traced.stderr, "");
    }

    #[test]
    fn timeout_zero_polls_every_target_once_after_a_poll_spends_the_quota() {
        let sandbox = Sandbox::new("once-spent");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        seed(&sandbox.snapshot(), OTHER, "PR_2", &["1"]);
        let spent = Hints {
            reset: Some(at(300)),
            ..Hints::default()
        };

        let traced = trace(
            &args(&[URL, OTHER], &sandbox.snapshot(), Some(0)),
            vec![
                paced(URL, poll(&["1"]), spent),
                returns(OTHER, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        assert_eq!(traced.envelope["events"][0]["target"], OTHER);
    }

    #[test]
    fn timeout_zero_polls_past_a_rate_limited_target_and_wakes() {
        let sandbox = Sandbox::new("once-limited-wake");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        seed(&sandbox.snapshot(), OTHER, "PR_2", &["1"]);

        let traced = trace(
            &args(&[URL, OTHER], &sandbox.snapshot(), Some(0)),
            vec![
                fails(URL, rate_limited(retry_after(60))),
                returns(OTHER, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        assert_eq!(traced.stderr, "");
    }

    #[test]
    fn timeout_zero_with_only_a_rate_limit_times_out_after_polling_every_target() {
        let sandbox = Sandbox::new("once-limited-quiet");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        seed(&sandbox.snapshot(), OTHER, "PR_2", &["1"]);

        let traced = trace(
            &args(&[URL, OTHER], &sandbox.snapshot(), Some(0)),
            vec![
                fails(URL, rate_limited(retry_after(60))),
                returns(OTHER, poll(&["1"])),
            ],
        );

        assert_eq!(traced.code, 3);
        assert_eq!(seconds(&traced), [0, 0]);
    }

    #[test]
    fn timeout_zero_polls_once_whatever_a_resolution_asks_to_wait() {
        let sandbox = Sandbox::new("startup-once");
        let startup = Startup {
            resolves: vec![Ok(retry_after(90))],
            ..Startup::default()
        };

        let traced = trace_starting(
            &args(&[URL], &sandbox.snapshot(), Some(0)),
            startup,
            vec![returns(URL, poll(&["1"]))],
        );

        assert_eq!(traced.code, 3);
        assert_eq!(seconds(&traced), [0]);
        assert_eq!(
            saved(&sandbox.snapshot())["targets"][URL]["snapshot"],
            json!(poll(&["1"]))
        );
    }

    #[test]
    fn other_hosts_poll_while_one_waits_out_a_rate_limited_viewer_lookup() {
        let sandbox = Sandbox::new("viewer-other-hosts");
        seed(&sandbox.snapshot(), ENTERPRISE, "PR_3", &["1"]);
        let mine = Args {
            on: vec![rule::parse("actor=@me").unwrap()],
            ..args(&[URL, ENTERPRISE], &sandbox.snapshot(), Some(60))
        };
        let mut next = poll(&["1", "2"]);
        next.comments[1].actor = Some(Account {
            id: Some(VIEWER_ID.to_owned()),
            ..octocat()
        });
        let startup = Startup {
            viewers: vec![Err(rate_limited(retry_after(3600))), Ok(Hints::default())],
            ..Startup::default()
        };

        let traced = trace_starting(
            &mine,
            startup,
            vec![returns(ENTERPRISE, poll(&["1"])), returns(ENTERPRISE, next)],
        );

        assert!(woke(&traced), "{}", traced.envelope);
        assert_eq!(seconds(&traced), [0, 30]);
    }

    #[test]
    fn a_failed_viewer_lookup_names_the_resolved_target() {
        let sandbox = Sandbox::new("viewer-failed");
        let mine = Args {
            on: vec![rule::parse("actor=@me").unwrap()],
            ..args(&["http://github.com/o/r/pull/1"], &sandbox.snapshot(), None)
        };
        let startup = Startup {
            viewers: vec![Err(auth_failure())],
            ..Startup::default()
        };

        let traced = trace_starting(&mine, startup, vec![]);

        assert_eq!(traced.code, 1);
        assert_eq!(traced.envelope["error"]["target"], URL);
    }

    #[test]
    fn the_viewer_lookup_waits_out_a_rate_limit() {
        let sandbox = Sandbox::new("viewer-limited");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let mine = Args {
            on: vec![rule::parse("actor=@me").unwrap()],
            ..args(&[URL], &sandbox.snapshot(), None)
        };
        let mut next = poll(&["1", "2"]);
        next.comments[1].actor = Some(Account {
            id: Some(VIEWER_ID.to_owned()),
            ..octocat()
        });
        let startup = Startup {
            viewers: vec![Err(rate_limited(retry_after(100)))],
            ..Startup::default()
        };

        let traced = trace_starting(&mine, startup, vec![returns(URL, next)]);

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [100]);
    }

    #[test]
    fn a_startup_success_leaves_the_backoff_to_a_complete_poll() {
        let sandbox = Sandbox::new("startup-backoff");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let startup = Startup {
            resolves: vec![Err(rate_limited(Hints::default()))],
            ..Startup::default()
        };

        let traced = trace_starting(
            &args(&[URL], &sandbox.snapshot(), None),
            startup,
            vec![
                fails(URL, rate_limited(Hints::default())),
                returns(URL, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [60, 180]);
    }

    #[test]
    fn a_retry_waits_out_a_longer_retry_after() {
        let sandbox = Sandbox::new("retry-after");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let unavailable = Failure::Transient {
            forge: Forge::GitHub,
            hints: retry_after(90),
            message: "HTTP 503".to_owned(),
        };

        let traced = trace(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![fails(URL, unavailable), returns(URL, poll(&["1", "2"]))],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [0, 90]);
        assert_eq!(
            traced.stderr,
            format!("babysit: poll failed for {URL}, retrying in 90s: HTTP 503\n")
        );
    }

    #[test]
    fn timeout_zero_with_a_failed_attempt_is_poll_failed() {
        let sandbox = Sandbox::new("timeout-zero-failed");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let traced = trace(
            &args(&[URL], &sandbox.snapshot(), Some(0)),
            vec![fails(URL, transient("HTTP 502"))],
        );

        assert_eq!(traced.code, 1);
        assert_eq!(traced.envelope["error"]["kind"], "poll_failed");
        assert_eq!(traced.stderr, "");
    }

    #[test]
    fn a_timeout_during_a_failure_streak_is_timed_out() {
        let sandbox = Sandbox::new("streak-timeout");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let traced = trace(
            &args(&[URL], &sandbox.snapshot(), Some(45)),
            vec![
                fails(URL, transient("HTTP 502")),
                fails(URL, transient("HTTP 502")),
            ],
        );

        assert_eq!(traced.code, 3);
        assert_eq!(traced.envelope, timed_out(&[]));
    }

    #[test]
    fn an_unhinted_rate_limit_waits_60s_doubling_to_15_minutes_until_a_success() {
        let sandbox = Sandbox::new("doubling");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let limited = || fails(URL, rate_limited(Hints::default()));
        let polls = (0..5)
            .map(|_| limited())
            .chain([returns(URL, poll(&["1"])), limited()])
            .chain([returns(URL, poll(&["1", "2"]))])
            .collect();

        let traced = trace(&args(&[URL], &sandbox.snapshot(), None), polls);

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [0, 60, 180, 420, 900, 1800, 1830, 1890]);
        assert_eq!(traced.stderr.lines().count(), 6);
        assert_eq!(
            traced.stderr.lines().next().unwrap(),
            "babysit: rate limited on github.com, waiting until 2026-10-20T19:20:17Z",
        );
    }

    #[test]
    fn a_rate_limit_that_names_its_wait_leaves_the_backoff_at_60s() {
        let sandbox = Sandbox::new("named-wait");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let traced = trace(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![
                fails(URL, rate_limited(retry_after(120))),
                fails(URL, rate_limited(Hints::default())),
                returns(URL, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [0, 120, 180]);
    }

    #[test]
    fn a_rate_limit_waits_for_retry_after_before_the_quota_reset() {
        let sandbox = Sandbox::new("rate-limit-hints");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let reset = Hints {
            reset: Some(at(500)),
            ..Hints::default()
        };

        let traced = trace(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![
                fails(
                    URL,
                    rate_limited(Hints {
                        reset: reset.reset,
                        ..retry_after(100)
                    }),
                ),
                fails(URL, rate_limited(reset)),
                returns(URL, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [0, 100, 500]);
    }

    #[test]
    fn other_hosts_keep_polling_through_a_rate_limit() {
        let sandbox = Sandbox::new("other-hosts");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        seed(&sandbox.snapshot(), ENTERPRISE, "PR_3", &["1"]);
        let quiet = || returns(ENTERPRISE, poll(&["1"]));

        let traced = trace(
            &args(&[URL, ENTERPRISE], &sandbox.snapshot(), None),
            vec![
                fails(URL, rate_limited(retry_after(100))),
                quiet(),
                quiet(),
                quiet(),
                quiet(),
                returns(URL, poll(&["1"])),
                returns(ENTERPRISE, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        let polls: Vec<_> = traced
            .polls
            .iter()
            .map(|(url, second)| (url.as_str(), *second))
            .collect();
        assert_eq!(
            polls,
            [
                (URL, 0),
                (ENTERPRISE, 0),
                (ENTERPRISE, 30),
                (ENTERPRISE, 60),
                (ENTERPRISE, 90),
                (URL, 100),
                (ENTERPRISE, 120),
            ],
        );
    }

    #[test]
    fn a_poll_paces_the_next_by_its_interval_and_a_spent_quota() {
        let sandbox = Sandbox::new("paced");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);
        let slower = Hints {
            poll_interval: Some(Duration::from_secs(45)),
            ..Hints::default()
        };
        let spent = Hints {
            reset: Some(at(300)),
            ..Hints::default()
        };

        let traced = trace(
            &args(&[URL], &sandbox.snapshot(), None),
            vec![
                paced(URL, poll(&["1"]), slower),
                paced(URL, poll(&["1"]), spent),
                returns(URL, poll(&["1", "2"])),
            ],
        );

        assert!(woke(&traced));
        assert_eq!(seconds(&traced), [0, 45, 300]);
        assert_eq!(
            traced.stderr,
            "babysit: rate limited on github.com, waiting until 2026-10-20T19:24:17Z\n"
        );
    }

    #[test]
    fn the_run_times_out_early_once_every_target_is_due_past_the_deadline() {
        let sandbox = Sandbox::new("early-timeout");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let traced = trace(
            &args(&[URL], &sandbox.snapshot(), Some(600)),
            vec![fails(URL, rate_limited(retry_after(900)))],
        );

        assert_eq!(traced.code, 3);
        assert_eq!(traced.envelope, timed_out(&[]));
        assert_eq!(traced.ended, 0);
    }

    #[test]
    fn a_wake_beats_a_deadline_killing_a_later_poll() {
        let sandbox = Sandbox::new("wake-over-timeout");
        seed(&sandbox.snapshot(), URL, NODE_ID, &["1"]);

        let traced = trace(
            &args(&[URL, OTHER], &sandbox.snapshot(), Some(600)),
            vec![
                returns(URL, poll(&["1", "2"])),
                fails(OTHER, Failure::TimedOut),
            ],
        );

        assert!(woke(&traced));
        assert_eq!(
            saved(&sandbox.snapshot())["targets"][URL]["snapshot"],
            json!(poll(&["1", "2"]))
        );
    }
}
