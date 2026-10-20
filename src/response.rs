/// One HTTP reply as `gh api -i` prints it: a status line, headers, a blank line, then the body.
pub struct Response {
    pub body: String,
    headers: Vec<(String, String)>,
    pub status: u16,
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The first value of the header `name`, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub fn parse(stdout: &str) -> Option<Response> {
    let (status_line, rest) = stdout.split_once('\n')?;
    let status = status_line.split(' ').nth(1)?.parse().ok()?;
    let mut headers = Vec::new();
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Some(Response {
                body: rest[offset..].to_owned(),
                headers,
                status,
            });
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.to_owned(), value.trim().to_owned()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_status_and_body_past_crlf_headers() {
        let response = parse(include_str!("github/fixtures/resolve_not_found.http")).unwrap();

        assert_eq!(response.status, 404);
        assert_eq!(response.header("x-ratelimit-remaining"), Some("4989"));
        assert!(response.body.starts_with(r#"{"message":"Not Found""#));
    }

    #[test]
    fn rejects_stdout_without_a_header_terminator() {
        assert!(parse("HTTP/2.0 200 OK\nContent-Type: application/json\r\n").is_none());
        assert!(parse("gh: Not Found").is_none());
    }
}
