/// A target as the caller named it: a GitHub pull request web URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub given: String,
    /// The host, port included.
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
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
    if [host, owner, repo].contains(&"") || host.contains('@') {
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
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number: number.parse().map_err(|_| invalid())?,
    })
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
                owner: "o".to_owned(),
                repo: "r".to_owned(),
                number: 12,
            }),
        );
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
        ] {
            assert!(parse(given).is_err(), "{given}");
        }
    }
}
