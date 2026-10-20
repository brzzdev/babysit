use std::io::{self, Write};

use serde::Serialize;

use crate::diff;
use crate::forge::{Colour, Failure, Merge, Snapshot, TargetState};

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
    pub head: String,
    pub colour: Option<Colour>,
    pub merge: Option<Merge>,
    pub unresolved_threads: Option<usize>,
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
    pub fn new(target: &str, snapshot: &Snapshot) -> Self {
        Self {
            target: target.to_owned(),
            state: snapshot.poll.state,
            head: snapshot.poll.head.clone(),
            colour: snapshot.reported.colour,
            merge: snapshot
                .reported
                .merge
                .filter(|_| snapshot.poll.state == TargetState::Open),
            unresolved_threads: Some(
                snapshot
                    .poll
                    .threads
                    .iter()
                    .filter(|thread| !thread.resolved)
                    .count(),
            ),
        }
    }
}
