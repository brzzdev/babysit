use std::io::{self, Write};

use serde::Serialize;

use crate::diff;
use crate::forge::{Colour, Failure, Merge, Snapshot, Target, TargetKind, TargetState};

pub const EXIT_ERROR: u8 = 1;
const EXIT_TIMED_OUT: u8 = 3;
const EXIT_WOKE: u8 = 0;

/// What a run prints on stdout once it ends past validation.
#[derive(Debug, Serialize)]
pub struct Envelope {
    pub outcome: Outcome,
    pub events: Vec<Event>,
    pub targets: Vec<Summary>,
    pub error: Option<Error>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Error,
    TimedOut,
    Woke,
}

#[derive(Debug, Serialize)]
pub struct Event {
    #[serde(flatten)]
    pub seen: diff::Event,
    pub target: String,
    pub observed_at: String,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub target: String,
    pub state: TargetState,
    /// `None` for an issue.
    #[serde(flatten)]
    pub change: Option<ChangeSummary>,
}

#[derive(Debug, Serialize)]
pub struct ChangeSummary {
    pub head: Option<String>,
    pub colour: Option<Colour>,
    pub merge: Option<Merge>,
    pub unresolved_threads: usize,
}

#[derive(Debug, Serialize)]
pub struct Error {
    pub kind: ErrorKind,
    pub forge: Option<Forge>,
    /// The target's URL, or the name as given before it resolved.
    pub target: Option<String>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Auth,
    CliMissing,
    LockHeld,
    PollFailed,
    SnapshotIo,
    TargetNotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Forge {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
    #[serde(rename = "slack")]
    Slack,
}

impl Envelope {
    pub fn ended(outcome: Outcome) -> Self {
        Self {
            outcome,
            events: Vec::new(),
            targets: Vec::new(),
            error: None,
        }
    }

    pub fn failed(error: Error) -> Self {
        Self {
            error: Some(error),
            ..Self::ended(Outcome::Error)
        }
    }

    pub fn from_failure(failure: Failure, target: &str) -> Self {
        let Some((forge, kind, message)) = failure.into_error() else {
            return Self::ended(Outcome::TimedOut);
        };
        Self::failed(Error {
            kind,
            forge: Some(forge),
            target: Some(target.to_owned()),
            message,
        })
    }

    pub fn exit_code(&self) -> u8 {
        match self.outcome {
            Outcome::Error => EXIT_ERROR,
            Outcome::TimedOut => EXIT_TIMED_OUT,
            Outcome::Woke => EXIT_WOKE,
        }
    }

    pub fn write(&self, out: &mut impl Write) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut *out, self)?;
        writeln!(out)?;
        out.flush()
    }
}

impl Failure {
    /// The error that ends a run on this failure, or `None` when the deadline killed the call.
    pub fn into_error(self) -> Option<(Forge, ErrorKind, String)> {
        match self {
            Failure::Fatal {
                forge,
                kind,
                message,
            } => Some((forge, kind, message)),
            Failure::RateLimited { forge, .. } => {
                Some((forge, ErrorKind::PollFailed, "rate limited".to_owned()))
            }
            Failure::TimedOut => None,
            Failure::Transient { forge, message, .. } => {
                Some((forge, ErrorKind::PollFailed, message))
            }
        }
    }
}

impl Summary {
    pub fn new(target: &Target, snapshot: &Snapshot) -> Self {
        let poll = &snapshot.poll;
        Self {
            target: target.url.clone(),
            state: poll.state,
            change: (target.kind == TargetKind::ChangeRequest).then(|| ChangeSummary {
                head: poll.head.clone(),
                colour: snapshot.reported.colour,
                merge: snapshot
                    .reported
                    .merge
                    .filter(|_| poll.state == TargetState::Open),
                unresolved_threads: poll
                    .threads
                    .iter()
                    .filter(|thread| !thread.resolved)
                    .count(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::forge::{Poll, poll};

    #[test]
    fn an_issue_summary_names_only_its_target_and_state() {
        let target = Target {
            forge: Forge::GitHub,
            forge_id: "I_1".to_owned(),
            host: "github.com".to_owned(),
            kind: TargetKind::Issue,
            url: "https://github.com/o/r/issues/1".to_owned(),
        };
        let issue = Poll {
            head: None,
            ..poll(&[])
        };

        let summary = Summary::new(&target, &issue.into());

        assert_eq!(
            serde_json::to_value(summary).unwrap(),
            json!({ "target": "https://github.com/o/r/issues/1", "state": "open" }),
        );
    }
}
