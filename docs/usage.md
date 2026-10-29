# Using babysit

babysit watches one or more targets, waits for an event its rules care about, prints one JSON
envelope on stdout, and exits. Run it again with the same `--snapshot` and it picks up where the
last run ended. Terms follow [`GLOSSARY.md`](../GLOSSARY.md).

## Invocation

```sh
babysit [--on RULE]... [--ignore RULE]... --snapshot PATH [--interval DURATION] [--timeout DURATION] [--brief] TARGET...
```

| Flag                   | Default | Meaning                                                                                 |
| ---------------------- | ------- | --------------------------------------------------------------------------------------- |
| `--on <RULE>`          | none    | Wake only on events matching at least one such rule; repeatable. Without one, every event wakes. |
| `--ignore <RULE>`      | none    | Never wake on an event matching this rule, even one an `--on` rule matches; repeatable  |
| `--snapshot <PATH>`    | required | The file that keeps what babysit has seen between runs                                 |
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

Each target is a pull request, merge request, issue, or Slack thread, named one of two ways:

- **A web URL.** The URL's shape decides the forge, so GitHub Enterprise and self-managed GitLab
  hosts work too. No query or fragment, except on a Slack permalink.
  - GitHub: `https://<host>/<owner>/<repo>/pull/<n>` or `…/issues/<n>`. An issue URL that names a
    pull request is watched as that pull request. A GitHub URL can't name a port.
  - GitLab: `https://<host>/<group>[/<subgroup>…]/<project>/-/merge_requests/<n>`,
    `…/-/issues/<n>`, or `…/-/work_items/<n>`.
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
the envelope prints as `target`; two names for the same target are watched once.

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

`body^=` matches the body as the forge returns it, exactly and case-sensitively, with no trimming
and whatever `--brief` prints:
`comment.edited` matches its new text. A prefix can't be empty, and an empty body, such as GitLab's
on every `review.submitted`, matches no prefix. It tells apart agents that post from one account
by the prefix each starts its posts with:

```sh
--ignore "thread.* actor=@me body^=\"Paul's Claude:\""  # my own replies, not my other agent's
```

Examples:

```sh
--on 'ci.settled colour=red,amber'                      # CI failed or needs someone
--on 'thread.* actor=@human'                            # any thread activity by a person
--on 'review.submitted state=changes_requested'
--on 'review.withdrawn subject=@me'                     # my review was dismissed or lapsed
--ignore 'actor=gitar-bot[bot]'                         # never wake on this bot
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
| `truncated`   | `true` when `--brief` changed the event's `body`; **absent** otherwise                   |

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

Issues report only `comment.*` and `state.*` events, and a Slack thread only `comment.*`.
`merge.*` events come only from an open target, and an event about the target as a whole
(`ci.settled`, `head.pushed`, `merge.*`) has no `at` or `actor`. Within one target, events are
ordered by `at`, untimed ones last; targets follow the order they were given in.

### CI colour

`ci.settled` reports the CI colour of the target's head once it settles:

| Colour  | When                                                                                       |
| ------- | ------------------------------------------------------------------------------------------ |
| `red`   | A deciding check failed. Red settles at once, even while other checks are still pending.   |
| `amber` | No deciding check failed or is pending, and at least one waits on someone: a GitHub check asking for action, a stale or waiting run, or a required status expected but not posted; a GitLab manual job, a skipped pipeline in a project that doesn't allow merging on one, or a finished pipeline GitLab set aside when it retargeted the merge request, which waits on a new pipeline. |
| `green` | Every deciding check passed and, on GitHub, every required check has reported.            |

Until then CI is unsettled and reports nothing. **No deciding checks doesn't mean green**: a head
with no checks never settles, and one whose required checks haven't all reported never settles
green.

The **deciding checks** are, on GitHub, the base branch's required checks when it has any and every
check when it has none; on GitLab, every job not allowed to fail, or the pipeline itself when no
job decides or the pipeline was skipped. A retargeted merge request's one deciding check is
`pipeline`, with `conclusion` `retargeted` and the merge request's pipelines page as its `url`. A
failed check that doesn't decide is a **warning**: `checks` lists the deciding checks that failed or
wait, and `warnings` the warnings.

GitLab CI comes from the merge request's head pipeline. While GitLab names none, as after a
retarget or a deleted head pipeline, it comes from the newest merge request pipeline that ran the
head; a branch pipeline never stands in.

Each colour is reported once per head. A push starts the new head uncoloured, and a re-run that
unsettles CI and settles it again reports again, even in the same colour.

## Output

stdout carries exactly one envelope, pretty-printed JSON:

| Field     | Value                                                                                         |
| --------- | --------------------------------------------------------------------------------------------- |
| `outcome` | `woke`, `timed_out`, or `error`                                                               |
| `events`  | The events that woke the run, across every target; empty unless `woke`                       |
| `targets` | A summary of each target polled successfully in this run, in the order given                 |
| `error`   | `null`, or the error that ended the run; a wake in the same round wins over it                |

A target summary is `target` and `state` (`open`, `closed`, or `merged`). A pull or merge request
adds `head`, its head commit; `colour`, the CI colour reported for that head or `null` while
unsettled; `merge`, the last known of `clean`, `behind`, or `conflicted`, which stays put while the
forge recomputes it and is `null` until first known or once the target isn't open; and
`unresolved_threads`. An issue's or Slack thread's summary has only `target` and `state`, and a
Slack thread is always `open`.

The examples below come from a live GitHub pull request and issue, recorded in
`src/github/fixtures/`. The test `the_usage_reference_examples_are_what_babysit_prints` in
`src/github.rs` replays those recordings through babysit's polling, diffing, rule matching, and
printing, and fails if a block marked `checked` here differs from its output or isn't one it
checks. The envelope is assembled in the test as a run assembles it.

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
      "merge": "clean",
      "unresolved_threads": 0
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
| 1    | `error`     | The run failed; `error` says why                                           |
| 2    | none        | A usage error: the message is on stderr and **no envelope** is printed     |
| 3    | `timed_out` | `--timeout` passed without a wake                                          |

Usage errors are found before any forge is polled: a malformed flag, rule, duration, or target; a
checkout reference babysit can't place; a duration past what the clock can hold; or a `--snapshot`
file that isn't a babysit snapshot file of this version.

`error` holds `kind`; `forge` (`github`, `gitlab`, `slack`, or `null` for a failure that isn't a
forge's, such as the snapshot file, or `git`, `gh auth token`, or `glab config get token` failing
while babysit places a checkout reference, which isn't retried);
`target` (the target's URL, or the name as given before it resolved, or `null`); and `message`, for
humans.

| `kind`             | Cause                                                                                        |
| ------------------ | -------------------------------------------------------------------------------------------- |
| `auth`             | The CLI isn't signed in or was refused (401, 403), both `gh` and `glab` hold a token for a checkout remote's host, or `BABYSIT_SLACK_TOKEN` is unset or refused |
| `cli_missing`      | `gh`, `glab`, `git`, or `curl` isn't installed or not on `PATH`, or `curl` is older than 8.3 |
| `lock_held`        | Another babysit run holds the `--snapshot` file                                              |
| `poll_failed`      | A call to the forge failed 10 times in a row for one target, or once under `--timeout 0`     |
| `snapshot_io`      | The `--snapshot` file couldn't be read, locked, or written                                   |
| `target_not_found` | The forge has no such target, it's gone or no longer visible, or it redirects to another host |

A failed call is retried at the interval until 10 in a row fail for one target; a success resets
the count. An `auth`, `target_not_found`, or `cli_missing` failure isn't retried: it ends the run.
Every call due in a round is still made, so a wake in the same round wins: the run prints `woke`
with `error: null`, and the failing target's entry stays unsaved, so the next run meets the error
again. Each call to a CLI gets 60 seconds. A rate limit isn't a failure: babysit waits for the
`retry-after` or the quota reset the forge names, or else backs off from 60 seconds, doubling up to
15 minutes, and notes the wait on stderr. Other hosts keep polling meanwhile.

With `--timeout`, the run ends `timed_out` once the next call would be due past the deadline, or
when the deadline kills a call in flight. `--timeout 0` makes one round of calls: each target is
resolved and polled once, nothing waits out a rate limit (a run that only hit one ends
`timed_out`), and a call that would have been retried fails the run as `poll_failed` instead.

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
- **An edit to a message's text is `comment.edited`**, its `at` and `actor` the edit's time and
  editor where Slack records them. A change to anything but the text, a reaction, and a deleted
  message report nothing.
- **An account's `login` is the Slack user id** (`U…` or `W…`), or the `bot_id` of an integration
  posting with no user. `bot` is `true` for a message carrying a `bot_id` and `false` otherwise; an
  editor's `bot` is `null`.
- **Errors**: an unset `BABYSIT_SLACK_TOKEN`, and Slack's `not_authed`, `invalid_auth`,
  `token_revoked`, `token_expired`, `account_inactive`, `missing_scope`, and
  `not_allowed_token_type`, are `auth`; `channel_not_found` and `thread_not_found` are
  `target_not_found`. HTTP 429 or `ratelimited` is a rate limit, waited out by its `retry-after`.
  Anything else, such as `invalid_cursor`, fails the poll for a retry.

## The snapshot file

The `--snapshot` file belongs to the caller. It holds one baseline per target, keyed by the
target's URL, so one file can serve several targets and several runs. Keep it between runs; delete
it to start over.

- **The first poll only records a baseline.** A target with no entry is polled once and that
  snapshot saved at once, without reporting anything: what is already true, such as red CI or an
  open thread, never wakes. A run without `--timeout 0` then keeps polling and wakes on new events.
- **Ignored events count as seen.** Each poll moves the run's baseline on, whether or not its
  events woke the run, and the run saves that baseline when it ends, so an event your rules didn't
  match won't come back on a later run with other rules. On a wake, only the targets polled in the
  waking round are saved; another target keeps its earlier entry and diffs again from it.
- **A wake is saved only after it's printed.** When the envelope can't be written, or the save after
  it fails, the file keeps the earlier baseline and the next run reports the same events again. A
  failed save after a printed envelope notes the failure on stderr and exits 1, though the envelope
  says `woke`: act on the events, and expect them again. A run killed mid-wait saves nothing past
  first baselines, so the next run diffs, ignored events included, from the last saved entry.
- **One run at a time.** babysit locks `<PATH>.lock` for the whole run; a second run on the same
  file ends at once with `lock_held`. The lock file stays behind and is harmless. Saves go through
  `<PATH>.tmp` and an atomic rename.
- A target whose URL moved on the same host keeps its baseline. A file reused for a different
  target at the same URL starts that target afresh.

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
