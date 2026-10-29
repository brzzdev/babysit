use std::io::{self, Write};

use serde::Serialize;

use crate::diff;
use crate::forge::{Colour, Failure, Merge, Snapshot, Target, TargetKind, TargetState};

pub const BRIEF_BODY_CHARS: usize = 200;
pub const EXIT_ERROR: u8 = 1;
const EXIT_TIMED_OUT: u8 = 3;
const EXIT_WOKE: u8 = 0;
const HTML_COMMENT_CLOSE: &str = "-->";
const HTML_COMMENT_OPEN: &str = "<!--";

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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
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

/// Shortens the body in place, answering whether an HTML comment or the cut hid any of its text.
pub fn shorten(body: &mut String) -> bool {
    let mut visible = String::new();
    let mut rest = body.as_str();
    let mut hid_comment = false;
    while let Some(start) = rest.find(HTML_COMMENT_OPEN) {
        hid_comment = true;
        visible.push_str(&rest[..start]);
        let html_comment = &rest[start + HTML_COMMENT_OPEN.len()..];
        rest = html_comment
            .find(HTML_COMMENT_CLOSE)
            .map_or("", |end| &html_comment[end + HTML_COMMENT_CLOSE.len()..]);
    }
    visible.push_str(rest);
    *body = visible.split_whitespace().collect::<Vec<_>>().join(" ");
    if body.chars().count() <= BRIEF_BODY_CHARS {
        return hid_comment;
    }
    *body = body.chars().take(BRIEF_BODY_CHARS - 1).collect();
    body.push('…');
    true
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

    fn shortened(body: &str) -> (String, bool) {
        let mut body = body.to_owned();
        let truncated = shorten(&mut body);
        (body, truncated)
    }

    #[test]
    fn a_shortened_body_drops_html_comments_and_collapses_whitespace() {
        assert_eq!(
            shortened("  Fix\n\n<!-- hidden\n -->the  bug<!-- open"),
            ("Fix the bug".to_owned(), true),
        );
        assert_eq!(shortened("<!-- only a comment -->"), (String::new(), true));
        assert_eq!(
            shortened("LGTM,\r\n  thanks\n"),
            ("LGTM, thanks".to_owned(), false),
        );
        assert_eq!(
            shortened("Short and plain"),
            ("Short and plain".to_owned(), false),
        );
    }

    #[test]
    fn a_shortened_body_is_cut_by_characters_not_bytes() {
        let long = "é".repeat(BRIEF_BODY_CHARS + 1);
        let at_limit = &long[..2 * BRIEF_BODY_CHARS];

        assert_eq!(
            shortened(&long),
            (format!("{}…", "é".repeat(BRIEF_BODY_CHARS - 1)), true),
        );
        assert_eq!(shortened(at_limit), (at_limit.to_owned(), false));
    }

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
