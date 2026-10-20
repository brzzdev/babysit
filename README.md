# babysit

Blocks until a GitHub pull request, GitLab merge request or issue thread does something a coding
agent should act on, then prints what happened and exits.

## Why

An agent babysitting a PR by polling from inside its own loop re-sends its whole context on every
check, paying model prices for what a `while` loop and `gh api` do for free. `babysit` does the
waiting in a plain process, and wakes the agent only for the events it was asked to care about: a
red or amber CI job, a human review thread, a reply on a watched issue — but not, say, a bot's
routine approval.

## Install

```sh
just install
```

Needs an authenticated [`gh`](https://cli.github.com) for GitHub and
[`glab`](https://gitlab.com/gitlab-org/cli) for GitLab.
