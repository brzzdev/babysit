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
    /// The comments outside any thread.
    pub comments: Vec<Comment>,
    pub head: String,
    pub reviews: Vec<Review>,
    pub state: TargetState,
    pub threads: Vec<Thread>,
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
    pub edited_at: Option<String>,
    pub editor: Option<Account>,
    pub id: String,
    pub review: Option<String>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub actor: Option<Account>,
    pub at: String,
    pub body: String,
    pub id: String,
    pub state: ReviewState,
    pub url: String,
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
    pub line: Option<u64>,
    pub path: Option<String>,
    /// Where a reply goes: the id of the thread's first comment.
    pub reply_to: String,
    pub resolved: bool,
    pub resolved_by: Option<Account>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// `None` where the forge doesn't say.
    pub bot: Option<bool>,
    pub login: String,
}

#[cfg(test)]
pub fn comment(id: &str) -> Comment {
    Comment {
        actor: Some(Account {
            bot: None,
            login: "octocat".to_owned(),
        }),
        at: "2026-10-20T19:00:00Z".to_owned(),
        body: format!("comment {id}"),
        edited_at: None,
        editor: None,
        id: id.to_owned(),
        review: None,
        url: format!("https://github.com/o/r/pull/1#issuecomment-{id}"),
    }
}

#[cfg(test)]
pub fn poll(ids: &[&str]) -> Poll {
    Poll {
        comments: ids.iter().map(|id| comment(id)).collect(),
        head: "abc123".to_owned(),
        reviews: Vec::new(),
        state: TargetState::Open,
        threads: Vec::new(),
    }
}
