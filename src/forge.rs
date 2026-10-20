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
    /// The account id of whoever the forge's CLI is signed in as on `host`.
    fn viewer(&self, host: &str, deadline: Option<SystemTime>) -> Result<String, Failure>;
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
    /// `None` when the head has no CI at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci: Option<Ci>,
    /// The comments outside any thread.
    pub comments: Vec<Comment>,
    pub head: String,
    pub reviews: Vec<Review>,
    pub state: TargetState,
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
    /// The colour last reported for the snapshot's head.
    pub colour: Option<Colour>,
}

impl Reported {
    fn is_empty(&self) -> bool {
        self.colour.is_none()
    }
}

/// A first poll as baseline: whatever colour its head already shows counts as reported.
impl From<Poll> for Snapshot {
    fn from(poll: Poll) -> Self {
        Self {
            reported: Reported {
                colour: poll.ci.as_ref().and_then(Ci::colour),
            },
            poll,
        }
    }
}

/// The current head's checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ci {
    pub checks: Vec<Check>,
    /// Whether every required check is visible, so a missing one can't pass for green.
    pub required_known: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub assessment: Assessment,
    /// The forge's own word for the check's outcome or status.
    pub conclusion: String,
    pub deciding: bool,
    pub job_id: Option<String>,
    pub name: String,
    pub url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

impl Ci {
    /// Red on the first deciding failure; green or amber only once every deciding check settles.
    pub fn colour(&self) -> Option<Colour> {
        let deciding: Vec<_> = self.checks.iter().filter(|check| check.deciding).collect();
        let any = |assessment| deciding.iter().any(|check| check.assessment == assessment);
        if any(Assessment::Failed) {
            return Some(Colour::Red);
        }
        if deciding.is_empty() || any(Assessment::Pending) {
            return None;
        }
        if any(Assessment::Waiting) {
            return Some(Colour::Amber);
        }
        self.required_known.then_some(Colour::Green)
    }
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
    pub review_id: Option<String>,
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
    pub line: Option<u64>,
    pub path: String,
    /// Where a reply goes, in the form the forge's reply call takes.
    pub reply_to: String,
    pub resolved: bool,
    pub resolved_by: Option<Account>,
    pub url: String,
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
        review_id: None,
        url: format!("https://github.com/o/r/pull/1#issuecomment-{id}"),
    }
}

#[cfg(test)]
pub fn poll(ids: &[&str]) -> Poll {
    Poll {
        ci: None,
        comments: ids.iter().map(|id| comment(id)).collect(),
        head: "abc123".to_owned(),
        reviews: Vec::new(),
        state: TargetState::Open,
        threads: Vec::new(),
        url: "https://github.com/o/r/pull/1".to_owned(),
    }
}
