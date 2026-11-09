# Using babysit

babysit watches one or more targets, waits for an event its rules care about or for their state to
meet its conditions, prints one JSON envelope on stdout or into `--out`'s file, and exits. Run it
again with the same `--snapshot` and it picks up where the last run ended. Terms follow
[`GLOSSARY.md`](../GLOSSARY.md).

## Invocation

```sh
babysit [--on RULE]... [--ignore RULE]... [--until CONDITION]... --snapshot PATH [--out FILE] [--interval DURATION] [--timeout DURATION] [--brief] TARGET...
babysit forget --snapshot PATH
```

The first form is a run; `forget` deletes a snapshot file (see
[Forgetting a snapshot file](#forgetting-a-snapshot-file)). The flags below are the run's.

| Flag                   | Default | Meaning                                                                                 |
| ---------------------- | ------- | --------------------------------------------------------------------------------------- |
| `--on <RULE>`          | none    | Wake only on events matching at least one such rule; repeatable. Without one, every event wakes, unless there is an `--until`. |
| `--ignore <RULE>`      | none    | Never wake on an event matching this rule, even one an `--on` rule matches; repeatable  |
| `--until <CONDITION>`  | none    | End the run once every target meets at least one such condition (see [Conditions](#conditions)); repeatable |
| `--snapshot <PATH>`    | required | The file that keeps what babysit has seen between runs                                 |
| `--out <FILE>`         | none    | Write the envelope to `FILE` instead of stdout (see [Output](#output))                  |
| `--interval <DURATION>` | `30s`  | Time between polls of each target; must be longer than zero                             |
| `--timeout <DURATION>` | none    | Wall-clock cap on the run; `0` polls each target once                                   |
| `--brief`              | off     | Print each `body` shortened (see [Events](#events))                                     |
| `-h`, `--help`         |         | Print help                                                                              |
| `-V`, `--version`      |         | Print `babysit <version>`                                                               |

A duration is a whole number followed by `s`, `m`, or `h`, such as `45s`, `10m`, or `2h`;
`--timeout` also takes a bare `0`. A forge that asks for slower polling, through
`x-poll-interval` or `retry-after`, stretches the interval.

babysit drives the forge's own CLI: an authenticated `gh` for GitHub, `glab` for GitLab. Slack has
none, so babysit calls its Web API through `curl` 8.3 or later (see [Slack](#slack)). Progress notes
such as retries and rate-limit waits go to stderr, prefixed `babysit:`, and are for humans.

### Targets

Each target is a pull request, merge request, issue, GitLab job or pipeline, or Slack thread, named
one of two ways:

- **A web URL.** The URL's shape decides the forge, so GitHub Enterprise and self-managed GitLab
  hosts work too. No query or fragment, except on a Slack permalink.
  - GitHub: `https://<host>/<owner>/<repo>/pull/<n>` or `…/issues/<n>`. An issue URL that names a
    pull request is watched as that pull request. A GitHub URL can't name a port.
  - GitLab: `https://<host>/<group>[/<subgroup>…]/<project>/-/merge_requests/<n>`,
    `…/-/issues/<n>`, `…/-/work_items/<n>`, `…/-/jobs/<id>`, or `…/-/pipelines/<id>`.
  - Slack: a thread's permalink, `https://<workspace>.slack.com/archives/<channel>/p<ts>`, where
    `<ts>` is the parent message's `ts` without its dot. A reply's permalink,
    `…/p<reply ts>?thread_ts=<parent ts>&cid=<channel>`, names the thread it belongs to; those two
    parameters are the only query a target may carry. The envelope reports the parent's
    permalink, so a parent and a reply permalink for one thread are one target and share one
    baseline.
- **A checkout reference**, inside a git checkout: on GitHub, `#<n>` is a pull request or issue; on
  GitLab, `#<n>` is an issue and `!<n>` a merge request. babysit picks the project `gh pr view` or
  `glab mr view` would: the default `gh repo set-default` or glab recorded, else the only remote on
  that forge. A host other than github.com or gitlab.com belongs to whichever CLI holds a token for
  it. Anything ambiguous is a usage error asking for the URL instead.

Several targets can be watched in one run. A target's identity is the URL its forge reports, which
the envelope prints as `target`; two names for the same target are watched once. A job's or
pipeline's is `https://<host>/<project>/-/jobs/<id>` or `…/-/pipelines/<id>`, with the project's
path spelled as GitLab spells it.

A **job or pipeline** is a target in its own right, with no merge request needed: a deploy, a
release, or a post-merge pipeline on the default branch.

- **Retrying** a job creates a new one, and the old URL stays failed: watch the URL
  `glab ci retry` prints. **Playing** a manual job keeps its id, so the same URL goes from amber
  to unsettled and settles again in the same run.
- **A bridge**, or trigger job, is a job: its `/-/jobs/<id>` URL resolves to it, with the status
  GitLab reports for it, which follows its downstream pipeline only under `strategy: depend`.
  Watch the downstream pipeline itself by its own URL, in whichever project it runs.
- **Retrying a job in a pipeline** reopens the pipeline, which reports `state.reopened` and
  settles again.

## Rules

A rule is one argument, so quote it: an optional kind pattern, then any number of `field=value`
terms, separated by spaces.

```text
[<kind pattern>] [<field><op><value>[,<value>…]]…
```

`<op>` is `^=` for `body` and `=` for every other field.

- **Kind pattern**: an exact event kind (`review.submitted`) or a whole noun (`thread.*`). It comes
  first. Without one, the rule covers every kind.
- **Terms** are ANDed: every term must hold. **Commas** are alternatives within a term:
  `state=approved,changes_requested`. A field appears at most once per rule.
- **Quoting**: a value may be double-quoted, and inside the quotes spaces and commas are literal,
  with `\"` and `\\` as the only escapes. A quoted value is a whole value: `body^="a, b","c"` has
  two. `actor="some-bot[bot]"` matches as `actor=some-bot[bot]` does. An unterminated quote, or one
  anywhere else in the rule, is a usage error.
- **Repeated `--on`** rules are ORed: an event wakes when any one matches.
- **`--ignore` wins**: an event matching any `--ignore` rule never wakes, whatever `--on` says.
- **A missing field never matches.** `actor=@human` doesn't match an event with no actor, and with
  no kind pattern, `colour=red` matches only `ci.settled`. A pattern whose kinds can't carry the
  field is a usage error: `thread.opened colour=red`.

| Field     | Carried by                            | Values                                     |
| --------- | ------------------------------------- | ------------------------------------------ |
| `actor`   | every kind with an actor, below       | an account selector                        |
| `body`    | every kind with a `body`, below       | a prefix the body starts with              |
| `subject` | `review.withdrawn`                    | an account selector                        |
| `colour`  | `ci.settled`                          | `amber`, `green`, `red`                    |
| `state`   | `review.submitted`                    | `approved`, `changes_requested`, `commented` |

An account selector is a login, matched case-insensitively (a GitHub app is `<name>[bot]`), or:

- `@bot`: an account the forge flags as a bot
- `@human`: an account the forge flags as not a bot
- `@me`: whoever `gh` or `glab` is signed in as on the target's host, or on Slack the token's
  user, matched by account id rather than login. babysit looks it up once per host, only when a
  rule names `@me`.

Where the forge doesn't say whether an account is a bot (its `bot` prints `null`), it matches
neither `@bot` nor `@human`: `--on 'actor=@human'` won't wake on it, and `--ignore 'actor=@bot'`
won't drop it.

`body^=` matches the body as the forge returns it, exactly and case-sensitively, with no trimming,
whatever `--brief` prints:
`comment.edited` matches its new text. A prefix can't be empty, and an empty body, such as GitLab's
on every `review.submitted`, matches no prefix. It tells apart agents that post from one account
by the prefix each starts its posts with:

```sh
--ignore "thread.* actor=@me body^=\"Paul’s Claude:\""  # my own replies, not my other agent's
```

Examples:

```sh
--on 'ci.settled colour=red,amber'                      # CI failed or needs someone
--on 'thread.* actor=@human'                            # any thread activity by a person
--on 'review.submitted state=changes_requested'
--on 'review.withdrawn subject=@me'                     # my review was dismissed or lapsed
--ignore 'actor=gitar-bot[bot]'                         # never wake on this bot
```

## Conditions

A condition tests a target's current state, as its summary in `targets[]` shows it (see
[Output](#output)), rather than its events. It is one argument, written as a rule with no kind
pattern: `field=value` terms separated by spaces, all of which must hold, with commas as
alternatives inside a term and the same quoting. A field appears at most once per condition, and a
malformed condition or an unknown field is a usage error.

| Field                | Values                                                  |
| -------------------- | ------------------------------------------------------- |
| `state`              | `closed`, `merged`, `open`                              |
| `colour`             | `amber`, `green`, `red`                                 |
| `absent_ci`          | `false`, `true`                                         |
| `merge`              | `behind`, `clean`, `conflicted`                         |
| `unresolved_threads` | a whole number                                          |

- **A target meets** the run's conditions when its summary passes at least one `--until`; repeated
  `--until` conditions are ORed. A field the summary holds as `null` matches no value.
- **The run's conditions are met** once every target meets them. babysit tests them after every
  round, the first poll's included, so a condition that already holds ends the run after one
  round: unlike a rule, a condition sees what was already true.
- **A met run** prints `outcome: "met"` with empty `events` and exits 0. It saves each target's
  latest baseline, as a `timed_out` run does, so the events of its last round count as seen.
- **With rules**, whichever comes first ends the run. When one round both wakes and meets the
  conditions, it ends `woke`. A run with `--until` and no `--on` wakes on no event at all.
- **Only a pull or merge request** carries `absent_ci`, `merge`, and `unresolved_threads`, and only
  a pull or merge request, a job, or a pipeline carries `colour`. A target none of whose conditions
  tests only fields it carries can never meet them, so the run ends with `condition_unfit` once it
  resolves: `colour=green` on an issue or Slack thread, or `merge=clean` on a job.

Waiting for CI to settle or prove absent, or the target to merge or close, takes one run. The
`state` condition ends it on a target already closed, whose CI may never settle, and the `state.*`
rule wakes it with the event on a close during the run:

```sh
babysit --snapshot .babysit.json --brief --until 'colour=green,red,amber' \
  --until 'absent_ci=true' --until 'state=closed,merged' --on 'state.*' \
  https://github.com/brzzdev/babysit-sandbox/pull/1
```

## Events

Every event prints as a JSON object: the fields of its kind, then the common ones.

| Common field  | Value                                                                                    |
| ------------- | ---------------------------------------------------------------------------------------- |
| `kind`        | The event kind                                                                           |
| `url`         | Where the event is on the web: the comment, review, or thread, else the target itself    |
| `at`          | When it happened, RFC 3339; **absent** where the forge doesn't record it                 |
| `actor`       | The account that caused it; **absent** where the forge doesn't record it                 |
| `subject`     | The account it happened to; only on `review.withdrawn`, **absent** where unknown         |
| `target`      | The target's URL                                                                         |
| `observed_at` | When babysit saw it, RFC 3339                                                            |
| `truncated`   | `true` when `--brief` removed an HTML comment from the event's `body` or cut it; **absent** otherwise |

An account is `{"bot": true | false | null, "login": "…"}`; `null` means the forge doesn't say.

Under `--brief`, a `body` prints without its HTML comments, each run of whitespace as one space,
trimmed, and cut at 200 characters, the last of them `…`. Characters are Unicode scalar values,
so a cut never splits one. An unclosed `<!--` hides the rest of the body. The full text stays
at the event's `url`.

Presence: a field marked *optional* below is **absent** when it has no value. A field marked
*nullable* is always present and prints `null` when it has no value. Everything else is always
present.

| Kind                | Fields                                                                                           | `at` / `actor`             |
| ------------------- | ------------------------------------------------------------------------------------------------ | -------------------------- |
| `ci.absent`         | `head_sha`; `minutes` since babysit first saw the head open with no check reported               | neither                    |
| `ci.settled`        | `colour`; `head_sha`; `checks` and `warnings`, arrays of checks (below)                          | neither                    |
| `comment.created`   | `comment_id`; `body`; `reply_to` (optional: GitLab's discussion id to reply under)               | both                       |
| `comment.edited`    | `comment_id`; `body`, the new text; `thread_id` (optional, for a comment in a thread); `reply_to` (optional) | the edit, and its editor |
| `head.pushed`       | `old_sha`; `new_sha`                                                                             | neither                    |
| `merge.behind`      | none: the forge wants the head brought up to date before merging                                 | neither                    |
| `merge.conflicted`  | none: the head conflicts with its base                                                           | neither                    |
| `review.submitted`  | `review_id` (optional: GitLab has none); `state`; `body` (empty on GitLab)                       | both where recorded; GitLab records no `at` |
| `review.withdrawn`  | `review_id` (optional); `subject` is the reviewer                                                | GitHub: the dismissal and its dismisser. GitLab, an approval that lapsed: neither |
| `state.closed`      | none                                                                                             | where the forge records them |
| `state.merged`      | none                                                                                             | where the forge records them |
| `state.reopened`    | none                                                                                             | where the forge records them |
| `thread.opened`     | `thread_id`; `comment_id`; `reply_to`; `body`; `path` and `line` (both optional, on a diff thread; `line` nullable within); `review_id` (optional) | both |
| `thread.replied`    | `thread_id`; `comment_id`; `reply_to`; `body`; `review_id` (optional)                            | both                       |
| `thread.resolved`   | `thread_id`; `reply_to`                                                                          | `actor` only: the resolver |
| `thread.unresolved` | `thread_id`; `reply_to`                                                                          | neither                    |

`reply_to` is what the forge's reply call takes: the first comment's id on GitHub, the discussion
id on GitLab. A check in `checks` or `warnings` is `name`; `conclusion`, the forge's own word in
lowercase, except `retargeted`; `url` (nullable); and `job_id` (optional: a GitHub Actions job or a
GitLab job).

Issues report only `comment.*` and `state.*` events, a job or pipeline only `ci.settled` and
`state.*`, and a Slack thread only `comment.*`.
`ci.absent` and `merge.*` events come only from an open target, and an event about the target as a
whole (`ci.*`, `head.pushed`, `merge.*`) has no `at` or `actor`. Within one target, events are
ordered by `at`, untimed ones last; targets follow the order they were given in.

### CI colour

`ci.settled` reports the CI colour of the target's head once it settles:

| Colour  | When                                                                                       |
| ------- | ------------------------------------------------------------------------------------------ |
| `red`   | A deciding check failed. Red settles at once, even while other checks are still pending.   |
| `amber` | No deciding check failed or is pending, and at least one waits on someone: a GitHub check asking for action, a held workflow, a stale or waiting run, or a required status expected but not posted; a GitLab manual job, a skipped pipeline in a project that doesn't allow merging on one, or a finished pipeline GitLab set aside when it retargeted the merge request, which waits on a new pipeline. |
| `green` | Every deciding check passed and, on GitHub, every required check has reported.            |

Until then CI is unsettled and reports nothing. **No deciding checks doesn't mean green**: a head
with no checks never settles, and one whose required checks haven't all reported never settles
green.

`ci.absent` reports such a head 15 minutes after babysit first saw it open with no check reported:
no GitHub check run, status, or held workflow, and no GitLab pipeline. It is reported once per
head, and runs that share a `--snapshot` share its clock, so a later run neither restarts it nor
reports the head again. A push starts the clock again for the new head, and so does reopening the
target. A check reporting after `ci.absent` settles CI as usual, with `ci.settled`. A target
summary's `absent_ci` is `true` while the target is open, its head was reported by `ci.absent`, and
no check reports on that head now, on later runs sharing the `--snapshot` too. A push clears it.
Since `ci.absent` never reports a head twice, a reopening or the head's only check disappearing
leaves it `true` with no clock to wait out.

The **deciding checks** are, on GitHub, the base branch's required checks when it has any and every
check when it has none; on GitLab, every job not allowed to fail, or the pipeline itself when no
job decides or the pipeline was skipped. A retargeted merge request's one deciding check is
`pipeline`, with `conclusion` `retargeted` and the merge request's pipelines page as its `url`. A
failed check that doesn't decide is a **warning**: `checks` lists the deciding checks that failed or
wait, and `warnings` the warnings.

A **held workflow** is a GitHub Actions workflow on a pull request's head that waits on a
maintainer's approval before running any job, as on a fork's pull request. It is a check that
waits, named after the workflow, with `conclusion` `action_required`, no `job_id`, and as its `url`
the workflow run's page, where a maintainer approves it. It decides unless the base has required
checks and every one is known to have reported, since a required check yet to report may sit
behind the approval. Once approved, its jobs report as checks of their own.

GitLab CI comes from the merge request's head pipeline. While GitLab names none, as after a
retarget or a deleted head pipeline, it comes from the newest merge request pipeline that ran the
head; a branch pipeline never stands in.

A pipeline target's CI is its jobs at their latest attempts, assessed as a merge request's pipeline
is, except that a skipped pipeline passes, with no merge for it to hold. A job target's CI is the
job alone, as one deciding check even when it is allowed to fail, with the job's `url` and `job_id`,
so `glab ci trace <job_id>` reaches its log. A job left `created` while its pipeline waits on a
deciding manual job waits too, and so does a skipped job, which never ran: both read amber. Its head
is the commit it ran on, which never changes, so it reports no `head.pushed` or `ci.absent`.

Each colour is reported once per head. A push starts the new head uncoloured, and a re-run that
unsettles CI and settles it again reports again, even in the same colour.

## Output

stdout carries exactly one envelope, pretty-printed JSON. With `--out FILE`, the envelope goes to
`FILE` instead, and stdout stays empty. Once babysit holds the snapshot file's lock and has found no
usage error, it deletes any `FILE` an earlier run left, then writes `FILE.tmp` and renames it over
`FILE` once the run ends. A reader of a file no other run writes finds no file or one whole
envelope, never part of one, and never an earlier run's. stderr and the exit status are as without
it. A usage error leaves `FILE` as it was, and a file that can't be written fails the run as a
failed write to stdout does: exit 1, no file, and nothing saved to the snapshot file. A run killed
before it ends leaves no file either. `FILE.tmp` is babysit's: an existing file there is replaced,
and a write that fails partway can leave it behind.

| Field     | Value                                                                                         |
| --------- | --------------------------------------------------------------------------------------------- |
| `outcome` | `woke`, `met`, `timed_out`, or `error`                                                        |
| `events`  | The events that woke the run, across every target; empty unless `woke`                       |
| `targets` | A summary of each target polled successfully in this run, in the order given                 |
| `error`   | `null`, or the error that ended the run; a wake in the same round wins over it                |

A target summary is `target` and `state` (`open`, `closed`, or `merged`). A pull or merge request
adds `head`, its head commit; `colour`, the CI colour reported for that head or `null` while
unsettled; `merge`, the last known of `clean`, `behind`, or `conflicted`, which stays put while the
forge recomputes it for the same head and is `null` until first known for the current head, so
after a push, or once the target isn't open; and
`unresolved_threads`. Between `colour` and `merge`, it has `absent_ci`: `true` while it is open and
its head is absent CI, else `false`. A job or pipeline adds only `head`, the commit it ran on, and
`colour`; it is `open` until it finishes, and `closed` once it succeeds, fails, is cancelled, or
is skipped. An issue's or Slack thread's summary has only `target` and `state`, and a Slack thread
is always `open`.

The examples below come from a live GitHub pull request and issue, recorded in
`src/github/fixtures/`, and a live GitLab job, recorded in `src/gitlab/fixtures/`. The test
`the_usage_reference_examples_are_what_babysit_prints` in `src/github.rs` replays the GitHub
recordings through babysit's polling, diffing, rule matching, and printing, and fails if a block
marked `checked` here differs from its output or isn't one it checks;
`the_usage_reference_job_example_is_what_babysit_writes_to_its_out_file` in `src/gitlab.rs` does
the same for the job, through `--out`. The envelope is assembled in the test as a run assembles it.

A wake on a human's line thread. Only matching events print: the review the thread was opened in
didn't match, so it's left out:

<!-- checked: woke-thread -->
```json
{
  "outcome": "woke",
  "events": [
    {
      "kind": "thread.opened",
      "thread_id": "PRRT_kwDOVSGghM6uvJvf",
      "comment_id": "4336093213",
      "reply_to": "4336093213",
      "body": "A line thread",
      "line": 1,
      "path": "one.txt",
      "review_id": "5598789451",
      "url": "https://github.com/brzzdev/babysit-sandbox/pull/1#discussion_r4336093213",
      "at": "2026-10-21T11:12:56Z",
      "actor": {
        "bot": false,
        "login": "brzzdev"
      },
      "target": "https://github.com/brzzdev/babysit-sandbox/pull/1",
      "observed_at": "2026-10-21T11:13:02Z"
    }
  ],
  "targets": [
    {
      "target": "https://github.com/brzzdev/babysit-sandbox/pull/1",
      "state": "open",
      "head": "683bf59580d1f4e7a42d6aa1d918477aa94b1730",
      "colour": "green",
      "absent_ci": false,
      "merge": "clean",
      "unresolved_threads": 1
    }
  ],
  "error": null
}
```

A wake on red CI. The event about the target as a whole carries no `at` or `actor`, and an empty
`warnings` still prints:

<!-- checked: woke-red -->
```json
{
  "outcome": "woke",
  "events": [
    {
      "kind": "ci.settled",
      "colour": "red",
      "head_sha": "54dcdb220c6c950e4d0327cd38867b7cee7b3b9e",
      "checks": [
        {
          "name": "check",
          "conclusion": "failure",
          "url": "https://github.com/brzzdev/babysit-sandbox/actions/runs/39590319427/job/119189356767",
          "job_id": "119189356767"
        }
      ],
      "warnings": [],
      "url": "https://github.com/brzzdev/babysit-sandbox/pull/1",
      "target": "https://github.com/brzzdev/babysit-sandbox/pull/1",
      "observed_at": "2026-10-21T11:13:02Z"
    }
  ],
  "targets": [
    {
      "target": "https://github.com/brzzdev/babysit-sandbox/pull/1",
      "state": "open",
      "head": "54dcdb220c6c950e4d0327cd38867b7cee7b3b9e",
      "colour": "red",
      "absent_ci": false,
      "merge": "clean",
      "unresolved_threads": 0
    }
  ],
  "error": null
}
```

A first run with `--timeout 0` on an issue and a pull request whose CI is still running, which
only records their baselines. The issue's summary has no pull request fields, and unsettled CI is
`null`:

<!-- checked: timed-out -->
```json
{
  "outcome": "timed_out",
  "events": [],
  "targets": [
    {
      "target": "https://github.com/brzzdev/babysit/issues/9",
      "state": "closed"
    },
    {
      "target": "https://github.com/brzzdev/babysit-sandbox/pull/1",
      "state": "open",
      "head": "54dcdb220c6c950e4d0327cd38867b7cee7b3b9e",
      "colour": null,
      "absent_ci": false,
      "merge": "clean",
      "unresolved_threads": 0
    }
  ],
  "error": null
}
```

A first run with `--until 'colour=green,red,amber'` on a pull request whose CI has already
settled, which ends after one round:

<!-- checked: met -->
```json
{
  "outcome": "met",
  "events": [],
  "targets": [
    {
      "target": "https://github.com/brzzdev/babysit-sandbox/pull/1",
      "state": "open",
      "head": "683bf59580d1f4e7a42d6aa1d918477aa94b1730",
      "colour": "green",
      "absent_ci": false,
      "merge": "clean",
      "unresolved_threads": 0
    }
  ],
  "error": null
}
```

A run with `--out wait.json --until 'colour=green,red,amber'` on a running GitLab job, which ends
once the job finishes. stdout stays empty, and `wait.json` holds the envelope. A job's summary has
only `head` and `colour` past `state`. Started on a job that reads amber before it has run, such as
an unplayed manual job, the same run ends `met` at once:

<!-- checked: met-job -->
```json
{
  "outcome": "met",
  "events": [],
  "targets": [
    {
      "target": "https://gitlab.com/gitlab-org/gitlab/-/jobs/17860941122",
      "state": "closed",
      "head": "2f82cf43f55ebc217f9bcfcf8d407b3a04158dd4",
      "colour": "green"
    }
  ],
  "error": null
}
```

A target the forge can't find:

<!-- checked: error -->
```json
{
  "outcome": "error",
  "events": [],
  "targets": [],
  "error": {
    "kind": "target_not_found",
    "forge": "github",
    "target": "https://github.com/brzzdev/babysit-sandbox/pull/9",
    "message": "not found or not visible to gh: gh: Not Found (HTTP 404)"
  }
}
```

## Outcomes and exit codes

| Exit | Outcome     | Meaning                                                                    |
| ---- | ----------- | -------------------------------------------------------------------------- |
| 0    | `woke`      | At least one event matched the rules                                       |
| 0    | `met`       | Every target met the conditions                                            |
| 0    | none        | `forget` deleted the snapshot file, or there was nothing to delete         |
| 1    | `error`     | babysit failed; `error` says why                                           |
| 2    | none        | A usage error: the message is on stderr and **no envelope** is printed     |
| 3    | `timed_out` | `--timeout` passed without a wake or the conditions being met              |

Usage errors are found before any forge is polled: a malformed flag, rule, condition, duration, or
target; a checkout reference babysit can't place; a duration past what the clock can hold; or a
`--snapshot` file that isn't a babysit snapshot file of this version (for `forget`, one that isn't a
babysit snapshot file at all); or an `--out` whose file or `.tmp` is the snapshot file, its `.lock`,
or its `.tmp`, by any path to the same file, a link or another letter case included.

`error` holds `kind`; `forge` (`github`, `gitlab`, `slack`, or `null` for a failure that isn't a
forge's, such as the snapshot file or `condition_unfit`, or `git`, `gh auth token`, or
`glab config get token` failing while babysit places a checkout reference, which isn't retried);
`target` (the target's URL, or the name as given before it resolved, or `null`); and `message`,
for humans.

| `kind`             | Cause                                                                                        |
| ------------------ | -------------------------------------------------------------------------------------------- |
| `auth`             | The CLI isn't signed in or was refused (401, 403), both `gh` and `glab` hold a token for a checkout remote's host, or `BABYSIT_SLACK_TOKEN` is unset or refused |
| `cli_missing`      | `gh`, `glab`, `git`, or `curl` isn't installed or not on `PATH`, or `curl` is older than 8.3 |
| `condition_unfit`  | Every condition tests a field the target's summary doesn't carry, such as `colour` on an issue or `merge` on a job |
| `lock_held`        | Another babysit run holds the `--snapshot` file                                              |
| `poll_failed`      | A call to the forge failed 10 times in a row for one target, once under `--timeout 0`, or once on Slack's `invalid_arguments` |
| `snapshot_io`      | The `--snapshot` file couldn't be read, locked, or written                                   |
| `target_not_found` | The forge has no such target, it's gone or no longer visible, or it redirects to another host |

A failed call is retried at the interval until 10 in a row fail for one target; a success resets
the count. An `auth`, `target_not_found`, or `cli_missing` failure, or Slack's `invalid_arguments`,
isn't retried: it ends the run.
Every call due in a round is still made, so a wake in the same round wins: the run prints `woke`
with `error: null`, and the failing target's entry stays unsaved, so the next run meets the error
again. Each call to a CLI gets 60 seconds. A rate limit isn't a failure: babysit waits for the
`retry-after` or the quota reset the forge names, or else backs off from 60 seconds, doubling up to
15 minutes, and notes the wait on stderr. Other hosts keep polling meanwhile.

With `--timeout`, the run ends `timed_out` once the next call would be due past the deadline, or
when the deadline kills a call in flight, unless the conditions were met first. `--timeout 0`
makes one round of calls: each target is resolved and polled once, nothing waits out a rate limit
(a run that only hit one ends `timed_out`), and a call that would have been retried fails the run
as `poll_failed` instead. A round meeting the conditions ends it `met`.

## Slack

babysit reads a Slack thread with the Web API's `conversations.replies`, through `curl`, using a
token the caller exports in `BABYSIT_SLACK_TOKEN`. curl reads the token from the environment
itself, so it never appears in a process's arguments. The supported token is an internal Slack
app's user token (`xoxp-`) with the `channels:history`, `groups:history`, `im:history`, and
`mpim:history` scopes. A user token makes `@me` that person; a bot token makes it the bot.

- **A poll fetches the whole thread**, following the cursor page by page; any page failing fails
  the poll, which changes no baseline. A call succeeds only on HTTP 200 with `ok` true.
- **The parent and every reply are comments.** `comment_id` is the message's `ts`, `at` its time,
  `url` its permalink, and `body` its `text`. `reply_to` is the parent's `ts`, the `thread_ts`
  that `chat.postMessage` takes to reply in the thread.
- **An edit to a message's text is `comment.edited`**, its `at` the edit's time where Slack records
  it, and its `actor` the account that posted the message, since only that account can edit it. A
  change to anything but the text, a reaction, and a deleted message report nothing.
- **An account's `login` is the Slack user id** (`U…` or `W…`), or the `bot_id` of an integration
  posting with no user. `bot` is `true` for a message carrying a `bot_id` and `false` otherwise, for
  an edit as for the message.
- **Errors**: an unset `BABYSIT_SLACK_TOKEN`, and Slack's `access_denied`, `account_inactive`,
  `ekm_access_denied`, `enterprise_is_restricted`, `invalid_auth`, `missing_scope`,
  `no_permission`, `not_allowed_token_type`, `not_authed`, `team_access_not_granted`,
  `token_expired`, and `token_revoked`, are `auth`; `channel_not_found` and `thread_not_found`
  are `target_not_found`; `invalid_arguments` is `poll_failed` at once. HTTP 429 or
  `ratelimited` is a rate limit, waited out by its `retry-after`. Anything else, such as
  `invalid_cursor`, fails the poll for a retry.

## The snapshot file

The `--snapshot` file belongs to the caller. It holds one baseline per target, keyed by the
target's URL, so one file can serve several targets and several runs. Keep it between runs;
forget it to start over.

- **The first poll only records a baseline.** A target with no entry is polled once and that
  snapshot saved at once, without reporting anything: what is already true, such as red CI or an
  open thread, never wakes. A run without `--timeout 0` then keeps polling and wakes on new events.
  A condition still tests that first poll, so it can end the run `met`.
- **Ignored events count as seen.** Each poll moves the run's baseline on, whether or not its
  events woke the run, and the run saves that baseline when it ends, so an event your rules didn't
  match won't come back on a later run with other rules. On a wake, only the targets polled in the
  waking round are saved; another target keeps its earlier entry and diffs again from it.
- **A wake is saved only after it's printed.** When the envelope can't be written, to stdout or
  `--out`'s file, or the save after it fails, the file keeps the earlier baseline and the next run
  reports the same events again. A failed save after a printed envelope notes the failure on stderr
  and exits 1, though the envelope says `woke`: act on the events, and expect them again. A run
  killed mid-wait saves nothing past first baselines, so the next run diffs, ignored events
  included, from the last saved entry.
- **One run at a time.** babysit locks `<PATH>.lock` for the whole run; a second run on the same
  file ends at once with `lock_held`. The lock file stays behind and is harmless. Saves go through
  `<PATH>.tmp` and an atomic rename.
- A target whose URL moved on the same host keeps its baseline. A file reused for a different
  target at the same URL starts that target afresh.

### Forgetting a snapshot file

`babysit forget --snapshot PATH` deletes the file, so the next run against `PATH` records a fresh
baseline for every target it held. It takes no targets and no other flags, and calls no forge,
`git`, `gh`, `glab`, or `curl`. In order:

1. When neither `PATH` nor `PATH.lock` exists, even when the directory is missing, it exits 0,
   prints nothing, and creates nothing.
2. It takes `PATH.lock` as a run does, without waiting. While another run holds it, it prints the
   `lock_held` envelope and exits 1, leaving every file as it was.
3. A `PATH` that isn't a babysit snapshot file of any version, older or newer, is a usage error:
   exit 2, the reason on stderr, and the file left alone.
4. It deletes `PATH` and any leftover `PATH.tmp`, keeps `PATH.lock`, and exits 0 with nothing on
   stdout. The lock stays because deleting it while another run opens it would let two runs each
   lock a different file.

A file it can't read, lock, or delete is `snapshot_io`, exit 1. Its envelopes have `forge` and
`target` set to `null`, and `targets` empty.

## A worked loop

An agent babysitting a pull request runs babysit, acts on the wake, and runs it again with the same
snapshot:

```sh
babysit --snapshot .babysit.json \
  --on 'thread.* actor=@human' --on 'ci.settled colour=red' --on 'state.*' \
  https://github.com/brzzdev/babysit-sandbox/pull/1
```

1. The first run records the baseline: green CI, no threads. It keeps polling and wakes on what
   happens next.
2. On a wake it prints the envelope and exits 0. The agent reads `events`, does the work (fixes the
   failing check, answers the thread, using `reply_to`), and pushes.
3. It runs the same command again. The baseline carries over, so only what happened since the last
   wake can wake it.
4. It stops when a run wakes on `state.merged` or `state.closed`, or ends in an `error`. Without a
   `state.*` rule, a merge wakes nothing and the run waits until its `--timeout`.

Replayed against the recorded pull request, those rules sort its steps like this, and
`the_usage_reference_worked_loop_wakes_on_a_human_thread_and_red_ci_alone` checks the Events and
Wakes columns. A step's events print only where they match, as the first example above shows:

<!-- checked: worked-loop -->
| Step | What happened                            | Events                                | Wakes |
| ---- | ---------------------------------------- | ------------------------------------- | ----- |
| 1    | A human comments outside any thread      | `comment.created`                     | no    |
| 2    | That comment is minimised                | none                                  | no    |
| 3    | `github-actions[bot]` approves           | `review.submitted`                    | no    |
| 4    | A human opens a line thread in a review  | `review.submitted`, `thread.opened`   | yes   |
| 5    | The human resolves the thread            | `thread.resolved`                     | yes   |
| 6    | A push                                   | `head.pushed`                         | no    |
| 7    | CI fails on the new head                 | `ci.settled` (red)                    | yes   |
| 8    | Another push                             | `head.pushed`                         | no    |
| 9    | CI passes                                | `ci.settled` (green)                  | no    |
