use serde::Serialize;

use crate::forge::{
    Account, Assessment, Check, Ci, Colour, Comment, Merge, Poll, Position, Reported, Review,
    ReviewState, Snapshot, TargetState, Thread,
};

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
    #[serde(rename = "ci.settled")]
    CiSettled {
        colour: Colour,
        head_sha: String,
        checks: Vec<CheckReport>,
        warnings: Vec<CheckReport>,
    },
    #[serde(rename = "comment.created")]
    CommentCreated {
        comment_id: String,
        body: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
    #[serde(rename = "comment.edited")]
    CommentEdited {
        comment_id: String,
        body: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
    #[serde(rename = "head.pushed")]
    HeadPushed { old_sha: String, new_sha: String },
    #[serde(rename = "merge.behind")]
    MergeBehind,
    #[serde(rename = "merge.conflicted")]
    MergeConflicted,
    #[serde(rename = "review.submitted")]
    ReviewSubmitted {
        #[serde(skip_serializing_if = "Option::is_none")]
        review_id: Option<String>,
        state: ReviewState,
        body: String,
    },
    #[serde(rename = "review.withdrawn")]
    ReviewWithdrawn {
        #[serde(skip_serializing_if = "Option::is_none")]
        review_id: Option<String>,
    },
    #[serde(rename = "state.closed")]
    StateClosed,
    #[serde(rename = "state.merged")]
    StateMerged,
    #[serde(rename = "state.reopened")]
    StateReopened,
    #[serde(rename = "thread.opened")]
    ThreadOpened {
        thread_id: String,
        comment_id: String,
        reply_to: String,
        body: String,
        #[serde(flatten)]
        position: Option<Position>,
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

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct CheckReport {
    pub name: String,
    pub conclusion: String,
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

impl Detail {
    pub fn body(&self) -> Option<&str> {
        match self {
            Self::CommentCreated { body, .. }
            | Self::CommentEdited { body, .. }
            | Self::ReviewSubmitted { body, .. }
            | Self::ThreadOpened { body, .. }
            | Self::ThreadReplied { body, .. } => Some(body),
            _ => None,
        }
    }

    pub fn body_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::CommentCreated { body, .. }
            | Self::CommentEdited { body, .. }
            | Self::ReviewSubmitted { body, .. }
            | Self::ThreadOpened { body, .. }
            | Self::ThreadReplied { body, .. } => Some(body),
            _ => None,
        }
    }

    /// The kind as printed, matching the `serde` renames above.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CiSettled { .. } => "ci.settled",
            Self::CommentCreated { .. } => "comment.created",
            Self::CommentEdited { .. } => "comment.edited",
            Self::HeadPushed { .. } => "head.pushed",
            Self::MergeBehind => "merge.behind",
            Self::MergeConflicted => "merge.conflicted",
            Self::ReviewSubmitted { .. } => "review.submitted",
            Self::ReviewWithdrawn { .. } => "review.withdrawn",
            Self::StateClosed => "state.closed",
            Self::StateMerged => "state.merged",
            Self::StateReopened => "state.reopened",
            Self::ThreadOpened { .. } => "thread.opened",
            Self::ThreadReplied { .. } => "thread.replied",
            Self::ThreadResolved { .. } => "thread.resolved",
            Self::ThreadUnresolved { .. } => "thread.unresolved",
        }
    }
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

/// A first poll as baseline: its colour, and an open target's mergeability, count as reported.
impl From<Poll> for Snapshot {
    fn from(poll: Poll) -> Self {
        Self {
            reported: Reported {
                colour: poll.ci.as_ref().and_then(Ci::colour),
                merge: poll.merge.filter(|_| poll.state == TargetState::Open),
            },
            poll,
        }
    }
}

/// The events between `baseline` and `poll`, in report order, and the snapshot that follows.
pub fn diff(baseline: &Snapshot, poll: Poll) -> (Vec<Event>, Snapshot) {
    let reported = &baseline.reported;
    let baseline = &baseline.poll;
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
        // Top-level and thread comment ids are separate namespaces, but a reply turns a GitLab
        // comment into its discussion's thread, keeping its id.
        let seen = match thread {
            None => baseline.comments.iter().find(|seen| seen.id == comment.id),
            Some(thread) => baseline
                .threads
                .iter()
                .flat_map(|seen_thread| &seen_thread.comments)
                .chain(
                    baseline
                        .comments
                        .iter()
                        .filter(|seen| seen.reply_to.as_deref() == Some(thread.reply_to.as_str())),
                )
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
    for seen in &baseline.reviews {
        if seen.lapses && poll.reviews.iter().all(|review| review.id != seen.id) {
            events.push(review_lapsed(seen));
        }
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
    // A new head starts uncoloured, so returning to an earlier one settles it again.
    let previous = match (&baseline.head, &poll.head) {
        (Some(old_sha), Some(new_sha)) if old_sha != new_sha => {
            events.push(head_pushed(old_sha, new_sha, &poll));
            None
        }
        _ => reported.colour,
    };
    // Unsettled CI keeps no colour, so settling again after a re-run is reported again.
    let colour = poll.ci.as_ref().and_then(Ci::colour);
    if let Some(ci) = &poll.ci
        && let Some(head) = &poll.head
        && let Some(colour) = colour
        && previous != Some(colour)
    {
        events.push(ci_settled(&poll, head, ci, colour));
    }
    if poll.state != baseline.state {
        events.push(state_changed(&poll));
    }
    // Only an open target's mergeability is news, so a merge reports nothing but itself.
    let merge = match poll.merge {
        Some(merge) if poll.state == TargetState::Open => Some(merge),
        _ => reported.merge,
    };
    // Mergeability first known after computing is already on the target, so it reports nothing.
    if let Some(changed) = merge
        && reported.merge.is_some()
        && merge != reported.merge
    {
        events.extend(merge_changed(&poll, changed));
    }
    events.sort_by(|a, b| {
        (a.at.is_none(), &a.at, &a.detail).cmp(&(b.at.is_none(), &b.at, &b.detail))
    });
    (
        events,
        Snapshot {
            poll,
            reported: Reported { colour, merge },
        },
    )
}

fn head_pushed(old_sha: &str, new_sha: &str, poll: &Poll) -> Event {
    on_target(
        poll,
        Detail::HeadPushed {
            old_sha: old_sha.to_owned(),
            new_sha: new_sha.to_owned(),
        },
    )
}

fn merge_changed(poll: &Poll, merge: Merge) -> Option<Event> {
    let detail = match merge {
        Merge::Behind => Detail::MergeBehind,
        Merge::Clean => return None,
        Merge::Conflicted => Detail::MergeConflicted,
    };
    Some(on_target(poll, detail))
}

fn state_changed(poll: &Poll) -> Event {
    let detail = match poll.state {
        TargetState::Closed => Detail::StateClosed,
        TargetState::Merged => Detail::StateMerged,
        TargetState::Open => Detail::StateReopened,
    };
    Event {
        at: poll.state_changed_at.clone(),
        actor: poll.state_changed_by.clone(),
        ..on_target(poll, detail)
    }
}

/// An event about the target as a whole, which the forge neither times nor attributes.
fn on_target(poll: &Poll, detail: Detail) -> Event {
    Event {
        detail,
        url: poll.url.clone(),
        at: None,
        actor: None,
        subject: None,
    }
}

/// Lists the deciding checks that failed or block, and as warnings the other checks that failed.
fn ci_settled(poll: &Poll, head: &str, ci: &Ci, colour: Colour) -> Event {
    let reports = |keep: fn(&Check) -> bool| {
        ci.checks
            .iter()
            .filter(|check| keep(check))
            .map(|check| CheckReport {
                name: check.name.clone(),
                conclusion: check.conclusion.clone(),
                url: check.url.clone(),
                job_id: check.job_id.clone(),
            })
            .collect()
    };
    on_target(
        poll,
        Detail::CiSettled {
            colour,
            head_sha: head.to_owned(),
            checks: reports(|check| {
                check.deciding
                    && matches!(check.assessment, Assessment::Failed | Assessment::Waiting)
            }),
            warnings: reports(|check| !check.deciding && check.assessment == Assessment::Failed),
        },
    )
}

fn comment_created(comment: &Comment) -> Event {
    Event {
        detail: Detail::CommentCreated {
            comment_id: comment.id.clone(),
            body: comment.body.clone(),
            reply_to: comment.reply_to.clone(),
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
            reply_to: thread.map_or(comment.reply_to.clone(), |thread| {
                Some(thread.reply_to.clone())
            }),
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
                review_id: review.forge_id.clone(),
                state: review.state,
                body: review.body.clone(),
            },
            url: review.url.clone(),
            at: review.at.clone(),
            actor: review.actor.clone(),
            subject: None,
        }),
        Some(seen) if withdrawn && seen.state != ReviewState::Dismissed => Some(Event {
            detail: Detail::ReviewWithdrawn {
                review_id: review.forge_id.clone(),
            },
            url: review.url.clone(),
            at: review.withdrawn_at.clone(),
            actor: review.withdrawn_by.clone(),
            subject: review.actor.clone(),
        }),
        _ => None,
    }
}

/// A lapsed review's forge records neither when it lapsed nor who made it.
fn review_lapsed(seen: &Review) -> Event {
    Event {
        detail: Detail::ReviewWithdrawn {
            review_id: seen.forge_id.clone(),
        },
        url: seen.url.clone(),
        at: None,
        actor: None,
        subject: seen.actor.clone(),
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
            position: thread.position.clone(),
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
fn label(event: &Event) -> String {
    match &event.detail {
        Detail::CiSettled { colour, .. } => format!("ci.settled {colour:?}"),
        detail => detail.kind().to_owned(),
    }
}

/// The events of each poll after `first` with the snapshot it leaves, reloading each snapshot as a
/// restart would.
#[cfg(test)]
pub fn replay(first: Poll, rest: Vec<Poll>) -> Vec<(Vec<Event>, Snapshot)> {
    let reload = |snapshot: &Snapshot| {
        serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap()
    };
    let mut snapshot = reload(&Snapshot::from(first));
    rest.into_iter()
        .map(|next| {
            let (events, next) = diff(&snapshot, next);
            snapshot = reload(&next);
            (events, snapshot.clone())
        })
        .collect()
}

/// `replay`'s events as labels.
#[cfg(test)]
pub fn steps(first: Poll, rest: Vec<Poll>) -> Vec<Vec<String>> {
    replay(first, rest)
        .into_iter()
        .map(|(events, _)| events.iter().map(label).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::forge::{comment, octocat, poll};

    const MERGE_REQUEST: &str = "https://gitlab.com/g/p/-/merge_requests/1";
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
            position: Some(Position {
                line: Some(84),
                path: "src/run.rs".to_owned(),
            }),
            reply_to: "10".to_owned(),
            resolved,
            resolved_by: resolved.then(octocat),
            url: THREAD_URL.to_owned(),
        }
    }

    fn review(state: ReviewState) -> Review {
        Review {
            actor: Some(octocat()),
            at: Some("2026-10-20T19:05:00Z".to_owned()),
            body: "Looks good".to_owned(),
            forge_id: Some("500".to_owned()),
            id: "500".to_owned(),
            lapses: false,
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

    fn details(baseline: &Poll, next: Poll) -> Vec<Detail> {
        diff(&baseline.clone().into(), next)
            .0
            .into_iter()
            .map(|event| event.detail)
            .collect()
    }

    #[test]
    fn a_new_comment_is_created() {
        let next = poll(&["1", "2"]);

        let (events, snapshot) = diff(&poll(&["1"]).into(), next.clone());

        assert_eq!(
            events,
            [Event {
                detail: Detail::CommentCreated {
                    comment_id: "2".to_owned(),
                    body: "comment 2".to_owned(),
                    reply_to: None,
                },
                url: "https://github.com/o/r/pull/1#issuecomment-2".to_owned(),
                at: Some("2026-10-20T19:00:00Z".to_owned()),
                actor: Some(octocat()),
                subject: None,
            }],
        );
        assert_eq!(snapshot.poll, next);
    }

    #[test]
    fn seen_and_vanished_comments_are_quiet() {
        let (events, _) = diff(&poll(&["1", "2"]).into(), poll(&["2"]));

        assert_eq!(events, []);
    }

    #[test]
    fn a_changed_body_is_edited_by_its_editor() {
        let hubot = Account {
            bot: Some(false),
            id: None,
            login: "hubot".to_owned(),
        };
        let mut next = poll(&["1"]);
        next.comments[0].body = "comment 1, reworded".to_owned();
        next.comments[0].edited_at = Some("2026-10-20T19:30:00Z".to_owned());
        next.comments[0].editor = Some(hubot.clone());

        let (events, _) = diff(&poll(&["1"]).into(), next);

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
                position: Some(Position {
                    line: None,
                    path: "src/run.rs".to_owned(),
                }),
                reply_to: "20".to_owned(),
                url: "https://github.com/o/r/pull/1#discussion_r20".to_owned(),
                ..thread(vec![reviewed("20", "500")], false)
            },
        ]);
        next.reviews = vec![review(ReviewState::ChangesRequested)];

        let (events, _) = diff(&baseline.into(), next);

        assert_eq!(
            events,
            [
                Event {
                    detail: Detail::ReviewSubmitted {
                        review_id: Some("500".to_owned()),
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
                        position: Some(Position {
                            line: None,
                            path: "src/run.rs".to_owned(),
                        }),
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
            id: None,
            login: "hubot".to_owned(),
        };
        let dismissed = Review {
            state: ReviewState::Dismissed,
            withdrawn_at: Some("2026-10-20T19:40:00Z".to_owned()),
            withdrawn_by: Some(hubot.clone()),
            ..review(ReviewState::Approved)
        };

        let (events, _) = diff(&baseline.into(), with_reviews(vec![dismissed]));

        assert_eq!(
            events,
            [Event {
                detail: Detail::ReviewWithdrawn {
                    review_id: Some("500".to_owned()),
                },
                url: "https://github.com/o/r/pull/1#pullrequestreview-500".to_owned(),
                at: Some("2026-10-20T19:40:00Z".to_owned()),
                actor: Some(hubot),
                subject: Some(octocat()),
            }],
        );
    }

    /// A GitLab reviewer standing, which has neither a time nor an id of its own.
    fn reviewer_standing(state: ReviewState, lapses: bool) -> Review {
        Review {
            at: None,
            forge_id: None,
            id: format!("{state:?}:gid://gitlab/User/1"),
            lapses,
            url: MERGE_REQUEST.to_owned(),
            ..review(state)
        }
    }

    #[test]
    fn a_lapsed_approval_withdraws_it_with_only_the_reviewer_known() {
        let baseline = with_reviews(vec![reviewer_standing(ReviewState::Approved, true)]);

        let (events, _) = diff(&baseline.into(), poll(&[]));

        assert_eq!(
            events,
            [Event {
                detail: Detail::ReviewWithdrawn { review_id: None },
                url: MERGE_REQUEST.to_owned(),
                at: None,
                actor: None,
                subject: Some(octocat()),
            }],
        );
    }

    #[test]
    fn a_standing_that_does_not_lapse_leaves_quietly() {
        let baseline = with_reviews(vec![reviewer_standing(
            ReviewState::ChangesRequested,
            false,
        )]);

        assert_eq!(details(&baseline, poll(&[])), []);
    }

    #[test]
    fn a_gitlab_comment_and_positionless_thread_print_their_reply_targets_only() {
        let mut next = with_threads(vec![Thread {
            position: None,
            ..thread(vec![comment("10")], false)
        }]);
        next.comments = vec![Comment {
            reply_to: Some("d1".to_owned()),
            ..comment("1")
        }];

        let (events, _) = diff(&poll(&[]).into(), next);

        let printed: Vec<_> = events
            .iter()
            .map(|event| serde_json::to_value(&event.detail).unwrap())
            .collect();
        assert_eq!(
            printed,
            [
                json!({
                    "kind": "comment.created",
                    "comment_id": "1",
                    "body": "comment 1",
                    "reply_to": "d1",
                }),
                json!({
                    "kind": "thread.opened",
                    "thread_id": THREAD,
                    "comment_id": "10",
                    "reply_to": "10",
                    "body": "comment 10",
                }),
            ],
        );
    }

    #[test]
    fn a_reply_turning_a_gitlab_comment_into_a_thread_reports_only_the_reply() {
        let discussion = |comments| Thread {
            position: None,
            reply_to: "d1".to_owned(),
            ..thread(comments, false)
        };
        let mut baseline = poll(&[]);
        baseline.comments = vec![Comment {
            reply_to: Some("d1".to_owned()),
            ..comment("10")
        }];

        let next = with_threads(vec![discussion(vec![comment("10"), comment("11")])]);

        assert_eq!(
            details(&baseline, next),
            [Detail::ThreadReplied {
                thread_id: THREAD.to_owned(),
                comment_id: "11".to_owned(),
                reply_to: "d1".to_owned(),
                body: "comment 11".to_owned(),
                review_id: None,
            }],
        );
    }

    #[test]
    fn a_positionless_thread_survives_the_snapshot_file() {
        let snapshot = Snapshot::from(with_threads(vec![Thread {
            position: None,
            ..thread(vec![comment("10")], false)
        }]));

        let saved = serde_json::to_string(&snapshot).unwrap();

        let restored: Snapshot = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.poll.threads, snapshot.poll.threads);
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

        let (events, _) = diff(&open.clone().into(), resolved.clone());
        let unresolved = details(&resolved, open);

        assert_eq!(
            events.iter().map(|event| &event.detail).collect::<Vec<_>>(),
            [
                &Detail::CommentCreated {
                    comment_id: "1".to_owned(),
                    body: "comment 1".to_owned(),
                    reply_to: None,
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

    const A: &str = "aaa111";
    const B: &str = "bbb222";

    fn check(name: &str, deciding: bool, assessment: Assessment) -> Check {
        Check {
            assessment,
            conclusion: format!("{assessment:?}").to_lowercase(),
            deciding,
            job_id: Some(format!("{name}-job")),
            name: name.to_owned(),
            url: Some(format!("https://github.com/o/r/actions/runs/1/job/{name}")),
        }
    }

    fn on(head: &str, checks: Vec<Check>) -> Poll {
        Poll {
            ci: Some(Ci {
                checks,
                required_known: true,
            }),
            head: Some(head.to_owned()),
            ..poll(&[])
        }
    }

    #[test]
    fn an_issue_reports_only_its_comments_and_state() {
        let issue = |ids: &[&str], state| Poll {
            head: None,
            state,
            ..poll(ids)
        };

        let events = steps(
            issue(&["1"], TargetState::Open),
            vec![issue(&["1", "2"], TargetState::Closed)],
        );

        assert_eq!(events, [["comment.created", "state.closed"]]);
    }

    #[test]
    fn red_fires_on_the_first_deciding_failure_while_others_run() {
        let running = || check("lint", true, Assessment::Pending);
        let baseline = on(A, vec![check("test", true, Assessment::Pending), running()]);
        let failed = on(A, vec![check("test", true, Assessment::Failed), running()]);

        let (events, snapshot) = diff(&baseline.into(), failed);

        assert_eq!(
            events,
            [Event {
                detail: Detail::CiSettled {
                    colour: Colour::Red,
                    head_sha: A.to_owned(),
                    checks: vec![CheckReport {
                        name: "test".to_owned(),
                        conclusion: "failed".to_owned(),
                        url: Some("https://github.com/o/r/actions/runs/1/job/test".to_owned()),
                        job_id: Some("test-job".to_owned()),
                    }],
                    warnings: Vec::new(),
                },
                url: poll(&[]).url,
                at: None,
                actor: None,
                subject: None,
            }],
        );
        assert_eq!(snapshot.reported.colour, Some(Colour::Red));
    }

    #[test]
    fn green_waits_for_every_deciding_check_and_fires_once() {
        let passed = || check("test", true, Assessment::Passed);
        let pending = on(A, vec![passed(), check("lint", true, Assessment::Pending)]);
        let green = || on(A, vec![passed(), check("lint", true, Assessment::Passed)]);

        let steps = steps(on(A, Vec::new()), vec![pending, green(), green()]);

        assert_eq!(steps, [vec![], vec!["ci.settled Green"], vec![]]);
    }

    #[test]
    fn amber_waits_until_nothing_else_is_unsettled() {
        let waiting = || check("deploy", true, Assessment::Waiting);
        let running = on(A, vec![waiting(), check("test", true, Assessment::Pending)]);
        let blocked = on(A, vec![waiting(), check("test", true, Assessment::Passed)]);

        assert_eq!(
            steps(on(A, Vec::new()), vec![running, blocked]),
            [vec![], vec!["ci.settled Amber"]],
        );
    }

    #[test]
    fn a_repeated_colour_fires_only_after_ci_went_unsettled() {
        let test = |assessment| check("test", true, assessment);
        let failing = || on(A, vec![test(Assessment::Failed)]);
        let rerun = || on(A, vec![test(Assessment::Pending)]);
        let failing_while_another_runs = on(
            A,
            vec![
                test(Assessment::Failed),
                check("lint", true, Assessment::Pending),
            ],
        );

        let steps = steps(
            failing(),
            vec![
                failing_while_another_runs,
                failing(),
                rerun(),
                failing(),
                rerun(),
                on(A, vec![test(Assessment::Passed)]),
            ],
        );

        assert_eq!(
            steps,
            [
                vec![],
                vec![],
                vec![],
                vec!["ci.settled Red"],
                vec![],
                vec!["ci.settled Green"],
            ],
        );
    }

    #[test]
    fn an_unknown_required_set_clears_green_so_it_settles_again() {
        let green = || on(A, vec![check("test", true, Assessment::Passed)]);
        let unknown = Poll {
            ci: Some(Ci {
                checks: vec![check("test", true, Assessment::Passed)],
                required_known: false,
            }),
            ..green()
        };

        let (_, cleared) = diff(&green().into(), unknown.clone());

        assert_eq!(cleared.reported.colour, None);
        assert_eq!(
            steps(green(), vec![unknown, green()]),
            [vec![], vec!["ci.settled Green"]],
        );
    }

    #[test]
    fn an_incomplete_required_set_holds_back_green_only() {
        let unknown = |assessment| Poll {
            ci: Some(Ci {
                checks: vec![check("test", true, assessment)],
                required_known: false,
            }),
            ..on(A, Vec::new())
        };

        let steps = steps(
            on(A, Vec::new()),
            vec![
                unknown(Assessment::Passed),
                unknown(Assessment::Waiting),
                unknown(Assessment::Failed),
            ],
        );

        assert_eq!(
            steps,
            [vec![], vec!["ci.settled Amber"], vec!["ci.settled Red"]]
        );
    }

    #[test]
    fn a_failed_check_that_does_not_decide_is_a_warning() {
        let optional = Check {
            job_id: None,
            url: None,
            ..check("coverage", false, Assessment::Failed)
        };
        let next = on(A, vec![check("test", true, Assessment::Passed), optional]);

        let (events, _) = diff(&on(A, Vec::new()).into(), next);

        assert_eq!(
            serde_json::to_value(&events[0].detail).unwrap(),
            json!({
                "kind": "ci.settled",
                "colour": "green",
                "head_sha": A,
                "checks": [],
                "warnings": [{ "name": "coverage", "conclusion": "failed", "url": null }],
            }),
        );
    }

    #[test]
    fn checks_that_do_not_decide_never_colour_and_warn_only_on_failure() {
        let next = on(
            A,
            vec![
                check("test", true, Assessment::Failed),
                check("bench", false, Assessment::Pending),
                check("preview", false, Assessment::Waiting),
            ],
        );

        let (events, _) = diff(&on(A, Vec::new()).into(), next);

        let [
            Event {
                detail:
                    Detail::CiSettled {
                        colour,
                        checks,
                        warnings,
                        ..
                    },
                ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(*colour, Colour::Red);
        assert_eq!(checks.len(), 1);
        assert_eq!(warnings, &[]);
    }

    #[test]
    fn a_push_reports_both_shas_and_starts_the_new_head_uncoloured() {
        let red = on(A, vec![check("test", true, Assessment::Failed)]);
        let running = on(B, vec![check("test", true, Assessment::Pending)]);

        let (events, snapshot) = diff(&red.into(), running);

        assert_eq!(
            events.iter().map(|event| &event.detail).collect::<Vec<_>>(),
            [&Detail::HeadPushed {
                old_sha: A.to_owned(),
                new_sha: B.to_owned(),
            }],
        );
        assert_eq!(snapshot.reported.colour, None);
    }

    #[test]
    fn returning_to_an_earlier_head_settles_it_again() {
        let green = |head| on(head, vec![check("test", true, Assessment::Passed)]);

        let steps = steps(green(A), vec![green(B), green(A)]);

        let settled_again = vec!["ci.settled Green", "head.pushed"];
        assert_eq!(steps, [settled_again.clone(), settled_again]);
    }

    fn mergeability(merge: Option<Merge>) -> Poll {
        Poll { merge, ..poll(&[]) }
    }

    const STATE_CHANGED_AT: &str = "2026-10-20T19:50:00Z";

    fn standing(state: TargetState, merge: Option<Merge>) -> Poll {
        Poll {
            state,
            state_changed_at: Some(STATE_CHANGED_AT.to_owned()),
            state_changed_by: Some(octocat()),
            ..mergeability(merge)
        }
    }

    #[test]
    fn turning_conflicted_or_behind_fires_and_computing_keeps_the_last_known_value() {
        let steps = steps(
            mergeability(Some(Merge::Clean)),
            vec![
                mergeability(None),
                mergeability(Some(Merge::Conflicted)),
                mergeability(None),
                mergeability(Some(Merge::Conflicted)),
                mergeability(Some(Merge::Behind)),
                mergeability(Some(Merge::Clean)),
                mergeability(Some(Merge::Behind)),
            ],
        );

        assert_eq!(
            steps,
            [
                vec![],
                vec!["merge.conflicted"],
                vec![],
                vec![],
                vec!["merge.behind"],
                vec![],
                vec!["merge.behind"],
            ],
        );
    }

    #[test]
    fn the_first_known_mergeability_counts_as_reported() {
        let conflicted = || mergeability(Some(Merge::Conflicted));

        let from_known = steps(conflicted(), vec![conflicted()]);
        let from_computing = steps(
            mergeability(None),
            vec![conflicted(), mergeability(Some(Merge::Behind))],
        );

        assert_eq!(from_known, [Vec::<String>::new()]);
        assert_eq!(from_computing, [vec![], vec!["merge.behind"]]);
    }

    #[test]
    fn a_merge_emits_only_state_merged() {
        let baseline = mergeability(Some(Merge::Clean));
        let merged = standing(TargetState::Merged, Some(Merge::Conflicted));

        let (events, snapshot) = diff(&baseline.into(), merged);

        assert_eq!(
            events,
            [Event {
                detail: Detail::StateMerged,
                url: poll(&[]).url,
                at: Some(STATE_CHANGED_AT.to_owned()),
                actor: Some(octocat()),
                subject: None,
            }],
        );
        assert_eq!(snapshot.reported.merge, Some(Merge::Clean));
    }

    #[test]
    fn a_target_first_seen_closed_adopts_its_mergeability_on_reopening() {
        let steps = steps(
            standing(TargetState::Closed, Some(Merge::Clean)),
            vec![standing(TargetState::Open, Some(Merge::Conflicted))],
        );

        assert_eq!(steps, [vec!["state.reopened"]]);
    }

    #[test]
    fn a_conflict_that_arose_while_closed_fires_on_reopening() {
        let steps = steps(
            mergeability(Some(Merge::Clean)),
            vec![
                standing(TargetState::Closed, Some(Merge::Conflicted)),
                standing(TargetState::Open, Some(Merge::Conflicted)),
            ],
        );

        assert_eq!(
            steps,
            [
                vec!["state.closed"],
                vec!["state.reopened", "merge.conflicted"]
            ],
        );
    }
}
