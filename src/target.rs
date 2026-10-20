use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::time::SystemTime;

use crate::envelope::ErrorKind;
use crate::exec::{self, CALL_LIMIT, Exec, Limit, Output};
use crate::reference::{self, Name, Reference};

const GITHUB_HOST: &str = "github.com";
const GITLAB_HOST: &str = "gitlab.com";
/// The `gh-resolved` value marking the remote `gh repo set-default` chose.
const RESOLVED_BASE: &str = "base";

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
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
    claims: RefCell<HashMap<String, Claim>>,
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
            claims: RefCell::default(),
            deadline,
            exec,
        }
    }

    pub fn locate(&self, name: &Name) -> Result<Reference, Error> {
        match name {
            Name::Checkout { given, number } => self.locate_in_checkout(given, *number),
            Name::Url(reference) => match self.claim(&reference.host)? {
                Claim::Both => Err(claimed_by_both(&reference.host)),
                Claim::GitHub => Ok(reference.clone()),
                Claim::GitLab => Err(Error::Usage(format!(
                    "`{}` is a GitHub URL, but {} is a GitLab host",
                    reference.given, reference.host,
                ))),
                Claim::Neither => Err(Error::Fatal {
                    kind: ErrorKind::Auth,
                    message: format!("neither gh nor glab holds a token for {}", reference.host),
                }),
            },
        }
    }

    fn claim(&self, host: &str) -> Result<Claim, Error> {
        match host {
            GITHUB_HOST => return Ok(Claim::GitHub),
            GITLAB_HOST => return Ok(Claim::GitLab),
            _ => {}
        }
        if let Some(claim) = self.claims.borrow().get(host) {
            return Ok(*claim);
        }
        let github = self
            .call("gh", &["auth", "token", "--hostname", host])?
            .is_some_and(|output| output.code == Some(0));
        let gitlab = self
            .call("glab", &["config", "get", "token", "--host", host])?
            .is_some_and(|output| output.code == Some(0) && !output.stdout.trim().is_empty());
        let claim = match (github, gitlab) {
            (true, true) => Claim::Both,
            (true, false) => Claim::GitHub,
            (false, true) => Claim::GitLab,
            (false, false) => Claim::Neither,
        };
        self.claims.borrow_mut().insert(host.to_owned(), claim);
        Ok(claim)
    }

    /// The project `gh pr view` would use: the default `gh repo set-default` chose, else the only
    /// GitHub remote.
    fn locate_in_checkout(&self, given: &str, number: u64) -> Result<Reference, Error> {
        let usage = |reason: &str| {
            Error::Usage(format!("`{given}` {reason}; pass the target's URL instead"))
        };
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
        for remote in fetch_remotes(&remotes.stdout) {
            match self.claim(&remote.host)? {
                Claim::Both => return Err(claimed_by_both(&remote.host)),
                Claim::GitHub => github.push(remote),
                Claim::GitLab => gitlab = true,
                Claim::Neither => {}
            }
        }
        if gitlab && !github.is_empty() {
            return Err(usage(
                "is ambiguous in a checkout with GitHub and GitLab remotes",
            ));
        }
        if gitlab {
            return Err(usage("names a GitLab issue, which babysit can't watch yet"));
        }
        let resolved = self
            .call(
                "git",
                &["config", "--get-regexp", r"^remote\..*\.gh-resolved$"],
            )?
            .map(|output| output.stdout)
            .unwrap_or_default();
        let (host, path) = match default_remote(&resolved, &github) {
            Some(default) => default,
            None => match &github[..] {
                [] => return Err(usage("needs a checkout with a GitHub remote")),
                [only] => (&only.host, only.path.as_str()),
                _ => {
                    return Err(usage(
                        "is ambiguous with several GitHub remotes and no `gh repo set-default`",
                    ));
                }
            },
        };
        let Some((owner, repo)) = path
            .split_once('/')
            .filter(|(owner, repo)| reference::is_name(owner) && reference::is_name(repo))
        else {
            return Err(usage(&format!("can't use the remote project `{path}`")));
        };
        Ok(Reference {
            given: given.to_owned(),
            host: host.clone(),
            number,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
    }

    /// Runs a local command; `None` when the program isn't installed.
    fn call(&self, program: &str, args: &[&str]) -> Result<Option<Output>, Error> {
        match self.exec.run(program, args, self.deadline) {
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

fn claimed_by_both(host: &str) -> Error {
    Error::Fatal {
        kind: ErrorKind::Auth,
        message: format!("both gh and glab hold a token for {host}, so its forge is unknown"),
    }
}

/// The host and project path of the remote `gh repo set-default` chose, among `github`.
fn default_remote<'r>(resolved: &'r str, github: &'r [Remote]) -> Option<(&'r String, &'r str)> {
    resolved.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        let name = key.strip_prefix("remote.")?.strip_suffix(".gh-resolved")?;
        let remote = github.iter().find(|remote| remote.name == name)?;
        match value {
            RESOLVED_BASE => Some((&remote.host, remote.path.as_str())),
            project => Some((&remote.host, project)),
        }
    })
}

/// The fetch remotes in `git remote -v` output whose URL names a host and project.
fn fetch_remotes(listing: &str) -> impl Iterator<Item = Remote> {
    listing.lines().filter_map(|line| {
        let (name, rest) = line.split_once('\t')?;
        let url = rest.strip_suffix(" (fetch)")?;
        let (host, path) = split_remote_url(url)?;
        Some(Remote {
            host,
            name: name.to_owned(),
            path,
        })
    })
}

/// Splits a remote URL, in URL or scp-like form, into its web host and project path. Only a web
/// URL keeps its port, since an SSH port isn't part of the forge's host.
fn split_remote_url(url: &str) -> Option<(String, String)> {
    let (authority, path, keeps_port) = if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        (authority, path, matches!(scheme, "http" | "https"))
    } else {
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (authority, path, false)
    };
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match (keeps_port, host_and_port.split_once(':')) {
        (false, Some((host, _))) => host,
        _ => host_and_port,
    };
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

    const GH_RESOLVED: &str = r"git config --get-regexp ^remote\..*\.gh-resolved$";

    fn exited(code: i32, stdout: &str) -> Output {
        Output {
            code: Some(code),
            stderr: String::new(),
            stdout: stdout.to_owned(),
        }
    }

    fn missing() -> Result<Output, exec::Error> {
        Err(exec::Error::Io(io::ErrorKind::NotFound.into()))
    }

    fn remotes(listing: &[(&str, &str)]) -> (String, Result<Output, exec::Error>) {
        let lines = listing
            .iter()
            .map(|(name, url)| format!("{name}\t{url} (fetch)\n{name}\t{url} (push)\n"));
        (
            "git remote -v".to_owned(),
            Ok(exited(0, &lines.collect::<String>())),
        )
    }

    fn tokens(host: &str, gh: bool, glab: bool) -> [(String, Result<Output, exec::Error>); 2] {
        [
            (
                format!("gh auth token --hostname {host}"),
                Ok(exited(i32::from(!gh), if gh { "gho_x\n" } else { "" })),
            ),
            (
                format!("glab config get token --host {host}"),
                Ok(exited(0, if glab { "glpat-x\n" } else { "" })),
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
        let script = Script::new(
            [remotes(&[("origin", "git@github.com:o/r.git")])]
                .into_iter()
                .chain([(GH_RESOLVED.to_owned(), Ok(exited(1, "")))]),
        );

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("github.com", "o", "r"))
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
                Ok(exited(0, "remote.upstream.gh-resolved base\n")),
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
                Ok(exited(0, "remote.origin.gh-resolved o/parent\n")),
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
            .chain(tokens("git.example.org", false, false))
            .chain(tokens("ghe.example.com:8443", true, false))
            .chain([(GH_RESOLVED.to_owned(), Ok(exited(1, "")))]),
        );

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("ghe.example.com:8443", "o", "r")),
        );
    }

    #[test]
    fn rejects_several_github_remotes_without_a_default() {
        let script = Script::new([
            remotes(&[
                ("origin", "git@github.com:me/r.git"),
                ("upstream", "git@github.com:o/r.git"),
            ]),
            (GH_RESOLVED.to_owned(), Ok(exited(1, ""))),
        ]);

        assert!(matches!(locate(&script, &checkout()), Err(Error::Usage(_)),));
    }

    #[test]
    fn rejects_mixed_github_and_gitlab_remotes() {
        let script = Script::new([remotes(&[
            ("github", "git@github.com:o/r.git"),
            ("gitlab", "git@gitlab.com:g/r.git"),
        ])]);

        assert!(matches!(locate(&script, &checkout()), Err(Error::Usage(_)),));
    }

    #[test]
    fn rejects_a_reference_outside_a_checkout() {
        let script = Script::new([("git remote -v".to_owned(), Ok(exited(128, "")))]);

        assert!(matches!(locate(&script, &checkout()), Err(Error::Usage(_)),));
    }

    #[test]
    fn reports_a_missing_git() {
        let script = Script::new([("git remote -v".to_owned(), missing())]);

        assert!(matches!(
            locate(&script, &checkout()),
            Err(Error::Fatal {
                kind: ErrorKind::CliMissing,
                ..
            }),
        ));
    }

    #[test]
    fn classifies_a_url_host_by_the_cli_holding_its_token() {
        let name = reference::parse_name("https://ghe.example.com/o/r/pull/12").unwrap();
        let Name::Url(reference) = &name else {
            unreachable!()
        };
        let script = Script::new([
            (
                "gh auth token --hostname ghe.example.com".to_owned(),
                Ok(exited(0, "gho_x\n")),
            ),
            (
                "glab config get token --host ghe.example.com".to_owned(),
                missing(),
            ),
        ]);

        assert_eq!(locate(&script, &name), Ok(reference.clone()));
    }

    #[test]
    fn needs_no_token_for_github_com() {
        let name = reference::parse_name("https://github.com/o/r/pull/12").unwrap();

        assert!(locate(&Script::new([]), &name).is_ok());
    }

    #[test]
    fn rejects_a_host_claimed_by_both_clis_or_neither() {
        for (gh, glab) in [(true, true), (false, false)] {
            let name = reference::parse_name("https://forge.example.com/o/r/pull/12").unwrap();
            let script = Script::new(tokens("forge.example.com", gh, glab));

            let Err(Error::Fatal { kind, message }) = locate(&script, &name) else {
                panic!("expected an auth error");
            };

            assert_eq!(kind, ErrorKind::Auth);
            assert!(message.contains("forge.example.com"), "{message}");
        }
    }

    #[test]
    fn asks_about_each_host_once() {
        let script = Script::new(
            [remotes(&[
                ("origin", "https://ghe.example.com/me/r.git"),
                ("upstream", "git@ghe.example.com:o/r.git"),
            ])]
            .into_iter()
            .chain(tokens("ghe.example.com", true, false))
            .chain([(
                GH_RESOLVED.to_owned(),
                Ok(exited(0, "remote.upstream.gh-resolved base\n")),
            )]),
        );

        assert_eq!(
            locate(&script, &checkout()),
            Ok(located("ghe.example.com", "o", "r")),
        );
    }
}
