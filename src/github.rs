use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};

use crate::envelope::{ErrorKind, Forge};
use crate::exec::Exec;
use crate::forge::{
    Account, Assessment, Backend, Check, Ci, Comment, Failure, Hints, Merge, Poll, Position,
    Review, ReviewState, Target, TargetKind, TargetState, Thread, run_cli,
};
use crate::graphql::{Connection, Paged, same_head};
use crate::reference::Reference;
use crate::response::{self, Response};

const BOT_SUFFIX: &str = "[bot]";
const CLI: &str = "gh";
const EXIT_AUTH: i32 = 4;
/// The rollup and status-context state of a required check that has not reported.
const EXPECTED: &str = "EXPECTED";
const GRAPHQL_NOT_FOUND: &str = "NOT_FOUND";
const GRAPHQL_RATE_LIMITED: &str = "RATE_LIMITED";
const HEADER_POLL_INTERVAL: &str = "x-poll-interval";
const HEADER_REMAINING: &str = "x-ratelimit-remaining";
const HEADER_RESET: &str = "x-ratelimit-reset";
const HEADER_RETRY_AFTER: &str = "retry-after";
const ISSUE_POLL_QUERY: &str = concat!(
    include_str!("github/issue_poll.graphql"),
    include_str!("github/fragments.graphql"),
);
const NODE_FIELD: &str = "node";
const POLL_QUERY: &str = concat!(
    include_str!("github/poll.graphql"),
    include_str!("github/fragments.graphql"),
    include_str!("github/thread_fragments.graphql"),
);
const SECONDARY_RATE_LIMIT: &str = "secondary rate limit";
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_GONE: u16 = 410;
const STATUS_NOT_FOUND: u16 = 404;
const STATUS_TOO_MANY_REQUESTS: u16 = 429;
const STATUS_UNAUTHORIZED: u16 = 401;
const THREAD_COMMENTS_QUERY: &str = concat!(
    include_str!("github/thread_comments.graphql"),
    include_str!("github/fragments.graphql"),
    include_str!("github/thread_fragments.graphql"),
);
const VIEWER_QUERY: &str = include_str!("github/viewer.graphql");

pub struct GitHub<'a, E> {
    exec: &'a E,
}

impl<'a, E: Exec> GitHub<'a, E> {
    pub fn new(exec: &'a E) -> Self {
        Self { exec }
    }

    /// The reply and its pacing, or the failure, classified from stdout before stderr.
    fn call(
        &self,
        args: &[&str],
        deadline: Option<SystemTime>,
    ) -> Result<(Response, Hints), Failure> {
        let output = run_cli(self.exec, Forge::GitHub, CLI, args, deadline)?;
        let reason = output.stderr.trim();
        if output.code == Some(EXIT_AUTH) {
            return Err(fatal(ErrorKind::Auth, reason));
        }
        let Some(response) = response::parse(&output.stdout) else {
            return Err(transient(&format!("gh printed no HTTP reply: {reason}")));
        };
        let hints = pacing(&response);
        if response.is_success() {
            return Ok((response, hints));
        }
        if rate_limited(&response) {
            return Err(rate_limit(hints));
        }
        match response.status {
            STATUS_UNAUTHORIZED | STATUS_FORBIDDEN => Err(fatal(ErrorKind::Auth, reason)),
            // A deleted issue, or any issue where issues are disabled, is gone.
            STATUS_GONE | STATUS_NOT_FOUND => {
                let message = format!("not found or not visible to gh: {reason}");
                Err(fatal(ErrorKind::TargetNotFound, &message))
            }
            status => Err(Failure::Transient {
                forge: Forge::GitHub,
                hints,
                message: format!("HTTP {status}: {reason}"),
            }),
        }
    }
}

impl<E: Exec> Backend for GitHub<'_, E> {
    fn resolve(
        &self,
        reference: &Reference,
        deadline: Option<SystemTime>,
    ) -> Result<(Target, Hints), Failure> {
        // The issues endpoint answers for a pull request too, with its own URL and node id.
        let resource = match reference.kind {
            TargetKind::ChangeRequest => "pulls",
            TargetKind::Issue => "issues",
        };
        let path = format!(
            "repos/{}/{resource}/{}",
            reference.project, reference.number,
        );
        let (response, hints) = self.call(
            &["api", "-i", "--hostname", &reference.host, &path],
            deadline,
        )?;
        if rest_names_secondary_limit(&response.body) {
            return Err(rate_limit(hints));
        }
        let found: RestTarget = decode(&response.body).map_err(|failure| failure.paced(hints))?;
        let target = Target {
            forge: Forge::GitHub,
            forge_id: found.node_id,
            host: reference.host.clone(),
            kind: match found.pull_request {
                Some(_) => TargetKind::ChangeRequest,
                None => reference.kind,
            },
            url: found.html_url,
        };
        Ok((target, hints))
    }

    fn poll(
        &self,
        target: &Target,
        deadline: Option<SystemTime>,
    ) -> Result<(Poll, Hints), Failure> {
        let mut hints = Hints::default();
        let polled = match target.kind {
            TargetKind::ChangeRequest => self.poll_pages(target, deadline, &mut hints),
            TargetKind::Issue => self.poll_issue(target, deadline, &mut hints),
        };
        match polled {
            Ok(poll) => Ok((poll, hints)),
            Err(failure) => Err(failure.paced(hints)),
        }
    }

    fn viewer(&self, host: &str, deadline: Option<SystemTime>) -> Result<(String, Hints), Failure> {
        let mut hints = Hints::default();
        let reply: Reply<ViewerData> = self
            .graphql(host, VIEWER_QUERY, &[], deadline, &mut hints)
            .map_err(|failure| failure.paced(hints))?;
        let viewer = reply
            .data
            .ok_or_else(|| transient("gh returned no viewer").paced(hints))?;
        Ok((viewer.viewer.id, hints))
    }
}

impl<E: Exec> GitHub<'_, E> {
    /// Pages every connection to its end, failing the poll if the head moves between pages.
    fn poll_pages(
        &self,
        target: &Target,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Poll, Failure> {
        let mut checks = Paged::new(Forge::GitHub, "checks");
        let mut comments = Paged::new(Forge::GitHub, "comments");
        let mut dismissals = Paged::new(Forge::GitHub, "dismissals");
        let mut reviews = Paged::new(Forge::GitHub, "reviews");
        let mut threads = Paged::new(Forge::GitHub, "threads");
        let mut first: Option<(String, Standing)> = None;
        let mut rollup_state = None;
        while !(checks.done()
            && comments.done()
            && dismissals.done()
            && reviews.done()
            && threads.done())
        {
            let mut variables = vec![("-f", format!("id={}", target.forge_id))];
            variables.extend(checks.variable());
            variables.extend(comments.variable());
            variables.extend(dismissals.variable());
            variables.extend(reviews.variable());
            variables.extend(threads.variable());
            let page: PullRequestPage = self
                .query(target, POLL_QUERY, &variables, deadline, hints)?
                .ok_or_else(|| gone(target))?;
            // CI is judged on the head only, so a latest listed commit that isn't it has none.
            let rollup = page
                .commits
                .nodes
                .into_iter()
                .map(|node| node.commit)
                .find(|commit| commit.oid == page.head_ref_oid)
                .and_then(|commit| commit.status_check_rollup);
            match &first {
                None => {
                    // A head with no CI has no rollup, so there are no checks to page.
                    if rollup.is_none() {
                        checks.finish();
                    }
                    rollup_state = rollup.as_ref().map(|rollup| rollup.state.clone());
                    first = Some((page.head_ref_oid, page.standing));
                }
                Some((head, _)) => same_head(Forge::GitHub, head, &page.head_ref_oid)?,
            }
            checks.take(rollup.and_then(|rollup| rollup.contexts))?;
            comments.take(page.comments)?;
            dismissals.take(page.dismissals)?;
            reviews.take(page.reviews)?;
            threads.take(page.review_threads)?;
        }
        let (head, standing) = first.expect("every poll fetches a first page");
        let mut polled_threads = Vec::new();
        for node in threads.nodes {
            let comments = self.thread_comments(
                target,
                &head,
                &node.thread.id,
                node.comments,
                deadline,
                hints,
            )?;
            polled_threads.extend(thread(node.thread, comments));
        }
        let merge = standing.merge();
        let state = standing.lifecycle.state.into();
        let (state_changed_at, state_changed_by) = standing.change();
        Ok(Poll {
            ci: rollup_state.map(|state| ci(&state, &checks.nodes)),
            comments: comments.nodes.into_iter().map(Comment::from).collect(),
            head: Some(head),
            merge,
            reviews: reviews
                .nodes
                .into_iter()
                .filter_map(|node| review(node, &dismissals.nodes))
                .collect(),
            state,
            state_changed_at,
            state_changed_by,
            threads: polled_threads,
            url: target.url.clone(),
        })
    }

    /// Pages the issue's comments to their end.
    fn poll_issue(
        &self,
        target: &Target,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Poll, Failure> {
        let mut comments = Paged::new(Forge::GitHub, "comments");
        loop {
            let mut variables = vec![("-f", format!("id={}", target.forge_id))];
            variables.extend(comments.variable());
            let page: IssuePage = self
                .query(target, ISSUE_POLL_QUERY, &variables, deadline, hints)?
                .ok_or_else(|| gone(target))?;
            comments.take(Some(page.comments))?;
            if !comments.done() {
                continue;
            }
            let state = page.lifecycle.state.into();
            let (state_changed_at, state_changed_by) = page.lifecycle.change();
            return Ok(Poll {
                ci: None,
                comments: comments.nodes.into_iter().map(Comment::from).collect(),
                head: None,
                merge: None,
                reviews: Vec::new(),
                state,
                state_changed_at,
                state_changed_by,
                threads: Vec::new(),
                url: target.url.clone(),
            });
        }
    }

    /// A thread's comments, paging past the first page the poll query brought, still on `head`.
    fn thread_comments(
        &self,
        target: &Target,
        head: &str,
        thread_id: &str,
        first: Connection<CommentNode>,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Vec<CommentNode>, Failure> {
        let mut comments = Paged::new(Forge::GitHub, "comments");
        comments.take(Some(first))?;
        while !comments.done() {
            let mut variables = vec![("-f", format!("id={thread_id}"))];
            variables.extend(comments.variable());
            let page: ThreadPage = self
                .query(target, THREAD_COMMENTS_QUERY, &variables, deadline, hints)?
                .ok_or_else(|| transient(&format!("review thread {thread_id} vanished")))?;
            same_head(Forge::GitHub, head, &page.pull_request.head_ref_oid)?;
            comments.take(Some(page.comments))?;
        }
        Ok(comments.nodes)
    }

    /// The queried `node`, or `None` where GitHub reports it `NOT_FOUND`.
    fn query<T: DeserializeOwned>(
        &self,
        target: &Target,
        query: &str,
        variables: &[(&str, String)],
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Option<T>, Failure> {
        let reply: Reply<Data<T>> =
            self.graphql(&target.host, query, variables, deadline, hints)?;
        let node = reply.data.and_then(|data| data.node);
        if node.is_none() && reply.errors.is_empty() {
            return Err(transient(&format!(
                "{} returned a null node with no error",
                target.url,
            )));
        }
        Ok(node)
    }

    /// The decoded reply, folding its pacing into `hints`; any GraphQL error but a missing `node`
    /// fails the call.
    fn graphql<T: DeserializeOwned>(
        &self,
        host: &str,
        query: &str,
        variables: &[(&str, String)],
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Reply<T>, Failure> {
        let query = format!("query={query}");
        let mut args = vec!["api", "-i", "--hostname", host, "graphql", "-f", &query];
        for (flag, variable) in variables {
            args.extend([*flag, variable.as_str()]);
        }
        let (response, pacing) = self.call(&args, deadline)?;
        *hints = hints.merge(pacing);
        let reply: Reply<T> = decode(&response.body)?;
        let secondary = reply.message.as_deref().is_some_and(names_secondary_limit);
        if secondary || reply.errors.iter().any(GraphQlError::is_rate_limit) {
            return Err(rate_limit(pacing));
        }
        if let Some(error) = reply.errors.iter().find(|error| !error.is_missing_node()) {
            return Err(transient(&error.message));
        }
        Ok(reply)
    }
}

fn pacing(response: &Response) -> Hints {
    Hints {
        poll_interval: response.seconds(HEADER_POLL_INTERVAL),
        reset: response
            .seconds(HEADER_RESET)
            .filter(|_| quota_spent(response))
            .map(|since_epoch| UNIX_EPOCH + since_epoch),
        retry_after: response.seconds(HEADER_RETRY_AFTER),
    }
}

/// A failed reply is a rate limit when it is a 429, spends the quota, or is a 403 naming a
/// secondary limit. A `retry-after` alone is not one.
fn rate_limited(response: &Response) -> bool {
    if response.status == STATUS_TOO_MANY_REQUESTS || quota_spent(response) {
        return true;
    }
    response.status == STATUS_FORBIDDEN && rest_names_secondary_limit(&response.body)
}

fn quota_spent(response: &Response) -> bool {
    response.header(HEADER_REMAINING) == Some("0")
}

fn rest_names_secondary_limit(body: &str) -> bool {
    serde_json::from_str::<RestError>(body).is_ok_and(|error| names_secondary_limit(&error.message))
}

fn names_secondary_limit(message: &str) -> bool {
    message.to_ascii_lowercase().contains(SECONDARY_RATE_LIMIT)
}

fn gone(target: &Target) -> Failure {
    let message = format!(
        "{} is gone: deleted, transferred, or no longer visible to gh's account",
        target.url,
    );
    fatal(ErrorKind::TargetNotFound, &message)
}

/// A pull request or issue as REST reports it; `pull_request` marks an issue that is one.
#[derive(Deserialize)]
struct RestTarget {
    html_url: String,
    node_id: String,
    pull_request: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct Reply<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
    /// A REST-shaped error, as a secondary rate limit can arrive on a 200.
    message: Option<String>,
}

#[derive(Deserialize)]
struct GraphQlError {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    message: String,
    #[serde(default)]
    path: Vec<serde_json::Value>,
}

impl GraphQlError {
    fn is_missing_node(&self) -> bool {
        self.kind.as_deref() == Some(GRAPHQL_NOT_FOUND) && self.path == [NODE_FIELD]
    }

    fn is_rate_limit(&self) -> bool {
        self.kind.as_deref() == Some(GRAPHQL_RATE_LIMITED) || names_secondary_limit(&self.message)
    }
}

#[derive(Deserialize)]
struct RestError {
    message: String,
}

#[derive(Deserialize)]
struct Data<T> {
    node: Option<T>,
}

#[derive(Deserialize)]
struct ViewerData {
    viewer: Viewer,
}

#[derive(Deserialize)]
struct Viewer {
    id: String,
}

/// A connection skipped with `@include(if: false)` is absent.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestPage {
    commits: Nodes<CommitNode>,
    comments: Option<Connection<CommentNode>>,
    dismissals: Option<Connection<DismissalNode>>,
    head_ref_oid: String,
    review_threads: Option<Connection<ThreadNode>>,
    reviews: Option<Connection<ReviewNode>>,
    #[serde(flatten)]
    standing: Standing,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Standing {
    #[serde(flatten)]
    lifecycle: Lifecycle,
    merge_state_status: String,
    mergeable: String,
    merged_at: Option<String>,
    merged_by: Option<Author>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lifecycle {
    state: State,
    /// The latest close or reopen, which is a merged pull request's close too.
    state_changes: Nodes<StateChangeNode>,
}

#[derive(Deserialize)]
struct IssuePage {
    comments: Connection<CommentNode>,
    #[serde(flatten)]
    lifecycle: Lifecycle,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum StateChangeNode {
    ClosedEvent(StateChange),
    ReopenedEvent(StateChange),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateChange {
    actor: Option<Author>,
    created_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPage {
    comments: Connection<CommentNode>,
    pull_request: PullRequestHead,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestHead {
    head_ref_oid: String,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Commit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    oid: String,
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    contexts: Option<Connection<ContextNode>>,
    state: String,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum ContextNode {
    CheckRun(CheckRunNode),
    StatusContext(StatusContextNode),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckRunNode {
    check_suite: Option<CheckSuite>,
    conclusion: Option<String>,
    database_id: u64,
    details_url: Option<String>,
    is_required: bool,
    name: String,
    status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckSuite {
    app: Option<DatabaseId>,
    /// Present only for GitHub Actions, whose check run ids are job ids.
    workflow_run: Option<WorkflowRun>,
}

#[derive(Deserialize)]
struct WorkflowRun {
    workflow: DatabaseId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatabaseId {
    database_id: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusContextNode {
    context: String,
    is_required: bool,
    state: String,
    target_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    author: Option<Author>,
    body: String,
    created_at: String,
    database_id: u64,
    editor: Option<Author>,
    last_edited_at: Option<String>,
    #[serde(default)]
    pull_request_review: Option<ReviewRef>,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissalNode {
    actor: Option<Author>,
    created_at: String,
    review: Option<ReviewRef>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRef {
    database_id: u64,
    state: ReviewNodeState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    author: Option<Author>,
    body: String,
    database_id: u64,
    state: ReviewNodeState,
    submitted_at: Option<String>,
    url: String,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ReviewNodeState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

#[derive(Deserialize)]
struct ThreadNode {
    comments: Connection<CommentNode>,
    #[serde(flatten)]
    thread: ThreadFields,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadFields {
    id: String,
    is_resolved: bool,
    line: Option<u64>,
    path: String,
    resolved_by: Option<Author>,
}

#[derive(Clone, Deserialize)]
struct Author {
    #[serde(rename = "__typename")]
    typename: Typename,
    id: Option<String>,
    login: String,
}

#[derive(Clone, Deserialize)]
enum Typename {
    Bot,
    User,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum State {
    Closed,
    Merged,
    Open,
}

/// GraphQL reports a bot's login bare where REST suffixes it `[bot]`, except through `User`-typed
/// fields such as `resolvedBy`, which name an app by its suffixed login.
impl From<Author> for Account {
    fn from(author: Author) -> Self {
        match author.typename {
            Typename::Bot => Self {
                bot: Some(true),
                id: author.id,
                login: format!("{}{BOT_SUFFIX}", author.login),
            },
            Typename::User => Self {
                bot: Some(author.login.ends_with(BOT_SUFFIX)),
                id: author.id,
                login: author.login,
            },
            Typename::Other => Self {
                bot: None,
                id: author.id,
                login: author.login,
            },
        }
    }
}

impl From<CommentNode> for Comment {
    fn from(node: CommentNode) -> Self {
        Self {
            actor: node.author.map(Account::from),
            at: node.created_at,
            body: node.body,
            edited_at: node.last_edited_at,
            editor: node.editor.map(Account::from),
            id: node.database_id.to_string(),
            reply_to: None,
            review_id: node
                .pull_request_review
                .map(|review| review.database_id.to_string()),
            url: node.url,
        }
    }
}

/// A pending review is the viewer's unsubmitted draft, so it is left out.
fn review(node: ReviewNode, dismissals: &[DismissalNode]) -> Option<Review> {
    let state = match node.state {
        ReviewNodeState::Approved => ReviewState::Approved,
        ReviewNodeState::ChangesRequested => ReviewState::ChangesRequested,
        ReviewNodeState::Commented => ReviewState::Commented,
        ReviewNodeState::Dismissed => ReviewState::Dismissed,
        ReviewNodeState::Pending => return None,
    };
    let dismissal = dismissals.iter().rfind(|dismissal| {
        dismissal
            .review
            .as_ref()
            .is_some_and(|review| review.database_id == node.database_id)
    });
    Some(Review {
        actor: node.author.map(Account::from),
        at: Some(node.submitted_at?),
        body: node.body,
        forge_id: Some(node.database_id.to_string()),
        id: node.database_id.to_string(),
        lapses: false,
        state,
        url: node.url,
        withdrawn_at: dismissal.map(|dismissal| dismissal.created_at.clone()),
        withdrawn_by: dismissal
            .and_then(|dismissal| dismissal.actor.clone())
            .map(Account::from),
    })
}

/// The thread with its submitted comments, or `None` when it holds only draft ones.
fn thread(node: ThreadFields, comments: Vec<CommentNode>) -> Option<Thread> {
    let comments: Vec<Comment> = comments
        .into_iter()
        .filter(|comment| {
            comment
                .pull_request_review
                .as_ref()
                .is_none_or(|review| review.state != ReviewNodeState::Pending)
        })
        .map(Comment::from)
        .collect();
    let first = comments.first()?;
    Some(Thread {
        id: node.id,
        position: Some(Position {
            line: node.line,
            path: node.path,
        }),
        reply_to: first.id.clone(),
        resolved: node.is_resolved,
        resolved_by: node.resolved_by.map(Account::from),
        url: first.url.clone(),
        comments,
    })
}

impl ContextNode {
    fn is_required(&self) -> bool {
        match self {
            Self::CheckRun(run) => run.is_required,
            Self::StatusContext(status) => status.is_required,
        }
    }
}

/// The head's checks at their latest attempts, deciding on the required ones when any exist.
fn ci(rollup_state: &str, contexts: &[ContextNode]) -> Ci {
    let any_required = contexts.iter().any(ContextNode::is_required);
    // Attempts of one check share its name and workflow, or its app outside Actions.
    let source = |run: &CheckRunNode| {
        let suite = run.check_suite.as_ref()?;
        let source = suite
            .workflow_run
            .as_ref()
            .map(|run| &run.workflow)
            .or(suite.app.as_ref());
        source.map(|source| source.database_id)
    };
    let superseded = |run: &CheckRunNode| {
        contexts.iter().any(|other| match other {
            ContextNode::CheckRun(other) => {
                other.name == run.name
                    && source(other) == source(run)
                    && other.database_id > run.database_id
            }
            ContextNode::StatusContext(_) => false,
        })
    };
    let expected_seen = contexts.iter().any(|context| match context {
        ContextNode::StatusContext(status) => status.state == EXPECTED,
        ContextNode::CheckRun(_) => false,
    });
    let checks = contexts
        .iter()
        .filter_map(|context| {
            let deciding = context.is_required() || !any_required;
            match context {
                ContextNode::CheckRun(run) if superseded(run) => None,
                ContextNode::CheckRun(run) => Some(Check {
                    assessment: check_run_assessment(&run.status, run.conclusion.as_deref()),
                    conclusion: run
                        .conclusion
                        .as_ref()
                        .unwrap_or(&run.status)
                        .to_ascii_lowercase(),
                    deciding,
                    job_id: run
                        .check_suite
                        .as_ref()
                        .and_then(|suite| suite.workflow_run.as_ref())
                        .map(|_| run.database_id.to_string()),
                    name: run.name.clone(),
                    url: run.details_url.clone(),
                }),
                ContextNode::StatusContext(status) => Some(Check {
                    assessment: status_assessment(&status.state),
                    conclusion: status.state.to_ascii_lowercase(),
                    deciding,
                    job_id: None,
                    name: status.context.clone(),
                    url: status.target_url.clone(),
                }),
            }
        })
        .collect();
    Ci {
        checks,
        // An expecting rollup with no expected context awaits a check it doesn't name.
        required_known: expected_seen || rollup_state != EXPECTED,
    }
}

/// A completed run without a conclusion, or a word this mapping doesn't know, is unsettled.
fn check_run_assessment(status: &str, conclusion: Option<&str>) -> Assessment {
    match (conclusion, status) {
        (Some("SUCCESS" | "NEUTRAL" | "SKIPPED"), _) => Assessment::Passed,
        (Some("CANCELLED" | "FAILURE" | "STARTUP_FAILURE" | "TIMED_OUT"), _) => Assessment::Failed,
        (Some("ACTION_REQUIRED" | "STALE"), _) | (None, "WAITING") => Assessment::Waiting,
        _ => Assessment::Pending,
    }
}

fn status_assessment(state: &str) -> Assessment {
    match state {
        "SUCCESS" => Assessment::Passed,
        "ERROR" | "FAILURE" => Assessment::Failed,
        EXPECTED => Assessment::Waiting,
        _ => Assessment::Pending,
    }
}

impl Standing {
    fn merge(&self) -> Option<Merge> {
        match (self.mergeable.as_str(), self.merge_state_status.as_str()) {
            ("CONFLICTING", _) => Some(Merge::Conflicted),
            ("UNKNOWN", _) | (_, "UNKNOWN") => None,
            (_, "BEHIND") => Some(Merge::Behind),
            _ => Some(Merge::Clean),
        }
    }

    /// When and by whom the pull request entered its state.
    fn change(self) -> (Option<String>, Option<Account>) {
        if matches!(self.lifecycle.state, State::Merged) {
            return (self.merged_at, self.merged_by.map(Account::from));
        }
        self.lifecycle.change()
    }
}

impl Lifecycle {
    /// When and by whom the latest close or reopen happened, if it set the current state.
    fn change(self) -> (Option<String>, Option<Account>) {
        let latest = self.state_changes.nodes.into_iter().last();
        let (at, by) = match (self.state, latest) {
            (State::Closed, Some(StateChangeNode::ClosedEvent(change)))
            | (State::Open, Some(StateChangeNode::ReopenedEvent(change))) => {
                (Some(change.created_at), change.actor)
            }
            _ => (None, None),
        };
        (at, by.map(Account::from))
    }
}

impl From<State> for TargetState {
    fn from(state: State) -> Self {
        match state {
            State::Closed => Self::Closed,
            State::Merged => Self::Merged,
            State::Open => Self::Open,
        }
    }
}

fn decode<T: DeserializeOwned>(body: &str) -> Result<T, Failure> {
    serde_json::from_str(body)
        .map_err(|error| transient(&format!("unreadable reply from gh: {error}")))
}

fn rate_limit(hints: Hints) -> Failure {
    Failure::RateLimited {
        forge: Forge::GitHub,
        hints,
    }
}

fn transient(message: &str) -> Failure {
    Failure::transient(Forge::GitHub, message)
}

fn fatal(kind: ErrorKind, message: &str) -> Failure {
    Failure::fatal(Forge::GitHub, kind, message)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use super::*;
    use serde_json::{Value, json};

    use crate::exec::{self, Limit, Output, Script};
    use crate::forge::Colour;
    use crate::reference;

    const NODE_ID: &str = "PR_kwDOVQ0d8c8AAAABJm7W-Q";
    const RESOLVE: &str = "gh api -i --hostname github.com repos/brzzdev/babysit/pulls/15";

    fn reference() -> Reference {
        reference::parse("https://github.com/brzzdev/babysit/pull/15").unwrap()
    }

    fn target() -> Target {
        Target {
            forge: Forge::GitHub,
            forge_id: NODE_ID.to_owned(),
            host: "github.com".to_owned(),
            kind: TargetKind::ChangeRequest,
            url: "https://github.com/brzzdev/babysit/pull/15".to_owned(),
        }
    }

    fn gh(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            code: Some(code),
            stderr: stderr.to_owned(),
            stdout: stdout.to_owned(),
        }
    }

    fn resolve_with(result: Result<Output, exec::Error>) -> Result<Target, Failure> {
        let exec = Script::new([(RESOLVE.to_owned(), result)]);
        GitHub::new(&exec)
            .resolve(&reference(), None)
            .map(|(target, _)| target)
    }

    fn kind(failure: &Failure) -> ErrorKind {
        match failure {
            Failure::Fatal { kind, .. } => *kind,
            other => panic!("not fatal: {other:?}"),
        }
    }

    #[test]
    fn resolves_a_pull_request_to_its_url_and_node_id() {
        let resolved = resolve_with(Ok(gh(0, include_str!("github/fixtures/resolve.http"), "")));

        assert_eq!(resolved.unwrap(), target());
    }

    fn issue() -> Target {
        Target {
            forge_id: ISSUE_ID.to_owned(),
            kind: TargetKind::Issue,
            url: "https://github.com/brzzdev/babysit/issues/9".to_owned(),
            ..target()
        }
    }

    fn resolve_issue(output: Output) -> Result<Target, Failure> {
        let exec = Script::new([(
            "gh api -i --hostname github.com repos/brzzdev/babysit/issues/9".to_owned(),
            Ok(output),
        )]);
        let reference = reference::parse("https://github.com/brzzdev/babysit/issues/9").unwrap();
        GitHub::new(&exec)
            .resolve(&reference, None)
            .map(|(target, _)| target)
    }

    #[test]
    fn resolves_an_issue_to_its_url_and_node_id() {
        let resolved = resolve_issue(gh(
            0,
            include_str!("github/fixtures/resolve_issue.http"),
            "",
        ));

        assert_eq!(resolved.unwrap(), issue());
    }

    #[test]
    fn resolves_an_issue_that_is_a_pull_request_to_the_pull_request() {
        let stdout = include_str!("github/fixtures/resolve_issue_pull.http");

        let resolved = resolve_issue(gh(0, stdout, "")).unwrap();

        assert_eq!(
            resolved,
            Target {
                forge: Forge::GitHub,
                forge_id: "PR_kwDOVQ0d8c8AAAABJqUrxg".to_owned(),
                host: "github.com".to_owned(),
                kind: TargetKind::ChangeRequest,
                url: "https://github.com/brzzdev/babysit/pull/38".to_owned(),
            },
        );
    }

    #[test]
    fn an_issue_gone_or_disabled_is_not_found() {
        let failure = resolve_issue(gh(
            1,
            include_str!("github/fixtures/resolve_issue_gone.http"),
            "gh: Issues are disabled for this repo (HTTP 410)",
        ))
        .unwrap_err();

        assert_eq!(kind(&failure), ErrorKind::TargetNotFound);
    }

    #[test]
    fn a_secondary_rate_limit_on_a_200_resolve_is_a_rate_limit() {
        let stdout = include_str!("github/fixtures/resolve_secondary_rate_limit.http");

        let failure = resolve_with(Ok(gh(0, stdout, ""))).unwrap_err();

        assert!(matches!(
            failure,
            Failure::RateLimited { hints, .. } if hints.retry_after == Some(Duration::from_secs(120))
        ));
    }

    #[test]
    fn a_missing_pull_request_is_not_found() {
        let failure = resolve_with(Ok(gh(
            1,
            include_str!("github/fixtures/resolve_not_found.http"),
            "gh: Not Found (HTTP 404)",
        )))
        .unwrap_err();

        assert_eq!(kind(&failure), ErrorKind::TargetNotFound);
    }

    const HEAD: &str = "dfd1945e66eb0008cad921a1230fd2c533598845";
    const ISSUE_ID: &str = "I_kwDOVQ0d8c8AAAABYhoF-w";
    const JOB_ID: &str = "118827308709";
    /// When #30 merged, which its close event shares.
    const MERGED_AT: &str = "2026-10-20T19:08:24Z";
    const POLLED_ID: &str = "PR_kwDOVQ0d8c8AAAABJn0ayQ";
    const THREAD_ID: &str = "PRRT_kwDOVQ0d8c6ueY8r";

    fn polled() -> Target {
        Target {
            forge: Forge::GitHub,
            forge_id: POLLED_ID.to_owned(),
            host: "github.com".to_owned(),
            kind: TargetKind::ChangeRequest,
            url: "https://github.com/brzzdev/babysit/pull/30".to_owned(),
        }
    }

    fn graphql(
        query: &str,
        variables: &str,
        stdout: &str,
    ) -> (String, Result<Output, exec::Error>) {
        let call = format!("gh api -i --hostname github.com graphql -f query={query} {variables}");
        (call, Ok(gh(0, stdout, "")))
    }

    fn first_page(stdout: &str) -> (String, Result<Output, exec::Error>) {
        graphql(POLL_QUERY, &format!("-f id={POLLED_ID}"), stdout)
    }

    fn second_page(stdout: &str, code: i32) -> (String, Result<Output, exec::Error>) {
        let call = format!(
            "gh api -i --hostname github.com graphql -f query={POLL_QUERY} -f id={POLLED_ID} \
             -F checks=false -F comments=false -F dismissals=false -f reviewsAfter=REVIEWS_1 \
             -F threads=false"
        );
        (call, Ok(gh(code, stdout, "")))
    }

    /// Reviews over two pages, then the thread's second page of comments answered by `thread_page`.
    fn paged(thread_page: &str) -> [(String, Result<Output, exec::Error>); 3] {
        [
            first_page(include_str!("github/fixtures/poll_page_1.http")),
            graphql(
                POLL_QUERY,
                &format!(
                    "-f id={POLLED_ID} -F checks=false -F comments=false -F dismissals=false -f reviewsAfter=REVIEWS_1 \
                     -F threads=false"
                ),
                include_str!("github/fixtures/poll_page_2.http"),
            ),
            graphql(
                THREAD_COMMENTS_QUERY,
                &format!("-f id={THREAD_ID} -f commentsAfter=THREAD_1"),
                thread_page,
            ),
        ]
    }

    fn poll_hinted(
        calls: impl IntoIterator<Item = (String, Result<Output, exec::Error>)>,
    ) -> Result<(Poll, Hints), Failure> {
        let exec = Script::new(calls);
        GitHub::new(&exec).poll(&polled(), None)
    }

    fn poll_with(
        calls: impl IntoIterator<Item = (String, Result<Output, exec::Error>)>,
    ) -> Result<Poll, Failure> {
        poll_hinted(calls).map(|(poll, _)| poll)
    }

    /// The failure of a poll whose first call answers `stdout`, exiting 1 as gh does on an error.
    fn poll_failing(stdout: &str) -> Failure {
        let (call, _) = first_page("");
        poll_with([(call, Ok(gh(1, stdout, "gh: failed")))]).unwrap_err()
    }

    fn message(failure: &Failure) -> &str {
        match failure {
            Failure::Fatal { message, .. } | Failure::Transient { message, .. } => message,
            other => panic!("no message: {other:?}"),
        }
    }

    fn retry_after(seconds: u64) -> Hints {
        Hints {
            retry_after: Some(Duration::from_secs(seconds)),
            ..Hints::default()
        }
    }

    fn rate_limit_hints(failure: &Failure) -> Hints {
        match failure {
            Failure::RateLimited { hints, .. } => *hints,
            other => panic!("not a rate limit: {other:?}"),
        }
    }

    fn transient_hints(failure: &Failure) -> Hints {
        match failure {
            Failure::Transient { hints, .. } => *hints,
            other => panic!("not transient: {other:?}"),
        }
    }

    const BRZZDEV_ID: &str = "MDQ6VXNlcjE1Njg3NDUw";
    const GITAR_ID: &str = "BOT_kgDOgitar";

    fn account(login: &str, bot: bool, id: &str) -> Account {
        Account {
            bot: Some(bot),
            id: Some(id.to_owned()),
            login: login.to_owned(),
        }
    }

    fn brzzdev() -> Account {
        account("brzzdev", false, BRZZDEV_ID)
    }

    fn gitar() -> Account {
        account("gitar-bot[bot]", true, GITAR_ID)
    }

    fn expected() -> Poll {
        let thread_comment = |id: &str, review: &str, body: &str, at: &str, actor| Comment {
            actor,
            at: at.to_owned(),
            body: body.to_owned(),
            edited_at: None,
            editor: None,
            id: id.to_owned(),
            reply_to: None,
            review_id: Some(review.to_owned()),
            url: format!("https://github.com/brzzdev/babysit/pull/30#discussion_r{id}"),
        };
        let review = |id: &str, at: &str, actor| Review {
            actor,
            at: Some(at.to_owned()),
            body: String::new(),
            forge_id: Some(id.to_owned()),
            id: id.to_owned(),
            lapses: false,
            state: ReviewState::Commented,
            url: format!("https://github.com/brzzdev/babysit/pull/30#pullrequestreview-{id}"),
            withdrawn_at: None,
            withdrawn_by: None,
        };
        Poll {
            ci: Some(Ci {
                checks: vec![Check {
                    assessment: Assessment::Passed,
                    conclusion: "success".to_owned(),
                    deciding: true,
                    job_id: Some(JOB_ID.to_owned()),
                    name: "test".to_owned(),
                    url: Some(format!(
                        "https://github.com/brzzdev/babysit/actions/runs/39480449747/job/{JOB_ID}"
                    )),
                }],
                required_known: true,
            }),
            comments: vec![Comment {
                actor: Some(brzzdev()),
                at: "2026-10-20T19:07:43Z".to_owned(),
                body: "Paul's Codex:\n\n> Review verdict: Approved\n".to_owned(),
                edited_at: Some("2026-10-20T19:20:00Z".to_owned()),
                editor: Some(brzzdev()),
                id: "6298972911".to_owned(),
                reply_to: None,
                review_id: None,
                url: "https://github.com/brzzdev/babysit/pull/30#issuecomment-6298972911"
                    .to_owned(),
            }],
            head: Some(HEAD.to_owned()),
            merge: None,
            reviews: vec![
                review("5590010588", "2026-10-20T18:52:30Z", Some(brzzdev())),
                review("5590011214", "2026-10-20T18:52:33Z", Some(gitar())),
            ],
            state: TargetState::Merged,
            state_changed_at: Some(MERGED_AT.to_owned()),
            state_changed_by: Some(brzzdev()),
            threads: vec![Thread {
                comments: vec![
                    thread_comment(
                        "4328942196",
                        "5590010588",
                        "Reject durations that overflow SystemTime.",
                        "2026-10-20T18:52:30Z",
                        Some(brzzdev()),
                    ),
                    thread_comment(
                        "4329001432",
                        "5590011214",
                        "Fixed in 0b1ff04.",
                        "2026-10-20T18:58:01Z",
                        Some(gitar()),
                    ),
                ],
                id: THREAD_ID.to_owned(),
                position: Some(Position {
                    line: Some(84),
                    path: "src/run.rs".to_owned(),
                }),
                reply_to: "4328942196".to_owned(),
                resolved: true,
                resolved_by: Some(brzzdev()),
                url: "https://github.com/brzzdev/babysit/pull/30#discussion_r4328942196".to_owned(),
            }],
            url: polled().url,
        }
    }

    #[test]
    fn polls_an_issue_for_its_comments_and_state_alone() {
        let exec = Script::new([graphql(
            ISSUE_POLL_QUERY,
            &format!("-f id={ISSUE_ID}"),
            include_str!("github/fixtures/issue_poll.http"),
        )]);
        let comment = |id: &str, body: &str, at: &str| Comment {
            actor: Some(brzzdev()),
            at: at.to_owned(),
            body: body.to_owned(),
            edited_at: None,
            editor: None,
            id: id.to_owned(),
            reply_to: None,
            review_id: None,
            url: format!("https://github.com/brzzdev/babysit/issues/9#issuecomment-{id}"),
        };

        let (poll, _) = GitHub::new(&exec).poll(&issue(), None).unwrap();

        assert_eq!(
            poll,
            Poll {
                ci: None,
                comments: vec![
                    comment(
                        "6290682886",
                        "Inherited from [How does a caller say which events wake baby",
                        "2026-10-20T12:19:30Z",
                    ),
                    comment("6297001219", "Resolved.", "2026-10-20T17:29:00Z"),
                ],
                head: None,
                merge: None,
                reviews: Vec::new(),
                state: TargetState::Closed,
                state_changed_at: Some("2026-10-20T17:29:02Z".to_owned()),
                state_changed_by: Some(brzzdev()),
                threads: Vec::new(),
                url: issue().url,
            },
        );
    }

    #[test]
    fn polls_comments_reviews_and_threads_with_bot_logins_suffixed() {
        let poll = poll_with([first_page(include_str!("github/fixtures/poll.http"))]);

        assert_eq!(poll.unwrap(), expected());
    }

    #[test]
    fn pages_every_connection_within_the_poll() {
        let poll = poll_with(paged(include_str!("github/fixtures/thread_comments.http")));

        assert_eq!(poll.unwrap(), expected());
    }

    #[test]
    fn a_head_that_moves_between_pages_fails_the_poll() {
        let moved = include_str!("github/fixtures/thread_comments.http").replace(HEAD, "f00d");

        let failure = poll_with(paged(&moved)).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }));
        assert_eq!(
            message(&failure),
            format!("the head moved from {HEAD} to f00d between pages")
        );
    }

    #[test]
    fn a_next_page_without_a_cursor_fails_the_poll() {
        let unfinished = include_str!("github/fixtures/poll_page_1.http")
            .replace(r#""endCursor":"REVIEWS_1""#, r#""endCursor":null"#);

        let failure = poll_with([first_page(&unfinished)]).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }));
        assert_eq!(
            message(&failure),
            "the reviews connection has a next page but no cursor"
        );
    }

    #[test]
    fn a_graphql_error_fails_the_poll() {
        let (call, _) = first_page("");
        let stdout = include_str!("github/fixtures/poll_invalid_cursor.http");
        let stderr = "gh: `Y3Vyc29y` does not appear to be a valid cursor.";

        let failure = poll_with([(call, Ok(gh(1, stdout, stderr)))]).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }));
        assert!(message(&failure).contains("does not appear to be a valid cursor"));
    }

    #[test]
    fn a_deleted_pull_request_is_target_not_found_naming_the_causes() {
        let failure = poll_failing(include_str!("github/fixtures/poll_not_found.http"));

        assert_eq!(kind(&failure), ErrorKind::TargetNotFound);
        assert_eq!(
            message(&failure),
            "https://github.com/brzzdev/babysit/pull/30 is gone: deleted, transferred, or no \
             longer visible to gh's account",
        );
    }

    #[test]
    fn a_not_found_error_off_the_targets_own_path_is_transient() {
        let nested = include_str!("github/fixtures/poll.http").replace(
            "]}}}}\n",
            r#"]}}},"errors":[{"type":"NOT_FOUND","path":["node","reviews"],
                "message":"Could not resolve to a node"}]}"#,
        );

        let failure = poll_with([first_page(&nested)]).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }));
        assert_eq!(message(&failure), "Could not resolve to a node");
    }

    #[test]
    fn a_thread_gone_mid_poll_is_transient() {
        let failure =
            poll_with(paged(include_str!("github/fixtures/poll_not_found.http"))).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }));
        assert_eq!(
            message(&failure),
            format!("review thread {THREAD_ID} vanished")
        );
    }

    #[test]
    fn bad_credentials_and_gh_exit_4_are_auth() {
        let rejected = poll_failing(include_str!("github/fixtures/poll_bad_credentials.http"));
        let (call, _) = first_page("");
        let logged_out = poll_with([(call, Ok(gh(4, "", "gh auth login")))]).unwrap_err();

        assert_eq!(kind(&rejected), ErrorKind::Auth);
        assert_eq!(kind(&logged_out), ErrorKind::Auth);
    }

    #[test]
    fn a_forbidden_reply_that_names_no_rate_limit_is_auth() {
        let forbidden = include_str!("github/fixtures/poll_secondary_rate_limit.http").replace(
            "You have exceeded a secondary rate limit",
            "Resource not accessible by integration",
        );

        assert_eq!(kind(&poll_failing(&forbidden)), ErrorKind::Auth);
    }

    #[test]
    fn empty_stdout_is_transient() {
        let (call, _) = first_page("");
        let stderr = "error connecting to api.github.com";

        let failure = poll_with([(call, Ok(gh(1, "", stderr)))]).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }));
        assert_eq!(
            message(&failure),
            "gh printed no HTTP reply: error connecting to api.github.com"
        );
    }

    #[test]
    fn a_call_past_the_call_limit_is_transient() {
        let (call, _) = first_page("");

        let failure =
            poll_with([(call, Err(exec::Error::TimedOut(Limit::CallLimit)))]).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }));
        assert_eq!(message(&failure), "gh did not answer within 60s");
    }

    #[test]
    fn a_server_error_is_transient_and_carries_its_retry_after() {
        let failure = poll_failing(include_str!("github/fixtures/poll_unavailable.http"));

        assert_eq!(
            transient_hints(&failure).retry_after,
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn a_graphql_rate_limit_waits_for_the_quota_reset() {
        let failure = poll_failing(include_str!("github/fixtures/poll_rate_limited.http"));

        assert_eq!(
            rate_limit_hints(&failure),
            Hints {
                reset: Some(UNIX_EPOCH + Duration::from_secs(1_792_528_442)),
                ..Hints::default()
            }
        );
    }

    #[test]
    fn a_secondary_rate_limit_waits_for_its_retry_after() {
        let failure = poll_failing(include_str!(
            "github/fixtures/poll_secondary_rate_limit.http"
        ));

        assert_eq!(rate_limit_hints(&failure), retry_after(120));
    }

    #[test]
    fn a_secondary_rate_limit_on_a_200_is_a_rate_limit() {
        let ok = include_str!("github/fixtures/poll_secondary_rate_limit.http")
            .replace("HTTP/2.0 403 Forbidden", "HTTP/2.0 200 OK");
        let (call, _) = first_page("");

        let failure = poll_with([(call, Ok(gh(0, &ok, "")))]).unwrap_err();

        assert_eq!(rate_limit_hints(&failure), retry_after(120));
    }

    #[test]
    fn a_transient_graphql_error_keeps_the_replys_retry_after() {
        let stdout = include_str!("github/fixtures/poll_invalid_cursor.http").replacen(
            "\r\n",
            "\r\nRetry-After: 90\r\n",
            1,
        );

        let failure = poll_failing(&stdout);

        assert_eq!(transient_hints(&failure), retry_after(90));
    }

    #[test]
    fn an_undecodable_reply_keeps_its_retry_after() {
        let garbled = include_str!("github/fixtures/poll.http")
            .replacen('\n', "\nRetry-After: 90\n", 1)
            .replace(r#"{"data":"#, r#"{"data""#);

        let failure = poll_with([first_page(&garbled)]).unwrap_err();

        assert_eq!(transient_hints(&failure), retry_after(90));
    }

    #[test]
    fn a_later_page_failing_keeps_the_earlier_pages_pacing() {
        let slower = include_str!("github/fixtures/poll_page_1.http").replacen(
            '\n',
            "\nX-Poll-Interval: 60\n",
            1,
        );

        let failure = poll_with([
            first_page(&slower),
            second_page(include_str!("github/fixtures/poll_unavailable.http"), 1),
        ])
        .unwrap_err();

        assert_eq!(
            transient_hints(&failure),
            Hints {
                poll_interval: Some(Duration::from_secs(60)),
                ..retry_after(90)
            }
        );
    }

    #[test]
    fn a_poll_that_spends_the_quota_is_still_a_poll() {
        let spent = include_str!("github/fixtures/poll.http")
            .replace("X-Ratelimit-Remaining: 4962", "X-Ratelimit-Remaining: 0");

        let (poll, hints) = poll_hinted([first_page(&spent)]).unwrap();

        assert_eq!(poll, expected());
        assert_eq!(
            hints.reset,
            Some(UNIX_EPOCH + Duration::from_secs(1_792_526_402))
        );
    }

    #[test]
    fn a_dismissed_review_carries_its_dismisser_and_time() {
        let dismissed = include_str!("github/fixtures/poll.http")
            .replace(
                &format!(r#""id":"{GITAR_ID}"}},"state":"COMMENTED""#),
                &format!(r#""id":"{GITAR_ID}"}},"state":"DISMISSED""#),
            )
            .replace(
                r#""nodes":[]},"reviews""#,
                r#""nodes":[{"actor":{"__typename":"User","login":"brzzdev",
                    "id":"MDQ6VXNlcjE1Njg3NDUw"},"createdAt":"2026-10-20T19:30:00Z",
                    "review":{"databaseId":5590011214,"state":"DISMISSED"}}]},"reviews""#,
            );

        let poll = poll_with([first_page(&dismissed)]).unwrap();

        let mut withdrawn = expected();
        withdrawn.reviews[1].state = ReviewState::Dismissed;
        withdrawn.reviews[1].withdrawn_at = Some("2026-10-20T19:30:00Z".to_owned());
        withdrawn.reviews[1].withdrawn_by = Some(brzzdev());
        assert_eq!(poll, withdrawn);
    }

    #[test]
    fn a_conflict_beats_being_behind_and_unknown_is_still_computing() {
        let read = |mergeable: &str, status: &str| {
            let fixture = include_str!("github/fixtures/poll.http").replace(
                r#""mergeable":"UNKNOWN","mergeStateStatus":"UNKNOWN""#,
                &format!(r#""mergeable":"{mergeable}","mergeStateStatus":"{status}""#),
            );
            poll_with([first_page(&fixture)]).unwrap().merge
        };

        assert_eq!(
            [
                read("MERGEABLE", "CLEAN"),
                read("MERGEABLE", "BLOCKED"),
                read("MERGEABLE", "BEHIND"),
                read("CONFLICTING", "BEHIND"),
                read("CONFLICTING", "DIRTY"),
                read("CONFLICTING", "UNKNOWN"),
                read("MERGEABLE", "UNKNOWN"),
            ],
            [
                Some(Merge::Clean),
                Some(Merge::Clean),
                Some(Merge::Behind),
                Some(Merge::Conflicted),
                Some(Merge::Conflicted),
                Some(Merge::Conflicted),
                None,
            ],
        );
    }

    #[test]
    fn a_head_the_base_requires_to_be_up_to_date_is_behind() {
        let stdout = include_str!("github/fixtures/poll_behind.http");

        assert_eq!(
            poll_with([first_page(stdout)]).unwrap().merge,
            Some(Merge::Behind)
        );
    }

    #[test]
    fn closing_and_reopening_are_attributed_to_their_timeline_events() {
        let read = |state: &str, event: &str| {
            let fixture = include_str!("github/fixtures/poll.http")
                .replace(r#""state":"MERGED""#, &format!(r#""state":"{state}""#))
                .replace(r#""ClosedEvent""#, &format!(r#""{event}""#));
            let poll = poll_with([first_page(&fixture)]).unwrap();
            (poll.state, poll.state_changed_at, poll.state_changed_by)
        };
        let changed = |state| (state, Some(MERGED_AT.to_owned()), Some(brzzdev()));

        assert_eq!(read("CLOSED", "ClosedEvent"), changed(TargetState::Closed));
        assert_eq!(read("OPEN", "ReopenedEvent"), changed(TargetState::Open));
        assert_eq!(
            read("CLOSED", "ReopenedEvent"),
            (TargetState::Closed, None, None)
        );
    }

    #[test]
    fn an_app_resolving_a_thread_is_a_bot() {
        let resolved_by_app = include_str!("github/fixtures/poll.http").replace(
            r#""resolvedBy":{"__typename":"User","login":"brzzdev","#,
            r#""resolvedBy":{"__typename":"User","login":"github-advanced-security[bot]","#,
        );

        let poll = poll_with([first_page(&resolved_by_app)]).unwrap();

        let resolver = poll.threads[0].resolved_by.as_ref().unwrap();
        assert_eq!(
            (resolver.login.as_str(), resolver.bot),
            ("github-advanced-security[bot]", Some(true))
        );
    }

    #[test]
    fn a_pending_review_and_its_draft_comments_are_left_out() {
        let drafted = include_str!("github/fixtures/poll.http")
            .replace(
                &format!(r#""id":"{GITAR_ID}"}},"state":"COMMENTED""#),
                &format!(r#""id":"{GITAR_ID}"}},"state":"PENDING""#),
            )
            .replace(
                r#""databaseId":5590011214,"state":"COMMENTED""#,
                r#""databaseId":5590011214,"state":"PENDING""#,
            );

        let poll = poll_with([first_page(&drafted)]).unwrap();

        let mut submitted = expected();
        submitted.reviews.pop();
        submitted.threads[0].comments.pop();
        assert_eq!(poll, submitted);
    }

    #[test]
    fn reads_the_viewers_account_id() {
        let exec = Script::new([(
            format!("gh api -i --hostname github.com graphql -f query={VIEWER_QUERY}"),
            Ok(gh(0, include_str!("github/fixtures/viewer.http"), "")),
        )]);

        let viewer = GitHub::new(&exec).viewer("github.com", None);

        assert_eq!(viewer.unwrap().0, BRZZDEV_ID);
    }

    #[test]
    fn a_call_killed_at_the_deadline_times_out() {
        let failure = resolve_with(Err(exec::Error::TimedOut(Limit::Deadline))).unwrap_err();

        assert!(matches!(failure, Failure::TimedOut));
    }

    #[test]
    fn a_missing_gh_is_cli_missing() {
        let failure =
            resolve_with(Err(exec::Error::Io(io::ErrorKind::NotFound.into()))).unwrap_err();

        assert_eq!(kind(&failure), ErrorKind::CliMissing);
    }

    /// The recorded poll with its head's `statusCheckRollup` replaced by `rollup`.
    fn with_rollup(rollup: Value) -> String {
        let fixture = include_str!("github/fixtures/poll.http");
        let (headers, body) = fixture.split_once("\n\n").unwrap();
        let mut body: Value = serde_json::from_str(body).unwrap();
        *body
            .pointer_mut("/data/node/commits/nodes/0/commit/statusCheckRollup")
            .unwrap() = rollup;
        format!("{headers}\n\n{body}")
    }

    fn rollup(state: &str, contexts: &Value) -> Value {
        json!({
            "state": state,
            "contexts": {
                "pageInfo": { "hasNextPage": false, "endCursor": "MQ" },
                "nodes": contexts,
            },
        })
    }

    fn ci_of(stdout: &str) -> Option<Ci> {
        poll_with([first_page(stdout)]).unwrap().ci
    }

    #[test]
    fn judges_each_check_on_its_latest_attempt_and_decides_on_the_required_ones() {
        let actions = json!({
            "app": { "databaseId": 15_368 },
            "workflowRun": { "workflow": { "databaseId": 390_459_010 } },
        });
        let run = |id: u64, conclusion: Option<&str>, status: &str| {
            json!({
                "__typename": "CheckRun", "checkSuite": actions, "conclusion": conclusion,
                "databaseId": id, "detailsUrl": format!("https://example.com/job/{id}"),
                "isRequired": true, "name": "test", "status": status,
            })
        };
        let contexts = json!([
            run(100, Some("FAILURE"), "COMPLETED"),
            run(101, None, "IN_PROGRESS"),
            {
                "__typename": "CheckRun",
                "checkSuite": { "app": { "databaseId": 254 }, "workflowRun": null },
                "conclusion": "FAILURE", "databaseId": 200,
                "detailsUrl": "https://codecov.io/1", "isRequired": false, "name": "codecov",
                "status": "COMPLETED",
            },
            {
                "__typename": "StatusContext", "context": "ci/deploy", "isRequired": true,
                "state": "EXPECTED", "targetUrl": null,
            },
        ]);

        let ci = ci_of(&with_rollup(rollup("FAILURE", &contexts)));

        assert_eq!(
            ci,
            Some(Ci {
                checks: vec![
                    Check {
                        assessment: Assessment::Pending,
                        conclusion: "in_progress".to_owned(),
                        deciding: true,
                        job_id: Some("101".to_owned()),
                        name: "test".to_owned(),
                        url: Some("https://example.com/job/101".to_owned()),
                    },
                    Check {
                        assessment: Assessment::Failed,
                        conclusion: "failure".to_owned(),
                        deciding: false,
                        job_id: None,
                        name: "codecov".to_owned(),
                        url: Some("https://codecov.io/1".to_owned()),
                    },
                    Check {
                        assessment: Assessment::Waiting,
                        conclusion: "expected".to_owned(),
                        deciding: true,
                        job_id: None,
                        name: "ci/deploy".to_owned(),
                        url: None,
                    },
                ],
                required_known: true,
            }),
        );
    }

    #[test]
    fn a_fresh_head_has_no_ci_yet_reads_clean() {
        let stdout = include_str!("github/fixtures/poll_fresh_head.http");

        let poll = poll_with([first_page(stdout)]).unwrap();

        assert_eq!(poll.ci, None);
        assert_eq!(poll.merge, Some(Merge::Clean));
    }

    #[test]
    fn a_latest_commit_other_than_the_head_has_no_ci() {
        let behind = include_str!("github/fixtures/poll.http")
            .replace(&format!(r#""oid":"{HEAD}""#), r#""oid":"f00d""#);

        assert_eq!(ci_of(&behind), None);
    }

    #[test]
    fn an_expected_rollup_naming_no_check_leaves_the_required_set_unknown() {
        let contexts = json!([{
            "__typename": "StatusContext", "context": "lint", "isRequired": false,
            "state": "SUCCESS", "targetUrl": null,
        }]);

        let ci = ci_of(&with_rollup(rollup("EXPECTED", &contexts))).unwrap();

        assert!(!ci.required_known);
        assert!(ci.checks[0].deciding);
    }

    /// Recorded while `gate` awaited approval: `after` (`needs: gate`) is absent from the rollup.
    #[test]
    fn a_job_held_behind_an_approval_is_not_listed_so_amber_settles() {
        let job = |id: u64, name: &str, status: &str, conclusion: Option<&str>| {
            json!({
                "__typename": "CheckRun",
                "checkSuite": {
                    "app": { "databaseId": 15_368 },
                    "workflowRun": {
                        "databaseId": 39_494_970_229_u64,
                        "workflow": { "databaseId": 391_007_203 },
                    },
                },
                "conclusion": conclusion, "databaseId": id,
                "detailsUrl": format!(
                    "https://github.com/brzzdev/babysit/actions/runs/39494970229/job/{id}"
                ),
                "isRequired": false, "name": name, "status": status,
            })
        };
        let contexts = json!([
            job(118_877_549_516, "gate", "WAITING", None),
            job(118_877_548_782, "independent", "COMPLETED", Some("SUCCESS")),
        ]);

        let ci = ci_of(&with_rollup(rollup("PENDING", &contexts))).unwrap();

        assert_eq!(ci.colour(), Some(Colour::Amber));
    }

    #[test]
    fn maps_every_github_row_of_the_colour_table() {
        use Assessment::{Failed, Passed, Pending, Waiting};
        let completed = |conclusion| check_run_assessment("COMPLETED", Some(conclusion));
        for (conclusion, assessment) in [
            ("ACTION_REQUIRED", Waiting),
            ("CANCELLED", Failed),
            ("FAILURE", Failed),
            ("NEUTRAL", Passed),
            ("SKIPPED", Passed),
            ("STALE", Waiting),
            ("STARTUP_FAILURE", Failed),
            ("SUCCESS", Passed),
            ("TIMED_OUT", Failed),
        ] {
            assert_eq!(completed(conclusion), assessment, "{conclusion}");
        }
        for (status, assessment) in [
            ("COMPLETED", Pending),
            ("IN_PROGRESS", Pending),
            ("PENDING", Pending),
            ("QUEUED", Pending),
            ("REQUESTED", Pending),
            ("WAITING", Waiting),
        ] {
            assert_eq!(check_run_assessment(status, None), assessment, "{status}");
        }
        for (state, assessment) in [
            ("ERROR", Failed),
            ("EXPECTED", Waiting),
            ("FAILURE", Failed),
            ("PENDING", Pending),
            ("SUCCESS", Passed),
        ] {
            assert_eq!(status_assessment(state), assessment, "{state}");
        }
    }
}
