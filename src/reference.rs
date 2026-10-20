/// A target as the caller named it: a GitHub pull request web URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub given: String,
    /// The host, port included.
    pub host: String,
    pub number: u64,
    pub owner: String,
    pub repo: String,
}

pub fn parse(given: &str) -> Result<Reference, String> {
    let invalid = || format!("`{given}` is not a GitHub pull request URL");
    let rest = given
        .strip_prefix("https://")
        .or_else(|| given.strip_prefix("http://"))
        .ok_or_else(invalid)?;
    if rest.contains(['?', '#']) {
        return Err(format!("`{given}` must not carry a query or fragment"));
    }
    let [host, owner, repo, "pull", number] = rest.split('/').collect::<Vec<_>>()[..] else {
        return Err(invalid());
    };
    if !is_authority(host) || !is_name(owner) || !is_name(repo) {
        return Err(invalid());
    }
    if number.starts_with('0') || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "`{number}` in `{given}` is not a pull request number"
        ));
    }
    Ok(Reference {
        given: given.to_owned(),
        host: host.to_owned(),
        number: number.parse().map_err(|_| invalid())?,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

/// Dot-separated labels of letters, digits and inner hyphens, then an optional `:` and a decimal
/// port from 1 to 65535.
fn is_authority(authority: &str) -> bool {
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

/// An owner or repository name, never `.` or `..`.
fn is_name(segment: &str) -> bool {
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
            parse("http://ghe.example.com:8443/o/r/pull/12"),
            Ok(Reference {
                given: "http://ghe.example.com:8443/o/r/pull/12".to_owned(),
                host: "ghe.example.com:8443".to_owned(),
                number: 12,
                owner: "o".to_owned(),
                repo: "r".to_owned(),
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
            "#12",
            "o/r#12",
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
        ] {
            assert!(parse(given).is_err(), "{given}");
        }
    }
}
