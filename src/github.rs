use std::io;
use std::time::SystemTime;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::envelope::{ErrorKind, Forge};
use crate::exec::{self, CALL_LIMIT, Exec, Limit};
use crate::forge::{Account, Backend, Comment, Failure, Poll, Target, TargetState};
use crate::reference::Reference;
use crate::response::{self, Response};

const CLI: &str = "gh";
const POLL_QUERY: &str = include_str!("github/poll.graphql");
const EXIT_AUTH: i32 = 4;
const EXIT_NOT_FOUND: i32 = 127;
const STATUS_NOT_FOUND: u16 = 404;

pub struct GitHub<'a, E> {
    exec: &'a E,
}

impl<'a, E: Exec> GitHub<'a, E> {
    pub fn new(exec: &'a E) -> Self {
        Self { exec }
    }

    fn call(&self, args: &[&str], deadline: Option<SystemTime>) -> Result<Response, Failure> {
        let output = match self.exec.run(CLI, args, deadline) {
            Ok(output) => output,
            Err(exec::Error::TimedOut(Limit::Deadline)) => return Err(Failure::TimedOut),
            Err(exec::Error::TimedOut(Limit::CallLimit)) => {
                let message = format!("gh did not answer within {}s", CALL_LIMIT.as_secs());
                return Err(fatal(ErrorKind::PollFailed, &message));
            }
            Err(exec::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Err(fatal(
                    ErrorKind::CliMissing,
                    "gh is not installed or not on PATH",
                ));
            }
            Err(exec::Error::Io(error)) => {
                return Err(fatal(
                    ErrorKind::PollFailed,
                    &format!("could not run gh: {error}"),
                ));
            }
        };
        let reason = output.stderr.trim();
        match output.code {
            Some(EXIT_NOT_FOUND) => return Err(fatal(ErrorKind::CliMissing, reason)),
            Some(EXIT_AUTH) => return Err(fatal(ErrorKind::Auth, reason)),
            _ => {}
        }
        let response = response::parse(&output.stdout).ok_or_else(|| {
            fatal(
                ErrorKind::PollFailed,
                &format!("gh printed no HTTP reply: {reason}"),
            )
        })?;
        if (200..300).contains(&response.status) {
            return Ok(response);
        }
        if response.status == STATUS_NOT_FOUND {
            let message = format!("not found or not visible to gh: {reason}");
            return Err(fatal(ErrorKind::TargetNotFound, &message));
        }
        Err(fatal(
            ErrorKind::PollFailed,
            &format!("HTTP {}: {reason}", response.status),
        ))
    }
}

impl<E: Exec> Backend for GitHub<'_, E> {
    fn resolve(
        &self,
        reference: &Reference,
        deadline: Option<SystemTime>,
    ) -> Result<Target, Failure> {
        let path = format!(
            "repos/{}/{}/pulls/{}",
            reference.owner, reference.repo, reference.number,
        );
        let response = self.call(
            &["api", "-i", "--hostname", &reference.host, &path],
            deadline,
        )?;
        let pull: PullRequest = decode(&response.body)?;
        Ok(Target {
            forge_id: pull.node_id,
            host: reference.host.clone(),
            url: pull.html_url,
        })
    }

    fn poll(&self, target: &Target, deadline: Option<SystemTime>) -> Result<Poll, Failure> {
        let query = format!("query={POLL_QUERY}");
        let id = format!("id={}", target.forge_id);
        let args = [
            "api",
            "-i",
            "--hostname",
            &target.host,
            "graphql",
            "-f",
            &query,
            "-f",
            &id,
        ];
        let response = self.call(&args, deadline)?;
        let reply: Reply = decode(&response.body)?;
        if let Some(error) = reply.errors.first() {
            return Err(fatal(ErrorKind::PollFailed, &error.message));
        }
        let Some(node) = reply.data.and_then(|data| data.node) else {
            return Err(fatal(
                ErrorKind::PollFailed,
                &format!("{} returned no pull request", target.url),
            ));
        };
        Ok(Poll {
            comments: node.comments.nodes.into_iter().map(Comment::from).collect(),
            head: node.head_ref_oid,
            state: node.state.into(),
        })
    }
}

#[derive(Deserialize)]
struct PullRequest {
    html_url: String,
    node_id: String,
}

#[derive(Deserialize)]
struct Reply {
    data: Option<Data>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct Data {
    node: Option<Node>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    comments: Connection<CommentNode>,
    head_ref_oid: String,
    state: State,
}

#[derive(Deserialize)]
struct Connection<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    author: Option<Author>,
    body: String,
    created_at: String,
    database_id: u64,
    url: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum State {
    Closed,
    Merged,
    Open,
}

impl From<CommentNode> for Comment {
    fn from(node: CommentNode) -> Self {
        Self {
            actor: node.author.map(|author| Account {
                bot: None,
                login: author.login,
            }),
            at: node.created_at,
            body: node.body,
            id: node.database_id.to_string(),
            url: node.url,
        }
    }
}

impl From<State> for TargetState {
    fn from(state: State) -> Self {
        match state {
            State::Closed => Self::Closed,
            State::Merged => Self::Merged,
            State::Open => Self::Open,
        }
    }
}

fn decode<T: DeserializeOwned>(body: &str) -> Result<T, Failure> {
    serde_json::from_str(body).map_err(|error| {
        fatal(
            ErrorKind::PollFailed,
            &format!("unreadable reply from gh: {error}"),
        )
    })
}

fn fatal(kind: ErrorKind, message: &str) -> Failure {
    Failure::Fatal {
        forge: Forge::GitHub,
        kind,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, Script};
    use crate::reference;

    const NODE_ID: &str = "PR_kwDOVQ0d8c8AAAABJm7W-Q";
    const RESOLVE: &str = "gh api -i --hostname github.com repos/brzzdev/babysit/pulls/15";

    fn reference() -> Reference {
        reference::parse("https://github.com/brzzdev/babysit/pull/15").unwrap()
    }

    fn target() -> Target {
        Target {
            forge_id: NODE_ID.to_owned(),
            host: "github.com".to_owned(),
            url: "https://github.com/brzzdev/babysit/pull/15".to_owned(),
        }
    }

    fn gh(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            code: Some(code),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    fn resolve_with(result: Result<Output, exec::Error>) -> Result<Target, Failure> {
        let exec = Script::new([(RESOLVE.to_owned(), result)]);
        GitHub::new(&exec).resolve(&reference(), None)
    }

    fn kind(failure: &Failure) -> ErrorKind {
        match failure {
            Failure::Fatal { kind, .. } => *kind,
            Failure::TimedOut => panic!("timed out"),
        }
    }

    #[test]
    fn resolves_a_pull_request_to_its_url_and_node_id() {
        let resolved = resolve_with(Ok(gh(0, include_str!("github/fixtures/resolve.http"), "")));

        assert_eq!(resolved.unwrap(), target());
    }

    #[test]
    fn a_missing_pull_request_is_not_found() {
        let failure = resolve_with(Ok(gh(
            1,
            include_str!("github/fixtures/resolve_not_found.http"),
            "gh: Not Found (HTTP 404)",
        )))
        .unwrap_err();

        assert_eq!(kind(&failure), ErrorKind::TargetNotFound);
    }

    #[test]
    fn polls_state_head_and_comments() {
        let exec = Script::new([(
            format!(
                "gh api -i --hostname github.com graphql -f query={POLL_QUERY} -f id={NODE_ID}"
            ),
            Ok(gh(0, include_str!("github/fixtures/poll.http"), "")),
        )]);

        let poll = GitHub::new(&exec).poll(&target(), None).unwrap();

        assert_eq!(
            poll,
            Poll {
                comments: vec![Comment {
                    actor: Some(Account {
                        bot: None,
                        login: "brzzdev".to_owned(),
                    }),
                    at: "2026-10-20T17:33:24Z".to_owned(),
                    body: "Paul's Codex:\n\n> Review verdict: Approved at \
					       fb5f9a7d7df000a39a0b88667dad43be6b4814ca\n"
                        .to_owned(),
                    id: "6297087190".to_owned(),
                    url: "https://github.com/brzzdev/babysit/pull/15#issuecomment-6297087190"
                        .to_owned(),
                }],
                head: "fb5f9a7d7df000a39a0b88667dad43be6b4814ca".to_owned(),
                state: TargetState::Merged,
            },
        );
    }

    #[test]
    fn a_call_killed_at_the_deadline_times_out() {
        let failure = resolve_with(Err(exec::Error::TimedOut(Limit::Deadline))).unwrap_err();

        assert!(matches!(failure, Failure::TimedOut));
    }

    #[test]
    fn a_missing_gh_is_cli_missing() {
        let failure =
            resolve_with(Err(exec::Error::Io(io::ErrorKind::NotFound.into()))).unwrap_err();

        assert_eq!(kind(&failure), ErrorKind::CliMissing);
    }
}
