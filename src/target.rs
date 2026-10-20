use std::cmp::Reverse;
use std::collections::HashMap;
use std::io;
use std::time::SystemTime;

use crate::envelope::{ErrorKind, Forge};
use crate::exec::{self, CALL_LIMIT, Exec, Limit, Output};
use crate::forge::{Failure, Snapshot, Target};
use crate::reference::{self, Name, Prefix, Reference};
use crate::snapshot_file::SnapshotFile;

const GITHUB_HOST: &str = "github.com";
const GITLAB_HOST: &str = "gitlab.com";
const EXIT_NOT_FOUND: i32 = 127;
const PASS_THE_URL: &str = "pass the target's URL instead";
/// The git config key, under a remote, where `gh repo set-default` records its choice.
const GH_RESOLVED_KEY: &str = "gh-resolved";
/// The legacy git config key, under a remote, where glab records its base repository.
const GLAB_RESOLVED_KEY: &str = "glab-resolved";
/// The resolution value naming the remote itself as the project, or prefixing another's path.
const RESOLVED_BASE: &str = "base";
/// The prefix of a legacy glab resolution naming the push repository instead of the base.
const RESOLVED_HEAD: &str = "head";

/// Reads the remote and project a CLI recorded as a checkout's default from its git config.
type RecordedDefault = fn(&str) -> Option<(&str, &str)>;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Fatal {
        /// `None` for a failure before any forge call.
        forge: Option<Forge>,
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
    /// The host, with the port of an HTTP(S) URL but not of an SSH one.
    authority: String,
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

    /// Locates every name, pairing each reference with the name as given. An error carries the
    /// name as given.
    pub fn locate_all<'n>(
        &mut self,
        names: &'n [Name],
    ) -> Result<Vec<(&'n str, Reference)>, (&'n str, Error)> {
        names
            .iter()
            .map(|name| {
                let given = name.given();
                self.locate(name)
                    .map(|reference| (given, reference))
                    .map_err(|error| (given, error))
            })
            .collect()
    }

    pub fn locate(&mut self, name: &Name) -> Result<Reference, Error> {
        match name {
            Name::Checkout {
                given,
                number,
                prefix,
            } => self.locate_in_checkout(given, *number, *prefix),
            Name::Url(reference) => Ok(reference.clone()),
        }
    }

    /// Which CLIs claim `authority`: gh by its bare host, since gh can't reach a host on a port, and
    /// glab with the port, as it keys its hosts.
    fn claim(&mut self, authority: &str) -> Result<Claim, Error> {
        let host = without_port(authority);
        if host.eq_ignore_ascii_case(GITHUB_HOST) {
            return Ok(Claim::GitHub);
        }
        if host.eq_ignore_ascii_case(GITLAB_HOST) {
            return Ok(Claim::GitLab);
        }
        if let Some(claim) = self.claims.get(authority) {
            return Ok(*claim);
        }
        let holds_token =
            |output: Output| output.code == Some(0) && !output.stdout.trim().is_empty();
        let github = self
            .call("gh", &["auth", "token", "--hostname", host])?
            .is_some_and(holds_token);
        let gitlab = self
            .call("glab", &["config", "get", "token", "--host", authority])?
            .is_some_and(holds_token);
        let claim = match (github, gitlab) {
            (true, true) => Claim::Both,
            (true, false) => Claim::GitHub,
            (false, true) => Claim::GitLab,
            (false, false) => Claim::Neither,
        };
        self.claims.insert(authority.to_owned(), claim);
        Ok(claim)
    }

    /// The project `gh pr view` or `glab mr view` would use: the default its CLI recorded, else the
    /// only remote on that forge.
    fn locate_in_checkout(
        &mut self,
        given: &str,
        number: u64,
        prefix: Prefix,
    ) -> Result<Reference, Error> {
        let usage = |reason: &str| Error::Usage(format!("`{given}` {reason}; {PASS_THE_URL}"));
        let Some(remotes) = self.call("git", &["remote", "-v"])? else {
            return Err(Error::Fatal {
                forge: None,
                kind: ErrorKind::CliMissing,
                message: "git is not installed or not on PATH".to_owned(),
            });
        };
        if remotes.code != Some(0) {
            return Err(usage("needs a git checkout"));
        }
        let mut github = Vec::new();
        let mut gitlab = Vec::new();
        let mut unclaimed = Vec::new();
        for remote in parse_remotes(&remotes.stdout) {
            match self.claim(&remote.authority)? {
                Claim::Both => {
                    return Err(Error::Fatal {
                        forge: None,
                        kind: ErrorKind::Auth,
                        message: format!(
                            "both gh and glab hold a token for {}, so its forge is unknown; \
                             {PASS_THE_URL}",
                            remote.authority,
                        ),
                    });
                }
                Claim::GitHub => github.push(remote),
                Claim::GitLab => gitlab.push(remote),
                Claim::Neither if !unclaimed.contains(&remote.authority) => {
                    unclaimed.push(remote.authority);
                }
                Claim::Neither => {}
            }
        }
        if !github.is_empty() && !gitlab.is_empty() {
            return Err(usage(
                "is ambiguous in a checkout with GitHub and GitLab remotes",
            ));
        }
        let (forge, remotes) = match prefix {
            Prefix::Bang if !github.is_empty() => {
                return Err(usage(
                    "names a GitLab merge request, in a checkout with only GitHub remotes",
                ));
            }
            Prefix::Bang => (Forge::GitLab, gitlab),
            Prefix::Hash if !gitlab.is_empty() => {
                return Err(usage("names a GitLab issue, which babysit can't watch yet"));
            }
            Prefix::Hash => (Forge::GitHub, github),
        };
        let (host, project) = self.choose_project(given, forge, &remotes, &unclaimed)?;
        Ok(Reference {
            forge,
            given: given.to_owned(),
            host,
            number,
            project,
        })
    }

    /// The host and project path of the remote `forge`'s CLI recorded as the default, else of the
    /// only one of `remotes`.
    fn choose_project(
        &self,
        given: &str,
        forge: Forge,
        remotes: &[Remote],
        unclaimed: &[String],
    ) -> Result<(String, String), Error> {
        let usage = |reason: &str| Error::Usage(format!("`{given}` {reason}; {PASS_THE_URL}"));
        let (forge_name, cli, chosen_by, pattern, recorded): (_, _, _, _, RecordedDefault) =
            match forge {
                Forge::GitHub => (
                    "GitHub",
                    "gh",
                    "`gh repo set-default`",
                    format!(r"^remote\..*\.{GH_RESOLVED_KEY}$"),
                    gh_default,
                ),
                Forge::GitLab => (
                    "GitLab",
                    "glab",
                    "glab's base repository choice",
                    format!(r"^remote\..*\.{GLAB_RESOLVED_KEY}(-base)?$"),
                    glab_default,
                ),
            };
        let resolved = self
            .call("git", &["config", "--get-regexp", &pattern])?
            .map(|output| output.stdout)
            .unwrap_or_default();
        let recorded = recorded(&resolved);
        let (host, path) = if let Some((name, value)) = recorded {
            let Some(remote) = remotes.iter().find(|remote| remote.name == name) else {
                return Err(usage(&format!(
                    "can't use {chosen_by} remote `{name}`, which isn't on a {forge_name} host"
                )));
            };
            let path = if value == RESOLVED_BASE {
                &remote.path
            } else {
                value
            };
            (remote.authority.as_str(), path)
        } else {
            match remotes {
                [] if unclaimed.is_empty() => {
                    return Err(usage(&format!(
                        "needs a checkout with a {forge_name} remote"
                    )));
                }
                [] => {
                    return Err(Error::Usage(format!(
                        "`{given}` needs a checkout with a {forge_name} remote, and neither gh nor \
                         glab holds a token for {}; log in with `{cli} auth login --hostname <host>`",
                        unclaimed.join(", "),
                    )));
                }
                [only] => (only.authority.as_str(), only.path.as_str()),
                _ => {
                    return Err(usage(&format!(
                        "is ambiguous with several {forge_name} remotes and no {chosen_by}"
                    )));
                }
            }
        };
        if !reference::is_project_path(forge, path) {
            return Err(usage(&format!("can't use the remote project `{path}`")));
        }
        let host = match forge {
            Forge::GitHub => without_port(host),
            Forge::GitLab => host,
        };
        Ok((host.to_owned(), path.to_owned()))
    }

    /// Runs a local command; `None` when the program isn't installed.
    fn call(&self, program: &str, args: &[&str]) -> Result<Option<Output>, Error> {
        match self.exec.run(program, args, self.deadline) {
            Ok(output) if output.code == Some(EXIT_NOT_FOUND) => Ok(None),
            Ok(output) => Ok(Some(output)),
            Err(exec::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(exec::Error::Io(error)) => Err(Error::Fatal {
                forge: None,
                kind: ErrorKind::PollFailed,
                message: format!("could not run {program}: {error}"),
            }),
            Err(exec::Error::TimedOut(Limit::CallLimit)) => Err(Error::Fatal {
                forge: None,
                kind: ErrorKind::PollFailed,
                message: format!("{program} did not answer within {}s", CALL_LIMIT.as_secs()),
            }),
            Err(exec::Error::TimedOut(Limit::Deadline)) => Err(Error::TimedOut),
        }
    }
}

/// The baseline `file` holds for `target`, guarding a reused path and re-keying a rename. Forge ids
/// are unique only within one host.
pub fn baseline(file: &mut SnapshotFile, target: &Target) -> Option<Snapshot> {
    if let Some(entry) = file.entry(&target.url) {
        return (entry.forge_id == target.forge_id).then(|| entry.snapshot.clone());
    }
    let host = url_host(&target.url)?;
    file.rename(&target.url, |entry| {
        entry.forge_id == target.forge_id && on_host(&entry.url, host)
    })
    .map(|entry| entry.snapshot.clone())
}

/// Fails a resolution that followed a redirect onto another host.
pub fn check_host(given: &str, reference: &Reference, target: &Target) -> Result<(), Failure> {
    if on_host(&target.url, &reference.host) {
        return Ok(());
    }
    let message = format!(
        "`{given}` resolves to {}, on another host, and babysit never takes a redirect across \
         hosts; pass that URL instead",
        target.url,
    );
    Err(Failure::Fatal {
        forge: reference.forge,
        kind: ErrorKind::TargetNotFound,
        message,
    })
}

impl From<Failure> for Error {
    fn from(failure: Failure) -> Self {
        match failure.into_error() {
            Some((forge, kind, message)) => Self::Fatal {
                forge: Some(forge),
                kind,
                message,
            },
            None => Self::TimedOut,
        }
    }
}

/// The host of a web URL such as `https://github.com/o/r/pull/1`.
fn url_host(url: &str) -> Option<&str> {
    url.split_once("://")?.1.split('/').next()
}

fn on_host(url: &str, host: &str) -> bool {
    url_host(url).is_some_and(|url_host| url_host.eq_ignore_ascii_case(host))
}

/// The remote name and `gh-resolved` value `gh repo set-default` last recorded.
fn gh_default(resolved: &str) -> Option<(&str, &str)> {
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
        let names_project =
            |(_, path): &(String, String)| reference::is_project_path(Forge::GitHub, path);
        let (authority, path) = match (fetched, pushed) {
            (Some(fetched), _) if names_project(&fetched) => fetched,
            (_, Some(pushed)) if names_project(&pushed) => pushed,
            (fetched, pushed) => fetched.or(pushed)?,
        };
        Some(Remote {
            authority,
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

/// The remote name and project glab resolved the base repository to: the first remote in glab's
/// order with a resolution, its `-base` key before the legacy one.
fn glab_default(resolved: &str) -> Option<(&str, &str)> {
    let entries: Vec<_> = resolved
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            let (name, key) = key.strip_prefix("remote.")?.rsplit_once('.')?;
            Some((name, key, value))
        })
        .collect();
    let mut names: Vec<_> = Vec::new();
    for (name, ..) in &entries {
        if !names.contains(name) {
            names.push(*name);
        }
    }
    names.sort_by_key(|name| Reverse(glab_remote_rank(name)));
    let base_key = format!("{GLAB_RESOLVED_KEY}-base");
    names.into_iter().find_map(|name| {
        let value = |wanted: &str| {
            entries
                .iter()
                .find(|(remote, key, _)| *remote == name && *key == wanted)
                .map(|(.., value)| *value)
        };
        let legacy = value(GLAB_RESOLVED_KEY).and_then(|value| {
            based(value).or((!value.starts_with(RESOLVED_HEAD)).then_some(value))
        });
        let project = value(&base_key).and_then(based).or(legacy)?;
        Some((name, project))
    })
}

/// `base`, or the project path after `base:`.
fn based(value: &str) -> Option<&str> {
    if value == RESOLVED_BASE {
        return Some(value);
    }
    value.strip_prefix(RESOLVED_BASE)?.strip_prefix(':')
}

/// Where glab sorts a remote when it looks for a resolution: higher first.
fn glab_remote_rank(name: &str) -> u8 {
    match name.to_ascii_lowercase().as_str() {
        "upstream" => 3,
        "gitlab" => 2,
        "origin" => 1,
        _ => 0,
    }
}

/// Splits a remote URL, in URL or scp-like form, into its authority and project path. Only an
/// HTTP(S) URL keeps its port, since an SSH port is the transport's, not the forge's.
fn split_remote_url(url: &str) -> Option<(String, String)> {
    let (scheme, authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        (Some(scheme), authority, path)
    } else {
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (None, authority, path)
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, authority)| authority);
    let authority = match scheme {
        Some("http" | "https") => authority,
        _ => without_port(authority),
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if !reference::is_authority(authority) || path.is_empty() {
        return None;
    }
    Some((authority.to_owned(), path.to_owned()))
}

fn without_port(authority: &str) -> &str {
    authority
        .split_once(':')
        .map_or(authority, |(host, _)| host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Script;

    const GH_RESOLVED: &str = r"git config --get-regexp ^remote\..*\.gh-resolved$";
    const GLAB_RESOLVED: &str = r"git config --get-regexp ^remote\..*\.glab-resolved(-base)?$";

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

    fn tokens(authority: &str, claim: Claim) -> [(String, Result<Output, exec::Error>); 2] {
        let gh = matches!(claim, Claim::Both | Claim::GitHub);
        let glab = matches!(claim, Claim::Both | Claim::GitLab);
        [
            (
                format!("gh auth token --hostname {}", without_port(authority)),
                Ok(Output::exited(
                    i32::from(!gh),
                    if gh { "gho_x\n" } else { "" },
                )),
            ),
            (
                format!("glab config get token --host {authority}"),
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
            forge: Forge::GitHub,
            given: "#12".to_owned(),
            host: host.to_owned(),
            number: 12,
            project: format!("{owner}/{repo}"),
        }
    }

    fn merge_request() -> Name {
        reference::parse_name("!12").unwrap()
    }

    fn located_on_gitlab(host: &str, project: &str) -> Reference {
        Reference {
            forge: Forge::GitLab,
            given: "!12".to_owned(),
            host: host.to_owned(),
            number: 12,
            project: project.to_owned(),
        }
    }

    #[test]
    fn locates_the_only_gitlab_remote_with_its_nested_groups() {
        let script = Script::new([
            remotes(&[("origin", "git@gitlab.com:g/sub/p.git")]),
            (GLAB_RESOLVED.to_owned(), Ok(Output::exited(1, ""))),
        ]);

        assert_eq!(
            locate(&script, &merge_request()),
            Ok(located_on_gitlab("gitlab.com", "g/sub/p")),
        );
    }

    #[test]
    fn locates_the_base_repository_glab_recorded() {
        let resolved = "remote.origin.glab-resolved head\n\
                        remote.upstream.glab-resolved-base base:g/parent\n";
        let script = Script::new(
            [remotes(&[
                ("origin", "https://gitlab.example.com/me/p.git"),
                ("upstream", "https://gitlab.example.com/g/p.git"),
            ])]
            .into_iter()
            .chain(tokens("gitlab.example.com", Claim::GitLab))
            .chain([(GLAB_RESOLVED.to_owned(), Ok(Output::exited(0, resolved)))]),
        );

        assert_eq!(
            locate(&script, &merge_request()),
            Ok(located_on_gitlab("gitlab.example.com", "g/parent")),
        );
    }

    #[test]
    fn takes_the_first_resolved_remote_in_glabs_order_as_glab_does() {
        let resolved = "remote.origin.glab-resolved-base base\n\
                        remote.upstream.glab-resolved base:headquarters/app\n";
        let script = Script::new([
            remotes(&[
                ("origin", "git@gitlab.com:me/app.git"),
                ("upstream", "git@gitlab.com:headquarters/app.git"),
            ]),
            (GLAB_RESOLVED.to_owned(), Ok(Output::exited(0, resolved))),
        ]);

        assert_eq!(
            locate(&script, &merge_request()),
            Ok(located_on_gitlab("gitlab.com", "headquarters/app")),
        );
    }

    #[test]
    fn keeps_the_port_of_an_https_gitlab_remote_but_not_of_an_ssh_one() {
        for (url, authority) in [
            (
                "https://gitlab.example.com:8443/g/p.git",
                "gitlab.example.com:8443",
            ),
            (
                "ssh://git@gitlab.example.com:2222/g/p.git",
                "gitlab.example.com",
            ),
        ] {
            let script = Script::new(
                [remotes(&[("origin", url)])]
                    .into_iter()
                    .chain(tokens(authority, Claim::GitLab))
                    .chain([(GLAB_RESOLVED.to_owned(), Ok(Output::exited(1, "")))]),
            );

            assert_eq!(
                locate(&script, &merge_request()),
                Ok(located_on_gitlab(authority, "g/p")),
                "{url}",
            );
        }
    }

    #[test]
    fn rejects_several_gitlab_remotes_without_a_recorded_base() {
        let script = Script::new([
            remotes(&[
                ("origin", "git@gitlab.com:me/p.git"),
                ("upstream", "git@gitlab.com:g/p.git"),
            ]),
            (
                GLAB_RESOLVED.to_owned(),
                Ok(Output::exited(0, "remote.origin.glab-resolved head\n")),
            ),
        ]);

        assert!(matches!(
            locate(&script, &merge_request()),
            Err(Error::Usage(_)),
        ));
    }

    #[test]
    fn rejects_a_reference_naming_the_other_forge() {
        for (name, url) in [
            (merge_request(), "git@github.com:o/r.git"),
            (checkout(), "git@gitlab.com:g/p.git"),
        ] {
            let script = Script::new([remotes(&[("origin", url)])]);

            assert!(
                matches!(locate(&script, &name), Err(Error::Usage(_))),
                "{url}",
            );
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
            .chain(tokens("ghe.example.com:8443", Claim::GitHub))
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
                    forge: None,
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

        let Err(Error::Fatal { kind, message, .. }) = locate(&script, &checkout()) else {
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

    #[test]
    fn a_redirect_to_another_host_is_not_followed() {
        let given = "https://ghe.example.com/o/r/pull/1";
        let redirected = Target {
            forge: Forge::GitHub,
            forge_id: "PR_1".to_owned(),
            host: "ghe.example.com".to_owned(),
            url: "https://github.com/o/r/pull/1".to_owned(),
        };
        let reference = reference::parse(given).unwrap();

        let Err(Failure::Fatal { kind, message, .. }) = check_host(given, &reference, &redirected)
        else {
            panic!("expected a fatal failure");
        };

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
