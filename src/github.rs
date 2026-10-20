use std::io;
use std::time::SystemTime;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::envelope::{ErrorKind, Forge};
use crate::exec::{self, CALL_LIMIT, Exec, Limit};
use crate::forge::{
    Account, Backend, Comment, Failure, Poll, Review, ReviewState, Target, TargetState, Thread,
};
use crate::reference::Reference;
use crate::response::{self, Response};

const CLI: &str = "gh";
const EXIT_AUTH: i32 = 4;
const EXIT_NOT_FOUND: i32 = 127;
const POLL_QUERY: &str = concat!(
    include_str!("github/poll.graphql"),
    include_str!("github/fragments.graphql"),
);
const STATUS_NOT_FOUND: u16 = 404;
const THREAD_COMMENTS_QUERY: &str = concat!(
    include_str!("github/thread_comments.graphql"),
    include_str!("github/fragments.graphql"),
);

pub struct GitHub<'a, E> {
    exec: &'a E,
}

impl<'a, E: Exec> GitHub<'a, E> {
    pub fn new(exec: &'a E) -> Self {
        Self { exec }
    }

    fn call(&self, args: &[&str], deadline: Option<SystemTime>) -> Result<Response, Failure> {
        let output = match self.exec.run(CLI, args, deadline) {
            Ok(output) => output,
            Err(exec::Error::TimedOut(Limit::Deadline)) => return Err(Failure::TimedOut),
            Err(exec::Error::TimedOut(Limit::CallLimit)) => {
                let message = format!("gh did not answer within {}s", CALL_LIMIT.as_secs());
                return Err(fatal(ErrorKind::PollFailed, &message));
            }
            Err(exec::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Err(fatal(
                    ErrorKind::CliMissing,
                    "gh is not installed or not on PATH",
                ));
            }
            Err(exec::Error::Io(error)) => {
                return Err(fatal(
                    ErrorKind::PollFailed,
                    &format!("could not run gh: {error}"),
                ));
            }
        };
        let reason = output.stderr.trim();
        match output.code {
            Some(EXIT_NOT_FOUND) => return Err(fatal(ErrorKind::CliMissing, reason)),
            Some(EXIT_AUTH) => return Err(fatal(ErrorKind::Auth, reason)),
            _ => {}
        }
        let response = response::parse(&output.stdout).ok_or_else(|| {
            fatal(
                ErrorKind::PollFailed,
                &format!("gh printed no HTTP reply: {reason}"),
            )
        })?;
        if (200..300).contains(&response.status) {
            return Ok(response);
        }
        if response.status == STATUS_NOT_FOUND {
            let message = format!("not found or not visible to gh: {reason}");
            return Err(fatal(ErrorKind::TargetNotFound, &message));
        }
        Err(fatal(
            ErrorKind::PollFailed,
            &format!("HTTP {}: {reason}", response.status),
        ))
    }
}

impl<E: Exec> Backend for GitHub<'_, E> {
    fn resolve(
        &self,
        reference: &Reference,
        deadline: Option<SystemTime>,
    ) -> Result<Target, Failure> {
        let path = format!(
            "repos/{}/{}/pulls/{}",
            reference.owner, reference.repo, reference.number,
        );
        let response = self.call(
            &["api", "-i", "--hostname", &reference.host, &path],
            deadline,
        )?;
        let pull: PullRequest = decode(&response.body)?;
        Ok(Target {
            forge_id: pull.node_id,
            host: reference.host.clone(),
            url: pull.html_url,
        })
    }

    /// Pages every connection to its end, failing the poll if the head moves between pages.
    fn poll(&self, target: &Target, deadline: Option<SystemTime>) -> Result<Poll, Failure> {
        let mut comments = Paged::new("comments");
        let mut reviews = Paged::new("reviews");
        let mut threads = Paged::new("threads");
        let mut first: Option<(String, State)> = None;
        while !(comments.done() && reviews.done() && threads.done()) {
            let mut variables = vec![("-f", format!("id={}", target.forge_id))];
            variables.extend(comments.variable());
            variables.extend(reviews.variable());
            variables.extend(threads.variable());
            let page: PullRequestPage =
                self.query(target, POLL_QUERY, &variables, "pull request", deadline)?;
            match &first {
                None => first = Some((page.head_ref_oid, page.state)),
                Some((head, _)) => same_head(head, &page.head_ref_oid)?,
            }
            comments.take(page.comments)?;
            reviews.take(page.reviews)?;
            threads.take(page.review_threads)?;
        }
        let (head, state) = first.expect("every poll fetches a first page");
        let mut polled_threads = Vec::new();
        for node in threads.nodes {
            let comments =
                self.thread_comments(target, &head, &node.thread.id, node.comments, deadline)?;
            polled_threads.extend(thread(node.thread, comments));
        }
        Ok(Poll {
            comments: comments.nodes.into_iter().map(Comment::from).collect(),
            head,
            reviews: reviews.nodes.into_iter().filter_map(review).collect(),
            state: state.into(),
            threads: polled_threads,
        })
    }
}

impl<E: Exec> GitHub<'_, E> {
    /// A thread's comments, paging past the first page the poll query brought, still on `head`.
    fn thread_comments(
        &self,
        target: &Target,
        head: &str,
        thread_id: &str,
        first: Connection<CommentNode>,
        deadline: Option<SystemTime>,
    ) -> Result<Vec<CommentNode>, Failure> {
        let mut comments = Paged::new("comments");
        comments.take(Some(first))?;
        while !comments.done() {
            let mut variables = vec![("-f", format!("id={thread_id}"))];
            variables.extend(comments.variable());
            let page: ThreadPage = self.query(
                target,
                THREAD_COMMENTS_QUERY,
                &variables,
                "review thread",
                deadline,
            )?;
            same_head(head, &page.pull_request.head_ref_oid)?;
            comments.take(Some(page.comments))?;
        }
        Ok(comments.nodes)
    }

    /// One GraphQL page of `node(id:)`, whose missing node is reported as a missing `what`.
    fn query<T: DeserializeOwned>(
        &self,
        target: &Target,
        query: &str,
        variables: &[(&str, String)],
        what: &str,
        deadline: Option<SystemTime>,
    ) -> Result<T, Failure> {
        let query = format!("query={query}");
        let mut args = vec![
            "api",
            "-i",
            "--hostname",
            &target.host,
            "graphql",
            "-f",
            &query,
        ];
        for (flag, variable) in variables {
            args.extend([*flag, variable.as_str()]);
        }
        let response = self.call(&args, deadline)?;
        let reply: Reply<T> = decode(&response.body)?;
        if let Some(error) = reply.errors.first() {
            return Err(fatal(ErrorKind::PollFailed, &error.message));
        }
        reply.data.and_then(|data| data.node).ok_or_else(|| {
            fatal(
                ErrorKind::PollFailed,
                &format!("{} returned no {what}", target.url),
            )
        })
    }
}

/// One connection's nodes gathered so far, and where its next page starts.
struct Paged<T> {
    name: &'static str,
    nodes: Vec<T>,
    page: Page,
}

enum Page {
    First,
    After(String),
    Done,
}

impl<T> Paged<T> {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            nodes: Vec::new(),
            page: Page::First,
        }
    }

    fn done(&self) -> bool {
        matches!(self.page, Page::Done)
    }

    /// The query variable that asks for the next page, or that skips a finished connection.
    fn variable(&self) -> Option<(&'static str, String)> {
        match &self.page {
            Page::First => None,
            Page::After(cursor) => Some(("-f", format!("{}After={cursor}", self.name))),
            Page::Done => Some(("-F", format!("{}=false", self.name))),
        }
    }

    fn take(&mut self, connection: Option<Connection<T>>) -> Result<(), Failure> {
        if self.done() {
            return Ok(());
        }
        let Some(connection) = connection else {
            let message = format!("the {} connection came back null", self.name);
            return Err(fatal(ErrorKind::PollFailed, &message));
        };
        self.nodes.extend(connection.nodes);
        self.page = match connection.page_info {
            PageInfo {
                has_next_page: false,
                ..
            } => Page::Done,
            PageInfo {
                end_cursor: Some(cursor),
                ..
            } => Page::After(cursor),
            PageInfo {
                end_cursor: None, ..
            } => {
                let message = format!("the {} connection has a next page but no cursor", self.name);
                return Err(fatal(ErrorKind::PollFailed, &message));
            }
        };
        Ok(())
    }
}

fn same_head(first: &str, page: &str) -> Result<(), Failure> {
    if first == page {
        return Ok(());
    }
    let message = format!("the head moved from {first} to {page} between pages");
    Err(fatal(ErrorKind::PollFailed, &message))
}

#[derive(Deserialize)]
struct PullRequest {
    html_url: String,
    node_id: String,
}

#[derive(Deserialize)]
struct Reply<T> {
    data: Option<Data<T>>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct Data<T> {
    node: Option<T>,
}

/// A connection skipped with `@include(if: false)` is absent.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestPage {
    comments: Option<Connection<CommentNode>>,
    head_ref_oid: String,
    review_threads: Option<Connection<ThreadNode>>,
    reviews: Option<Connection<ReviewNode>>,
    state: State,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPage {
    comments: Connection<CommentNode>,
    pull_request: HeadNode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadNode {
    head_ref_oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    nodes: Vec<T>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    end_cursor: Option<String>,
    has_next_page: bool,
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

#[derive(Deserialize)]
struct Author {
    #[serde(rename = "__typename")]
    typename: Typename,
    login: String,
}

#[derive(Deserialize)]
enum Typename {
    Bot,
    User,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum State {
    Closed,
    Merged,
    Open,
}

/// GraphQL reports a bot's login bare; REST, and so rules, carry the `[bot]` suffix.
impl From<Author> for Account {
    fn from(author: Author) -> Self {
        match author.typename {
            Typename::Bot => Self {
                bot: Some(true),
                login: format!("{}[bot]", author.login),
            },
            Typename::User => Self {
                bot: Some(false),
                login: author.login,
            },
            Typename::Other => Self {
                bot: None,
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
            review: node
                .pull_request_review
                .map(|review| review.database_id.to_string()),
            url: node.url,
        }
    }
}

/// A pending review is the viewer's unsubmitted draft, so it and its comments are left out.
fn review(node: ReviewNode) -> Option<Review> {
    let state = match node.state {
        ReviewNodeState::Approved => ReviewState::Approved,
        ReviewNodeState::ChangesRequested => ReviewState::ChangesRequested,
        ReviewNodeState::Commented => ReviewState::Commented,
        ReviewNodeState::Dismissed => ReviewState::Dismissed,
        ReviewNodeState::Pending => return None,
    };
    Some(Review {
        actor: node.author.map(Account::from),
        at: node.submitted_at?,
        body: node.body,
        id: node.database_id.to_string(),
        state,
        url: node.url,
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
        line: node.line,
        path: node.path,
        reply_to: first.id.clone(),
        resolved: node.is_resolved,
        resolved_by: node.resolved_by.map(Account::from),
        url: first.url.clone(),
        comments,
    })
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
    serde_json::from_str(body).map_err(|error| {
        fatal(
            ErrorKind::PollFailed,
            &format!("unreadable reply from gh: {error}"),
        )
    })
}

fn fatal(kind: ErrorKind, message: &str) -> Failure {
    Failure::Fatal {
        forge: Forge::GitHub,
        kind,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, Script};
    use crate::reference;

    const NODE_ID: &str = "PR_kwDOVQ0d8c8AAAABJm7W-Q";
    const RESOLVE: &str = "gh api -i --hostname github.com repos/brzzdev/babysit/pulls/15";

    fn reference() -> Reference {
        reference::parse("https://github.com/brzzdev/babysit/pull/15").unwrap()
    }

    fn target() -> Target {
        Target {
            forge_id: NODE_ID.to_owned(),
            host: "github.com".to_owned(),
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
        GitHub::new(&exec).resolve(&reference(), None)
    }

    fn kind(failure: &Failure) -> ErrorKind {
        match failure {
            Failure::Fatal { kind, .. } => *kind,
            Failure::TimedOut => panic!("timed out"),
        }
    }

    #[test]
    fn resolves_a_pull_request_to_its_url_and_node_id() {
        let resolved = resolve_with(Ok(gh(0, include_str!("github/fixtures/resolve.http"), "")));

        assert_eq!(resolved.unwrap(), target());
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
    const POLLED_ID: &str = "PR_kwDOVQ0d8c8AAAABJn0ayQ";
    const THREAD_ID: &str = "PRRT_kwDOVQ0d8c6ueY8r";

    fn polled() -> Target {
        Target {
            forge_id: POLLED_ID.to_owned(),
            host: "github.com".to_owned(),
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

    /// Reviews over two pages, then the thread's second page of comments answered by `thread_page`.
    fn paged(thread_page: &str) -> [(String, Result<Output, exec::Error>); 3] {
        [
            first_page(include_str!("github/fixtures/poll_page_1.http")),
            graphql(
                POLL_QUERY,
                &format!(
                    "-f id={POLLED_ID} -F comments=false -f reviewsAfter=REVIEWS_1 -F threads=false"
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

    fn poll_with(
        calls: impl IntoIterator<Item = (String, Result<Output, exec::Error>)>,
    ) -> Result<Poll, Failure> {
        let exec = Script::new(calls);
        GitHub::new(&exec).poll(&polled(), None)
    }

    fn message(failure: &Failure) -> &str {
        match failure {
            Failure::Fatal { message, .. } => message,
            Failure::TimedOut => panic!("timed out"),
        }
    }

    fn account(login: &str, bot: bool) -> Account {
        Account {
            bot: Some(bot),
            login: login.to_owned(),
        }
    }

    fn expected() -> Poll {
        let thread_comment = |id: &str, review: &str, body: &str, at: &str, actor| Comment {
            actor,
            at: at.to_owned(),
            body: body.to_owned(),
            edited_at: None,
            editor: None,
            id: id.to_owned(),
            review: Some(review.to_owned()),
            url: format!("https://github.com/brzzdev/babysit/pull/30#discussion_r{id}"),
        };
        let review = |id: &str, at: &str, actor| Review {
            actor,
            at: at.to_owned(),
            body: String::new(),
            id: id.to_owned(),
            state: ReviewState::Commented,
            url: format!("https://github.com/brzzdev/babysit/pull/30#pullrequestreview-{id}"),
        };
        Poll {
            comments: vec![Comment {
                actor: Some(account("brzzdev", false)),
                at: "2026-10-20T19:07:43Z".to_owned(),
                body: "Paul's Codex:\n\n> Review verdict: Approved\n".to_owned(),
                edited_at: Some("2026-10-20T19:20:00Z".to_owned()),
                editor: Some(account("brzzdev", false)),
                id: "6298972911".to_owned(),
                review: None,
                url: "https://github.com/brzzdev/babysit/pull/30#issuecomment-6298972911"
                    .to_owned(),
            }],
            head: HEAD.to_owned(),
            reviews: vec![
                review(
                    "5590010588",
                    "2026-10-20T18:52:30Z",
                    Some(account("brzzdev", false)),
                ),
                review(
                    "5590011214",
                    "2026-10-20T18:52:33Z",
                    Some(account("gitar-bot[bot]", true)),
                ),
            ],
            state: TargetState::Merged,
            threads: vec![Thread {
                comments: vec![
                    thread_comment(
                        "4328942196",
                        "5590010588",
                        "Reject durations that overflow SystemTime.",
                        "2026-10-20T18:52:30Z",
                        Some(account("brzzdev", false)),
                    ),
                    thread_comment(
                        "4329001432",
                        "5590011214",
                        "Fixed in 0b1ff04.",
                        "2026-10-20T18:58:01Z",
                        Some(account("gitar-bot[bot]", true)),
                    ),
                ],
                id: THREAD_ID.to_owned(),
                line: Some(84),
                path: "src/run.rs".to_owned(),
                reply_to: "4328942196".to_owned(),
                resolved: true,
                resolved_by: Some(account("brzzdev", false)),
                url: "https://github.com/brzzdev/babysit/pull/30#discussion_r4328942196".to_owned(),
            }],
        }
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

        assert_eq!(kind(&failure), ErrorKind::PollFailed);
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

        assert_eq!(kind(&failure), ErrorKind::PollFailed);
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

        assert_eq!(kind(&failure), ErrorKind::PollFailed);
        assert!(message(&failure).contains("does not appear to be a valid cursor"));
    }

    #[test]
    fn a_pending_review_and_its_draft_comments_are_left_out() {
        let drafted = include_str!("github/fixtures/poll.http")
            .replace(
                r#""login":"gitar-bot"},"state":"COMMENTED""#,
                r#""login":"gitar-bot"},"state":"PENDING""#,
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
}
