use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::envelope::{ErrorKind, Forge};
use crate::reference::Reference;

/// A forge's view of targets. Every call ends by the run's wall-clock `deadline`, if any.
pub trait Backend {
    fn resolve(
        &self,
        reference: &Reference,
        deadline: Option<SystemTime>,
    ) -> Result<Target, Failure>;
    fn poll(&self, target: &Target, deadline: Option<SystemTime>) -> Result<Poll, Failure>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub forge_id: String,
    pub host: String,
    /// The forge-reported web URL: the target's identity for the run.
    pub url: String,
}

#[derive(Debug)]
pub enum Failure {
    Fatal {
        forge: Forge,
        kind: ErrorKind,
        message: String,
    },
    /// The run's deadline killed the call.
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Poll {
    pub comments: Vec<Comment>,
    pub head: String,
    pub state: TargetState,
}

pub type Snapshot = Poll;

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
    pub id: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// `None` where the forge doesn't say.
    pub bot: Option<bool>,
    pub login: String,
}
