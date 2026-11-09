---
name: babysit
description: Wait on a GitHub pull request, GitLab merge request, issue, or Slack thread with the `babysit` CLI, which blocks until CI settles; a review, thread, comment, or Slack reply lands; or the target merges or closes. Use whenever you must wait for something to happen on a PR, MR, issue, or Slack thread, in place of `gh pr checks --watch`, `glab ci status --live`, or a polling loop, or when the `babysit` command comes up.
metadata:
  author: Paul Breeze
---

# Waiting with `babysit`

Checked against babysit 0.5.0. Run `babysit --version` first; the caller's contract,
[`docs/usage.md`](https://github.com/brzzdev/babysit/blob/main/docs/usage.md), is the authority
when something here doesn't match.

```sh
babysit --snapshot <PATH> --brief [--on RULE]... [--ignore RULE]... [--until CONDITION]... [--timeout DURATION] TARGET...
```

Pass `--brief`: bot reviews and summaries run to thousands of characters, mostly hidden HTML
comments, and you read the envelope on every wake. It prints each `body` without HTML comments,
with its whitespace collapsed, and cut to 200 characters ending in `…`. An event whose `body` lost
an HTML comment or was cut carries `"truncated": true`. `body^=` still matches the full body, so
write a prefix against the forge's text. The full text stays at the event's `url`; read it when the
cut part matters.

A target is a web URL (`…/pull/<n>`, `…/issues/<n>`, `…/-/merge_requests/<n>`), a Slack thread's
permalink (the parent's, or a reply's with its `?thread_ts=…&cid=…`), or inside a checkout `#<n>`
(a GitHub PR or issue, a GitLab issue) or `!<n>` (a GitLab MR). Several targets can share one run.

A Slack thread needs `curl` 8.3 or later and a Slack user token (`xoxp-`) with the
`channels:history`, `groups:history`, `im:history`, and `mpim:history` scopes, exported as
`BABYSIT_SLACK_TOKEN`. When it is unset, ask the user for it rather than looking for one.

## The loop

1. **Pick a snapshot file.** It is the loop's memory between runs. Keep it inside the git dir, where
   it is never committed and each worktree gets its own:
   ```sh
   snap="$(git rev-parse --git-path babysit-pr-42.json)"
   ```
   Its directory must already exist, or the run fails with `snapshot_io`. Give each loop its own
   file: events a run's rules ignore still count as seen, so a file shared by loops with different
   rules loses events, and one file allows one run at a time (`lock_held`).
2. **Take stock**, except in an `--until` wait such as the CI-only set below: skip to step 3,
   since a condition sees what is already true (see [Conditions](#conditions)). The first poll of
   a target only records its **baseline**: red CI or an open thread that is already there never
   wakes. Run once with `--timeout 0`, which polls each target once and exits 3 (on a resumed
   snapshot it can wake instead: act on its events too), and read `targets[]`: `state`, `colour`
   (`null` while CI is unsettled), `merge`, and `unresolved_threads`. Each red or amber `colour`,
   `conflicted` `merge`, and nonzero `unresolved_threads` it shows is yours to deal with: none of
   them will wake you.
3. **Wait.** Start the same command without `--timeout` as a background job, before you deal with
   anything step 2 showed, so anything landing meanwhile wakes it. It runs until a wake, `met`, or
   an error; `met` ends an `--until` wait, so go to step 6:
   - **Claude Code**: `run_in_background`. The harness re-invokes you when it exits, however long
     that takes.
   - **Codex**: the command keeps running in its session but nothing re-invokes you, so poll it
     with an empty `write_stdin` until it exits. `codex exec` kills it when the turn ends, so keep
     polling within the turn.

   Add a `--timeout` only when you wait on a condition babysit can't wake on, and recheck that
   condition on every exit.
4. **Act on the exit status** and, on a wake, on every event in `events` (both below).
5. **Run the identical command again.** The baseline carries over, so only what happened since the
   last wake can wake the next run. Stop when a run wakes on `state.merged` or `state.closed`, or
   `targets[].state` reads `merged` or `closed`, when the goal you were waiting for is reached, or
   on an error you can't fix.
6. **Forget the snapshot file** when step 5 stops on a merge, a close, or your goal. Keep it after
   an error, so a resumed loop picks up where it left off:
   ```sh
   babysit forget --snapshot "$snap"
   ```
   Run it whether or not the file exists: it exits 0 either way. `lock_held` means a run still
   holds the file: stop it if it's your own background run, wait for it to exit, and forget again;
   otherwise leave the file.

## Exit status

| Exit | `outcome`   | Do                                                                              |
| ---- | ----------- | ------------------------------------------------------------------------------- |
| 0    | `woke`      | Act on `events`, then re-run                                                    |
| 0    | `met`       | Every target meets an `--until`. Read `targets[]` for the state that met it, and carry on |
| 1    | `error`     | Read `error.kind` (below). Exit 1 with `outcome: "woke"` means the save failed after printing: act on the events, and expect them again |
| 2    | none        | A usage error: no envelope on stdout, the reason on stderr. Fix the command     |
| 3    | `timed_out` | No wake, and no `--until` met, before `--timeout`: step 2's usual exit, or recheck what the timeout was for, then re-run |

| `error.kind`       | Do                                                                              |
| ------------------ | ------------------------------------------------------------------------------- |
| `auth`             | Tell the user: `gh` or `glab` isn't signed in or was refused, both hold a token for the remote's host, or `BABYSIT_SLACK_TOKEN` is unset or refused |
| `cli_missing`      | Tell the user which of `gh`, `glab`, `git`, or `curl` is missing, or that `curl` is older than 8.3 |
| `condition_unfit`  | The target is an issue or Slack thread, and every condition tests a field only a PR or MR carries (`colour`, `absent_ci`, `merge`, `unresolved_threads`). Fix the command |
| `lock_held`        | Another run owns this snapshot file. Wait for it, or use another file; the `.lock` file stays, so leave it |
| `poll_failed`      | The forge failed 10 calls in a row, once under `--timeout 0`, or once on Slack's `invalid_arguments`. Report it before starting another run |
| `snapshot_io`      | The snapshot file or its directory can't be read or written                     |
| `target_not_found` | The target is wrong, gone, or not visible to the CLI's account                  |

## Rules

A rule is one quoted argument: an optional kind pattern (`ci.settled`, or a whole noun like
`thread.*`), then `field=value` terms that must all hold. Commas are alternatives within a term,
and a double-quoted value keeps its spaces and commas, with `\"` and `\\` for a literal quote and backslash.
Repeated `--on` rules are ORed, and `--ignore` beats `--on`. With no `--on`, every event wakes,
including your own pushes.

| Field     | Carried by         | Values                                           |
| --------- | ------------------ | ------------------------------------------------ |
| `actor`   | most kinds         | a login (`name[bot]` for a GitHub app), `@bot`, `@human`, `@me` |
| `body`    | `comment.*`, `review.submitted`, `thread.opened`, `thread.replied` | `body^=<prefix>` only: the body starts with it, case-sensitively |
| `colour`  | `ci.settled`       | `amber`, `green`, `red`                          |
| `state`   | `review.submitted` | `approved`, `changes_requested`, `commented`     |
| `subject` | `review.withdrawn` | an account selector, as for `actor`              |

Starting sets:

```sh
# A PR or MR until it lands
--on 'ci.*' --on 'comment.*' --on 'merge.conflicted' --on 'review.*' --on 'state.*' \
  --on 'thread.*' --ignore 'comment.* actor=@me' --ignore 'thread.* actor=@me'

# Only until CI settles or proves absent, to carry on with work that needs it green: one run.
# `state=closed,merged` ends it on a PR already closed, whose CI may never settle
--until 'colour=green,red,amber' --until 'absent_ci=true' --until 'state=closed,merged' \
  --on 'state.*'

# A reply on an issue
--on 'comment.* actor=@human' --on 'state.*' --ignore 'comment.* actor=@me'

# A reply in a Slack thread
--on 'comment.*' --ignore 'comment.* actor=@me'
```

What bites:

- **A missing field never matches.** `ci.*`, `head.pushed`, `merge.*`, and `thread.unresolved`
  carry no actor, so `--ignore 'actor=@me'` never drops them, and `actor=@human` never matches them. An account whose
  bot flag the forge doesn't report matches neither `@bot` nor `@human`. Only `ci.settled` carries
  a `colour`, so `ci.* colour=red` never matches `ci.absent`.
- **`@me` is whoever `gh` or `glab` is signed in as.** When you post through the user's CLI, that is
  the user too, so ignoring `@me` also hides their own comments, and on `state.*` it hides the merge
  or close they make. Scope it to `comment.*` and `thread.*`, and drop it when they review through
  the same account. Where another agent posts from that account too, ignoring `@me` hides its posts
  as well, so add the prefix your own posts start with:
  `--ignore "thread.* actor=@me body^=\"Paul’s Claude:\""`.
- **Review bots post real findings in threads**, so `actor=@human` drops real work. Silence a noisy
  bot by login instead: `--ignore 'actor=gitar-bot[bot]'`.
- **CI settles only when it can.** A head with no checks never settles, and GitHub holds back green
  until every required check has reported, so wait on `ci.*`, whose `ci.absent` covers a head no
  check reports on, or pair `--until 'absent_ci=true'` with the colours. `ci.settled` is reported
  when the colour changes, so a second check failing while CI is red wakes nothing.
- **Nothing reports** a conflict clearing, or a job that isn't a deciding check finishing while
  other jobs decide. A GitLab `when: manual` job outside `rules:` is one: it defaults to
  `allow_failure: true`, while inside `rules:` it decides and holds CI amber.
- Issues report only `comment.*` and `state.*`; a Slack thread only `comment.*`, and its
  `targets[].state` stays `open`, so stop on your goal.
- **On Slack, `@me` is the token's user** and a login is a Slack user id (`U…` or `W…`), or an
  integration's `bot_id`, which counts as `@bot`.

## Conditions

`--until` ends the run once every target's summary in `targets[]` passes one of its conditions,
the first poll included, so a state that already holds ends the run at once with `met`. A
condition is a rule with no kind pattern: `field=value` terms that must all hold, commas as
alternatives, repeated `--until` ORed. Its fields are the summary's: `state` (`closed`, `merged`,
`open`), and on a PR or MR only, `colour` (`amber`, `green`, `red`), `absent_ci` (`false`,
`true`), `merge` (`behind`, `clean`, `conflicted`), and `unresolved_threads` (a number).

- **With `--on`, whichever comes first ends the run**, and a round that does both ends `woke`.
- **With no `--on`, the run ends only on `met`, `--timeout`, or an error.**
- **`absent_ci` carries across runs** sharing the snapshot file: once `ci.absent` reported the
  head, it reads `true` whenever the PR or MR is open and no check reports on that head, until a
  push.

## Acting on a wake

stdout carries only the envelope; lines on stderr prefixed `babysit:` (retries, rate-limit waits)
are progress for humans. Capture the envelope and the exit status apart, since a pipe reports
`jq`'s status instead:

```sh
out="$(babysit …)"; exit_code=$?
jq '.events[] | {kind, target, url, body, truncated, reply_to, path, line, colour, checks, warnings}' <<<"$out"
```

Run the forge commands below from the target's checkout: `{owner}`, `{repo}`, and `:fullpath`
resolve from the current directory's repo. Elsewhere, take the host from the target's URL: for
`gh`, set `GH_HOST=<host>` and `GH_REPO=<host>/<owner>/<repo>`; for `glab api`, pass
`--hostname <host>` and put the URL-encoded project path in place of `:fullpath`.

- **Red or amber CI:** `checks` lists the deciding checks that failed or wait; `warnings` lists
  failed checks that don't decide. A check's `job_id` reaches its log: `gh run view --job <job_id>
  --log-failed`, or `glab ci trace <job_id>`, adding `--repo <project URL>` outside the checkout.
  A GitLab `pipeline` check whose `conclusion` is `retargeted` means GitLab retargeted the MR and
  wants a new pipeline: `glab api -X POST 'projects/:fullpath/merge_requests/<iid>/pipelines'`.
  A GitHub check with `conclusion` `action_required`, no `job_id`, and a workflow run page
  (`…/actions/runs/<id>`) as its `url` is a held workflow, waiting on a maintainer's approval
  there. Tell the user and leave the approval to them, since it runs code from outside the repo.
- **`ci.absent`** means no check has reported on `head_sha` in the `minutes` since babysit first
  saw it: the repo, or the paths this head changes, may run no CI. Tell the user, and stop
  waiting for green unless they expect CI; if you keep watching, a check that reports later still
  wakes `ci.settled`.
- **Answer a thread** with its `reply_to`, which is the first comment's id on GitHub and the
  discussion id on GitLab:
  ```sh
  gh api 'repos/{owner}/{repo}/pulls/<n>/comments/<reply_to>/replies' -f body='…'
  glab api 'projects/:fullpath/merge_requests/<iid>/discussions/<reply_to>/notes' -f body='…'
  glab api 'projects/:fullpath/issues/<iid>/discussions/<reply_to>/notes' -f body='…'
  ```
  A GitHub `comment.created` stands outside any thread: answer it with `gh pr comment <n>` or
  `gh issue comment <n>`. A Slack event's `reply_to` is the parent's `ts`: reply in the thread
  through `chat.postMessage` with it as `thread_ts`.
- **`merge.conflicted`** means the head conflicts with its base. **`merge.behind`** means the forge
  wants the head updated before merging; ask before rebasing for it.
- Each event's `url` links it on the web, for the line you report to the user.
