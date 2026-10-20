use serde::Serialize;

use crate::forge::{Account, Comment, Poll, Review, ReviewState, Snapshot, Thread};

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Event {
    #[serde(flatten)]
    pub detail: Detail,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<Account>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<Account>,
}

/// Variants are in kind order, so the derived `Ord` sorts by kind, then by the leading id.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind")]
pub enum Detail {
    #[serde(rename = "comment.created")]
    CommentCreated { comment_id: String, body: String },
    #[serde(rename = "comment.edited")]
    CommentEdited {
        comment_id: String,
        body: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
    #[serde(rename = "review.submitted")]
    ReviewSubmitted {
        review_id: String,
        state: ReviewState,
        body: String,
    },
    #[serde(rename = "review.withdrawn")]
    ReviewWithdrawn { review_id: String },
    #[serde(rename = "thread.opened")]
    ThreadOpened {
        thread_id: String,
        comment_id: String,
        reply_to: String,
        body: String,
        path: String,
        line: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        review_id: Option<String>,
    },
    #[serde(rename = "thread.replied")]
    ThreadReplied {
        thread_id: String,
        comment_id: String,
        reply_to: String,
        body: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        review_id: Option<String>,
    },
    #[serde(rename = "thread.resolved")]
    ThreadResolved { thread_id: String, reply_to: String },
    #[serde(rename = "thread.unresolved")]
    ThreadUnresolved { thread_id: String, reply_to: String },
}

/// The events between `baseline` and `poll`, in report order, and the snapshot that follows.
pub fn diff(baseline: &Snapshot, poll: Poll) -> (Vec<Event>, Snapshot) {
    let mut events = Vec::new();
    let threaded = poll.threads.iter().flat_map(|thread| {
        thread
            .comments
            .iter()
            .map(move |comment| (Some(thread), comment))
    });
    for (thread, comment) in poll
        .comments
        .iter()
        .map(|comment| (None, comment))
        .chain(threaded)
    {
        // Top-level and thread comment ids are separate namespaces.
        let seen = match thread {
            None => baseline.comments.iter().find(|seen| seen.id == comment.id),
            Some(_) => baseline
                .threads
                .iter()
                .flat_map(|seen_thread| &seen_thread.comments)
                .find(|seen| seen.id == comment.id),
        };
        match (seen, thread) {
            (None, None) => events.push(comment_created(comment)),
            (None, Some(thread)) => events.push(thread_comment(thread, comment)),
            (Some(seen), _) if seen.body != comment.body => {
                events.push(comment_edited(comment, thread));
            }
            (Some(_), _) => {}
        }
    }
    for review in &poll.reviews {
        let seen = baseline.reviews.iter().find(|seen| seen.id == review.id);
        events.extend(review_change(seen, review));
    }
    for thread in &poll.threads {
        let was_resolved = baseline
            .threads
            .iter()
            .find(|seen| seen.id == thread.id)
            .is_some_and(|seen| seen.resolved);
        if thread.resolved != was_resolved {
            events.push(resolution(thread));
        }
    }
    events.sort_by(|a, b| {
        (a.at.is_none(), &a.at, &a.detail).cmp(&(b.at.is_none(), &b.at, &b.detail))
    });
    (events, poll)
}

fn comment_created(comment: &Comment) -> Event {
    Event {
        detail: Detail::CommentCreated {
            comment_id: comment.id.clone(),
            body: comment.body.clone(),
        },
        url: comment.url.clone(),
        at: Some(comment.at.clone()),
        actor: comment.actor.clone(),
        subject: None,
    }
}

fn comment_edited(comment: &Comment, thread: Option<&Thread>) -> Event {
    Event {
        detail: Detail::CommentEdited {
            comment_id: comment.id.clone(),
            body: comment.body.clone(),
            thread_id: thread.map(|thread| thread.id.clone()),
            reply_to: thread.map(|thread| thread.reply_to.clone()),
        },
        url: comment.url.clone(),
        at: comment.edited_at.clone(),
        actor: comment.editor.clone(),
        subject: None,
    }
}

/// A review first seen dismissed was submitted and withdrawn in between, so it reports nothing.
fn review_change(seen: Option<&Review>, review: &Review) -> Option<Event> {
    let withdrawn = review.state == ReviewState::Dismissed;
    match seen {
        None if !withdrawn => Some(Event {
            detail: Detail::ReviewSubmitted {
                review_id: review.id.clone(),
                state: review.state,
                body: review.body.clone(),
            },
            url: review.url.clone(),
            at: Some(review.at.clone()),
            actor: review.actor.clone(),
            subject: None,
        }),
        Some(seen) if withdrawn && seen.state != ReviewState::Dismissed => Some(Event {
            detail: Detail::ReviewWithdrawn {
                review_id: review.id.clone(),
            },
            url: review.url.clone(),
            at: review.withdrawn_at.clone(),
            actor: review.withdrawn_by.clone(),
            subject: review.actor.clone(),
        }),
        _ => None,
    }
}

fn thread_comment(thread: &Thread, comment: &Comment) -> Event {
    let opens = thread.comments.first() == Some(comment);
    let detail = if opens {
        Detail::ThreadOpened {
            thread_id: thread.id.clone(),
            comment_id: comment.id.clone(),
            reply_to: thread.reply_to.clone(),
            body: comment.body.clone(),
            path: thread.path.clone(),
            line: thread.line,
            review_id: comment.review_id.clone(),
        }
    } else {
        Detail::ThreadReplied {
            thread_id: thread.id.clone(),
            comment_id: comment.id.clone(),
            reply_to: thread.reply_to.clone(),
            body: comment.body.clone(),
            review_id: comment.review_id.clone(),
        }
    };
    Event {
        detail,
        url: comment.url.clone(),
        at: Some(comment.at.clone()),
        actor: comment.actor.clone(),
        subject: None,
    }
}

fn resolution(thread: &Thread) -> Event {
    let thread_id = thread.id.clone();
    let reply_to = thread.reply_to.clone();
    let (detail, actor) = if thread.resolved {
        (
            Detail::ThreadResolved {
                thread_id,
                reply_to,
            },
            thread.resolved_by.clone(),
        )
    } else {
        (
            Detail::ThreadUnresolved {
                thread_id,
                reply_to,
            },
            None,
        )
    };
    Event {
        detail,
        url: thread.url.clone(),
        at: None,
        actor,
        subject: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{comment, octocat, poll};

    const THREAD: &str = "PRRT_1";
    const THREAD_URL: &str = "https://github.com/o/r/pull/1#discussion_r10";

    fn reviewed(id: &str, review: &str) -> Comment {
        Comment {
            review_id: Some(review.to_owned()),
            at: "2026-10-20T19:05:00Z".to_owned(),
            url: format!("https://github.com/o/r/pull/1#discussion_r{id}"),
            ..comment(id)
        }
    }

    fn thread(comments: Vec<Comment>, resolved: bool) -> Thread {
        Thread {
            comments,
            id: THREAD.to_owned(),
            line: Some(84),
            path: "src/run.rs".to_owned(),
            reply_to: "10".to_owned(),
            resolved,
            resolved_by: resolved.then(octocat),
            url: THREAD_URL.to_owned(),
        }
    }

    fn review(state: ReviewState) -> Review {
        Review {
            actor: Some(octocat()),
            at: "2026-10-20T19:05:00Z".to_owned(),
            body: "Looks good".to_owned(),
            id: "500".to_owned(),
            state,
            url: "https://github.com/o/r/pull/1#pullrequestreview-500".to_owned(),
            withdrawn_at: None,
            withdrawn_by: None,
        }
    }

    fn with_threads(threads: Vec<Thread>) -> Poll {
        Poll {
            threads,
            ..poll(&[])
        }
    }

    fn with_reviews(reviews: Vec<Review>) -> Poll {
        Poll {
            reviews,
            ..poll(&[])
        }
    }

    fn details(baseline: &Snapshot, next: Poll) -> Vec<Detail> {
        diff(baseline, next)
            .0
            .into_iter()
            .map(|event| event.detail)
            .collect()
    }

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
                at: Some("2026-10-20T19:00:00Z".to_owned()),
                actor: Some(octocat()),
                subject: None,
            }],
        );
        assert_eq!(snapshot, next);
    }

    #[test]
    fn seen_and_vanished_comments_are_quiet() {
        let (events, _) = diff(&poll(&["1", "2"]), poll(&["2"]));

        assert_eq!(events, []);
    }

    #[test]
    fn a_changed_body_is_edited_by_its_editor() {
        let hubot = Account {
            bot: Some(false),
            login: "hubot".to_owned(),
        };
        let mut next = poll(&["1"]);
        next.comments[0].body = "comment 1, reworded".to_owned();
        next.comments[0].edited_at = Some("2026-10-20T19:30:00Z".to_owned());
        next.comments[0].editor = Some(hubot.clone());

        let (events, _) = diff(&poll(&["1"]), next);

        assert_eq!(
            events,
            [Event {
                detail: Detail::CommentEdited {
                    comment_id: "1".to_owned(),
                    body: "comment 1, reworded".to_owned(),
                    thread_id: None,
                    reply_to: None,
                },
                url: "https://github.com/o/r/pull/1#issuecomment-1".to_owned(),
                at: Some("2026-10-20T19:30:00Z".to_owned()),
                actor: Some(hubot),
                subject: None,
            }],
        );
    }

    #[test]
    fn an_edit_that_keeps_the_body_is_quiet() {
        let mut next = poll(&["1"]);
        next.comments[0].edited_at = Some("2026-10-20T19:30:00Z".to_owned());

        assert_eq!(details(&poll(&["1"]), next), []);
    }

    #[test]
    fn an_edited_thread_comment_names_its_thread() {
        let baseline = with_threads(vec![thread(vec![comment("10")], false)]);
        let mut edited = comment("10");
        edited.body = "reworded".to_owned();

        let events = details(&baseline, with_threads(vec![thread(vec![edited], false)]));

        assert_eq!(
            events,
            [Detail::CommentEdited {
                comment_id: "10".to_owned(),
                body: "reworded".to_owned(),
                thread_id: Some(THREAD.to_owned()),
                reply_to: Some("10".to_owned()),
            }],
        );
    }

    #[test]
    fn a_thread_comment_sharing_a_top_level_comments_id_is_its_own_comment() {
        let baseline = poll(&["10"]);
        let next = Poll {
            threads: vec![thread(
                vec![Comment {
                    body: "an inline comment".to_owned(),
                    ..reviewed("10", "500")
                }],
                false,
            )],
            ..poll(&["10"])
        };

        let opened = details(&baseline, next.clone());
        let unchanged = details(&next, next.clone());

        assert!(
            matches!(opened.as_slice(), [Detail::ThreadOpened { .. }]),
            "{opened:?}"
        );
        assert_eq!(unchanged, []);
    }

    #[test]
    fn a_review_submits_and_opens_and_replies_per_inline_comment() {
        let baseline = with_threads(vec![thread(vec![comment("10")], false)]);
        let mut next = with_threads(vec![
            thread(vec![comment("10"), reviewed("11", "500")], false),
            Thread {
                id: "PRRT_2".to_owned(),
                line: None,
                reply_to: "20".to_owned(),
                url: "https://github.com/o/r/pull/1#discussion_r20".to_owned(),
                ..thread(vec![reviewed("20", "500")], false)
            },
        ]);
        next.reviews = vec![review(ReviewState::ChangesRequested)];

        let (events, _) = diff(&baseline, next);

        assert_eq!(
            events,
            [
                Event {
                    detail: Detail::ReviewSubmitted {
                        review_id: "500".to_owned(),
                        state: ReviewState::ChangesRequested,
                        body: "Looks good".to_owned(),
                    },
                    url: "https://github.com/o/r/pull/1#pullrequestreview-500".to_owned(),
                    at: Some("2026-10-20T19:05:00Z".to_owned()),
                    actor: Some(octocat()),
                    subject: None,
                },
                Event {
                    detail: Detail::ThreadOpened {
                        thread_id: "PRRT_2".to_owned(),
                        comment_id: "20".to_owned(),
                        reply_to: "20".to_owned(),
                        body: "comment 20".to_owned(),
                        path: "src/run.rs".to_owned(),
                        line: None,
                        review_id: Some("500".to_owned()),
                    },
                    url: "https://github.com/o/r/pull/1#discussion_r20".to_owned(),
                    at: Some("2026-10-20T19:05:00Z".to_owned()),
                    actor: Some(octocat()),
                    subject: None,
                },
                Event {
                    detail: Detail::ThreadReplied {
                        thread_id: THREAD.to_owned(),
                        comment_id: "11".to_owned(),
                        reply_to: "10".to_owned(),
                        body: "comment 11".to_owned(),
                        review_id: Some("500".to_owned()),
                    },
                    url: "https://github.com/o/r/pull/1#discussion_r11".to_owned(),
                    at: Some("2026-10-20T19:05:00Z".to_owned()),
                    actor: Some(octocat()),
                    subject: None,
                },
            ],
        );
    }

    #[test]
    fn a_dismissal_withdraws_the_review_with_the_reviewer_as_subject() {
        let baseline = with_reviews(vec![review(ReviewState::Approved)]);
        let hubot = Account {
            bot: Some(false),
            login: "hubot".to_owned(),
        };
        let dismissed = Review {
            state: ReviewState::Dismissed,
            withdrawn_at: Some("2026-10-20T19:40:00Z".to_owned()),
            withdrawn_by: Some(hubot.clone()),
            ..review(ReviewState::Approved)
        };

        let (events, _) = diff(&baseline, with_reviews(vec![dismissed]));

        assert_eq!(
            events,
            [Event {
                detail: Detail::ReviewWithdrawn {
                    review_id: "500".to_owned(),
                },
                url: "https://github.com/o/r/pull/1#pullrequestreview-500".to_owned(),
                at: Some("2026-10-20T19:40:00Z".to_owned()),
                actor: Some(hubot),
                subject: Some(octocat()),
            }],
        );
    }

    #[test]
    fn a_review_submitted_and_dismissed_between_polls_is_quiet() {
        let next = with_reviews(vec![review(ReviewState::Dismissed)]);

        assert_eq!(details(&poll(&[]), next.clone()), []);
        assert_eq!(details(&next, next.clone()), []);
    }

    #[test]
    fn resolving_and_unresolving_a_thread_report_untimed_and_last() {
        let open = with_threads(vec![thread(vec![comment("10")], false)]);
        let mut resolved = with_threads(vec![thread(vec![comment("10")], true)]);
        resolved.comments = vec![comment("1")];

        let (events, _) = diff(&open, resolved.clone());
        let unresolved = details(&resolved, open);

        assert_eq!(
            events.iter().map(|event| &event.detail).collect::<Vec<_>>(),
            [
                &Detail::CommentCreated {
                    comment_id: "1".to_owned(),
                    body: "comment 1".to_owned(),
                },
                &Detail::ThreadResolved {
                    thread_id: THREAD.to_owned(),
                    reply_to: "10".to_owned(),
                },
            ],
        );
        assert_eq!(
            (&events[1].url, &events[1].at, &events[1].actor),
            (&THREAD_URL.to_owned(), &None, &Some(octocat()))
        );
        assert_eq!(
            unresolved,
            [Detail::ThreadUnresolved {
                thread_id: THREAD.to_owned(),
                reply_to: "10".to_owned(),
            }],
        );
    }
}
