# babysit

Blocks until a GitHub pull request, GitLab merge request or issue does something a coding
agent should act on, then reports it and exits, written in Rust over the `gh` and `glab` CLIs.
Serves the `babysit` skill. Forgejo is planned, not in v1.

## Docs

- [`docs/usage.md`](docs/usage.md): the caller's contract (targets, rules, events, envelope,
  exit codes, snapshot file). Change it in the same commit as the behaviour it describes, its
  `checked` blocks included.
- [`GLOSSARY.md`](GLOSSARY.md): the domain terms. Name things in code, tests and docs with them.
- [`skills/babysit/SKILL.md`](skills/babysit/SKILL.md): the skill agents load to call babysit,
  distilled from `docs/usage.md`. Update it in the same commit as a usage change it covers.

## Style

- Keep dependencies minimal

## Agent skills

### Issue tracker

Issues live in this repo's GitHub Issues, driven by `gh`. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical roles, each label string equal to its name. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
