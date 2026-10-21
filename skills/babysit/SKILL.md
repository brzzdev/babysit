---
name: babysit
description: Wait on a GitHub pull request, GitLab merge request or issue with the `babysit` CLI, which blocks until CI settles, a review, thread or comment lands, or the target merges or closes. Use whenever you must wait for something to happen on a PR, MR or issue instead of polling or sleeping, or when the `babysit` command comes up.
metadata:
  author: Paul Breeze
---

# Waiting with `babysit`

Checked against babysit 0.1.0. Run `babysit --version` first; the caller's contract,
[`docs/usage.md`](https://github.com/brzzdev/babysit/blob/main/docs/usage.md), is the authority
when something here doesn't match.

```sh
babysit --snapshot <PATH> [--on RULE]... [--ignore RULE]... [--timeout DURATION] TARGET...
```

A target is a web URL (`…/pull/<n>`, `…/issues/<n>`, `…/-/merge_requests/<n>`), or inside a
checkout `#<n>` (a GitHub PR or issue, a GitLab issue) or `!<n>` (a GitLab MR). Several targets can
share one run.

## The loop

1. **Pick a snapshot file.** It is the loop's memory between runs. Keep it inside the git dir, where
   it is never committed and each worktree gets its own:
   ```sh
   snap="$(git rev-parse --git-path babysit-pr-42.json)"
   ```
   Its directory must already exist, or the run fails with `snapshot_io`. Give each loop its own
   file: events a run's rules ignore still count as seen, so a file shared by loops with different
   rules loses events, and one file allows one run at a time (`lock_held`).
2. **Take stock.** The first poll of a target only records its **baseline**: red CI or an open
   thread that is already there never wakes. Run once with `--timeout 0`, which polls each target
   once and exits 3, and read `targets[]`: `state`, `colour` (`null` while CI is unsettled), `merge`
   and `unresolved_threads`. Before you wait, deal with each red or amber `colour`, `conflicted`
   `merge` and nonzero `unresolved_threads` it shows: none of them will wake you.
3. **Wait.** Run the same command as a background job, so its exit re-invokes you. Set `--timeout`
   below your shell tool's cap on a command (`--timeout 9m` under a 10-minute cap), so babysit ends
   itself and saves its baseline rather than being killed.
4. **Act on the exit status** and, on a wake, on every event in `events` (both below).
5. **Run the identical command again.** The baseline carries over, so only what happened since the
   last wake can wake the next run. Stop when a run wakes on `state.merged` or `state.closed`, or
   `targets[].state` reads `merged` or `closed`, when the goal you were waiting for is reached, or
   on an error you can't fix.

## Exit status

| Exit | `outcome`   | Do                                                                              |
| ---- | ----------- | ------------------------------------------------------------------------------- |
| 0    | `woke`      | Act on `events`, then re-run                                                    |
| 1    | `error`     | Read `error.kind` (below). Exit 1 with `outcome: "woke"` means the save failed after printing: act on the events, and expect them again |
| 2    | none        | A usage error: no envelope on stdout, the reason on stderr. Fix the command     |
| 3    | `timed_out` | Nothing matched before `--timeout`: check `targets[].state`, then re-run without reporting |

| `error.kind`       | Do                                                                              |
| ------------------ | ------------------------------------------------------------------------------- |
| `auth`             | Tell the user: `gh` or `glab` isn't signed in or was refused, or both hold a token for the remote's host |
| `cli_missing`      | Tell the user which of `gh`, `glab` or `git` is missing                         |
| `lock_held`        | Another run owns this snapshot file. Wait for it, or watch with another file    |
| `poll_failed`      | The forge failed 10 calls in a row, or once under `--timeout 0`. Report it before starting another run |
| `snapshot_io`      | The snapshot file or its directory can't be read or written                     |
| `target_not_found` | The target is wrong, gone, or not visible to the CLI's account                  |

## Rules

A rule is one quoted argument: an optional kind pattern (`ci.settled`, or a whole noun like
`thread.*`), then `field=value` terms that must all hold. Commas are alternatives within a term.
Repeated `--on` rules are ORed, and `--ignore` beats `--on`. With no `--on`, every event wakes,
including your own pushes.

| Field     | Carried by         | Values                                           |
| --------- | ------------------ | ------------------------------------------------ |
| `actor`   | most kinds         | a login (`name[bot]` for a GitHub app), `@bot`, `@human`, `@me` |
| `colour`  | `ci.settled`       | `amber`, `green`, `red`                          |
| `state`   | `review.submitted` | `approved`, `changes_requested`, `commented`     |
| `subject` | `review.withdrawn` | an account selector, as for `actor`              |

Starting sets:

```sh
# A PR or MR until it lands
--on 'ci.settled' --on 'merge.conflicted' --on 'review.*' --on 'state.*' --on 'thread.*' \
  --ignore 'comment.* actor=@me' --ignore 'thread.* actor=@me'

# Only until CI settles, to carry on with work that needs it green
--on 'ci.settled' --on 'state.*'

# A reply on an issue
--on 'comment.* actor=@human' --on 'state.*' --ignore 'comment.* actor=@me'
```

What bites:

- **A missing field never matches.** `ci.settled`, `head.pushed`, `merge.*` and `thread.unresolved`
  carry no actor, so `--ignore 'actor=@me'` never drops them, and `actor=@human` never matches them. An account whose
  bot flag the forge doesn't report matches neither `@bot` nor `@human`.
- **`@me` is whoever `gh` or `glab` is signed in as.** When you post through the user's CLI, that is
  the user too, so ignoring `@me` also hides their own comments, and on `state.*` it hides the merge
  or close they make. Scope it to `comment.*` and `thread.*`, and drop it when they review through
  the same account.
- **Review bots post real findings in threads**, so `actor=@human` drops real work. Silence a noisy
  bot by login instead: `--ignore 'actor=gitar-bot[bot]'`.
- **CI settles only when it can.** A head with no checks never settles, and GitHub holds back green
  until every required check has reported. A loop that waits for green alone needs `--timeout`.
- Issues report only `comment.*` and `state.*`.

## Acting on a wake

stdout carries only the envelope; lines on stderr prefixed `babysit:` (retries, rate-limit waits)
are progress for humans. Capture the envelope and the exit status apart, since a pipe reports
`jq`'s status instead:

```sh
out="$(babysit …)"; status=$?
jq '.events[] | {kind, target, url, body, reply_to, path, line, colour, checks, warnings}' <<<"$out"
```

Run the forge commands below from the target's checkout: `{owner}`, `{repo}` and `:fullpath`
resolve from the current directory's repo. Elsewhere, set `GH_REPO=<owner>/<repo>` for `gh`, and
put the URL-encoded project path in place of `:fullpath` for `glab api`.

- **Red or amber CI:** `checks` lists the deciding checks that failed or wait; `warnings` lists
  failed checks that don't decide. A check's `job_id` reaches its log: `gh run view --job <job_id>
  --log-failed`, or `glab ci trace <job_id>`.
- **Answer a thread** with its `reply_to`, which is the first comment's id on GitHub and the
  discussion id on GitLab:
  ```sh
  gh api 'repos/{owner}/{repo}/pulls/<n>/comments/<reply_to>/replies' -f body='…'
  glab api 'projects/:fullpath/merge_requests/<iid>/discussions/<reply_to>/notes' -f body='…'
  glab api 'projects/:fullpath/issues/<iid>/discussions/<reply_to>/notes' -f body='…'
  ```
  A GitHub `comment.created` stands outside any thread: answer it with `gh pr comment <n>` or
  `gh issue comment <n>`.
- **`merge.conflicted`** means the head conflicts with its base. **`merge.behind`** means the forge
  wants the head updated before merging; ask before rebasing for it.
- Each event's `url` links it on the web, for the line you report to the user.
