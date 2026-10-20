# Forge facts: what GitHub and GitLab expose about PRs, MRs, threads and CI

Status: research, resolves [#2](https://github.com/brzzdev/babysit/issues/2). Sources read 2026-10-20.

## Question

For GitHub PRs, GitLab MRs and issues on both forges: which facts can be read (head, state,
mergeability/conflict, behind-base, reviews and approvals, review threads and resolution, timeline
and discussion comments and replies, edits, check runs, commit statuses, pipelines and jobs), from
which endpoints, with what ids and timestamps, and what full set of states CI can be in?

Items marked **verify manually** are things the docs leave ambiguous or contradict; settle them
against a live PR/MR before the spec relies on them.

## Sources

GitHub REST (API version `2026-03-10`):

- Pulls: https://docs.github.com/en/rest/pulls/pulls
- Reviews: https://docs.github.com/en/rest/pulls/reviews
- Review comments: https://docs.github.com/en/rest/pulls/comments
- Issue comments: https://docs.github.com/en/rest/issues/comments
- Issues: https://docs.github.com/en/rest/issues/issues
- Timeline: https://docs.github.com/en/rest/issues/timeline and event types at
  https://docs.github.com/en/rest/using-the-rest-api/issue-event-types
- Check runs: https://docs.github.com/en/rest/checks/runs
- Checks guide: https://docs.github.com/en/rest/guides/using-the-rest-api-to-interact-with-checks
- Commit statuses: https://docs.github.com/en/rest/commits/statuses
- Compare: https://docs.github.com/en/rest/commits/commits#compare-two-commits
- Polling: https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api
- Rate limits: https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api

GitHub GraphQL: the public schema https://docs.github.com/public/fpt/schema.docs.graphql, rendered
at https://docs.github.com/en/graphql/reference (types `PullRequest`, `PullRequestReviewThread`,
`StatusCheckRollup`, enums `MergeStateStatus`, `MergeableState`, `CheckConclusionState`,
`CheckStatusState`, `StatusState`, `PullRequestReviewState`, `PullRequestReviewDecision`).

GitLab REST (sources are the `doc/` tree of gitlab-org/gitlab, rendered on docs.gitlab.com):

- Merge requests: https://docs.gitlab.com/api/merge_requests/
- Approvals: https://docs.gitlab.com/api/merge_request_approvals/
- Discussions: https://docs.gitlab.com/api/discussions/
- Notes: https://docs.gitlab.com/api/notes/
- Pipelines: https://docs.gitlab.com/api/pipelines/
- Jobs: https://docs.gitlab.com/api/jobs/
- Commit statuses: https://docs.gitlab.com/api/commits/#commit-status
- Resource state events: https://docs.gitlab.com/api/resource_state_events/
- GraphQL reference: https://docs.gitlab.com/api/graphql/reference/
- `allow_failure`: https://docs.gitlab.com/ci/yaml/#allow_failure
- Manual jobs: https://docs.gitlab.com/ci/jobs/job_control/#types-of-manual-jobs
- Merged results pipelines: https://docs.gitlab.com/ci/pipelines/merged_results_pipelines/
- gitlab.com rate limits: https://docs.gitlab.com/user/gitlab_com/rate_limits/

## Findings: GitHub

### PR head, state and mergeability

`GET /repos/{owner}/{repo}/pulls/{pull_number}` returns `head.sha`, `state` (`open`, `closed`),
`merged`, `draft`, `updated_at`, `mergeable` (true/false/**null**), `rebaseable`, and
`mergeable_state` (a string, **no enum documented in REST**). `mergeable: null` means GitHub has
started a background job; the docs say to re-request later. `merge_commit_sha` is the test merge
commit while open.

GraphQL `PullRequest` gives the typed equivalents:

- `state: PullRequestState` = `OPEN | CLOSED | MERGED` (REST needs `state` + `merged` for the same).
- `headRefOid`, `baseRefOid`, `isDraft`, `isInMergeQueue`, `updatedAt`, `lastEditedAt`.
- `mergeable: MergeableState` = `MERGEABLE | CONFLICTING | UNKNOWN` ("based on the existence of
  merge conflicts"). `UNKNOWN` is the GraphQL face of REST's `null`.
- `mergeStateStatus: MergeStateStatus` = `BEHIND` (head ref out of date), `BLOCKED`, `CLEAN`,
  `DIRTY` (merge commit cannot be cleanly created), `DRAFT` (deprecated, use `isDraft`),
  `HAS_HOOKS`, `UNKNOWN`, `UNSTABLE` (mergeable with non-passing commit status).

Behind-base comes in two flavours. `mergeStateStatus == BEHIND` is GitHub's verdict that the head
is out of date; the schema says only "The head ref is out of date", and whether it fires without
a "require branches to be up to date" rule is **verify manually**. A raw count is available from
`GET /repos/{owner}/{repo}/compare/{base}...{head}` (`status` = `diverged | ahead | behind |
identical`, `ahead_by`, `behind_by`), which costs a call and moves on every push to base.

### Reviews and approvals

- REST `GET .../pulls/{n}/reviews`: all reviews, chronological. Each has numeric `id`, `node_id`,
  `user`, `state` (string; REST lists no enum), `commit_id`, `submitted_at`. Pending reviews have no
  `submitted_at`.
- GraphQL `PullRequestReviewState` = `APPROVED | CHANGES_REQUESTED | COMMENTED | DISMISSED |
  PENDING`. `PullRequestReview` has `submittedAt`, `updatedAt`, `author`, `commit`.
- `reviews`: every review. `latestReviews`: "latest reviews per user ... that are not also pending
  review". `latestOpinionatedReviews(writersOnly:)`: "latest reviews per user". The schema does
  **not** say which states each drops. The watcher's claim that `latestReviews` drops `COMMENTED`
  reviews is not stated in the docs; the name `latestOpinionatedReviews` suggests that is the one
  that ignores `COMMENTED`. **Verify manually**. Using `reviews` and folding per author is safe
  either way.
- `reviewDecision: PullRequestReviewDecision` = `APPROVED | CHANGES_REQUESTED | REVIEW_REQUIRED`,
  nullable. Null when no review is required is the common reading; **verify manually**.
- REST has no review-decision field; it is GraphQL-only.
- Timeline event `reviewed` carries `state` in lowercase (`commented`, `changes_requested`,
  `approved`, `dismissed`) with `submitted_at`; `review_dismissed`, `review_requested`,
  `review_request_removed` are separate events.

### Review threads and comments

- Resolution exists **only in GraphQL**: `PullRequest.reviewThreads` → `PullRequestReviewThread`
  with `id`, `isResolved`, `isOutdated`, `isCollapsed`, `resolvedBy`, `path`, `line`, `comments`.
  The thread has **no timestamp** (no `resolvedAt`, no `updatedAt`), and resolution produces no
  timeline item type (none in `PullRequestTimelineItemsItemType`). Detecting "resolved" or
  "unresolved" therefore means diffing the set of `isResolved` per thread id between polls.
- Thread comments (`PullRequestReviewComment`): `id`, `databaseId`, `createdAt`, `updatedAt`,
  `lastEditedAt`, `replyTo`, `pullRequestReview`, `outdated`, `author`.
- REST `GET .../pulls/{n}/comments`: review comments with `id`, `in_reply_to_id`, `created_at`,
  `updated_at`; accepts `sort=created|updated`, `direction`, `since` (updated after). No resolution
  state. Replies show up here as comments with `in_reply_to_id`.

### Conversation (timeline) comments and edits

- REST `GET /repos/{owner}/{repo}/issues/{n}/comments` serves both PRs and issues: `id`,
  `created_at`, `updated_at`, `user`, `author_association`; `since` filters by "last updated
  after". Ordered by ascending id.
- GraphQL `IssueComment`: `databaseId`, `createdAt`, `updatedAt`, `lastEditedAt`, `isMinimized`.
- Edits: `updated_at` moves on edit (the timeline docs define it as "when the comment was updated
  or created, if the comment is never updated"). `lastEditedAt` distinguishes a body edit from other
  updates. Whether reactions or minimising bump `updated_at` is **verify manually**.
- REST `GET .../issues/{n}/timeline` mixes events and comments (`commented` carries `updated_at`;
  `reviewed`, `committed`, `head_ref_force_pushed`, `merged`, `closed`, `reopened`,
  `ready_for_review`, `convert_to_draft`...). Review-thread comments are not a documented event
  type there.
- Issues: `state` = `open | closed`, `state_reason` = `completed | reopened | not_planned |
  duplicate | null`.

### CI: check runs and commit statuses

GitHub has two independent CI channels, and a PR's CI is their union.

**Check runs** (Actions and GitHub Apps): `GET /repos/{owner}/{repo}/commits/{ref}/check-runs`
(`filter=latest` default, or `all`; `check_name`, `status`, `app_id`; max 100/page; capped at the
1000 most recent check suites). Fields: `id`, `name`, `head_sha`, `status`, `conclusion`,
`started_at`, `completed_at`, `check_suite.id`, `app`.

- `status`: `queued | in_progress | completed | waiting | requested | pending`. Only Actions sets
  `waiting`, `requested`, `pending` (e.g. `waiting` for an environment approval).
- `conclusion` (null until completed): `success | failure | neutral | cancelled | skipped |
  timed_out | action_required`, plus `stale` (set only by GitHub after 14 days incomplete).
  GraphQL `CheckConclusionState` additionally has `STARTUP_FAILURE`; the checks guide lists
  `startup_failure` for check **suites** only, while the REST response schema for runs omits both
  `stale` and `startup_failure`. Treat the GraphQL enum as the superset. Whether a check **run** can
  report `startup_failure` is **verify manually**.
- A suite reports "the highest priority check run conclusion"; the precedence order is not
  published beyond the example (`timed_out` beats `success` and `neutral`). **Verify manually** if
  suites are ever used.

**Commit statuses** (legacy external CI): `GET /repos/{owner}/{repo}/commits/{ref}/statuses` (all,
newest first) and `GET .../commits/{ref}/status` (combined). Per-status `state`: `error | failure |
pending | success`, keyed by `context`. Combined `state`: `failure` if any context is error or
failure, `pending` if there are **no statuses** or any is pending, `success` if the latest per
context is all success. Note "no statuses = pending": a repo that only uses Actions reads pending
here forever.

**GraphQL rollup**: `PullRequest.statusCheckRollup` ("for the PR's head ref") has `state:
StatusState` = `ERROR | EXPECTED | FAILURE | PENDING | SUCCESS` and `contexts`, a union of
`CheckRun` and `StatusContext`, with `checkRunCount`, `checkRunCountsByState`,
`statusContextCountsByState`. Both node types have `isRequired(pullRequestNumber:)`, which is the
only documented way to know whether a red check blocks merge. `EXPECTED` is a required status that
has not reported yet. `statusCheckRollup` is nullable; the docs do not say a fresh head with no
checks yet reads null or empty, nor how long checks take to appear. The watcher treats an empty
rollup as pending; **verify manually**.

### Ids, timestamps, polling

- REST objects carry a numeric `id` and a `node_id` (the GraphQL global id). REST review comments,
  issue comments and reviews have independent id spaces.
- Conditional requests: "Making a conditional request does not count against your primary rate
  limit if a `304` response is returned and the request was made while correctly authorized."
  Honour `x-poll-interval` if present. The docs say nothing about ETags on GraphQL;
  assume none (**verify manually**).
- Limits: 5,000 req/h primary for a PAT (15,000 for Enterprise Cloud apps; 1,000/h/repo for
  `GITHUB_TOKEN`). Secondary: 900 points/min REST, 2,000 points/min GraphQL; 403 or 429 with
  `retry-after` or `x-ratelimit-reset`. GraphQL has its own separately metered primary limit.

## Findings: GitLab

### MR head, state and mergeability

`GET /projects/:id/merge_requests/:merge_request_iid` returns:

- `sha` (source head), `diff_refs.{base_sha,head_sha,start_sha}` (empty right after creation),
  `state` = `opened | closed | merged | locked`, `draft`, `updated_at`, `user_notes_count`,
  `blocking_discussions_resolved`, `merge_commit_sha` (null until merged).
- `has_conflicts`: "Returns `false` unless `merge_status` is `cannot_be_merged`", and mergeability
  is computed **asynchronously when the endpoint is called**; poll again for the updated value.
- `detailed_merge_status` (REST): `approvals_syncing`, `checking`, `ci_must_pass`,
  `ci_still_running`, `commits_status`, `conflict`, `discussions_not_resolved`, `draft_status`,
  `jira_association_missing`, `locked_lfs_files`, `locked_paths`, `mergeable`,
  `merge_request_blocked`, `merge_time`, `need_rebase`, `not_approved`, `not_open`, `preparing`,
  `requested_changes`, `security_policy_pipeline_check`, `security_policy_violations`,
  `status_checks_must_pass`, `title_regex`, `unchecked`. It reports **one** blocking reason, so a
  conflicting MR that also fails CI shows only one of the two. Which wins is not documented
  (**verify manually**); read `has_conflicts` separately.
- GraphQL `DetailedMergeStatus` spells some values differently (`BLOCKED_STATUS`,
  `EXTERNAL_STATUS_CHECKS`, `JIRA_ASSOCIATION`, `SECURITY_POLICIES_VIOLATIONS`,
  `TITLE_NOT_MATCHING`). Pick one transport and stick to its spelling.
- Behind-base: `include_diverged_commits_count=true` adds `diverged_commits_count` ("commits the
  source branch is behind the target branch"); `need_rebase` appears in `detailed_merge_status`
  only when the project's merge method requires it. GraphQL has `shouldBeRebased`.
- `merge_status` is deprecated since 15.6; `has_conflicts` still depends on it.

### Approvals and reviewers

- `GET .../merge_requests/:iid/approvals`: `approved`, `approved_by[].user`,
  `approved_by[].approved_at`, plus EE-only `approvals_left`, `approvals_required`,
  `approval_rules_left`. `approved` differs by edition: EE is true when rules are satisfied (true
  with no approvers when no rules apply); CE is true only with at least one approval.
- `GET .../merge_requests/:iid/reviewers`: per-reviewer `state` and `created_at`. REST examples
  show `unreviewed` and `reviewed`; GraphQL `MergeRequestReviewState` is the full set: `APPROVED`,
  `REQUESTED_CHANGES`, `REVIEWED`, `REVIEW_STARTED`, `UNAPPROVED`, `UNREVIEWED`. "Requested
  changes" is therefore reviewer state, not a review object as on GitHub. There is no timestamp
  for the state change (`created_at` is when the reviewer was added). **Verify manually** what
  REST returns for each state.

### Discussions, notes and replies

- `GET .../merge_requests/:iid/discussions` (also for issues): each discussion has a string `id`
  (40-hex), `individual_note`, and `notes[]` with `id`, `type` (`DiscussionNote`, `DiffNote`,
  `null`), `author`, `created_at`, `updated_at`, `system`, `resolvable`, `resolved`,
  `resolved_by`, `resolved_at`, `position`.
- Resolution is stored **per note**, and `resolved` is "merge requests only"; issue discussions are
  not resolvable. A thread is unresolved when its resolvable notes are not all resolved. The docs
  do not define a discussion-level flag; **verify manually** that the first note's `resolved`
  tracks the thread, as the watcher assumes.
- `resolved_at` exists, so a resolve or reopen has a timestamp (unlike GitHub). Whether resolving
  also bumps the note's `updated_at` is **verify manually**.
- Notes API (`GET .../merge_requests/:iid/notes`, `sort`, `order_by=created_at|updated_at`;
  issues also take `activity_filter=all_notes|only_comments|only_activity`): the discussions docs
  state "Items of type `DiscussionNote` are not returned as part of the Note API", which confirms
  the watcher: replies inside threads are missing from the notes list. **Verify manually** that
  `DiffNote` replies are also missing, since the docs mention only `DiscussionNote`.
- System notes (`system: true`) record pushes and other changes; some changes (labels, state,
  milestone) move to resource events, e.g. `GET .../merge_requests/:iid/resource_state_events`.
- Edits: notes carry only `updated_at`; there is no `last_edited_at` in the documented response.
  **Verify manually** whether it is present anyway.
- Default page size is 20 (max 100) for both lists.

### CI: pipelines and jobs

- MR fields: `head_pipeline` ("Pipeline that runs on the HEAD commit of the merge request's source
  branch ... Use instead of `pipeline`") with `id`, `iid`, `sha`, `ref`, `source`, `status`,
  `detailed_status.{group,label,icon}`, `created_at`, `updated_at`, `finished_at`. `pipeline` is
  the slimmer legacy field. Both are "exposed only if the current user can view pipelines". The
  docs do not explain when `head_pipeline` is null while a pipeline exists (the watcher falls back
  to `pipeline`); **verify manually**.
- `GET .../merge_requests/:iid/pipelines` lists all MR pipelines (`id`, `sha`, `ref`, `status`).
- Pipeline `status` (pipelines API, GraphQL `PipelineStatusEnum`): `created`,
  `waiting_for_resource`, `preparing`, `waiting_for_callback`, `pending`, `running`, `success`,
  `failed`, `canceling`, `canceled`, `skipped`, `manual`, `scheduled`. The MR doc's
  `head_pipeline.status` lists only `success, failed, running, pending`, which is an incomplete
  copy; assume the full set.
- Job `status` (jobs API, `CiJobStatus`): the same 13 values. Jobs: `GET
  /projects/:id/pipelines/:pipeline_id/jobs?scope[]=failed`; each has `allow_failure`, and GraphQL
  `CiJob` adds `manualJob`, `retried`.
- **`allow_failure`**: a failed job with `allow_failure: true` leaves the pipeline `success` with an
  orange warning ("passed with warnings"). The signal is GraphQL `Pipeline.warnings: Boolean!` or a
  failed job with `allow_failure: true` in the jobs list. REST `detailed_status.group` likely reads
  `success-with-warnings`, but that value is not documented (**verify manually**). The yaml doc's
  sentence "the associated commit is marked as passed with no warnings" contradicts the warning icon
  it just described (**verify manually**).
- **Manual jobs**: `when: manual` outside `rules` defaults to `allow_failure: true` (optional; does
  not affect pipeline status). `when: manual` inside `rules` defaults to `allow_failure: false`
  (blocking): the pipeline stops at that stage and the UI shows **blocked**. The API pipeline status
  for a blocked pipeline is presumably `manual`, but no doc says so outright (**verify manually**).
  For an agent, a blocking manual job is amber: nothing will move until a human acts.
- **Merged results pipelines** (Premium+): run on "a temporary merged commit that combines code
  from the source and target branches. This commit doesn't exist in either branch." Identify them by
  GraphQL `Pipeline.mergeRequestEventType` = `MERGED_RESULT` (vs `DETACHED`, `MERGE_TRAIN`), and
  REST `source = merge_request_event`. The docs do not state what `sha` holds or that the merge
  commit's second parent is the MR head (the watcher's matching rule); **verify manually**. When
  the branches conflict, GitLab falls back to a detached MR pipeline. A successful merged-results
  pipeline can override a failed branch pipeline for "Pipelines must succeed" (GitLab issue 385841).
- External CI: `POST /projects/:id/statuses/:sha` (`pending | running | success | failed |
  canceled | skipped`) appends a job to a pipeline (or creates one with `source: external`), so on
  GitLab external statuses arrive as jobs and pipelines, not as a separate channel. Read with `GET
  .../repository/commits/:sha/statuses`.
- Ids: pipelines and jobs have global numeric `id`; MRs and issues are addressed by project-scoped
  `iid`; discussions by a hex string; notes by numeric `id`.

### Polling

gitlab.com allows 2,000 authenticated API requests per minute per user. The REST docs do not
mention ETag or `304` support, nor whether a `304` is free; the watcher's claim that GitLab answers
`If-None-Match` with `304` but still counts it is **verify manually**.

## Implications for the event vocabulary and CI colour

Proposed raw mapping for the grilling session. These are proposals, not decisions:

| Colour | GitHub (check run conclusion / status state) | GitLab (pipeline or job status) |
|---|---|---|
| green | `success`, `neutral`, `skipped`; status `success` | `success` without warnings, `skipped` |
| amber (needs a human, or soft fail) | `action_required`, `stale`; check `status` `waiting`; status `expected` | `manual` (blocking), success with `warnings`, `scheduled`? |
| red | `failure`, `timed_out`, `cancelled`, `startup_failure`; status `error`, `failure` | `failed`, `canceled` |
| running (no event) | `queued`, `in_progress`, `requested`, `pending`; status `pending` | `created`, `waiting_for_resource`, `preparing`, `waiting_for_callback`, `pending`, `running`, `canceling` |

Other facts that shape the vocabulary:

- "CI belongs to the current head" needs a head check on both forges: GitHub by querying checks
  for `head.sha`, GitLab by comparing the pipeline's `sha` to the MR `sha` (merged-results caveat
  above).
- New threads, replies and edits: GitHub needs GraphQL for resolution and REST or GraphQL for
  comment timestamps. GitLab's discussions endpoint alone covers comments, replies, edits
  (`updated_at`) and resolution (`resolved_at`).
- Author filtering: GitHub exposes `user.login` and `user.type` (`Bot`) on REST, `author` (an
  `Actor`, `__typename` `Bot` or `User`) on GraphQL; GitLab exposes `author.username` and `system`
  notes have an author too, so filter `system: true` separately.
- Mergeability is asynchronous on both forges (`mergeable: null` / `UNKNOWN`; `checking`,
  `unchecked`, `preparing`): an unknown value must never read as a change.

## Open questions (verify manually)

1. GitHub: which review states `latestReviews` and `latestOpinionatedReviews` drop (watcher says
   `latestReviews` drops `COMMENTED`).
2. GitHub: `reviewDecision` is null when no review is required.
3. GitHub: `mergeStateStatus == BEHIND` without a "require up to date" branch rule; REST
   `mergeable_state` values (undocumented enum, presumably the lowercase `MergeStateStatus`).
4. GitHub: whether a check run (not only a suite) can conclude `startup_failure`; suite conclusion
   precedence.
5. GitHub: whether reactions or minimising bump a comment's `updated_at`.
6. GitHub: no ETag/304 on GraphQL; `statusCheckRollup` on a fresh head with no checks yet (null
   or empty).
7. GitLab: `detailed_merge_status` precedence when several blockers apply.
8. GitLab: discussion resolution read from the first note's `resolved`; whether resolving bumps
   `updated_at`.
9. GitLab: whether `DiffNote` replies are also excluded from the notes API; whether notes carry
   `last_edited_at`.
10. GitLab: what REST `reviewers[].state` returns for each GraphQL review state.
11. GitLab: when `head_pipeline` is null while `pipeline` is set.
12. GitLab: merged-results pipeline `sha` is the temporary merge commit, and its second parent is
    the MR head.
13. GitLab: API status of a pipeline blocked on a manual job (`manual`?).
14. GitLab: REST `detailed_status.group` for "passed with warnings" (`success-with-warnings`?), and
    the contradictory "marked as passed with no warnings" sentence in the `allow_failure` doc.
15. GitLab: ETag/304 support on REST and whether a 304 counts against the rate limit.
