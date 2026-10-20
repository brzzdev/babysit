use crate::envelope::Forge;
use crate::forge::TargetKind;

/// A target as the caller named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Name {
    /// `#<n>` or `!<n>`, meaning the project the current checkout points at.
    Checkout {
        given: String,
        number: u64,
        prefix: Prefix,
    },
    Url(Reference),
}

/// What a checkout reference's prefix says the number names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prefix {
    /// `!`: a GitLab merge request.
    Bang,
    /// `#`: a GitHub pull request or issue, or a GitLab issue.
    Hash,
}

impl Name {
    pub fn given(&self) -> &str {
        match self {
            Self::Checkout { given, .. } => given,
            Self::Url(reference) => &reference.given,
        }
    }
}

/// A target on a known host and project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub forge: Forge,
    pub given: String,
    /// The host, with its port where a GitLab URL names one.
    pub host: String,
    /// What the name asks for; a GitHub issue may turn out to be a pull request.
    pub kind: TargetKind,
    pub number: u64,
    /// The project's full path: `owner/repo` on GitHub, every group then the project on GitLab.
    pub project: String,
}

pub fn parse_name(given: &str) -> Result<Name, String> {
    let checkout = |prefix, number| {
        Ok(Name::Checkout {
            given: given.to_owned(),
            number: parse_number(number, given)?,
            prefix,
        })
    };
    if let Some(number) = given.strip_prefix('#') {
        return checkout(Prefix::Hash, number);
    }
    if let Some(number) = given.strip_prefix('!') {
        return checkout(Prefix::Bang, number);
    }
    parse(given).map(Name::Url)
}

pub fn parse(given: &str) -> Result<Reference, String> {
    let invalid = || {
        format!(
            "`{given}` is not a GitHub pull request or issue, or GitLab merge request or issue, URL"
        )
    };
    let rest = given
        .strip_prefix("https://")
        .or_else(|| given.strip_prefix("http://"))
        .ok_or_else(invalid)?;
    if rest.contains(['?', '#']) {
        return Err(format!("`{given}` must not carry a query or fragment"));
    }
    let segments: Vec<_> = rest.split('/').collect();
    let (forge, kind, host, project, number) = match &segments[..] {
        // `groups/` is reserved on GitLab: its work items belong to a group, not a project.
        [host, project @ .., "-", "issues" | "work_items", number]
            if project.len() >= 2 && project[0] != "groups" =>
        {
            (Forge::GitLab, TargetKind::Issue, *host, project, *number)
        }
        [host, project @ .., "-", "merge_requests", number] if project.len() >= 2 => (
            Forge::GitLab,
            TargetKind::ChangeRequest,
            *host,
            project,
            *number,
        ),
        [host, project @ .., "issues", number] if project.len() == 2 => {
            (Forge::GitHub, TargetKind::Issue, *host, project, *number)
        }
        [host, project @ .., "pull", number] if project.len() == 2 => (
            Forge::GitHub,
            TargetKind::ChangeRequest,
            *host,
            project,
            *number,
        ),
        _ => return Err(invalid()),
    };
    let project = project.join("/");
    if !is_authority(host) || !is_project_path(forge, &project) {
        return Err(invalid());
    }
    if forge == Forge::GitHub && host.contains(':') {
        return Err(format!(
            "`{given}` names a port, and gh can't reach a GitHub host through one"
        ));
    }
    Ok(Reference {
        forge,
        given: given.to_owned(),
        host: host.to_owned(),
        kind,
        number: parse_number(number, given)?,
        project,
    })
}

/// A decimal number from 1 up, never zero-padded.
fn parse_number(number: &str, given: &str) -> Result<u64, String> {
    let invalid =
        || format!("`{number}` in `{given}` is not an issue, pull or merge request number");
    if number.starts_with('0') || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    number.parse().map_err(|_| invalid())
}

/// Dot-separated labels of letters, digits and inner hyphens, then an optional `:` and a decimal
/// port from 1 to 65535.
pub fn is_authority(authority: &str) -> bool {
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let label_is_valid = |label: &str| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    };
    let port_is_valid = |port: &str| {
        port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port != 0)
    };
    host.split('.').all(label_is_valid) && port.is_none_or(port_is_valid)
}

/// A project's full path on `forge`: `owner/repo` on GitHub, one or more groups then the project
/// on GitLab.
pub fn is_project_path(forge: Forge, path: &str) -> bool {
    let segments = path.split('/').count();
    let counted = match forge {
        Forge::GitHub => segments == 2,
        Forge::GitLab => segments >= 2,
    };
    counted && path.split('/').all(is_name)
}

/// An owner or repository name, never `.` or `..`.
pub fn is_name(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_pull_request_url() {
        assert_eq!(
            parse_name("http://ghe.example.com/o/r/pull/12"),
            Ok(Name::Url(Reference {
                forge: Forge::GitHub,
                given: "http://ghe.example.com/o/r/pull/12".to_owned(),
                host: "ghe.example.com".to_owned(),
                kind: TargetKind::ChangeRequest,
                number: 12,
                project: "o/r".to_owned(),
            })),
        );
    }

    #[test]
    fn parses_a_merge_request_url_keeping_nested_groups_and_the_port() {
        assert_eq!(
            parse_name("https://gitlab.example.com:8443/g/sub/p/-/merge_requests/12"),
            Ok(Name::Url(Reference {
                forge: Forge::GitLab,
                given: "https://gitlab.example.com:8443/g/sub/p/-/merge_requests/12".to_owned(),
                host: "gitlab.example.com:8443".to_owned(),
                kind: TargetKind::ChangeRequest,
                number: 12,
                project: "g/sub/p".to_owned(),
            })),
        );
    }

    #[test]
    fn parses_an_issue_url() {
        assert_eq!(
            parse_name("https://github.com/o/r/issues/12"),
            Ok(Name::Url(Reference {
                forge: Forge::GitHub,
                given: "https://github.com/o/r/issues/12".to_owned(),
                host: "github.com".to_owned(),
                kind: TargetKind::Issue,
                number: 12,
                project: "o/r".to_owned(),
            })),
        );
    }

    #[test]
    fn parses_a_gitlab_issue_url() {
        assert_eq!(
            parse_name("https://gitlab.com/g/sub/p/-/issues/12"),
            Ok(Name::Url(Reference {
                forge: Forge::GitLab,
                given: "https://gitlab.com/g/sub/p/-/issues/12".to_owned(),
                host: "gitlab.com".to_owned(),
                kind: TargetKind::Issue,
                number: 12,
                project: "g/sub/p".to_owned(),
            })),
        );
    }

    #[test]
    fn parses_a_gitlab_work_item_url_as_an_issue() {
        assert_eq!(
            parse_name("https://gitlab.com/g/sub/p/-/work_items/12"),
            Ok(Name::Url(Reference {
                forge: Forge::GitLab,
                given: "https://gitlab.com/g/sub/p/-/work_items/12".to_owned(),
                host: "gitlab.com".to_owned(),
                kind: TargetKind::Issue,
                number: 12,
                project: "g/sub/p".to_owned(),
            })),
        );
    }

    #[test]
    fn rejects_a_port_on_a_github_url() {
        let error = parse_name("https://ghe.example.com:8443/o/r/pull/12").unwrap_err();

        assert!(error.contains("names a port"), "{error}");
    }

    #[test]
    fn parses_checkout_references() {
        assert_eq!(
            parse_name("#12"),
            Ok(Name::Checkout {
                given: "#12".to_owned(),
                number: 12,
                prefix: Prefix::Hash,
            }),
        );
        assert_eq!(
            parse_name("!12"),
            Ok(Name::Checkout {
                given: "!12".to_owned(),
                number: 12,
                prefix: Prefix::Bang,
            }),
        );
    }

    #[test]
    fn accepts_names_with_hyphens_underscores_and_dots() {
        assert!(parse("https://github.com/my-org/my_repo.rs/pull/1").is_ok());
    }

    #[test]
    fn rejects_anything_but_the_exact_url() {
        for given in [
            "12",
            "#",
            "#0",
            "#012",
            "#+12",
            "#12a",
            "##12",
            "!012",
            "!",
            "o/r#12",
            "g/p!12",
            "ftp://github.com/o/r/pull/12",
            "https://github.com/o/r/pull/12/",
            "https://github.com/o/r/pull/12/files",
            "https://github.com/o/r/pull/12?w=1",
            "https://github.com/o/r/pull/12#issuecomment-1",
            "https://github.com/o/r/pull/012",
            "https://github.com/o/r/pull/0",
            "https://github.com/o/r/pull/+12",
            "https://github.com/o/r/pull/",
            "https://github.com//r/pull/12",
            "https://user@github.com/o/r/pull/12",
            "https://bad host/o/r/pull/12",
            "https://github.com:/o/r/pull/12",
            "https://github.com:8a/o/r/pull/12",
            "https://github.com:99999/o/r/pull/12",
            "https://github.com:0/o/r/pull/12",
            "https://./o/r/pull/12",
            "https://-/o/r/pull/12",
            "https://a..b/o/r/pull/12",
            "https://github.com/o /r/pull/12",
            "https://github.com/o/r\t/pull/12",
            "https://github.com/../r/pull/12",
            "https://github.com/o/../pull/12",
            "https://github.com/o/r%20x/pull/12",
            "https://github.com/o/r/s/pull/12",
            "https://github.com/o/r/issues/12/",
            "https://github.com/o/r/issues/012",
            "https://github.com/o/r/issues/12#issuecomment-1",
            "https://gitlab.com/p/-/merge_requests/12",
            "https://gitlab.com/g/p/merge_requests/12",
            "https://gitlab.com/g/p/-/merge_requests/12/diffs",
            "https://gitlab.com/g/p/-/merge_requests/012",
            "https://gitlab.com/g//p/-/merge_requests/12",
            "https://gitlab.com/g/p/-/merge_requests/12#note_1",
            "https://gitlab.com/g/p/-/issues/12/",
            "https://gitlab.com/g/p/-/issues/012",
            "https://gitlab.com/g/p/-/issues/12#note_1",
            "https://gitlab.com/p/-/work_items/12",
            "https://gitlab.com/g/p/work_items/12",
            "https://gitlab.com/g/p/-/work_items/12/",
            "https://gitlab.com/g/p/-/work_items/12/designs",
            "https://gitlab.com/g/p/-/work_items/012",
            "https://gitlab.com/g/p/-/work_items/12#note_1",
            "https://gitlab.com/groups/g/-/work_items/12",
        ] {
            assert!(parse_name(given).is_err(), "{given}");
        }
    }
}
