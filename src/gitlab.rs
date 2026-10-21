use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::envelope::{ErrorKind, Forge};
use crate::exec::Exec;
use crate::forge::{
    Account, Assessment, Backend, Check, Ci, Comment, Failure, Hints, Merge, Poll, Position,
    Review, ReviewState, Target, TargetKind, TargetState, Thread, run_cli,
};
use crate::graphql::{Connection, Nodes, Paged, same_head};
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
const ISSUE_GID: &str = "gid://gitlab/Issue/";
const ISSUE_POLL_QUERY: &str = include_str!("gitlab/issue_poll.graphql");
const MERGED_RESULT: &str = "MERGED_RESULT";
const MERGE_REQUEST_GID: &str = "gid://gitlab/MergeRequest/";
const MERGE_TRAIN: &str = "MERGE_TRAIN";
const PIPELINE_CHECK: &str = "pipeline";
const POLL_QUERY: &str = include_str!("gitlab/poll.graphql");
const RETARGETED: &str = "retargeted";
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
        let (resource, global_id) = match reference.kind {
            TargetKind::ChangeRequest => ("merge_requests", MERGE_REQUEST_GID),
            TargetKind::Issue => ("issues", ISSUE_GID),
        };
        let path = format!(
            "projects/{}/{resource}/{}",
            reference.project.replace('/', "%2F"),
            reference.number,
        );
        let (response, hints) = self.call(
            &["api", "-i", "--hostname", &reference.host, &path],
            deadline,
        )?;
        let found: RestTarget = decode(&response.body).map_err(|failure| failure.paced(hints))?;
        let target = Target {
            forge: Forge::GitLab,
            forge_id: format!("{global_id}{}", found.id),
            host: reference.host.clone(),
            kind: reference.kind,
            url: found.web_url,
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
    /// Pages every connection to its end, failing the poll if the head or its CI source changes.
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
        let mut fallback = None;
        let mut first: Option<(String, MergeRequestPage)> = None;
        while !(approvers.done() && jobs.done() && notes.done() && reviewers.done()) {
            let mut variables = vec![("-f", format!("id={}", target.forge_id))];
            variables.extend(approvers.variable());
            variables.extend(jobs.variable());
            variables.extend(notes.variable());
            variables.extend(reviewers.variable());
            if let Some((_, first_page)) = &first
                && let Some(pipeline) = fallback.and_then(|index| first_page.candidate(index))
                && !jobs.done()
            {
                variables.push(("-F", "fallback=true".to_owned()));
                variables.push(("-f", format!("fallbackPipeline={}", pipeline.id)));
            }
            let data: Data = self.graphql(&target.host, POLL_QUERY, &variables, deadline, hints)?;
            let Some(mut page) = data.merge_request else {
                return Err(gone(target));
            };
            let Some(head) = page.diff_head_sha.take() else {
                return Err(transient(&format!("{} has no head commit yet", target.url)));
            };
            match &first {
                None => {
                    fallback =
                        self.choose_pipeline(target, &head, &page, &mut jobs, deadline, hints)?;
                }
                Some((first_head, first_page)) => {
                    same_head(Forge::GitLab, first_head, &head)?;
                    if first_page.pipeline_id() != page.pipeline_id()
                        || fallback.is_some()
                            && (first_page.candidate_ids() != page.candidate_ids()
                                || first_page.retargeted() != page.retargeted())
                    {
                        return Err(transient("the head pipeline changed between pages"));
                    }
                }
            }
            approvers.take(page.approved_by.take())?;
            // The fallback's jobs are only asked for from the second page.
            if first.is_some() || fallback.is_none() {
                let fallback_pipeline = page
                    .project
                    .as_mut()
                    .and_then(|project| project.fallback_pipeline.as_mut());
                let pipeline = page.head_pipeline.as_mut().or(fallback_pipeline);
                jobs.take(pipeline.and_then(|pipeline| pipeline.jobs.take()))?;
            }
            notes.take(page.notes.take())?;
            reviewers.take(page.reviewers.take())?;
            if first.is_none() {
                first = Some((head, page));
            }
        }
        let (head, page) = first.expect("every poll fetches a first page");
        let ci = match fallback.and_then(|index| page.candidate(index)) {
            Some(pipeline) if page.retargeted() => {
                Some(retargeted_ci(&pipeline.status, &target.url))
            }
            Some(pipeline) => Some(ci(
                pipeline,
                jobs.nodes,
                &target.url,
                page.skipped_accepted(),
            )),
            None => self.head_ci(target, &head, &page, jobs.nodes, deadline, hints)?,
        };
        let (comments, threads) = conversation(notes.nodes);
        let mut reviews: Vec<_> = approvers
            .nodes
            .into_iter()
            .map(|user| reviewer_standing(target, user, ReviewState::Approved))
            .collect();
        reviews.extend(reviewers.nodes.into_iter().filter_map(|reviewer| {
            let state = match reviewer.merge_request_interaction?.review_state.as_str() {
                REVIEW_STATE_REQUESTED_CHANGES => ReviewState::ChangesRequested,
                REVIEW_STATE_REVIEWED => ReviewState::Commented,
                _ => return None,
            };
            Some(reviewer_standing(target, reviewer.user, state))
        }));
        let merge = page.standing.merge();
        let state = page.standing.lifecycle.state.into();
        let (state_changed_at, state_changed_by) = page.standing.change();
        Ok(Poll {
            ci,
            comments,
            head: Some(head),
            merge,
            reviews,
            state,
            state_changed_at,
            state_changed_by,
            threads,
            url: target.url.clone(),
        })
    }

    /// Pages the issue's notes to their end.
    fn poll_issue(
        &self,
        target: &Target,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Poll, Failure> {
        let mut notes = Paged::new(Forge::GitLab, "notes");
        let lifecycle = loop {
            let mut variables = vec![("-f", format!("id={}", target.forge_id))];
            variables.extend(notes.variable());
            let data: IssueData =
                self.graphql(&target.host, ISSUE_POLL_QUERY, &variables, deadline, hints)?;
            let page = data.issue.ok_or_else(|| gone(target))?;
            notes.take(Some(page.notes))?;
            if notes.done() {
                break page.lifecycle;
            }
        };
        let state = lifecycle.state.into();
        let (state_changed_at, state_changed_by) = lifecycle.change();
        Ok(Poll {
            ci: None,
            // An issue's resolvable discussions never become threads.
            comments: notes.nodes.into_iter().map(reply).collect(),
            head: None,
            merge: None,
            reviews: Vec::new(),
            state,
            state_changed_at,
            state_changed_by,
            threads: Vec::new(),
            url: target.url.clone(),
        })
    }

    /// Picks the first page's fallback candidate, finishing `jobs` where CI needs none.
    fn choose_pipeline(
        &self,
        target: &Target,
        head: &str,
        page: &MergeRequestPage,
        jobs: &mut Paged<Job>,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Option<usize>, Failure> {
        if let Some(pipeline) = &page.head_pipeline {
            if !(pipeline.sha == head || pipeline.merged()) {
                jobs.finish();
            }
            return Ok(None);
        }
        let fallback = self.fallback(head, page, &target.host, deadline, hints)?;
        if fallback.is_none() || page.retargeted() {
            jobs.finish();
        }
        Ok(fallback)
    }

    /// The newest merge request pipeline that ran `head`.
    fn fallback(
        &self,
        head: &str,
        page: &MergeRequestPage,
        host: &str,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Option<usize>, Failure> {
        let Some(candidates) = &page.pipelines else {
            return Ok(None);
        };
        for (index, candidate) in candidates.nodes.iter().enumerate() {
            if self.built_on(head, candidate, page.project_id, host, deadline, hints)? {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    /// The head pipeline's CI, where it ran `head`.
    fn head_ci(
        &self,
        target: &Target,
        head: &str,
        page: &MergeRequestPage,
        jobs: Vec<Job>,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<Option<Ci>, Failure> {
        let Some(pipeline) = &page.head_pipeline else {
            return Ok(None);
        };
        if !self.built_on(
            head,
            pipeline,
            page.project_id,
            &target.host,
            deadline,
            hints,
        )? {
            return Ok(None);
        }
        Ok(Some(ci(
            pipeline,
            jobs,
            &target.url,
            page.skipped_accepted(),
        )))
    }

    /// Whether `pipeline` ran `head`: on it, or on a merge commit whose second parent it is,
    /// which only REST lists.
    fn built_on(
        &self,
        head: &str,
        pipeline: &Pipeline,
        project_id: u64,
        host: &str,
        deadline: Option<SystemTime>,
        hints: &mut Hints,
    ) -> Result<bool, Failure> {
        if pipeline.sha == head {
            return Ok(true);
        }
        if !pipeline.merged() {
            return Ok(false);
        }
        let path = format!(
            "projects/{project_id}/repository/commits/{}?stats=false",
            pipeline.sha,
        );
        // Only the merge request's own absence ends the run.
        let (response, pacing) = self
            .call(&["api", "-i", "--hostname", host, &path], deadline)
            .map_err(|failure| match failure {
                Failure::Fatal {
                    kind: ErrorKind::TargetNotFound,
                    message,
                    ..
                } => transient(&message),
                other => other,
            })?;
        *hints = hints.merge(pacing);
        let commit: Commit = decode(&response.body)?;
        Ok(commit
            .parent_ids
            .get(1)
            .is_some_and(|parent| parent == head))
    }
}

/// The pipeline's jobs at their latest attempts, deciding unless their failure is a warning,
/// and the pipeline itself where it was skipped or no job decides.
fn ci(pipeline: &Pipeline, jobs: Vec<Job>, target_url: &str, skipped_accepted: bool) -> Ci {
    let origin = web_origin(target_url);
    // Jobs held behind a deciding manual job stay created while the pipeline waits on it.
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
    let skipped = pipeline.status == CI_SKIPPED;
    if skipped || !checks.iter().any(|check| check.deciding) {
        checks.push(Check {
            assessment: match skipped {
                true if skipped_accepted => Assessment::Passed,
                true => Assessment::Waiting,
                false => job_assessment(&pipeline.status),
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

/// A finished retargeted pipeline waits on a new run, whatever its colour.
fn retargeted_ci(status: &str, target_url: &str) -> Ci {
    let assessment = match job_assessment(status) {
        Assessment::Pending => Assessment::Pending,
        _ => Assessment::Waiting,
    };
    Ci {
        checks: vec![Check {
            assessment,
            conclusion: RETARGETED.to_owned(),
            deciding: true,
            job_id: None,
            name: PIPELINE_CHECK.to_owned(),
            url: Some(format!("{target_url}/pipelines")),
        }],
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
    for mut note in notes {
        if !note.discussion.resolvable {
            comments.push(reply(note));
            continue;
        }
        let discussion_id = rest_id(&note.discussion.id).to_owned();
        let resolved = note.discussion.resolved;
        let resolved_by = note.discussion.resolved_by.take();
        let position = note.position.take();
        let comment = Comment {
            reply_to: None,
            ..reply(note)
        };
        if let Some(thread) = threads.iter_mut().find(|thread| thread.id == discussion_id) {
            thread.comments.push(comment);
            continue;
        }
        threads.push(Thread {
            id: discussion_id.clone(),
            position: position.map(|position| Position {
                line: position.new_line.or(position.old_line),
                path: position.file_path,
            }),
            reply_to: discussion_id,
            resolved,
            resolved_by: resolved_by.map(Account::from),
            url: comment.url.clone(),
            comments: vec![comment],
        });
    }
    (comments, threads)
}

/// The note as a comment replying to its discussion.
fn reply(note: Note) -> Comment {
    Comment {
        actor: note.author.map(Account::from),
        at: note.created_at,
        body: note.body,
        edited_at: note.last_edited_at,
        editor: note.last_edited_by.map(Account::from),
        id: rest_id(&note.id).to_owned(),
        reply_to: Some(rest_id(&note.discussion.id).to_owned()),
        review_id: None,
        url: note.url,
    }
}

/// A reviewer's standing as a review: GitLab neither times it nor gives it an id, and an approval
/// lapses when it leaves the approver list.
fn reviewer_standing(target: &Target, user: User, state: ReviewState) -> Review {
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

/// A merge request or issue as REST reports it.
#[derive(Deserialize)]
struct RestTarget {
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
struct IssueData {
    issue: Option<IssuePage>,
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
    head_pipeline: Option<Pipeline>,
    notes: Option<Connection<Note>>,
    /// The merge request's own pipelines, newest first.
    pipelines: Option<Nodes<Pipeline>>,
    project: Option<Project>,
    project_id: u64,
    retargeted: Option<bool>,
    reviewers: Option<Connection<Reviewer>>,
    #[serde(flatten)]
    standing: Standing,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Standing {
    conflicts: bool,
    /// `None` on an instance too old to report it.
    detailed_merge_status: Option<String>,
    #[serde(flatten)]
    lifecycle: Lifecycle,
    merge_user: Option<User>,
    merged_at: Option<String>,
    should_be_rebased: bool,
}

/// GitLab records when a target closed, but not who closed it or when it reopened.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lifecycle {
    closed_at: Option<String>,
    state: State,
}

impl MergeRequestPage {
    fn candidate(&self, index: usize) -> Option<&Pipeline> {
        self.pipelines.as_ref()?.nodes.get(index)
    }

    fn candidate_ids(&self) -> Vec<&str> {
        self.pipelines
            .iter()
            .flat_map(|pipelines| &pipelines.nodes)
            .map(|pipeline| pipeline.id.as_str())
            .collect()
    }

    fn pipeline_id(&self) -> Option<&str> {
        self.head_pipeline
            .as_ref()
            .map(|pipeline| pipeline.id.as_str())
    }

    /// Whether GitLab cleared the head pipeline on retargeting and hasn't recorded one since.
    fn retargeted(&self) -> bool {
        self.retargeted == Some(true)
    }

    fn skipped_accepted(&self) -> bool {
        self.project
            .as_ref()
            .and_then(|project| project.allow_merge_on_skipped_pipeline)
            .unwrap_or(false)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pipeline {
    id: String,
    jobs: Option<Connection<Job>>,
    merge_request_event_type: Option<String>,
    path: String,
    sha: String,
    status: String,
}

impl Pipeline {
    /// Whether GitLab ran the pipeline on a merge commit rather than on the head itself.
    fn merged(&self) -> bool {
        matches!(
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
    fallback_pipeline: Option<Pipeline>,
}

#[derive(Deserialize)]
struct IssuePage {
    #[serde(flatten)]
    lifecycle: Lifecycle,
    notes: Connection<Note>,
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

/// `locked` is a merge in progress or an issue locked to comments, so the target is still open.
#[derive(Clone, Copy, Deserialize)]
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

impl Standing {
    /// `None` while GitLab is still computing it. The detailed status names only the first
    /// failing check, so a conflict or required rebase behind a draft or a missing approval is
    /// read from its own field.
    fn merge(&self) -> Option<Merge> {
        match self.detailed_merge_status.as_deref()? {
            "CHECKING" | "PREPARING" | "UNCHECKED" => None,
            _ if self.conflicts => Some(Merge::Conflicted),
            _ if self.should_be_rebased => Some(Merge::Behind),
            _ => Some(Merge::Clean),
        }
    }

    fn change(self) -> (Option<String>, Option<Account>) {
        if matches!(self.lifecycle.state, State::Merged) {
            return (self.merged_at, self.merge_user.map(Account::from));
        }
        self.lifecycle.change()
    }
}

impl Lifecycle {
    fn change(self) -> (Option<String>, Option<Account>) {
        match self.state {
            State::Closed => (self.closed_at, None),
            State::Locked | State::Merged | State::Opened => (None, None),
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
    use crate::diff;
    use crate::exec::{self, Output, Script};
    use crate::forge::Colour;
    use crate::reference;

    const BLOCKERS_MR_ID: &str = "552635815";
    const FORGE_ID: &str = "gid://gitlab/MergeRequest/552084316";
    const HEAD: &str = "25892092f1db0711fcea689c27db39f6d928d2eb";
    /// The recorded head pipeline's merge commit.
    const MERGE_SHA: &str = "d29dd22357102fba3709a18e01c6a96e025174a3";
    const PLAIN_MR_ID: &str = "552726042";
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
        resolve_url(URL, RESOLVE, stdout, code)
    }

    fn resolve_url(url: &str, call: &str, stdout: &str, code: i32) -> Result<Target, Failure> {
        let exec = Script::new([(call.to_owned(), Ok(Output::exited(code, stdout)))]);
        GitLab::new(&exec)
            .resolve(&reference::parse(url).unwrap(), None)
            .map(|(target, _)| target)
    }

    fn graphql(variables: &str, stdout: &str) -> (String, Result<Output, exec::Error>) {
        query_call(POLL_QUERY, FORGE_ID, variables, stdout)
    }

    fn query_call(
        query: &str,
        id: &str,
        variables: &str,
        stdout: &str,
    ) -> (String, Result<Output, exec::Error>) {
        let call = format!(
            "glab api -i --hostname gitlab.com graphql -f query={query} -f id={id}{variables}"
        );
        (call, Ok(Output::exited(0, stdout)))
    }

    fn poll_with(
        calls: impl IntoIterator<Item = (String, Result<Output, exec::Error>)>,
    ) -> Result<Poll, Failure> {
        poll_target(&target(), calls)
    }

    fn poll_target(
        target: &Target,
        calls: impl IntoIterator<Item = (String, Result<Output, exec::Error>)>,
    ) -> Result<Poll, Failure> {
        let exec = Script::new(calls);
        GitLab::new(&exec).poll(target, None).map(|(poll, _)| poll)
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

    fn issue() -> Target {
        Target {
            forge_id: format!("{ISSUE_GID}198218557"),
            kind: TargetKind::Issue,
            url: "https://gitlab.com/gitlab-org/cli/-/work_items/8510".to_owned(),
            ..target()
        }
    }

    /// Polls `target` with one reply to `query`.
    fn poll_one(target: &Target, query: &str, stdout: &str) -> Poll {
        poll_target(target, [query_call(query, &target.forge_id, "", stdout)]).unwrap()
    }

    fn merge_request(id: &str) -> Target {
        Target {
            forge_id: format!("{MERGE_REQUEST_GID}{id}"),
            ..target()
        }
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
    fn an_unplayed_manual_job_holds_the_pipeline_only_when_it_decides() {
        let colour = |id, stdout| {
            let poll = poll_one(&merge_request(id), POLL_QUERY, stdout);
            poll.ci.and_then(|ci| ci.colour())
        };

        let optional = colour(
            "552635132",
            include_str!("gitlab/fixtures/poll_optional_manual.http"),
        );
        let blocking = colour(
            "552635421",
            include_str!("gitlab/fixtures/poll_blocking_manual.http"),
        );

        assert_eq!(optional, Some(Colour::Green));
        assert_eq!(blocking, Some(Colour::Amber));
    }

    #[test]
    fn failures_that_do_not_decide_warn_and_a_deciding_manual_job_holds_the_pipeline_amber() {
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
    fn the_pipeline_decides_where_no_job_does() {
        let warned = ci_of(
            pipeline("SUCCESS", &json!([job(1, "lint", "FAILED", true)])),
            false,
        );
        let failed = ci_of(pipeline("FAILED", &json!([])), false);

        assert_eq!(warned.unwrap().colour(), Some(Colour::Green));
        assert_eq!(failed.unwrap().colour(), Some(Colour::Red));
    }

    #[test]
    fn a_merge_commit_that_cannot_be_found_fails_only_the_poll() {
        let missing = include_str!("gitlab/fixtures/resolve_not_found.http");
        let exec = Script::new([
            graphql("", include_str!("gitlab/fixtures/poll.http")),
            (commit_lookup("").0, Ok(Output::exited(1, missing))),
        ]);

        let failure = GitLab::new(&exec).poll(&target(), None).unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }), "{failure:?}");
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
        let stdout = edited(|merge_request| {
            merge_request["headPipeline"] = pipeline;
            merge_request["project"]["allowMergeOnSkippedPipeline"] = json!(skipped_accepted);
        });
        poll_with([graphql("", &stdout)]).unwrap().ci
    }

    /// The recorded poll with its merge request edited.
    fn edited(edit: impl FnOnce(&mut Value)) -> String {
        let fixture = include_str!("gitlab/fixtures/poll.http");
        let (headers, body) = fixture.rsplit_once('\n').unwrap();
        let mut body: Value = serde_json::from_str(body).unwrap();
        edit(body.pointer_mut("/data/mergeRequest").unwrap());
        format!("{headers}\n{body}")
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

    /// The recorded poll's merge request with no head pipeline and its own pipelines `candidates`.
    fn headless(cleared: Cleared, candidates: &[Value]) -> String {
        edited(|merge_request| behead(merge_request, cleared, candidates))
    }

    /// `headless` but answering the query for `fallback`'s jobs.
    fn headless_with(candidates: &[Value], fallback: Value) -> String {
        edited(|merge_request| {
            behead(merge_request, Cleared::Deleted, candidates);
            merge_request["project"]["fallbackPipeline"] = fallback;
        })
    }

    fn behead(merge_request: &mut Value, cleared: Cleared, candidates: &[Value]) {
        merge_request["headPipeline"] = Value::Null;
        merge_request["pipelines"] = json!({ "nodes": candidates });
        merge_request["retargeted"] = json!(matches!(cleared, Cleared::Retargeted));
    }

    /// How GitLab came to clear the head pipeline.
    #[derive(Clone, Copy, Debug)]
    enum Cleared {
        Deleted,
        Retargeted,
    }

    fn candidate(id: u64, sha: &str, status: &str) -> Value {
        json!({
            "id": format!("gid://gitlab/Ci::Pipeline/{id}"),
            "mergeRequestEventType": "DETACHED",
            "path": format!("/gitlab-org/cli/-/pipelines/{id}"),
            "sha": sha,
            "status": status,
        })
    }

    /// Pages after the first, asking for the fallback pipeline's jobs after `jobs_after`.
    fn fallback_page(
        jobs_after: Option<&str>,
        stdout: &str,
    ) -> (String, Result<Output, exec::Error>) {
        let jobs = jobs_after.map_or(String::new(), |cursor| format!(" -f jobsAfter={cursor}"));
        graphql(
            &format!(
                " -F approvers=false{jobs} -F notes=false -F reviewers=false -F fallback=true \
                 -f fallbackPipeline=gid://gitlab/Ci::Pipeline/7"
            ),
            stdout,
        )
    }

    #[test]
    fn a_retargeted_merge_request_waits_amber_on_a_new_run_once_its_old_pipeline_finishes() {
        let running = headless(Cleared::Retargeted, &[candidate(7, HEAD, "RUNNING")]);
        let retargeted = include_str!("gitlab/fixtures/poll_retargeted.http");

        let running = poll_with([graphql("", &running)]).unwrap().ci.unwrap();
        let finished = poll_one(&merge_request("552882065"), POLL_QUERY, retargeted)
            .ci
            .unwrap();

        assert_eq!(
            finished.checks,
            [Check {
                assessment: Assessment::Waiting,
                conclusion: "retargeted".to_owned(),
                deciding: true,
                job_id: None,
                name: "pipeline".to_owned(),
                url: Some(format!("{URL}/pipelines")),
            }],
        );
        assert_eq!(finished.colour(), Some(Colour::Amber));
        assert_eq!(running.colour(), None);
    }

    #[test]
    fn without_a_merge_request_pipeline_on_the_head_there_is_no_ci() {
        for cleared in [Cleared::Deleted, Cleared::Retargeted] {
            let stdout = headless(cleared, &[candidate(7, "0ther", "SUCCESS")]);

            let poll = poll_with([graphql("", &stdout)]).unwrap();

            assert_eq!(poll.ci, None, "{cleared:?}");
        }
    }

    #[test]
    fn the_search_skips_a_merged_results_pipeline_on_another_head_for_an_older_one_on_this_head() {
        let mut merged = candidate(8, MERGE_SHA, "SUCCESS");
        merged["mergeRequestEventType"] = json!("MERGED_RESULT");
        let candidates = [merged, candidate(7, HEAD, "SUCCESS")];
        let other = include_str!("gitlab/fixtures/commit.http").replace(HEAD, "0ther");
        let fallback = pipeline("SUCCESS", &json!([job(1, "test", "SUCCESS", false)]));

        let poll = poll_with([
            graphql("", &headless(Cleared::Deleted, &candidates)),
            commit_lookup(&other),
            fallback_page(None, &headless_with(&candidates, fallback)),
        ])
        .unwrap();

        assert_eq!(poll.ci.and_then(|ci| ci.colour()), Some(Colour::Green));
    }

    #[test]
    fn a_new_pipeline_between_pages_leaves_a_poll_with_a_head_pipeline_alone() {
        let created = include_str!("gitlab/fixtures/poll_page_2.http").replace(
            r#""mergeRequest":{"#,
            &format!(
                r#""mergeRequest":{{"pipelines":{{"nodes":[{}]}},"#,
                candidate(9, HEAD, "RUNNING")
            ),
        );
        let one_page = poll_with(recorded(include_str!("gitlab/fixtures/commit.http")));

        let stitched = poll_with(
            paged(&created)
                .into_iter()
                .chain([commit_lookup(include_str!("gitlab/fixtures/commit.http"))]),
        );

        assert_eq!(stitched.unwrap(), one_page.unwrap());
    }

    #[test]
    fn a_deleted_head_pipeline_reports_ci_from_the_pipeline_surviving_on_the_head() {
        let target = merge_request("552882785");
        let jobs = " -F approvers=false -F notes=false -F reviewers=false -F fallback=true \
                    -f fallbackPipeline=gid://gitlab/Ci::Pipeline/2972592330";

        let poll = poll_target(
            &target,
            [
                query_call(
                    POLL_QUERY,
                    &target.forge_id,
                    "",
                    include_str!("gitlab/fixtures/poll_head_pipeline_deleted.http"),
                ),
                query_call(
                    POLL_QUERY,
                    &target.forge_id,
                    jobs,
                    include_str!("gitlab/fixtures/poll_head_pipeline_deleted_jobs.http"),
                ),
            ],
        )
        .unwrap();

        let ci = poll.ci.unwrap();
        let checks: Vec<_> = ci
            .checks
            .iter()
            .map(|check| (check.name.as_str(), check.job_id.as_deref()))
            .collect();
        assert_eq!(checks, [("test", Some("17353736186"))]);
        assert_eq!(ci.colour(), Some(Colour::Red));
    }

    #[test]
    fn a_lost_head_pipeline_falls_back_to_the_newest_pipeline_on_the_head_paging_its_jobs() {
        let candidates = [
            candidate(8, "0ther", "SUCCESS"),
            candidate(7, HEAD, "FAILED"),
        ];
        let jobs = |cursor: Option<&str>, jobs| {
            json!({
                "pageInfo": { "hasNextPage": cursor.is_some(), "endCursor": cursor.unwrap_or("JOBS_2") },
                "nodes": jobs,
            })
        };
        let mut first_jobs = pipeline("FAILED", &json!([]));
        first_jobs["jobs"] = jobs(Some("JOBS_1"), json!([job(1, "test", "SUCCESS", false)]));
        let mut second_jobs = first_jobs.clone();
        second_jobs["jobs"] = jobs(None, json!([job(2, "lint", "FAILED", false)]));

        let poll = poll_with([
            graphql("", &headless(Cleared::Deleted, &candidates)),
            fallback_page(None, &headless_with(&candidates, first_jobs)),
            fallback_page(Some("JOBS_1"), &headless_with(&candidates, second_jobs)),
        ])
        .unwrap();

        let ci = poll.ci.unwrap();
        let checks: Vec<_> = ci.checks.iter().map(|check| check.name.as_str()).collect();
        assert_eq!(checks, ["test", "lint"]);
        assert_eq!(ci.colour(), Some(Colour::Red));
    }

    #[test]
    fn a_fallback_pipeline_that_changes_or_goes_between_pages_fails_the_poll() {
        let candidates = [candidate(7, HEAD, "SUCCESS")];
        let fallback = pipeline("SUCCESS", &json!([job(1, "test", "SUCCESS", false)]));
        let newer = [candidate(9, HEAD, "RUNNING"), candidate(7, HEAD, "SUCCESS")];

        for second in [
            headless_with(&newer, fallback),
            headless_with(&candidates, Value::Null),
        ] {
            let failure = poll_with([
                graphql("", &headless(Cleared::Deleted, &candidates)),
                fallback_page(None, &second),
            ])
            .unwrap_err();

            assert!(matches!(failure, Failure::Transient { .. }), "{failure:?}");
        }
    }

    #[test]
    fn retargeted_amber_goes_straight_to_green_when_gitlab_reselects_the_old_pipeline() {
        let reselected = edited(|merge_request| {
            merge_request["headPipeline"] =
                pipeline("SUCCESS", &json!([job(1, "test", "SUCCESS", false)]));
        });

        let kinds = replay(
            rest_id(FORGE_ID),
            &headless(Cleared::Retargeted, &[candidate(7, HEAD, "RUNNING")]),
            &[
                &headless(Cleared::Retargeted, &[candidate(7, HEAD, "SUCCESS")]),
                &reselected,
            ],
        );

        assert_eq!(kinds, [["ci.settled Amber"], ["ci.settled Green"]]);
    }

    #[test]
    fn resolves_an_issue_by_either_url_to_its_web_url_and_global_id() {
        for url in [
            "https://gitlab.com/gitlab-org/cli/-/issues/8510",
            "https://gitlab.com/gitlab-org/cli/-/work_items/8510",
        ] {
            let resolved = resolve_url(
                url,
                "glab api -i --hostname gitlab.com projects/gitlab-org%2Fcli/issues/8510",
                include_str!("gitlab/fixtures/resolve_issue.http"),
                0,
            );

            assert_eq!(resolved.unwrap(), issue(), "{url}");
        }
    }

    #[test]
    fn a_moved_issue_polls_as_closed_with_even_resolvable_notes_as_comments() {
        let resolvable = include_str!("gitlab/fixtures/issue_poll.http")
            .replace(r#""resolvable":false"#, r#""resolvable":true"#);

        let poll = poll_one(&issue(), ISSUE_POLL_QUERY, &resolvable);

        assert_eq!(
            (
                poll.head,
                poll.state,
                poll.state_changed_at,
                poll.state_changed_by
            ),
            (
                None,
                TargetState::Closed,
                Some("2026-08-26T18:26:37Z".to_owned()),
                None
            ),
        );
        assert_eq!(poll.threads, []);
        let comments: Vec<_> = poll
            .comments
            .iter()
            .map(|comment| (comment.id.as_str(), comment.reply_to.as_deref()))
            .collect();
        assert_eq!(
            comments,
            [
                (
                    "3718419876",
                    Some("316c04ca905dd31c1301cd74435c0b0cb6574693")
                ),
                (
                    "3718420850",
                    Some("18602a6353ea3e1bfa51e9875d95f9f390a1fa84")
                ),
                (
                    "3740922557",
                    Some("47f61eae8ef5b4973e8caadccfb2ea3320a26133")
                ),
            ],
        );
    }

    #[test]
    fn a_merged_merge_request_records_who_merged_it_and_when() {
        let target = merge_request("551223307");
        let commit = (
            "glab api -i --hostname gitlab.com projects/34675721/repository/commits/\
             ec1adbb9c1edbf4afe2d3ef394ac1b5855a3d0fa?stats=false"
                .to_owned(),
            Ok(Output::exited(
                0,
                include_str!("gitlab/fixtures/commit_merged.http"),
            )),
        );
        let stdout = include_str!("gitlab/fixtures/poll_merged.http");

        let poll = poll_target(
            &target,
            [query_call(POLL_QUERY, &target.forge_id, "", stdout), commit],
        )
        .unwrap();

        assert_eq!(poll.state, TargetState::Merged);
        assert_eq!(
            poll.state_changed_at.as_deref(),
            Some("2026-10-19T23:03:24Z")
        );
        assert_eq!(
            poll.state_changed_by.map(|account| account.login),
            Some("phikai".to_owned()),
        );
    }

    #[test]
    fn reads_a_conflict_or_rebase_behind_another_blocker_and_holds_while_computing() {
        let stdout = include_str!("gitlab/fixtures/poll_conflict.http");
        let target = merge_request("546745527");

        let merges: Vec<_> = [
            ("CONFLICT", true, false),
            ("DRAFT_STATUS", true, false),
            ("NEED_REBASE", false, true),
            ("NOT_APPROVED", false, true),
            ("CHECKING", true, false),
            ("UNCHECKED", true, false),
            ("PREPARING", true, false),
            ("MERGEABLE", false, false),
        ]
        .into_iter()
        .map(|(status, conflicts, rebase)| {
            let stdout = stdout
                .replace(r#""CONFLICT""#, &format!(r#""{status}""#))
                .replace(
                    r#""conflicts":true"#,
                    &format!(r#""conflicts":{conflicts}"#),
                )
                .replace(
                    r#""shouldBeRebased":false"#,
                    &format!(r#""shouldBeRebased":{rebase}"#),
                );
            poll_one(&target, POLL_QUERY, &stdout).merge
        })
        .collect();

        assert_eq!(
            merges,
            [
                Some(Merge::Conflicted),
                Some(Merge::Conflicted),
                Some(Merge::Behind),
                Some(Merge::Behind),
                None,
                None,
                None,
                Some(Merge::Clean),
            ],
        );
    }

    #[test]
    fn a_draft_still_reports_the_conflict_it_outranks() {
        let stdout = include_str!("gitlab/fixtures/poll_draft_conflict.http");

        let poll = poll_one(&merge_request(BLOCKERS_MR_ID), POLL_QUERY, stdout);

        assert_eq!(poll.merge, Some(Merge::Conflicted));
    }

    fn replay(id: &str, first: &str, rest: &[&str]) -> Vec<Vec<String>> {
        let target = merge_request(id);
        let poll = |stdout| poll_one(&target, POLL_QUERY, stdout);
        diff::steps(
            poll(first),
            rest.iter().map(|stdout| poll(stdout)).collect(),
        )
    }

    #[test]
    fn a_plain_note_is_a_comment_and_an_approval_a_review_without_a_time() {
        let unreviewed = include_str!("gitlab/fixtures/poll_unreviewed.http");
        let commented = include_str!("gitlab/fixtures/poll_commented.http");
        let approved = include_str!("gitlab/fixtures/poll_approved.http");

        let kinds = replay(PLAIN_MR_ID, unreviewed, &[commented, approved]);
        let review = &poll_one(&merge_request(PLAIN_MR_ID), POLL_QUERY, approved).reviews[0];

        assert_eq!(kinds, [["comment.created"], ["review.submitted"]]);
        assert_eq!(review.state, ReviewState::Approved);
        assert_eq!(review.at, None);
    }

    #[test]
    fn resolving_a_thread_reports_only_the_resolution_though_it_bumps_the_edit_time() {
        let kinds = replay(
            BLOCKERS_MR_ID,
            include_str!("gitlab/fixtures/poll_thread_open.http"),
            &[include_str!("gitlab/fixtures/poll_thread_resolved.http")],
        );

        assert_eq!(kinds, [["thread.resolved"]]);
    }

    #[test]
    fn a_published_draft_note_batch_reports_each_note_and_the_thread_its_reply_reopened() {
        let kinds = replay(
            BLOCKERS_MR_ID,
            include_str!("gitlab/fixtures/poll_pipeline_passed.http"),
            &[include_str!(
                "gitlab/fixtures/poll_draft_notes_published.http"
            )],
        );

        assert_eq!(
            kinds,
            [["thread.opened", "thread.replied", "thread.unresolved"]],
        );
    }

    #[test]
    fn a_pipeline_settles_red_then_green_on_the_next_head() {
        let kinds = replay(
            BLOCKERS_MR_ID,
            include_str!("gitlab/fixtures/poll_pipeline_running.http"),
            &[
                include_str!("gitlab/fixtures/poll_pipeline_failed.http"),
                include_str!("gitlab/fixtures/poll_pipeline_rerunning.http"),
                include_str!("gitlab/fixtures/poll_pipeline_passed.http"),
            ],
        );

        assert_eq!(
            kinds,
            [
                vec!["ci.settled Red"],
                vec!["head.pushed"],
                vec!["ci.settled Green"],
            ],
        );
    }
}
