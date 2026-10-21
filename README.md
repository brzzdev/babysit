# babysit

Blocks until a GitHub pull request, GitLab merge request or issue thread does something a coding
agent should act on, then prints what happened and exits.

## Why

An agent babysitting a PR by polling from inside its own loop re-sends its whole context on every
check, paying model prices for what a `while` loop and `gh api` do for free. `babysit` does the
waiting in a plain process and wakes the agent only for the events it was asked to care about, such
as a red or amber CI job, a human review thread or a reply on a watched issue. A bot's routine
approval can stay silent.

## Example

Wake on a human's thread activity, red CI, or the pull request merging or closing:

```sh
babysit --snapshot .babysit.json \
  --on 'thread.* actor=@human' --on 'ci.settled colour=red' --on 'state.*' \
  https://github.com/brzzdev/babysit-sandbox/pull/1
```

The first run against a fresh `--snapshot` only records a baseline, then keeps polling. When a
human opens a line thread, it prints one JSON envelope and exits 0:

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

The agent acts on the events, then runs the same command again.

Rules, event kinds, the envelope, exit codes and the snapshot file are all in the
[usage reference](docs/usage.md).

## Forges

GitHub and GitLab. Forgejo is planned.

babysit calls the forge's own CLI, so it needs an authenticated [`gh`](https://cli.github.com) for
GitHub or [`glab`](https://gitlab.com/gitlab-org/cli) for GitLab.

## Install

### From a release

Each [release](https://github.com/brzzdev/babysit/releases) has an archive for macOS and Linux on
arm64 and x86-64. Set `target` to yours, then download the latest archive and put `babysit` in
`~/.local/bin`, which must be on `PATH`:

```sh
target=aarch64-apple-darwin # or x86_64-apple-darwin, aarch64-unknown-linux-gnu, x86_64-unknown-linux-gnu
latest_url="$(curl -fsSLo /dev/null -w '%{url_effective}' https://github.com/brzzdev/babysit/releases/latest)"
version="${latest_url##*/v}"
curl -fsSL "https://github.com/brzzdev/babysit/releases/download/v$version/babysit-$version-$target.tar.gz" |
  tar -xzf - babysit
mkdir -p ~/.local/bin && install babysit ~/.local/bin/babysit && rm babysit
babysit --version
```

The release's `SHA256SUMS` file holds each archive's checksum.

### From source

With Rust 1.99 or newer and [`just`](https://github.com/casey/just):

```sh
just install
```

This builds a release binary and installs it to `~/.local/bin`, which must be on `PATH`.
