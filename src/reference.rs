use crate::envelope::Forge;
use crate::forge::TargetKind;

/// The domain every Slack workspace host sits under.
const SLACK_DOMAIN: &str = ".slack.com";
/// The digits after the dot in a Slack `ts`.
const SLACK_TS_MICROS: usize = 6;

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

/// A target on a known host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub forge: Forge,
    pub given: String,
    /// The host, with its port where a GitLab URL names one.
    pub host: String,
    pub locus: Locus,
}

/// Where on its host a reference points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Locus {
    /// A pull request, merge request, or issue in a project.
    Numbered {
        /// What the name asks for; a GitHub issue may turn out to be a pull request.
        kind: TargetKind,
        number: u64,
        /// The project's full path: `owner/repo` on GitHub, every group then the project on
        /// GitLab.
        project: String,
    },
    /// A Slack thread: its channel id and its parent message's `ts`.
    SlackThread { channel: String, ts: String },
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
            "`{given}` is not a GitHub pull request or issue, GitLab merge request or issue, or \
             Slack thread, URL"
        )
    };
    let rest = given
        .strip_prefix("https://")
        .or_else(|| given.strip_prefix("http://"))
        .ok_or_else(invalid)?;
    if let Some(reference) = parse_slack_thread(given, rest)? {
        return Ok(reference);
    }
    if rest.contains(['?', '#']) {
        return Err(format!("`{given}` must not carry a query or fragment"));
    }
    let segments: Vec<_> = rest.split('/').collect();
    let (forge, kind, host, project, number) = match &segments[..] {
        // `groups/` is a reserved GitLab route for group-level pages, never a project.
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
        locus: Locus::Numbered {
            kind,
            number: parse_number(number, given)?,
            project,
        },
    })
}

/// A Slack permalink, `https://<workspace>.slack.com/archives/<channel>/p<ts>`, where a reply's
/// `?thread_ts=<parent ts>&cid=<channel>` names the thread it belongs to; `None` off Slack.
fn parse_slack_thread(given: &str, rest: &str) -> Result<Option<Reference>, String> {
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    if !host.ends_with(SLACK_DOMAIN) {
        return Ok(None);
    }
    let invalid = || format!("`{given}` is not a Slack thread permalink");
    if !is_authority(&host) || host.contains(':') {
        return Err(invalid());
    }
    if path.contains('#') {
        return Err(format!("`{given}` must not carry a fragment"));
    }
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let ["archives", channel, message] = path.split('/').collect::<Vec<_>>()[..] else {
        return Err(invalid());
    };
    let is_channel = !channel.is_empty()
        && channel
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
    if !is_channel {
        return Err(invalid());
    }
    let Some(message_ts) = message.strip_prefix('p').and_then(permalink_ts) else {
        return Err(invalid());
    };
    let mut thread_ts = None;
    let mut cid_seen = false;
    for parameter in query.split('&').filter(|parameter| !parameter.is_empty()) {
        match parameter.split_once('=') {
            Some(("thread_ts", value)) if thread_ts.is_none() && is_ts(value) => {
                thread_ts = Some(value);
            }
            Some(("cid", value)) if !cid_seen && value == channel => cid_seen = true,
            _ => {
                return Err(format!(
                    "`{given}` may carry only a reply's `thread_ts` and its own channel as `cid`"
                ));
            }
        }
    }
    let ts = thread_ts.map_or(message_ts, str::to_owned);
    Ok(Some(Reference {
        forge: Forge::Slack,
        given: given.to_owned(),
        host,
        locus: Locus::SlackThread {
            channel: channel.to_owned(),
            ts,
        },
    }))
}

/// The `ts` a permalink's `p<digits>` spells without its dot, which falls before the last six.
fn permalink_ts(digits: &str) -> Option<String> {
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let split = digits.len().checked_sub(SLACK_TS_MICROS)?;
    let ts = format!("{}.{}", &digits[..split], &digits[split..]);
    is_ts(&ts).then_some(ts)
}

/// A Slack message timestamp: whole seconds, a dot, then six digits.
fn is_ts(ts: &str) -> bool {
    let Some((seconds, micros)) = ts.split_once('.') else {
        return false;
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    digits(seconds)
        && !seconds.starts_with('0')
        && digits(micros)
        && micros.len() == SLACK_TS_MICROS
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
        Forge::Slack => false,
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
                locus: Locus::Numbered {
                    kind: TargetKind::ChangeRequest,
                    number: 12,
                    project: "o/r".to_owned(),
                },
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
                locus: Locus::Numbered {
                    kind: TargetKind::ChangeRequest,
                    number: 12,
                    project: "g/sub/p".to_owned(),
                },
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
                locus: Locus::Numbered {
                    kind: TargetKind::Issue,
                    number: 12,
                    project: "o/r".to_owned(),
                },
            })),
        );
    }

    #[test]
    fn parses_a_gitlab_issue_or_work_item_url_as_an_issue() {
        for given in [
            "https://gitlab.com/g/sub/p/-/issues/12",
            "https://gitlab.com/g/sub/p/-/work_items/12",
        ] {
            assert_eq!(
                parse_name(given),
                Ok(Name::Url(Reference {
                    forge: Forge::GitLab,
                    given: given.to_owned(),
                    host: "gitlab.com".to_owned(),
                    locus: Locus::Numbered {
                        kind: TargetKind::Issue,
                        number: 12,
                        project: "g/sub/p".to_owned(),
                    },
                })),
                "{given}",
            );
        }
    }

    #[test]
    fn parses_a_slack_parent_or_reply_permalink_as_the_parent_thread() {
        let thread = |given: &str| Reference {
            forge: Forge::Slack,
            given: given.to_owned(),
            host: "acme.slack.com".to_owned(),
            locus: Locus::SlackThread {
                channel: "C0A1B2C3D4E".to_owned(),
                ts: "1792500000.000100".to_owned(),
            },
        };
        for given in [
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500000000100",
            "https://Acme.Slack.com/archives/C0A1B2C3D4E/p1792500000000100",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500060000200\
             ?thread_ts=1792500000.000100&cid=C0A1B2C3D4E",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500060000200\
             ?cid=C0A1B2C3D4E&thread_ts=1792500000.000100",
        ] {
            assert_eq!(parse_name(given), Ok(Name::Url(thread(given))), "{given}");
        }
    }

    #[test]
    fn rejects_anything_but_a_slack_thread_permalink() {
        for given in [
            "https://slack.com/archives/C0A1B2C3D4E/p1792500000000100",
            "https://acme.slack.com:443/archives/C0A1B2C3D4E/p1792500000000100",
            "https://acme.slack.com/archives/C0A1B2C3D4E",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500000000100/",
            "https://acme.slack.com/archives/c0a1b2c3d4e/p1792500000000100",
            "https://acme.slack.com/archives/C0A1B2C3D4E/1792500000000100",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p100",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p0792500000000100",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p17925000000001é0",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500000000100#x",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500000000100?x=1",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500060000200?cid=C0OTHER0001",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500060000200\
             ?thread_ts=1792500000000100",
            "https://acme.slack.com/archives/C0A1B2C3D4E/p1792500060000200\
             ?thread_ts=1792500000.000100&thread_ts=1792500060.000200",
        ] {
            assert!(parse_name(given).is_err(), "{given}");
        }
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
            "https://gitlab.com/groups/g/-/work_items/12",
        ] {
            assert!(parse_name(given).is_err(), "{given}");
        }
    }
}
