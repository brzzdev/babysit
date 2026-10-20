use serde::Serialize;

use crate::forge::{Account, Poll, Snapshot};

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Event {
    #[serde(flatten)]
    pub detail: Detail,
    pub url: String,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<Account>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind")]
pub enum Detail {
    #[serde(rename = "comment.created")]
    CommentCreated { comment_id: String, body: String },
}

/// The events between `baseline` and `poll`, and the snapshot that follows.
pub fn diff(baseline: &Snapshot, poll: Poll) -> (Vec<Event>, Snapshot) {
    let events = poll
        .comments
        .iter()
        .filter(|comment| !baseline.comments.iter().any(|seen| seen.id == comment.id))
        .map(|comment| Event {
            detail: Detail::CommentCreated {
                comment_id: comment.id.clone(),
                body: comment.body.clone(),
            },
            url: comment.url.clone(),
            at: comment.at.clone(),
            actor: comment.actor.clone(),
        })
        .collect();
    (events, poll)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::poll;

    #[test]
    fn a_new_comment_is_created() {
        let next = poll(&["1", "2"]);

        let (events, snapshot) = diff(&poll(&["1"]), next.clone());

        assert_eq!(
            events,
            [Event {
                detail: Detail::CommentCreated {
                    comment_id: "2".to_owned(),
                    body: "comment 2".to_owned(),
                },
                url: "https://github.com/o/r/pull/1#issuecomment-2".to_owned(),
                at: "2026-10-20T19:00:00Z".to_owned(),
                actor: Some(Account {
                    bot: None,
                    login: "octocat".to_owned(),
                }),
            }],
        );
        assert_eq!(snapshot, next);
    }

    #[test]
    fn seen_and_vanished_comments_are_quiet() {
        let (events, _) = diff(&poll(&["1", "2"]), poll(&["2"]));

        assert_eq!(events, []);
    }
}
