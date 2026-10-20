# babysit

Blocks until a GitHub pull request, GitLab merge request or issue thread does something a coding
agent should act on, then reports it and exits, written in Rust over the `gh` and `glab` CLIs.
Serves the `babysit` skill. Forgejo is planned, not in v1.

## Style

- Rust 2024 edition
- Keep dependencies minimal

## Agent skills

### Issue tracker

Issues live in this repo's GitHub Issues, driven by `gh`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical roles, each label string equal to its name. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
