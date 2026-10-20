use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::envelope::{ErrorKind, Forge};
use crate::exec::Exec;
use crate::forge::{
    Account, Assessment, Backend, Check, Ci, Comment, Failure, Hints, Poll, Position, Review,
    ReviewState, Target, TargetKind, TargetState, Thread, run_cli,
};
use crate::graphql::{Connection, Paged, same_head};
use crate::reference::Reference;
use crate::response::{self, Response};

const CI_CREATED: &str = "CREATED";
const CI_MANUAL: &str = "MANUAL";
const CI_SKIPPED: &str = "SKIPPED";
const CLI: &str = "glab";
const HEADER_RATE_LIMIT_NAME: &str = "ratelimit-name";
const HEADER_REMAINING: &str = "ratelimit-remaining";
const HEADER_RESET: &str = "ratelimit-reset";
const HEADER_RETRY_AFTER: &str = "retry-after";
const MERGED_RESULT: &str = "MERGED_RESULT";
const MERGE_REQUEST_GID: &str = "gid://gitlab/MergeRequest/";
const MERGE_TRAIN: &str = "MERGE_TRAIN";
const PIPELINE_CHECK: &str = "pipeline";
const POLL_QUERY: &str = include_str!("gitlab/poll.graphql");
const REVIEW_STATE_REQUESTED_CHANGES: &str = "REQUESTED_CHANGES";
const REVIEW_STATE_REVIEWED: &str = "REVIEWED";
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_NOT_FOUND: u16 = 404;
const STATUS_TOO_MANY_REQUESTS: u16 = 429;
const STATUS_UNAUTHORIZED: u16 = 401;
/// The rate limit GitLab applies to a request it took as anonymous.
const UNAUTHENTICATED: &str = "throttle_unauthenticated_api";
const VIEWER_QUERY: &str = include_str!("gitlab/viewer.graphql");

pub struct GitLab<'a, E> {
    exec: &'a E,
}

impl<'a, E: Exec> GitLab<'a, E> {
    pub fn new(exec: &'a E) -> Self {
        Self { exec }
    }

    /// The reply and its pacing, or the failure, classified from stdout before stderr.
    fn call(
        &self,
        args: &[&str],
        deadline: Option<SystemTime>,
    ) -> Result<(Response, Hints), Failure> {
        let output = run_cli(self.exec, Forge::GitLab, CLI, args, deadline)?;
        let reason = output.stderr.trim();
        let Some(response) = response::parse(&output.stdout) else {
            return Err(transient(&format!("glab printed no HTTP reply: {reason}")));
        };
        if response.header(HEADER_RATE_LIMIT_NAME) == Some(UNAUTHENTICATED) {
            return Err(fatal(
                ErrorKind::Auth,
                "GitLab took glab's request as anonymous; log in with `glab auth login`",
            ));
        }
        let hints = pacing(&response);
        if response.is_success() {
            return Ok((response, hints));
        }
        if response.status == STATUS_TOO_MANY_REQUESTS || quota_spent(&response) {
            return Err(Failure::RateLimited {
                forge: Forge::GitLab,
                hints,
            });
        }
        match response.status {
            STATUS_UNAUTHORIZED | STATUS_FORBIDDEN => Err(fatal(ErrorKind::Auth, reason)),
            STATUS_NOT_FOUND => {
                let message = format!("not found or not visible to glab: {reason}");
                Err(fatal(ErrorKind::TargetNotFound, &message))
            }
            status => Err(Failure::Transient {
                forge: Forge::GitLab,
                hints,
                message: format!("HTTP {status}: {reason}"),
            }),
        }
    }

    /// The decoded reply, folding its pacing into `hints`; any GraphQL error fails the call.
    fn graphql<T: DeserializeOwned>(
        &self,
        host: &str,
        query: &str,
        variables: &[(&str, String)],
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<T, Failure> {
        let query = format!("query={query}");
        let mut args = vec!["api", "-i", "--hostname", host, "graphql", "-f", &query];
        for (flag, variable) in variables {
            args.extend([*flag, variable.as_str()]);
        }
        let (response, pacing) = self.call(&args, deadline)?;
        *hints = hints.merge(pacing);
        let reply: Reply<T> = decode(&response.body)?;
        if let Some(error) = reply.errors.first() {
            return Err(transient(&error.message));
        }
        reply
            .data
            .ok_or_else(|| transient("glab returned neither data nor an error"))
    }
}

impl<E: Exec> Backend for GitLab<'_, E> {
    fn resolve(
        &self,
        reference: &Reference,
        deadline: Option<SystemTime>,
    ) -> Result<(Target, Hints), Failure> {
        let path = format!(
            "projects/{}/merge_requests/{}",
            reference.project.replace('/', "%2F"),
            reference.number,
        );
        let (response, hints) = self.call(
            &["api", "-i", "--hostname", &reference.host, &path],
            deadline,
        )?;
        let merge_request: MergeRequest =
            decode(&response.body).map_err(|failure| failure.paced(hints))?;
        let target = Target {
            forge: Forge::GitLab,
            forge_id: format!("{MERGE_REQUEST_GID}{}", merge_request.id),
            host: reference.host.clone(),
            kind: TargetKind::ChangeRequest,
            url: merge_request.web_url,
        };
        Ok((target, hints))
    }

    fn poll(
        &self,
        target: &Target,
        deadline: Option<SystemTime>,
    ) -> Result<(Poll, Hints), Failure> {
        let mut hints = Hints::default();
        match self.poll_pages(target, deadline, &mut hints) {
            Ok(poll) => Ok((poll, hints)),
            Err(failure) => Err(failure.paced(hints)),
        }
    }

    fn viewer(&self, host: &str, deadline: Option<SystemTime>) -> Result<(String, Hints), Failure> {
        let mut hints = Hints::default();
        let data: ViewerData = self
            .graphql(host, VIEWER_QUERY, &[], deadline, &mut hints)
            .map_err(|failure| failure.paced(hints))?;
        let Some(user) = data.current_user else {
            let message = format!("glab is not logged in to {host}; run `glab auth login`");
            return Err(fatal(ErrorKind::Auth, &message));
        };
        Ok((user.id, hints))
    }
}

impl<E: Exec> GitLab<'_, E> {
    /// Pages every connection to its end, failing the poll if the head moves between pages.
    fn poll_pages(
        &self,
        target: &Target,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Poll, Failure> {
        let mut approvers = Paged::new(Forge::GitLab, "approvers");
        let mut jobs = Paged::new(Forge::GitLab, "jobs");
        let mut notes = Paged::new(Forge::GitLab, "notes");
        let mut reviewers = Paged::new(Forge::GitLab, "reviewers");
        let mut first: Option<(String, MergeRequestPage)> = None;
        while !(approvers.done() && jobs.done() && notes.done() && reviewers.done()) {
            let mut variables = vec![("-f", format!("id={}", target.forge_id))];
            variables.extend(approvers.variable());
            variables.extend(jobs.variable());
            variables.extend(notes.variable());
            variables.extend(reviewers.variable());
            let data: Data = self.graphql(&target.host, POLL_QUERY, &variables, deadline, hints)?;
            let Some(mut page) = data.merge_request else {
                return Err(gone(target));
            };
            let Some(head) = page.diff_head_sha.take() else {
                return Err(transient(&format!("{} has no head commit yet", target.url)));
            };
            match &first {
                None => {
                    let pipeline = page.head_pipeline.as_ref();
                    if !pipeline.is_some_and(|pipeline| pipeline.may_build(&head)) {
                        jobs.finish();
                    }
                }
                Some((first_head, first_page)) => {
                    same_head(Forge::GitLab, first_head, &head)?;
                    if first_page.pipeline_id() != page.pipeline_id() {
                        return Err(transient("the head pipeline changed between pages"));
                    }
                }
            }
            approvers.take(page.approved_by.take())?;
            jobs.take(
                page.head_pipeline
                    .as_mut()
                    .and_then(|pipeline| pipeline.jobs.take()),
            )?;
            notes.take(page.notes.take())?;
            reviewers.take(page.reviewers.take())?;
            if first.is_none() {
                first = Some((head, page));
            }
        }
        let (head, page) = first.expect("every poll fetches a first page");
        let skipped_accepted = page
            .project
            .and_then(|project| project.allow_merge_on_skipped_pipeline)
            .unwrap_or(false);
        let ci = match page.head_pipeline {
            Some(pipeline)
                if self.built_on(
                    &head,
                    &pipeline,
                    page.project_id,
                    &target.host,
                    deadline,
                    hints,
                )? =>
            {
                Some(ci(&pipeline, jobs.nodes, &target.url, skipped_accepted))
            }
            _ => None,
        };
        let (comments, threads) = conversation(notes.nodes);
        let mut reviews: Vec<_> = approvers
            .nodes
            .into_iter()
            .map(|user| standing(target, user, ReviewState::Approved))
            .collect();
        reviews.extend(reviewers.nodes.into_iter().filter_map(|reviewer| {
            let state = match reviewer.merge_request_interaction?.review_state.as_str() {
                REVIEW_STATE_REQUESTED_CHANGES => ReviewState::ChangesRequested,
                REVIEW_STATE_REVIEWED => ReviewState::Commented,
                _ => return None,
            };
            Some(standing(target, reviewer.user, state))
        }));
        Ok(Poll {
            ci,
            comments,
            head: Some(head),
            merge: None,
            reviews,
            state: page.state.into(),
            state_changed_at: None,
            state_changed_by: None,
            threads,
            url: target.url.clone(),
        })
    }

    /// Whether `pipeline` ran `head`: on it, or on a merge commit whose second parent it is,
    /// which only REST lists.
    fn built_on(
        &self,
        head: &str,
        pipeline: &HeadPipeline,
        project_id: u64,
        host: &str,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<bool, Failure> {
        if pipeline.sha == head {
            return Ok(true);
        }
        if !pipeline.may_build(head) {
            return Ok(false);
        }
        let path = format!(
            "projects/{project_id}/repository/commits/{}?stats=false",
            pipeline.sha,
        );
        let (response, pacing) = self.call(&["api", "-i", "--hostname", host, &path], deadline)?;
        *hints = hints.merge(pacing);
        let commit: Commit = decode(&response.body)?;
        Ok(commit
            .parent_ids
            .get(1)
            .is_some_and(|parent| parent == head))
    }
}

/// The head pipeline's jobs at their latest attempts, deciding unless allowed to fail, with a
/// skipped pipeline as a check of its own.
fn ci(pipeline: &HeadPipeline, jobs: Vec<Job>, target_url: &str, skipped_accepted: bool) -> Ci {
    let origin = web_origin(target_url);
    // Jobs held behind a blocking manual job stay created while the pipeline waits on it.
    let blocked = pipeline.status == CI_MANUAL;
    let mut checks: Vec<_> = jobs
        .into_iter()
        .filter(|job| !(blocked && job.status == CI_CREATED))
        .map(|job| Check {
            assessment: job_assessment(&job.status),
            conclusion: job.status.to_ascii_lowercase(),
            deciding: !job.allow_failure,
            job_id: Some(rest_id(&job.id).to_owned()),
            name: job.name,
            url: job.web_path.map(|path| format!("{origin}{path}")),
        })
        .collect();
    if pipeline.status == CI_SKIPPED {
        checks.push(Check {
            assessment: if skipped_accepted {
                Assessment::Passed
            } else {
                Assessment::Waiting
            },
            conclusion: pipeline.status.to_ascii_lowercase(),
            deciding: true,
            job_id: None,
            name: PIPELINE_CHECK.to_owned(),
            url: Some(format!("{origin}{}", pipeline.path)),
        });
    }
    Ci {
        checks,
        required_known: true,
    }
}

/// A status this mapping doesn't know is unsettled.
fn job_assessment(status: &str) -> Assessment {
    match status {
        CI_SKIPPED | "SUCCESS" => Assessment::Passed,
        "CANCELED" | "FAILED" => Assessment::Failed,
        CI_MANUAL => Assessment::Waiting,
        _ => Assessment::Pending,
    }
}

/// The scheme and authority of a web URL such as `https://gitlab.com/o/p/-/merge_requests/1`.
fn web_origin(url: &str) -> &str {
    let authority = url.find("://").map_or(0, |scheme| scheme + "://".len());
    url[authority..]
        .find('/')
        .map_or(url, |path| &url[..authority + path])
}

/// Threads where GitLab lets them be resolved, and otherwise comments replying to their
/// discussion.
fn conversation(notes: Vec<Note>) -> (Vec<Comment>, Vec<Thread>) {
    let mut comments = Vec::new();
    let mut threads: Vec<Thread> = Vec::new();
    for note in notes {
        let discussion = note.discussion;
        let discussion_id = rest_id(&discussion.id).to_owned();
        let comment = Comment {
            actor: note.author.map(Account::from),
            at: note.created_at,
            body: note.body,
            edited_at: note.last_edited_at,
            editor: note.last_edited_by.map(Account::from),
            id: rest_id(&note.id).to_owned(),
            reply_to: None,
            review_id: None,
            url: note.url,
        };
        if !discussion.resolvable {
            comments.push(Comment {
                reply_to: Some(discussion_id),
                ..comment
            });
            continue;
        }
        if let Some(thread) = threads.iter_mut().find(|thread| thread.id == discussion_id) {
            thread.comments.push(comment);
            continue;
        }
        threads.push(Thread {
            id: discussion_id.clone(),
            position: note.position.map(|position| Position {
                line: position.new_line.or(position.old_line),
                path: position.file_path,
            }),
            reply_to: discussion_id,
            resolved: discussion.resolved,
            resolved_by: discussion.resolved_by.map(Account::from),
            url: comment.url.clone(),
            comments: vec![comment],
        });
    }
    (comments, threads)
}

/// A reviewer's standing as a review: GitLab neither times it nor gives it an id, and an approval
/// lapses when it leaves the approver list.
fn standing(target: &Target, user: User, state: ReviewState) -> Review {
    let id = format!("{state:?}:{}", user.id);
    Review {
        actor: Some(user.into()),
        at: None,
        body: String::new(),
        forge_id: None,
        id,
        lapses: state == ReviewState::Approved,
        state,
        url: target.url.clone(),
        withdrawn_at: None,
        withdrawn_by: None,
    }
}

/// The REST id at the end of a global id such as `gid://gitlab/Note/12`.
fn rest_id(global_id: &str) -> &str {
    global_id.rsplit('/').next().unwrap_or(global_id)
}

fn pacing(response: &Response) -> Hints {
    Hints {
        poll_interval: None,
        reset: response
            .seconds(HEADER_RESET)
            .filter(|_| quota_spent(response))
            .map(|since_epoch| UNIX_EPOCH + since_epoch),
        retry_after: response.seconds(HEADER_RETRY_AFTER),
    }
}

fn quota_spent(response: &Response) -> bool {
    response.header(HEADER_REMAINING) == Some("0")
}

fn gone(target: &Target) -> Failure {
    let message = format!(
        "{} is gone: deleted, moved, or no longer visible to glab's account",
        target.url,
    );
    fatal(ErrorKind::TargetNotFound, &message)
}

fn decode<T: DeserializeOwned>(body: &str) -> Result<T, Failure> {
    serde_json::from_str(body)
        .map_err(|error| transient(&format!("unreadable reply from glab: {error}")))
}

fn transient(message: &str) -> Failure {
    Failure::transient(Forge::GitLab, message)
}

fn fatal(kind: ErrorKind, message: &str) -> Failure {
    Failure::fatal(Forge::GitLab, kind, message)
}

#[derive(Deserialize)]
struct Commit {
    parent_ids: Vec<String>,
}

#[derive(Deserialize)]
struct MergeRequest {
    id: u64,
    web_url: String,
}

#[derive(Deserialize)]
struct Reply<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    merge_request: Option<MergeRequestPage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewerData {
    current_user: Option<Viewer>,
}

#[derive(Deserialize)]
struct Viewer {
    id: String,
}

/// A connection skipped with `@include(if: false)` is absent.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeRequestPage {
    approved_by: Option<Connection<User>>,
    diff_head_sha: Option<String>,
    head_pipeline: Option<HeadPipeline>,
    notes: Option<Connection<Note>>,
    project: Option<Project>,
    project_id: u64,
    reviewers: Option<Connection<Reviewer>>,
    state: State,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadPipeline {
    id: String,
    jobs: Option<Connection<Job>>,
    merge_request_event_type: Option<String>,
    path: String,
    sha: String,
    status: String,
}

impl MergeRequestPage {
    fn pipeline_id(&self) -> Option<&str> {
        self.head_pipeline
            .as_ref()
            .map(|pipeline| pipeline.id.as_str())
    }
}

impl HeadPipeline {
    /// Whether the pipeline ran `head`, or may have through a merge commit only REST can show.
    fn may_build(&self, head: &str) -> bool {
        self.sha == head
            || matches!(
                self.merge_request_event_type.as_deref(),
                Some(MERGED_RESULT | MERGE_TRAIN)
            )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Job {
    allow_failure: bool,
    id: String,
    name: String,
    status: String,
    web_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Project {
    allow_merge_on_skipped_pipeline: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Note {
    author: Option<User>,
    body: String,
    created_at: String,
    discussion: Discussion,
    id: String,
    last_edited_at: Option<String>,
    last_edited_by: Option<User>,
    position: Option<NotePosition>,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Discussion {
    id: String,
    resolvable: bool,
    resolved: bool,
    resolved_by: Option<User>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotePosition {
    file_path: String,
    new_line: Option<u64>,
    old_line: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reviewer {
    merge_request_interaction: Option<Interaction>,
    #[serde(flatten)]
    user: User,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Interaction {
    review_state: String,
}

#[derive(Deserialize)]
struct User {
    bot: bool,
    id: String,
    username: String,
}

/// `locked` is a merge in progress, so the merge request is still open.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Closed,
    Locked,
    Merged,
    Opened,
}

impl From<User> for Account {
    fn from(user: User) -> Self {
        Self {
            bot: Some(user.bot),
            id: Some(user.id),
            login: user.username,
        }
    }
}

impl From<State> for TargetState {
    fn from(state: State) -> Self {
        match state {
            State::Closed => Self::Closed,
            State::Locked | State::Opened => Self::Open,
            State::Merged => Self::Merged,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::exec::{self, Output, Script};
    use crate::forge::Colour;
    use crate::reference;

    const FORGE_ID: &str = "gid://gitlab/MergeRequest/552084316";
    const HEAD: &str = "25892092f1db0711fcea689c27db39f6d928d2eb";
    /// The recorded head pipeline's merge commit.
    const MERGE_SHA: &str = "d29dd22357102fba3709a18e01c6a96e025174a3";
    /// The fixture's `Ratelimit-Reset`.
    const RESET: u64 = 1_792_533_720;
    const RESOLVE: &str = "glab api -i --hostname gitlab.com \
                           projects/gitlab-org%2Fcli/merge_requests/4157";
    const URL: &str = "https://gitlab.com/gitlab-org/cli/-/merge_requests/4157";

    fn target() -> Target {
        Target {
            forge: Forge::GitLab,
            forge_id: FORGE_ID.to_owned(),
            host: "gitlab.com".to_owned(),
            kind: TargetKind::ChangeRequest,
            url: URL.to_owned(),
        }
    }

    fn resolve_with(stdout: &str, code: i32) -> Result<Target, Failure> {
        let exec = Script::new([(RESOLVE.to_owned(), Ok(Output::exited(code, stdout)))]);
        GitLab::new(&exec)
            .resolve(&reference::parse(URL).unwrap(), None)
            .map(|(target, _)| target)
    }

    fn graphql(variables: &str, stdout: &str) -> (String, Result<Output, exec::Error>) {
        let call = format!(
            "glab api -i --hostname gitlab.com graphql -f query={POLL_QUERY} -f id={FORGE_ID}\
             {variables}"
        );
        (call, Ok(Output::exited(0, stdout)))
    }

    fn poll_with(
        calls: impl IntoIterator<Item = (String, Result<Output, exec::Error>)>,
    ) -> Result<Poll, Failure> {
        let exec = Script::new(calls);
        GitLab::new(&exec)
            .poll(&target(), None)
            .map(|(poll, _)| poll)
    }

    /// The recorded poll, its merged-results pipeline sending it on to `commit`.
    fn recorded(commit: &str) -> [(String, Result<Output, exec::Error>); 2] {
        [
            graphql("", include_str!("gitlab/fixtures/poll.http")),
            commit_lookup(commit),
        ]
    }

    fn paged(second: &str) -> [(String, Result<Output, exec::Error>); 2] {
        [
            graphql("", include_str!("gitlab/fixtures/poll_page_1.http")),
            graphql(
                " -F approvers=false -F jobs=false -f notesAfter=NOTES_1 -F reviewers=false",
                second,
            ),
        ]
    }

    fn kind(failure: &Failure) -> ErrorKind {
        match failure {
            Failure::Fatal { kind, .. } => *kind,
            other => panic!("not fatal: {other:?}"),
        }
    }

    #[test]
    fn resolves_a_merge_request_to_its_web_url_and_global_id() {
        let resolved = resolve_with(include_str!("gitlab/fixtures/resolve.http"), 0);

        assert_eq!(resolved.unwrap(), target());
    }

    #[test]
    fn a_missing_merge_request_is_not_found() {
        let stdout = include_str!("gitlab/fixtures/resolve_not_found.http");

        assert_eq!(
            kind(&resolve_with(stdout, 1).unwrap_err()),
            ErrorKind::TargetNotFound,
        );
    }

    #[test]
    fn an_anonymous_request_is_auth_even_when_it_succeeds() {
        let anonymous = include_str!("gitlab/fixtures/resolve.http")
            .replace("throttle_authenticated_api", UNAUTHENTICATED);

        for (stdout, code) in [
            (anonymous.as_str(), 0),
            (
                include_str!("gitlab/fixtures/resolve_unauthenticated.http"),
                1,
            ),
        ] {
            assert_eq!(
                kind(&resolve_with(stdout, code).unwrap_err()),
                ErrorKind::Auth
            );
        }
    }

    #[test]
    fn a_429_is_a_rate_limit_waiting_out_retry_after_and_the_reset() {
        let stdout = include_str!("gitlab/fixtures/poll_rate_limited.http");

        let failure = poll_with([graphql("", stdout)]).unwrap_err();

        let Failure::RateLimited { hints, .. } = failure else {
            panic!("not a rate limit: {failure:?}");
        };
        assert_eq!(hints.retry_after, Some(Duration::from_secs(60)));
        assert_eq!(hints.reset, Some(UNIX_EPOCH + Duration::from_secs(RESET)),);
    }

    #[test]
    fn a_null_merge_request_with_no_error_is_not_found() {
        let stdout = include_str!("gitlab/fixtures/poll_not_found.http");

        let failure = poll_with([graphql("", stdout)]).unwrap_err();

        assert_eq!(kind(&failure), ErrorKind::TargetNotFound);
    }

    #[test]
    fn looks_up_the_viewer_by_global_id() {
        let call = format!("glab api -i --hostname gitlab.com graphql -f query={VIEWER_QUERY}");
        let exec = Script::new([(
            call,
            Ok(Output::exited(
                0,
                include_str!("gitlab/fixtures/viewer.http"),
            )),
        )]);

        let (id, _) = GitLab::new(&exec).viewer("gitlab.com", None).unwrap();

        assert_eq!(id, "gid://gitlab/User/40203473");
    }

    #[test]
    fn maps_resolvable_threads_and_replyable_comments() {
        let poll = poll_with(recorded(include_str!("gitlab/fixtures/commit.http"))).unwrap();

        assert_eq!(poll.head.as_deref(), Some(HEAD));
        assert_eq!(poll.state, TargetState::Open);
        let comments: Vec<_> = poll
            .comments
            .iter()
            .map(|comment| (comment.id.as_str(), comment.reply_to.as_deref()))
            .collect();
        assert_eq!(
            comments,
            [
                (
                    "4049949479",
                    Some("4ac8a6493822a310c760a852d76c3bf3d1e8a94b")
                ),
                (
                    "4050018035",
                    Some("18a04798274948e1f6a9b1ef0e3c5e9abc3920b9")
                ),
            ],
        );
        let threads: Vec<_> = poll
            .threads
            .iter()
            .map(|thread| {
                let ids: Vec<_> = thread
                    .comments
                    .iter()
                    .map(|comment| comment.id.as_str())
                    .collect();
                (
                    thread.id.as_str(),
                    thread.resolved,
                    thread.position.clone(),
                    ids,
                )
            })
            .collect();
        let at = |path: &str, line| {
            Some(Position {
                line: Some(line),
                path: format!("internal/commands/artifactregistry/login/{path}"),
            })
        };
        assert_eq!(
            threads,
            [
                (
                    "ddfe22cff03efeaa98ea8474688a16904ae0e973",
                    true,
                    at("login_test.go", 321),
                    vec!["4049949528", "4050140331"],
                ),
                (
                    "f929b9bfaf8a77f42b116e18da646ab6e264d7bc",
                    false,
                    None,
                    vec!["4050919082"]
                ),
                (
                    "55bb7312299fb319489710a0e78b15aba5ed3d83",
                    false,
                    at("sbt.go", 138),
                    vec!["4052025509"]
                ),
                (
                    "8afff014b3db3f6d57def1974433775f42203479",
                    false,
                    at("login.go", 113),
                    vec!["4052025569"]
                ),
            ],
        );
    }
    #[test]
    fn maps_approvals_and_reviewer_states_to_reviews() {
        let poll = poll_with(recorded(include_str!("gitlab/fixtures/commit.http"))).unwrap();

        let reviews: Vec<_> = poll
            .reviews
            .iter()
            .map(|review| {
                let actor = review.actor.as_ref().unwrap();
                (review.state, actor.login.as_str(), actor.bot, review.lapses)
            })
            .collect();
        assert_eq!(
            reviews,
            [
                (ReviewState::Approved, "GitLabDuo", Some(true), true),
                (
                    ReviewState::ChangesRequested,
                    "jdrpereira",
                    Some(false),
                    false
                ),
            ],
        );
    }

    #[test]
    fn stitches_pages_into_the_poll_one_page_would_give() {
        let one_page = poll_with(recorded(include_str!("gitlab/fixtures/commit.http")));

        let stitched = poll_with(
            paged(include_str!("gitlab/fixtures/poll_page_2.http"))
                .into_iter()
                .chain([commit_lookup(include_str!("gitlab/fixtures/commit.http"))]),
        );

        assert_eq!(stitched.unwrap(), one_page.unwrap());
    }

    #[test]
    fn a_head_that_moves_between_pages_fails_the_poll() {
        let moved = include_str!("gitlab/fixtures/poll_page_2.http").replace(HEAD, "0ther");

        let failure = poll_with(paged(&moved)).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }), "{failure:?}");
    }

    #[test]
    fn a_head_pipeline_that_changes_between_pages_fails_the_poll() {
        let rerun = include_str!("gitlab/fixtures/poll_page_2.http")
            .replace("Ci::Pipeline/2967491026", "Ci::Pipeline/2967491027");

        let failure = poll_with(paged(&rerun)).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }), "{failure:?}");
    }

    #[test]
    fn judges_a_merged_results_pipeline_whose_merge_commit_has_the_head_as_second_parent() {
        let ci = recorded_ci(include_str!("gitlab/fixtures/commit.http")).unwrap();

        assert_eq!(ci.checks.len(), 18);
        let unit = ci.checks.iter().find(|check| check.name == "tests:unit");
        assert_eq!(
            unit,
            Some(&Check {
                assessment: Assessment::Passed,
                conclusion: "success".to_owned(),
                deciding: true,
                job_id: Some("17315802984".to_owned()),
                name: "tests:unit".to_owned(),
                url: Some("https://gitlab.com/gitlab-org/cli/-/jobs/17315802984".to_owned()),
            }),
        );
        assert_eq!(ci.colour(), Some(Colour::Green));
    }

    #[test]
    fn a_merged_results_pipeline_built_on_another_head_has_no_ci() {
        let other = include_str!("gitlab/fixtures/commit.http").replace(HEAD, "0ther");

        assert_eq!(recorded_ci(&other), None);
    }

    #[test]
    fn a_merge_request_without_a_head_pipeline_has_no_ci() {
        assert_eq!(ci_of(Value::Null, false), None);
    }

    #[test]
    fn allowed_failures_warn_and_a_blocking_manual_job_holds_the_pipeline_amber() {
        let pipeline = pipeline(
            "MANUAL",
            &json!([
                job(1, "test", "SUCCESS", false),
                job(2, "lint", "FAILED", true),
                job(3, "deploy", "MANUAL", false),
                job(4, "smoke", "CREATED", false),
                job(5, "review-app", "MANUAL", true),
            ]),
        );

        let ci = ci_of(pipeline, false).unwrap();

        let checks: Vec<_> = ci
            .checks
            .iter()
            .map(|check| (check.name.as_str(), check.deciding, check.assessment))
            .collect();
        assert_eq!(
            checks,
            [
                ("test", true, Assessment::Passed),
                ("lint", false, Assessment::Failed),
                ("deploy", true, Assessment::Waiting),
                ("review-app", false, Assessment::Waiting),
            ],
        );
        assert_eq!(ci.colour(), Some(Colour::Amber));
    }

    #[test]
    fn a_skipped_pipeline_is_amber_unless_the_project_accepts_skipped_pipelines() {
        let skipped = || pipeline("SKIPPED", &json!([job(1, "test", "SKIPPED", false)]));

        let refused = ci_of(skipped(), false).unwrap();
        let accepted = ci_of(skipped(), true).unwrap();

        assert_eq!(refused.colour(), Some(Colour::Amber));
        assert_eq!(
            refused.checks.last().map(|check| check.url.as_deref()),
            Some(Some("https://gitlab.com/gitlab-org/cli/-/pipelines/7")),
        );
        assert_eq!(accepted.colour(), Some(Colour::Green));
    }

    #[test]
    fn maps_every_gitlab_row_of_the_colour_table() {
        use Assessment::{Failed, Passed, Pending, Waiting};
        for (status, assessment) in [
            ("CANCELED", Failed),
            ("CANCELING", Pending),
            ("CREATED", Pending),
            ("FAILED", Failed),
            ("MANUAL", Waiting),
            ("PENDING", Pending),
            ("PREPARING", Pending),
            ("RUNNING", Pending),
            ("SCHEDULED", Pending),
            ("SKIPPED", Passed),
            ("SUCCESS", Passed),
            ("WAITING_FOR_CALLBACK", Pending),
            ("WAITING_FOR_RESOURCE", Pending),
        ] {
            assert_eq!(job_assessment(status), assessment, "{status}");
        }
    }

    fn commit_lookup(stdout: &str) -> (String, Result<Output, exec::Error>) {
        let call = format!(
            "glab api -i --hostname gitlab.com projects/34675721/repository/commits/{MERGE_SHA}?stats=false"
        );
        (call, Ok(Output::exited(0, stdout)))
    }

    fn recorded_ci(commit: &str) -> Option<Ci> {
        poll_with(recorded(commit)).unwrap().ci
    }

    /// The recorded poll with its head pipeline, built on the head, replaced by `pipeline`.
    fn ci_of(pipeline: Value, skipped_accepted: bool) -> Option<Ci> {
        let fixture = include_str!("gitlab/fixtures/poll.http");
        let (headers, body) = fixture.rsplit_once('\n').unwrap();
        let mut body: Value = serde_json::from_str(body).unwrap();
        let merge_request = body.pointer_mut("/data/mergeRequest").unwrap();
        merge_request["headPipeline"] = pipeline;
        merge_request["project"]["allowMergeOnSkippedPipeline"] = json!(skipped_accepted);
        let stdout = format!("{headers}\n{body}");
        poll_with([graphql("", &stdout)]).unwrap().ci
    }

    fn pipeline(status: &str, jobs: &Value) -> Value {
        json!({
            "id": "gid://gitlab/Ci::Pipeline/7",
            "jobs": {
                "pageInfo": { "hasNextPage": false, "endCursor": "MQ" },
                "nodes": jobs,
            },
            "mergeRequestEventType": "DETACHED",
            "path": "/gitlab-org/cli/-/pipelines/7",
            "sha": HEAD,
            "status": status,
        })
    }

    fn job(id: u64, name: &str, status: &str, allow_failure: bool) -> Value {
        json!({
            "allowFailure": allow_failure,
            "id": format!("gid://gitlab/Ci::Build/{id}"),
            "name": name,
            "status": status,
            "webPath": format!("/gitlab-org/cli/-/jobs/{id}"),
        })
    }
}
