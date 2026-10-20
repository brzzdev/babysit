/// One HTTP reply as `gh api -i` prints it: a status line, headers, a blank line, then the body.
pub struct Response {
    pub status: u16,
    pub body: String,
}

pub fn parse(stdout: &str) -> Option<Response> {
    let (status_line, rest) = stdout.split_once('\n')?;
    let status = status_line.split(' ').nth(1)?.parse().ok()?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            return Some(Response {
                status,
                body: rest[offset..].to_owned(),
            });
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
        assert!(response.body.starts_with(r#"{"message":"Not Found""#));
    }

    #[test]
    fn rejects_stdout_without_a_header_terminator() {
        assert!(parse("HTTP/2.0 200 OK\nContent-Type: application/json\r\n").is_none());
        assert!(parse("gh: Not Found").is_none());
    }
}
