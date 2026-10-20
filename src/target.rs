use std::collections::HashMap;
use std::io;
use std::time::SystemTime;

use crate::envelope::ErrorKind;
use crate::exec::{self, CALL_LIMIT, Exec, Limit, Output};
use crate::forge::{Backend, Failure, Snapshot, Target};
use crate::reference::{self, Name, Reference};
use crate::snapshot_file::SnapshotFile;

const GITHUB_HOST: &str = "github.com";
const GITLAB_HOST: &str = "gitlab.com";
const EXIT_NOT_FOUND: i32 = 127;
const PASS_THE_URL: &str = "pass the target's URL instead";
/// The git config key, under a remote, where `gh repo set-default` records its choice.
const GH_RESOLVED_KEY: &str = "gh-resolved";
/// The `gh-resolved` value marking the remote `gh repo set-default` chose.
const RESOLVED_BASE: &str = "base";

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The forge failed to resolve a located reference.
    Backend(Failure),
    Fatal {
        kind: ErrorKind,
        message: String,
    },
    /// The run's deadline killed a call.
    TimedOut,
    Usage(String),
}

/// Works out which project a name means and which forge its host belongs to, asking each host's
/// CLIs at most once per run.
pub struct Locator<'a, E> {
    claims: HashMap<String, Claim>,
    deadline: Option<SystemTime>,
    exec: &'a E,
}

/// Which CLIs hold a token for a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    Both,
    GitHub,
    GitLab,
    Neither,
}

struct Remote {
    host: String,
    name: String,
    path: String,
}

impl<'a, E: Exec> Locator<'a, E> {
    pub fn new(exec: &'a E, deadline: Option<SystemTime>) -> Self {
        Self {
            claims: HashMap::new(),
            deadline,
            exec,
        }
    }

    /// Locates every name before resolving any, then merges names that resolve to the same URL,
    /// in the order they first occur. An error carries the name as given.
    pub fn resolve<'n>(
        &mut self,
        backend: &impl Backend,
        names: &'n [Name],
    ) -> Result<Vec<Target>, (&'n str, Error)> {
        let mut references = Vec::new();
        for name in names {
            references.push(self.locate(name).map_err(|error| (name.given(), error))?);
        }
        let mut targets: Vec<Target> = Vec::new();
        for (name, reference) in names.iter().zip(&references) {
            let given = name.given();
            let target = backend
                .resolve(reference, self.deadline)
                .map_err(|failure| (given, Error::Backend(failure)))?;
            let same_host = split_remote_url(&target.url)
                .is_some_and(|(host, _)| host.eq_ignore_ascii_case(&reference.host));
            if !same_host {
                let message = format!(
                    "`{given}` redirects to {}, on another host, and babysit never follows a \
                     redirect across hosts; pass that URL instead",
                    target.url,
                );
                let kind = ErrorKind::TargetNotFound;
                return Err((given, Error::Fatal { kind, message }));
            }
            if targets.iter().all(|known| known.url != target.url) {
                targets.push(target);
            }
        }
        Ok(targets)
    }

    pub fn locate(&mut self, name: &Name) -> Result<Reference, Error> {
        match name {
            Name::Checkout { given, number } => self.locate_in_checkout(given, *number),
            Name::Url(reference) => Ok(reference.clone()),
        }
    }

    fn claim(&mut self, host: &str) -> Result<Claim, Error> {
        if host.eq_ignore_ascii_case(GITHUB_HOST) {
            return Ok(Claim::GitHub);
        }
        if host.eq_ignore_ascii_case(GITLAB_HOST) {
            return Ok(Claim::GitLab);
        }
        if let Some(claim) = self.claims.get(host) {
            return Ok(*claim);
        }
        let holds_token =
            |output: Output| output.code == Some(0) && !output.stdout.trim().is_empty();
        let github = self
            .call("gh", &["auth", "token", "--hostname", host])?
            .is_some_and(holds_token);
        let gitlab = self
            .call("glab", &["config", "get", "token", "--host", host])?
            .is_some_and(holds_token);
        let claim = match (github, gitlab) {
            (true, true) => Claim::Both,
            (true, false) => Claim::GitHub,
            (false, true) => Claim::GitLab,
            (false, false) => Claim::Neither,
        };
        self.claims.insert(host.to_owned(), claim);
        Ok(claim)
    }

    /// The project `gh pr view` would use: the default `gh repo set-default` chose, else the only
    /// GitHub remote.
    fn locate_in_checkout(&mut self, given: &str, number: u64) -> Result<Reference, Error> {
        let usage = |reason: &str| Error::Usage(format!("`{given}` {reason}; {PASS_THE_URL}"));
        let Some(remotes) = self.call("git", &["remote", "-v"])? else {
            return Err(Error::Fatal {
                kind: ErrorKind::CliMissing,
                message: "git is not installed or not on PATH".to_owned(),
            });
        };
        if remotes.code != Some(0) {
            return Err(usage("needs a git checkout"));
        }
        let mut github = Vec::new();
        let mut gitlab = false;
        let mut unclaimed = Vec::new();
        for remote in parse_remotes(&remotes.stdout) {
            match self.claim(&remote.host)? {
                Claim::Both => {
                    return Err(Error::Fatal {
                        kind: ErrorKind::Auth,
                        message: format!(
                            "both gh and glab hold a token for {}, so its forge is unknown; \
                             {PASS_THE_URL}",
                            remote.host,
                        ),
                    });
                }
                Claim::GitHub => github.push(remote),
                Claim::GitLab => gitlab = true,
                Claim::Neither if !unclaimed.contains(&remote.host) => {
                    unclaimed.push(remote.host);
                }
                Claim::Neither => {}
            }
        }
        if gitlab {
            return Err(usage(if github.is_empty() {
                "names a GitLab issue, which babysit can't watch yet"
            } else {
                "is ambiguous in a checkout with GitHub and GitLab remotes"
            }));
        }
        let pattern = format!(r"^remote\..*\.{GH_RESOLVED_KEY}$");
        let resolved = self
            .call("git", &["config", "--get-regexp", &pattern])?
            .map(|output| output.stdout)
            .unwrap_or_default();
        let (host, path) = if let Some((name, value)) = default_remote(&resolved) {
            let Some(remote) = github.iter().find(|remote| remote.name == name) else {
                return Err(usage(&format!(
                    "can't use `gh repo set-default` remote `{name}`, which isn't on a GitHub host"
                )));
            };
            let path = if value == RESOLVED_BASE {
                &remote.path
            } else {
                value
            };
            (remote.host.as_str(), path)
        } else {
            match &github[..] {
                [] if unclaimed.is_empty() => {
                    return Err(usage("needs a checkout with a GitHub remote"));
                }
                [] => {
                    return Err(Error::Usage(format!(
                        "`{given}` needs a checkout with a GitHub remote, and neither gh nor glab \
                         holds a token for {}; log in with `gh auth login --hostname <host>`",
                        unclaimed.join(", "),
                    )));
                }
                [only] => (only.host.as_str(), only.path.as_str()),
                _ => {
                    return Err(usage(
                        "is ambiguous with several GitHub remotes and no `gh repo set-default`",
                    ));
                }
            }
        };
        let Some((owner, repo)) = owner_and_repo(path) else {
            return Err(usage(&format!("can't use the remote project `{path}`")));
        };
        Ok(Reference {
            given: given.to_owned(),
            host: host.to_owned(),
            number,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
    }

    /// Runs a local command; `None` when the program isn't installed.
    fn call(&self, program: &str, args: &[&str]) -> Result<Option<Output>, Error> {
        match self.exec.run(program, args, self.deadline) {
            Ok(output) if output.code == Some(EXIT_NOT_FOUND) => Ok(None),
            Ok(output) => Ok(Some(output)),
            Err(exec::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(exec::Error::Io(error)) => Err(Error::Fatal {
                kind: ErrorKind::PollFailed,
                message: format!("could not run {program}: {error}"),
            }),
            Err(exec::Error::TimedOut(Limit::CallLimit)) => Err(Error::Fatal {
                kind: ErrorKind::PollFailed,
                message: format!("{program} did not answer within {}s", CALL_LIMIT.as_secs()),
            }),
            Err(exec::Error::TimedOut(Limit::Deadline)) => Err(Error::TimedOut),
        }
    }
}

/// The baseline `file` holds for `target`. An entry under its URL for another forge id is a reused
/// path, so `None` for a fresh baseline; an entry elsewhere with its forge id is a rename, re-keyed
/// under its URL.
pub fn baseline(file: &mut SnapshotFile, target: &Target) -> Option<Snapshot> {
    if let Some(entry) = file.entry(&target.url) {
        return (entry.forge_id == target.forge_id).then(|| entry.snapshot.clone());
    }
    let renamed = file
        .entries()
        .find(|entry| entry.forge_id == target.forge_id)?
        .url
        .clone();
    file.rekey(&renamed, &target.url);
    file.entry(&target.url).map(|entry| entry.snapshot.clone())
}

/// The remote name and `gh-resolved` value `gh repo set-default` last recorded.
fn default_remote(resolved: &str) -> Option<(&str, &str)> {
    resolved.lines().rev().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        let name = key
            .strip_prefix("remote.")?
            .strip_suffix(GH_RESOLVED_KEY)?
            .strip_suffix('.')?;
        Some((name, value))
    })
}

/// The remotes in `git remote -v` output, each at its fetch URL, or its last push URL where only
/// that names an `owner/repo` project.
fn parse_remotes(listing: &str) -> impl Iterator<Item = Remote> {
    listing.lines().filter_map(move |line| {
        let (name, fetch) = remote_url(line, " (fetch)")?;
        let fetched = split_remote_url(fetch);
        let pushed = listing
            .lines()
            .rev()
            .filter_map(|line| remote_url(line, " (push)"))
            .find(|(pushed, _)| *pushed == name)
            .and_then(|(_, push)| split_remote_url(push));
        let names_project = |(_, path): &(String, String)| owner_and_repo(path).is_some();
        let (host, path) = match (fetched, pushed) {
            (Some(fetched), _) if names_project(&fetched) => fetched,
            (_, Some(pushed)) if names_project(&pushed) => pushed,
            (fetched, pushed) => fetched.or(pushed)?,
        };
        Some(Remote {
            host,
            name: name.to_owned(),
            path,
        })
    })
}

/// A `git remote -v` line's remote name and URL, when it lists the URL for `direction`.
fn remote_url<'a>(line: &'a str, direction: &str) -> Option<(&'a str, &'a str)> {
    let (name, rest) = line.split_once('\t')?;
    Some((name, rest.strip_suffix(direction)?))
}

fn owner_and_repo(path: &str) -> Option<(&str, &str)> {
    path.split_once('/')
        .filter(|(owner, repo)| reference::is_name(owner) && reference::is_name(repo))
}

/// Splits a remote URL, in URL or scp-like form, into its host and project path. The port is
/// dropped, as `gh` drops it, since `gh` can't reach a host on one.
fn split_remote_url(url: &str) -> Option<(String, String)> {
    let (authority, path) = if let Some((_, rest)) = url.split_once("://") {
        rest.split_once('/')?
    } else {
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (authority, path)
    };
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host_and_port
        .split_once(':')
        .map_or(host_and_port, |(host, _)| host);
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if !reference::is_authority(host) || path.is_empty() {
        return None;
    }
    Some((host.to_owned(), path.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Script;
    use crate::github::GitHub;

    const GH_RESOLVED: &str = r"git config --get-regexp ^remote\..*\.gh-resolved$";

    fn missing() -> Result<Output, exec::Error> {
        Err(exec::Error::Io(io::ErrorKind::NotFound.into()))
    }

    fn remotes(listing: &[(&str, &str)]) -> (String, Result<Output, exec::Error>) {
        let lines = listing
            .iter()
            .map(|(name, url)| format!("{name}\t{url} (fetch)\n{name}\t{url} (push)\n"));
        (
            "git remote -v".to_owned(),
            Ok(Output::exited(0, &lines.collect::<String>())),
        )
    }

    fn tokens(host: &str, claim: Claim) -> [(String, Result<Output, exec::Error>); 2] {
        let gh = matches!(claim, Claim::Both | Claim::GitHub);
        let glab = matches!(claim, Claim::Both | Claim::GitLab);
        [
            (
                format!("gh auth token --hostname {host}"),
                Ok(Output::exited(
                    i32::from(!gh),
                    if gh { "gho_x\n" } else { "" },
                )),
            ),
            (
                format!("glab config get token --host {host}"),
                Ok(Output::exited(0, if glab { "glpat-x\n" } else { "" })),
            ),
        ]
    }

    fn checkout() -> Name {
        reference::parse_name("#12").unwrap()
    }

    fn locate(script: &Script, name: &Name) -> Result<Reference, Error> {
        Locator::new(script, None).locate(name)
    }

    fn located(host: &str, owner: &str, repo: &str) -> Reference {
        Reference {
            given: "#12".to_owned(),
            host: host.to_owned(),
            number: 12,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        }
    }

    #[test]
    fn locates_the_only_github_remote() {
        let script = Script::new([
            remotes(&[("origin", "git@github.com:o/r.git")]),
            (GH_RESOLVED.to_owned(), Ok(Output::exited(1, ""))),
        ]);

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("github.com", "o", "r"))
        );
    }

    #[test]
    fn falls_back_to_the_push_url_when_the_fetch_url_names_no_project() {
        let listing = "origin\t/srv/git/r.git (fetch)\norigin\tgit@github.com:o/r.git (push)\n";
        let script = Script::new([
            ("git remote -v".to_owned(), Ok(Output::exited(0, listing))),
            (
                GH_RESOLVED.to_owned(),
                Ok(Output::exited(0, "remote.origin.gh-resolved base\n")),
            ),
        ]);

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("github.com", "o", "r")),
        );
    }

    #[test]
    fn falls_back_to_the_last_push_url_when_the_fetch_path_is_no_project() {
        let listing = "origin\tssh://mirror/srv/git/r.git (fetch)\n\
                       origin\tgit@github.com:me/r.git (push)\n\
                       origin\tgit@github.com:o/r.git (push)\n";
        let script = Script::new([
            ("git remote -v".to_owned(), Ok(Output::exited(0, listing))),
            (GH_RESOLVED.to_owned(), Ok(Output::exited(1, ""))),
        ]);

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("github.com", "o", "r")),
        );
    }

    #[test]
    fn locates_the_default_among_several_remotes() {
        let script = Script::new([
            remotes(&[
                ("origin", "https://github.com/me/r.git"),
                ("upstream", "ssh://git@github.com:22/o/r"),
            ]),
            (
                GH_RESOLVED.to_owned(),
                Ok(Output::exited(0, "remote.upstream.gh-resolved base\n")),
            ),
        ]);

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("github.com", "o", "r"))
        );
    }

    #[test]
    fn locates_a_default_project_that_is_no_remote() {
        let script = Script::new([
            remotes(&[("origin", "https://github.com/me/r")]),
            (
                GH_RESOLVED.to_owned(),
                Ok(Output::exited(0, "remote.origin.gh-resolved o/parent\n")),
            ),
        ]);

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("github.com", "o", "parent")),
        );
    }

    #[test]
    fn locates_an_enterprise_remote_by_its_token_and_skips_unclaimed_hosts() {
        let script = Script::new(
            [remotes(&[
                ("mirror", "https://git.example.org/o/r.git"),
                ("origin", "https://ghe.example.com:8443/o/r.git"),
            ])]
            .into_iter()
            .chain(tokens("git.example.org", Claim::Neither))
            .chain(tokens("ghe.example.com", Claim::GitHub))
            .chain([(GH_RESOLVED.to_owned(), Ok(Output::exited(1, "")))]),
        );

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("ghe.example.com", "o", "r")),
        );
    }

    #[test]
    fn rejects_several_github_remotes_without_a_default() {
        let script = Script::new([
            remotes(&[
                ("origin", "git@github.com:me/r.git"),
                ("upstream", "git@github.com:o/r.git"),
            ]),
            (GH_RESOLVED.to_owned(), Ok(Output::exited(1, ""))),
        ]);

        assert!(matches!(locate(&script, &checkout()), Err(Error::Usage(_))));
    }

    #[test]
    fn rejects_mixed_github_and_gitlab_remotes() {
        let script = Script::new([remotes(&[
            ("github", "git@github.com:o/r.git"),
            ("gitlab", "git@gitlab.com:g/r.git"),
        ])]);

        assert!(matches!(locate(&script, &checkout()), Err(Error::Usage(_))));
    }

    #[test]
    fn rejects_a_reference_outside_a_checkout() {
        let script = Script::new([("git remote -v".to_owned(), Ok(Output::exited(128, "")))]);

        assert!(matches!(locate(&script, &checkout()), Err(Error::Usage(_))));
    }

    #[test]
    fn reports_a_missing_git() {
        for missing in [missing(), Ok(Output::exited(127, ""))] {
            let script = Script::new([("git remote -v".to_owned(), missing)]);

            assert!(matches!(
                locate(&script, &checkout()),
                Err(Error::Fatal {
                    kind: ErrorKind::CliMissing,
                    ..
                }),
            ));
        }
    }

    #[test]
    fn rejects_a_default_remote_on_an_unclaimed_host() {
        let script = Script::new(
            [remotes(&[
                ("origin", "git@github-work:me/r.git"),
                ("upstream", "https://github.com/o/r"),
            ])]
            .into_iter()
            .chain(tokens("github-work", Claim::Neither))
            .chain([(
                GH_RESOLVED.to_owned(),
                Ok(Output::exited(0, "remote.origin.gh-resolved base\n")),
            )]),
        );

        assert!(matches!(locate(&script, &checkout()), Err(Error::Usage(_))));
    }

    #[test]
    fn takes_a_url_as_given_without_asking_for_tokens() {
        let name = reference::parse_name("https://ghe.example.com/o/r/pull/12").unwrap();
        let Name::Url(reference) = &name else {
            unreachable!()
        };

        assert_eq!(locate(&Script::new([]), &name), Ok(reference.clone()));
    }

    #[test]
    fn rejects_a_remote_host_claimed_by_both_clis() {
        let script = Script::new(
            [remotes(&[("origin", "https://forge.example.com/o/r.git")])]
                .into_iter()
                .chain(tokens("forge.example.com", Claim::Both)),
        );

        let Err(Error::Fatal { kind, message }) = locate(&script, &checkout()) else {
            panic!("expected an auth error");
        };

        assert_eq!(kind, ErrorKind::Auth);
        assert!(message.contains("forge.example.com"), "{message}");
    }

    #[test]
    fn names_unclaimed_remote_hosts_when_no_github_remote_is_left() {
        let script = Script::new(
            [remotes(&[
                ("a", "https://ghe.example.com/o/r.git"),
                ("b", "https://gitea.example.org/o/r.git"),
                ("c", "git@ghe.example.com:o/r.git"),
            ])]
            .into_iter()
            .chain(tokens("ghe.example.com", Claim::Neither))
            .chain(tokens("gitea.example.org", Claim::Neither))
            .chain([(GH_RESOLVED.to_owned(), Ok(Output::exited(1, "")))]),
        );

        let Err(Error::Usage(message)) = locate(&script, &checkout()) else {
            panic!("expected a usage error");
        };

        assert!(
            message.contains("token for ghe.example.com, gitea.example.org;"),
            "{message}",
        );
    }

    fn resolves_to(path: &str, html_url: &str) -> (String, Result<Output, exec::Error>) {
        let body = format!(r#"{{"html_url":"{html_url}","node_id":"PR_{html_url}"}}"#);
        (
            format!("gh api -i --hostname {path}"),
            Ok(Output::exited(0, &format!("HTTP/2.0 200 OK\r\n\r\n{body}"))),
        )
    }

    fn urls(names: &[&str]) -> Vec<Name> {
        names
            .iter()
            .map(|name| reference::parse_name(name).unwrap())
            .collect()
    }

    #[test]
    fn names_resolving_to_the_same_url_merge_in_first_occurrence_order() {
        let renamed = "https://github.com/o/r/pull/12";
        let other = "https://github.com/o/r/pull/3";
        let script = Script::new([
            resolves_to("github.com repos/old/r/pulls/12", renamed),
            resolves_to("github.com repos/o/r/pulls/3", other),
            resolves_to("github.com repos/o/r/pulls/12", renamed),
        ]);
        let names = urls(&["https://github.com/old/r/pull/12", other, renamed]);

        let targets = Locator::new(&script, None)
            .resolve(&GitHub::new(&script), &names)
            .unwrap();

        let resolved: Vec<_> = targets.iter().map(|target| target.url.as_str()).collect();
        assert_eq!(resolved, [renamed, other]);
    }

    #[test]
    fn a_redirect_to_another_host_is_not_followed() {
        let script = Script::new([resolves_to(
            "ghe.example.com repos/o/r/pulls/1",
            "https://github.com/o/r/pull/1",
        )]);
        let names = urls(&["https://ghe.example.com/o/r/pull/1"]);

        let Err((given, Error::Fatal { kind, message })) =
            Locator::new(&script, None).resolve(&GitHub::new(&script), &names)
        else {
            panic!("expected a fatal error");
        };

        assert_eq!(given, "https://ghe.example.com/o/r/pull/1");
        assert_eq!(kind, ErrorKind::TargetNotFound);
        assert!(
            message.contains("https://github.com/o/r/pull/1"),
            "{message}"
        );
    }

    #[test]
    fn asks_about_each_host_once() {
        let script = Script::new(
            [remotes(&[
                ("origin", "https://ghe.example.com/me/r.git"),
                ("upstream", "git@ghe.example.com:o/r.git"),
            ])]
            .into_iter()
            .chain(tokens("ghe.example.com", Claim::GitHub))
            .chain([(
                GH_RESOLVED.to_owned(),
                Ok(Output::exited(0, "remote.upstream.gh-resolved base\n")),
            )]),
        );

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("ghe.example.com", "o", "r")),
        );
    }
}
