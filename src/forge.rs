use std::io;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::envelope::{ErrorKind, Forge};
use crate::exec::{self, CALL_LIMIT, Exec, Limit, Output};
use crate::reference::Reference;

const EXIT_NOT_FOUND: i32 = 127;

/// A forge's view of targets. Every call ends by the run's wall-clock `deadline`, if any.
pub trait Backend {
    fn resolve(
        &self,
        reference: &Reference,
        deadline: Option<SystemTime>,
    ) -> Result<(Target, Hints), Failure>;
    fn poll(&self, target: &Target, deadline: Option<SystemTime>)
    -> Result<(Poll, Hints), Failure>;
    /// The account id of whoever the forge's CLI is signed in as on `host`.
    fn viewer(&self, host: &str, deadline: Option<SystemTime>) -> Result<(String, Hints), Failure>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub forge: Forge,
    pub forge_id: String,
    pub host: String,
    pub kind: TargetKind,
    /// The forge-reported web URL: the target's identity for the run.
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// A GitHub pull request or GitLab merge request.
    ChangeRequest,
    /// An issue, or a Slack thread, which has only comments.
    Issue,
}

#[derive(Debug)]
pub enum Failure {
    Fatal {
        forge: Forge,
        kind: ErrorKind,
        message: String,
    },
    RateLimited {
        forge: Forge,
        hints: Hints,
    },
    /// The run's deadline killed the call.
    TimedOut,
    Transient {
        forge: Forge,
        hints: Hints,
        message: String,
    },
}

/// How the forge asks to be paced, read off its replies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hints {
    pub poll_interval: Option<Duration>,
    /// When the host's spent quota refills.
    pub reset: Option<SystemTime>,
    pub retry_after: Option<Duration>,
}

/// Runs a forge's CLI, failing where it is missing, can't start, or was killed at a limit.
pub fn run_cli(
    exec: &impl Exec,
    forge: Forge,
    cli: &str,
    args: &[&str],
    deadline: Option<SystemTime>,
) -> Result<Output, Failure> {
    let output = match exec.run(cli, args, deadline) {
        Ok(output) => output,
        Err(exec::Error::TimedOut(Limit::Deadline)) => return Err(Failure::TimedOut),
        Err(exec::Error::TimedOut(Limit::CallLimit)) => {
            let message = format!("{cli} did not answer within {}s", CALL_LIMIT.as_secs());
            return Err(Failure::transient(forge, &message));
        }
        Err(exec::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            let message = format!("{cli} is not installed or not on PATH");
            return Err(Failure::fatal(forge, ErrorKind::CliMissing, &message));
        }
        Err(exec::Error::Io(error)) => {
            let message = format!("could not run {cli}: {error}");
            return Err(Failure::transient(forge, &message));
        }
    };
    if output.code == Some(EXIT_NOT_FOUND) {
        let reason = output.stderr.trim();
        return Err(Failure::fatal(forge, ErrorKind::CliMissing, reason));
    }
    Ok(output)
}

impl Failure {
    pub fn fatal(forge: Forge, kind: ErrorKind, message: &str) -> Self {
        Self::Fatal {
            forge,
            kind,
            message: message.to_owned(),
        }
    }

    pub fn transient(forge: Forge, message: &str) -> Self {
        Self::Transient {
            forge,
            hints: Hints::default(),
            message: message.to_owned(),
        }
    }

    /// The failure with `hints` folded in, where it is to be retried.
    pub fn paced(self, hints: Hints) -> Self {
        match self {
            Self::Transient {
                forge,
                hints: own,
                message,
            } => Self::Transient {
                forge,
                hints: own.merge(hints),
                message,
            },
            other => other,
        }
    }
}

impl Hints {
    /// The stricter of the two for each hint, across the calls of one poll.
    pub fn merge(self, other: Self) -> Self {
        Self {
            poll_interval: self.poll_interval.max(other.poll_interval),
            reset: self.reset.max(other.reset),
            retry_after: self.retry_after.max(other.retry_after),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Poll {
    /// `None` when the head has no CI at all; snapshots keep only the colour reported for it.
    #[serde(skip)]
    pub ci: Option<Ci>,
    /// The comments outside any thread.
    pub comments: Vec<Comment>,
    /// `None` for an issue, which has no commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// `None` while the forge is still computing it; snapshots keep the last known value.
    #[serde(skip)]
    pub merge: Option<Merge>,
    pub reviews: Vec<Review>,
    pub state: TargetState,
    /// When the target entered its state, where the forge records it.
    #[serde(skip)]
    pub state_changed_at: Option<String>,
    #[serde(skip)]
    pub state_changed_by: Option<Account>,
    pub threads: Vec<Thread>,
    /// The target's URL; snapshots leave it out, since events come only from fresh polls.
    #[serde(skip)]
    pub url: String,
}

/// A poll as last evaluated, with what has been reported about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(flatten)]
    pub poll: Poll,
    #[serde(default, skip_serializing_if = "Reported::is_empty")]
    pub reported: Reported,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reported {
    /// The colour reported for the snapshot's head, kept only while its CI stays settled.
    pub colour: Option<Colour>,
    /// The last known mergeability, frozen while the target isn't open.
    pub merge: Option<Merge>,
}

impl Reported {
    fn is_empty(&self) -> bool {
        self.colour.is_none() && self.merge.is_none()
    }
}

/// The current head's checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ci {
    pub checks: Vec<Check>,
    /// Whether every required check is visible, so a missing one can't pass for green.
    pub required_known: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub assessment: Assessment,
    /// The forge's own word for the check's outcome or status.
    pub conclusion: String,
    pub deciding: bool,
    pub job_id: Option<String>,
    pub name: String,
    pub url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assessment {
    Failed,
    Passed,
    Pending,
    /// Settled short of passing until someone acts: amber.
    Waiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Colour {
    Amber,
    Green,
    Red,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Merge {
    /// The forge requires the head to be brought up to date before it merges.
    Behind,
    Clean,
    Conflicted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetState {
    Closed,
    Merged,
    Open,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub actor: Option<Account>,
    pub at: String,
    pub body: String,
    pub edited_at: Option<String>,
    pub editor: Option<Account>,
    pub id: String,
    /// Where a reply goes, for a comment outside any thread the forge still takes replies to.
    #[serde(default)]
    pub reply_to: Option<String>,
    pub review_id: Option<String>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub actor: Option<Account>,
    /// `None` where the forge doesn't record when the review was given.
    pub at: Option<String>,
    pub body: String,
    /// The forge's own id for the review, where it has one.
    #[serde(default)]
    pub forge_id: Option<String>,
    /// Identifies the review across polls.
    pub id: String,
    /// Whether the review is withdrawn by the forge no longer listing it.
    #[serde(default)]
    pub lapses: bool,
    pub state: ReviewState,
    pub url: String,
    pub withdrawn_at: Option<String>,
    pub withdrawn_by: Option<Account>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub comments: Vec<Comment>,
    pub id: String,
    /// `None` for a thread on the target as a whole rather than on a diff.
    #[serde(flatten)]
    pub position: Option<Position>,
    /// Where a reply goes, in the form the forge's reply call takes.
    pub reply_to: String,
    pub resolved: bool,
    pub resolved_by: Option<Account>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub line: Option<u64>,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// `None` where the forge doesn't say.
    pub bot: Option<bool>,
    /// The forge's permanent account id; snapshots leave it out, since events come only from
    /// fresh polls.
    #[serde(skip)]
    pub id: Option<String>,
    pub login: String,
}

#[cfg(test)]
pub fn fatal_kind(failure: &Failure) -> ErrorKind {
    match failure {
        Failure::Fatal { kind, .. } => *kind,
        other => panic!("not fatal: {other:?}"),
    }
}

#[cfg(test)]
pub fn octocat() -> Account {
    Account {
        bot: None,
        id: None,
        login: "octocat".to_owned(),
    }
}

#[cfg(test)]
pub fn comment(id: &str) -> Comment {
    Comment {
        actor: Some(octocat()),
        at: "2026-10-20T19:00:00Z".to_owned(),
        body: format!("comment {id}"),
        edited_at: None,
        editor: None,
        id: id.to_owned(),
        reply_to: None,
        review_id: None,
        url: format!("https://github.com/o/r/pull/1#issuecomment-{id}"),
    }
}

#[cfg(test)]
pub fn poll(ids: &[&str]) -> Poll {
    Poll {
        ci: None,
        comments: ids.iter().map(|id| comment(id)).collect(),
        head: Some("abc123".to_owned()),
        merge: None,
        reviews: Vec::new(),
        state: TargetState::Open,
        state_changed_at: None,
        state_changed_by: None,
        threads: Vec::new(),
        url: "https://github.com/o/r/pull/1".to_owned(),
    }
}
