use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::clock::rfc3339;
use crate::envelope::{ErrorKind, Forge};
use crate::exec::Exec;
use crate::forge::{
    Account, Backend, Comment, Failure, Hints, Poll, Target, TargetKind, TargetState, run_cli,
};
use crate::reference::{Locus, Reference};
use crate::response;

const AUTH_ERRORS: [&str; 7] = [
    "account_inactive",
    "invalid_auth",
    "missing_scope",
    "not_allowed_token_type",
    "not_authed",
    "token_expired",
    "token_revoked",
];
const CLI: &str = "curl";
/// What curl exits with when it can't start a call, as for an option older than it is.
const CURL_FAILED_INIT: i32 = 2;
/// curl's flags for every call. curl reads the token from the environment itself, so it never
/// appears among any process's arguments, and `--disable`, which must come first, keeps a
/// `.curlrc` from adding `verbose` and printing the token to stderr.
const CURL_FLAGS: [&str; 10] = [
    "--disable",
    "--silent",
    "--show-error",
    "--include",
    "--suppress-connect-headers",
    "--get",
    "--variable",
    "%BABYSIT_SLACK_TOKEN",
    "--expand-header",
    "Authorization: Bearer {{BABYSIT_SLACK_TOKEN}}",
];
const ERROR_RATE_LIMITED: &str = "ratelimited";
const HEADER_RETRY_AFTER: &str = "retry-after";
const METHOD_AUTH_TEST: &str = "auth.test";
const METHOD_REPLIES: &str = "conversations.replies";
const NOT_FOUND_ERRORS: [&str; 2] = ["channel_not_found", "thread_not_found"];
/// The most messages `conversations.replies` returns in one page.
const PAGE_LIMIT: &str = "1000";
const STATUS_OK: u16 = 200;
const STATUS_TOO_MANY_REQUESTS: u16 = 429;
/// What Slack leaves of a deleted parent that still has replies.
const SUBTYPE_TOMBSTONE: &str = "tombstone";
const TOKEN_VARIABLE: &str = "BABYSIT_SLACK_TOKEN";

pub struct Slack<'a, E> {
    exec: &'a E,
    /// Whether `BABYSIT_SLACK_TOKEN` holds a token.
    token_set: bool,
}

impl<'a, E: Exec> Slack<'a, E> {
    pub fn new(exec: &'a E) -> Self {
        Self {
            exec,
            token_set: env::var_os(TOKEN_VARIABLE).is_some_and(|token| !token.is_empty()),
        }
    }

    /// The decoded reply to a Web API `method` and its pacing, or the failure.
    fn call<T: DeserializeOwned>(
        &self,
        host: &str,
        method: &str,
        parameters: &[(&str, &str)],
        deadline: Option<SystemTime>,
    ) -> Result<(T, Hints), Failure> {
        if !self.token_set {
            let message = format!(
                "{TOKEN_VARIABLE} is not set; export a Slack user token (xoxp-) with the \
                 channels:history, groups:history, im:history, and mpim:history scopes"
            );
            return Err(fatal(ErrorKind::Auth, &message));
        }
        let parameters: Vec<_> = parameters
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        let url = format!("https://{host}/api/{method}");
        let mut args = CURL_FLAGS.to_vec();
        for parameter in &parameters {
            args.extend(["--data-urlencode", parameter]);
        }
        args.push(&url);
        let output = run_cli(self.exec, Forge::Slack, CLI, &args, deadline)?;
        let reason = output.stderr.trim();
        if output.code == Some(CURL_FAILED_INIT) {
            let message =
                format!("curl couldn't start the call; babysit needs curl 8.3 or later: {reason}");
            return Err(fatal(ErrorKind::CliMissing, &message));
        }
        if output.code != Some(0) {
            return Err(transient(&format!("curl failed: {reason}")));
        }
        let Some(response) = response::parse(&output.stdout) else {
            return Err(transient(&format!("curl printed no HTTP reply: {reason}")));
        };
        let hints = Hints {
            retry_after: response.seconds(HEADER_RETRY_AFTER),
            ..Hints::default()
        };
        if response.status == STATUS_TOO_MANY_REQUESTS {
            return Err(rate_limited(hints));
        }
        if response.status != STATUS_OK {
            return Err(transient(&format!("HTTP {}", response.status)).paced(hints));
        }
        let status: Status = decode(&response.body).map_err(|failure| failure.paced(hints))?;
        if !status.ok {
            return Err(refused(status.error.as_deref().unwrap_or_default(), hints));
        }
        let data = decode(&response.body).map_err(|failure| failure.paced(hints))?;
        Ok((data, hints))
    }
}

impl<E: Exec> Backend for Slack<'_, E> {
    fn resolve(
        &self,
        reference: &Reference,
        _deadline: Option<SystemTime>,
    ) -> Result<(Target, Hints), Failure> {
        let Locus::SlackThread { channel, ts } = &reference.locus else {
            unreachable!("a Slack reference always names a thread");
        };
        let target = Target {
            forge: Forge::Slack,
            forge_id: format!("{channel}/{ts}"),
            host: reference.host.clone(),
            kind: TargetKind::Issue,
            url: permalink(&reference.host, channel, ts),
        };
        Ok((target, Hints::default()))
    }

    /// Fetches the whole thread, page after page; any page failing fails the poll.
    fn poll(
        &self,
        target: &Target,
        deadline: Option<SystemTime>,
    ) -> Result<(Poll, Hints), Failure> {
        let (channel, parent) = target
            .forge_id
            .split_once('/')
            .expect("a Slack target's forge id is its channel and parent ts");
        let mut hints = Hints::default();
        let mut messages = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut parameters = vec![("channel", channel), ("ts", parent), ("limit", PAGE_LIMIT)];
            if !cursor.is_empty() {
                parameters.push(("cursor", &cursor));
            }
            let (page, pacing): (Replies, _) = self
                .call(&target.host, METHOD_REPLIES, &parameters, deadline)
                .map_err(|failure| failure.paced(hints))?;
            hints = hints.merge(pacing);
            // Every page repeats the parent.
            let later_page = !messages.is_empty();
            messages.extend(
                page.messages
                    .into_iter()
                    .filter(|message| !(later_page && message.ts == parent)),
            );
            cursor = page
                .response_metadata
                .and_then(|metadata| metadata.next_cursor)
                .unwrap_or_default();
            if cursor.is_empty() {
                break;
            }
        }
        let comments = messages
            .into_iter()
            .filter(|message| message.subtype.as_deref() != Some(SUBTYPE_TOMBSTONE))
            .map(|message| comment(target, channel, parent, message))
            .collect();
        let poll = Poll {
            ci: None,
            comments,
            head: None,
            merge: None,
            reviews: Vec::new(),
            state: TargetState::Open,
            state_changed_at: None,
            state_changed_by: None,
            threads: Vec::new(),
            url: target.url.clone(),
        };
        Ok((poll, hints))
    }

    fn viewer(&self, host: &str, deadline: Option<SystemTime>) -> Result<(String, Hints), Failure> {
        let (identity, hints): (Identity, _) = self.call(host, METHOD_AUTH_TEST, &[], deadline)?;
        Ok((identity.user_id, hints))
    }
}

/// The thread's parent or a reply as a comment outside any thread, replying under the parent.
fn comment(target: &Target, channel: &str, parent: &str, message: Message) -> Comment {
    let url = if message.ts == parent {
        target.url.clone()
    } else {
        let reply = permalink(&target.host, channel, &message.ts);
        format!("{reply}?thread_ts={parent}&cid={channel}")
    };
    let bot = message.bot_id.is_some();
    let actor = message
        .user
        .or(message.bot_id)
        .map(|id| account(id, Some(bot)));
    let edited = message.edited.unwrap_or_default();
    Comment {
        actor,
        at: time(&message.ts),
        body: message.text,
        edited_at: edited.ts.as_deref().map(time),
        editor: edited.user.map(|id| account(id, None)),
        id: message.ts,
        reply_to: Some(parent.to_owned()),
        review_id: None,
        url,
    }
}

/// A Slack user or integration, by an id that is also its login.
fn account(id: String, bot: Option<bool>) -> Account {
    Account {
        bot,
        id: Some(id.clone()),
        login: id,
    }
}

fn permalink(host: &str, channel: &str, ts: &str) -> String {
    format!("https://{host}/archives/{channel}/p{}", ts.replace('.', ""))
}

/// A message `ts` as RFC 3339, to the second.
fn time(ts: &str) -> String {
    let seconds = ts.split_once('.').map_or(ts, |(seconds, _)| seconds);
    seconds.parse().map_or_else(
        |_| ts.to_owned(),
        |seconds| rfc3339(UNIX_EPOCH + Duration::from_secs(seconds)),
    )
}

/// The failure a Web API `error` names.
fn refused(error: &str, hints: Hints) -> Failure {
    if error == ERROR_RATE_LIMITED {
        return rate_limited(hints);
    }
    if AUTH_ERRORS.contains(&error) {
        let message = format!("Slack refused the token in {TOKEN_VARIABLE}: {error}");
        return fatal(ErrorKind::Auth, &message);
    }
    if NOT_FOUND_ERRORS.contains(&error) {
        let message = format!("not found or not visible to {TOKEN_VARIABLE}'s user: {error}");
        return fatal(ErrorKind::TargetNotFound, &message);
    }
    transient(&format!("Slack answered {error}")).paced(hints)
}

fn rate_limited(hints: Hints) -> Failure {
    Failure::RateLimited {
        forge: Forge::Slack,
        hints,
    }
}

fn decode<T: DeserializeOwned>(body: &str) -> Result<T, Failure> {
    serde_json::from_str(body)
        .map_err(|error| transient(&format!("unreadable reply from Slack: {error}")))
}

fn transient(message: &str) -> Failure {
    Failure::transient(Forge::Slack, message)
}

fn fatal(kind: ErrorKind, message: &str) -> Failure {
    Failure::fatal(Forge::Slack, kind, message)
}

/// Whether a Web API call succeeded, which HTTP 200 alone doesn't say.
#[derive(Deserialize)]
struct Status {
    error: Option<String>,
    ok: bool,
}

#[derive(Deserialize)]
struct Identity {
    user_id: String,
}

#[derive(Deserialize)]
struct Replies {
    messages: Vec<Message>,
    response_metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct Metadata {
    /// Empty, null, or absent on the last page.
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    bot_id: Option<String>,
    edited: Option<Edited>,
    subtype: Option<String>,
    #[serde(default)]
    text: String,
    ts: String,
    user: Option<String>,
}

#[derive(Default, Deserialize)]
struct Edited {
    ts: Option<String>,
    user: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::{fs, process};

    use serde_json::{Value, json};

    use super::*;
    use crate::cli::Args;
    use crate::clock::SystemClock;
    use crate::diff::{self, diff};
    use crate::envelope::{Envelope, Outcome, Summary};
    use crate::exec::{self, Output, Script};
    use crate::forge::{Snapshot, fatal_kind};
    use crate::reference;
    use crate::rule;
    use crate::run::{self, Backends};

    const CURSOR: &str = "bmV4dF90czoxNzkyNTAwMTgwMDAwMzAw";
    const ME: &str = "U0PAUL00001";
    const PERMALINK: &str = "https://brzz.slack.com/archives/C0A1B2C3D4E/p1792500000000100";
    const REPLY: &str = "https://brzz.slack.com/archives/C0A1B2C3D4E/p1792500060000200\
                         ?thread_ts=1792500000.000100&cid=C0A1B2C3D4E";

    type Call = (String, Result<Output, exec::Error>);

    fn slack(exec: &Script) -> Slack<'_, Script> {
        Slack {
            exec,
            token_set: true,
        }
    }

    fn curl(method: &str, parameters: &str) -> String {
        format!(
            "curl --disable --silent --show-error --include --suppress-connect-headers --get \
             --variable %BABYSIT_SLACK_TOKEN --expand-header Authorization: Bearer {{{{BABYSIT_SLACK_TOKEN}}}}{parameters} \
             https://brzz.slack.com/api/{method}"
        )
    }

    fn replies(cursor: &str, stdout: &str) -> Call {
        let cursor = if cursor.is_empty() {
            String::new()
        } else {
            format!(" --data-urlencode cursor={cursor}")
        };
        let parameters = format!(
            " --data-urlencode channel=C0A1B2C3D4E --data-urlencode ts=1792500000.000100 \
             --data-urlencode limit=1000{cursor}"
        );
        let call = curl("conversations.replies", &parameters);
        (call, Ok(Output::exited(0, stdout)))
    }

    fn target_of(url: &str) -> Target {
        let exec = Script::new([]);
        let reference = reference::parse(url).unwrap();
        slack(&exec).resolve(&reference, None).unwrap().0
    }

    fn poll_calls(calls: impl IntoIterator<Item = Call>) -> Result<Poll, Failure> {
        let exec = Script::new(calls);
        slack(&exec)
            .poll(&target_of(PERMALINK), None)
            .map(|(poll, _)| poll)
    }

    fn poll(stdout: &str) -> Poll {
        poll_calls([replies("", stdout)]).unwrap()
    }

    fn events(baseline: &str, next: Poll) -> Vec<diff::Event> {
        diff(&Snapshot::from(poll(baseline)), next).0
    }

    fn printed(events: &[diff::Event]) -> Vec<Value> {
        events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect()
    }

    fn wakes(events: &[diff::Event], on: &[&str], ignore: &[&str]) -> Vec<String> {
        let parse = |rules: &[&str]| -> Vec<_> {
            rules
                .iter()
                .map(|rule| rule::parse(rule).unwrap())
                .collect()
        };
        let (on, ignore) = (parse(on), parse(ignore));
        events
            .iter()
            .filter(|event| rule::wakes(&on, &ignore, event, Some(ME)))
            .map(|event| event.detail.kind().to_owned())
            .collect()
    }

    /// A directory for one test's `--snapshot`, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("babysit-{}-slack-{name}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn snapshot(&self) -> PathBuf {
            self.0.join("snapshot.json")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// `babysit --snapshot <snapshot> --timeout 0 <target>`, the thread answering `stdout`.
    fn run_once(snapshot: &Path, target: &str, stdout: &str) -> (u8, Value) {
        let args = Args {
            ignore: Vec::new(),
            on: Vec::new(),
            targets: vec![reference::parse_name(target).unwrap()],
            snapshot: snapshot.to_owned(),
            interval: Duration::from_secs(30),
            timeout: Some(Duration::ZERO),
        };
        let exec = Script::new([replies("", stdout)]);
        let slack = slack(&exec);
        let backends = Backends {
            github: &slack,
            gitlab: &slack,
            slack: &slack,
        };
        let mut printed = Vec::new();
        let code = run::run(
            &args,
            &backends,
            &Script::new([]),
            &SystemClock,
            &mut printed,
            &mut Vec::new(),
        )
        .unwrap();
        (code, serde_json::from_slice(&printed).unwrap())
    }

    #[test]
    fn a_reply_permalink_then_its_parent_permalink_resume_one_baseline() {
        let scratch = Scratch::new("resume");

        let (first_code, first) = run_once(
            &scratch.snapshot(),
            REPLY,
            include_str!("slack/fixtures/poll.http"),
        );
        let (second_code, second) = run_once(
            &scratch.snapshot(),
            PERMALINK,
            include_str!("slack/fixtures/poll_replied.http"),
        );

        assert_eq!(first_code, 3);
        assert_eq!(
            first,
            json!({
                "outcome": "timed_out",
                "events": [],
                "targets": [{ "target": PERMALINK, "state": "open" }],
                "error": null,
            }),
        );
        assert_eq!(second_code, 0);
        let created: Vec<_> = second["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| (event["kind"].clone(), event["comment_id"].clone()))
            .collect();
        assert_eq!(
            created,
            [
                (json!("comment.created"), json!("1792500180.000300")),
                (json!("comment.created"), json!("1792500240.000400")),
            ],
        );
    }

    #[test]
    fn a_reply_permalink_and_its_parent_permalink_are_one_target() {
        assert_eq!(target_of(REPLY), target_of(PERMALINK));
        assert_eq!(target_of(REPLY).url, PERMALINK);
        assert_ne!(
            target_of("https://brzz.slack.com/archives/C0OTHER0001/p1792500000000100"),
            target_of(PERMALINK),
        );
    }

    #[test]
    fn a_first_poll_summarises_the_thread_as_an_open_issue() {
        let target = target_of(PERMALINK);
        let envelope = Envelope {
            targets: vec![Summary::new(
                &target,
                &poll(include_str!("slack/fixtures/poll.http")).into(),
            )],
            ..Envelope::ended(Outcome::TimedOut)
        };

        assert_eq!(
            serde_json::to_value(&envelope).unwrap(),
            json!({
                "outcome": "timed_out",
                "events": [],
                "targets": [{ "target": PERMALINK, "state": "open" }],
                "error": null,
            }),
        );
    }

    #[test]
    fn a_poll_passes_curl_the_token_by_name_only() {
        let call = "curl --disable --silent --show-error --include --suppress-connect-headers --get \
                    --variable %BABYSIT_SLACK_TOKEN \
                    --expand-header Authorization: Bearer {{BABYSIT_SLACK_TOKEN}} \
                    --data-urlencode channel=C0A1B2C3D4E --data-urlencode ts=1792500000.000100 \
                    --data-urlencode limit=1000 https://brzz.slack.com/api/conversations.replies";
        let stdout = include_str!("slack/fixtures/poll.http");

        assert!(poll_calls([(call.to_owned(), Ok(Output::exited(0, stdout)))]).is_ok());
    }

    #[test]
    fn a_new_reply_is_a_comment_created_replying_under_the_parent() {
        let replied = poll(include_str!("slack/fixtures/poll_replied.http"));

        let events = events(include_str!("slack/fixtures/poll.http"), replied);

        assert_eq!(
            printed(&events)[0],
            json!({
                "kind": "comment.created",
                "comment_id": "1792500180.000300",
                "body": "Left two comments",
                "reply_to": "1792500000.000100",
                "url": "https://brzz.slack.com/archives/C0A1B2C3D4E/p1792500180000300\
                        ?thread_ts=1792500000.000100&cid=C0A1B2C3D4E",
                "at": "2026-10-20T12:43:00Z",
                "actor": { "bot": false, "login": "U0TEAMMATE1" },
            }),
        );
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn editing_the_parent_or_a_reply_is_a_comment_edited_by_its_editor() {
        let edited = poll(include_str!("slack/fixtures/poll_edited.http"));

        let events = events(include_str!("slack/fixtures/poll.http"), edited);

        assert_eq!(
            printed(&events),
            [
                json!({
                    "kind": "comment.edited",
                    "comment_id": "1792500000.000100",
                    "body": "Can someone review babysit#64? It's small.",
                    "reply_to": "1792500000.000100",
                    "url": PERMALINK,
                    "at": "2026-10-20T12:45:00Z",
                    "actor": { "bot": null, "login": "U0PAUL00001" },
                }),
                json!({
                    "kind": "comment.edited",
                    "comment_id": "1792500060.000200",
                    "body": "Looking now, give me ten minutes",
                    "reply_to": "1792500000.000100",
                    "url": "https://brzz.slack.com/archives/C0A1B2C3D4E/p1792500060000200\
                            ?thread_ts=1792500000.000100&cid=C0A1B2C3D4E",
                    "at": "2026-10-20T12:45:20Z",
                    "actor": { "bot": null, "login": "U0TEAMMATE1" },
                }),
            ],
        );
    }

    #[test]
    fn ignoring_me_drops_my_own_reply_and_edit() {
        let exec = Script::new([(
            curl("auth.test", ""),
            Ok(Output::exited(
                0,
                include_str!("slack/fixtures/viewer.http"),
            )),
        )]);
        let mine = poll(include_str!("slack/fixtures/poll_edited_by_me.http"));

        let events = events(include_str!("slack/fixtures/poll.http"), mine);

        assert_eq!(slack(&exec).viewer("brzz.slack.com", None).unwrap().0, ME);
        assert_eq!(
            wakes(&events, &[], &[]),
            ["comment.created", "comment.edited"]
        );
        assert_eq!(wakes(&events, &[], &["actor=@me"]), [""; 0]);
    }

    #[test]
    fn an_integration_with_no_user_wakes_on_bot() {
        let posted = poll(include_str!("slack/fixtures/poll_bot.http"));

        let events = events(include_str!("slack/fixtures/poll.http"), posted);

        assert_eq!(
            events[0].actor,
            Some(Account {
                bot: Some(true),
                id: Some("B0DEPLOY001".to_owned()),
                login: "B0DEPLOY001".to_owned(),
            }),
        );
        assert_eq!(wakes(&events, &["actor=@bot"], &[]), ["comment.created"]);
        assert_eq!(wakes(&events, &["actor=@human"], &[]), [""; 0]);
    }

    #[test]
    fn a_thread_over_two_pages_reports_each_new_reply_once() {
        let paged = poll_calls([
            replies("", include_str!("slack/fixtures/poll_page_1.http")),
            replies(CURSOR, include_str!("slack/fixtures/poll_page_2.http")),
        ])
        .unwrap();
        let comments = paged.comments.len();

        let events = events(include_str!("slack/fixtures/poll.http"), paged);

        let ids: Vec<_> = printed(&events)
            .into_iter()
            .map(|event| event["comment_id"].clone())
            .collect();
        assert_eq!(
            ids,
            [json!("1792500180.000300"), json!("1792500240.000400")]
        );
        assert_eq!(comments, 4);
    }

    #[test]
    fn a_last_page_may_end_with_a_null_or_absent_cursor() {
        let last = include_str!("slack/fixtures/poll_page_2.http");
        let null_cursor = last.replace(r#""next_cursor":"""#, r#""next_cursor":null"#);
        let no_metadata = last.replace(r#","response_metadata":{"next_cursor":""}"#, "");

        for (case, last_page) in [("null cursor", null_cursor), ("no metadata", no_metadata)] {
            assert_ne!(last_page, last, "{case}");
            let paged = poll_calls([
                replies("", include_str!("slack/fixtures/poll_page_1.http")),
                replies(CURSOR, &last_page),
            ]);

            assert_eq!(
                paged.map(|poll| poll.comments.len()).ok(),
                Some(4),
                "{case}"
            );
        }
    }

    #[test]
    fn a_page_that_fails_fails_the_whole_poll_for_a_retry() {
        let failure = poll_calls([
            replies("", include_str!("slack/fixtures/poll_page_1.http")),
            replies(
                CURSOR,
                include_str!("slack/fixtures/poll_invalid_cursor.http"),
            ),
        ])
        .unwrap_err();

        assert!(matches!(failure, Failure::Transient { .. }), "{failure:?}");
    }

    #[test]
    fn a_curl_too_old_for_variable_is_cli_missing() {
        let exec = Script::new([(
            replies("", "").0,
            Ok(Output {
                code: Some(2),
                stderr: "curl: option --variable: is unknown".to_owned(),
                stdout: String::new(),
            }),
        )]);

        let failure = slack(&exec).poll(&target_of(PERMALINK), None).unwrap_err();

        assert_eq!(fatal_kind(&failure), ErrorKind::CliMissing);
    }

    #[test]
    fn a_deleted_message_reports_nothing() {
        let deleted = poll(include_str!("slack/fixtures/poll_deleted.http"));
        let replied = poll(include_str!("slack/fixtures/poll_replied.http"));

        assert_eq!(diff(&Snapshot::from(replied), deleted).0, []);
    }

    #[test]
    fn a_429_or_a_ratelimited_error_waits_out_retry_after() {
        let ratelimited = include_str!("slack/fixtures/poll_thread_not_found.http")
            .replace("thread_not_found", "ratelimited")
            .replace("x-slack-failure", "retry-after: 30\r\nx-slack-failure");

        for stdout in [
            include_str!("slack/fixtures/poll_rate_limited.http"),
            &ratelimited,
        ] {
            let failure = poll_calls([replies("", stdout)]).unwrap_err();

            let Failure::RateLimited { hints, .. } = failure else {
                panic!("not a rate limit: {failure:?}");
            };
            assert_eq!(hints.retry_after, Some(Duration::from_secs(30)));
        }
    }

    #[test]
    fn a_refused_token_is_auth_and_a_missing_thread_is_not_found() {
        let refused = include_str!("slack/fixtures/poll_invalid_auth.http");
        let missing = include_str!("slack/fixtures/poll_thread_not_found.http");

        let no_channel = missing.replace("thread_not_found", "channel_not_found");

        let refused = poll_calls([replies("", refused)]).unwrap_err();
        let missing = poll_calls([replies("", missing)]).unwrap_err();
        let no_channel = poll_calls([replies("", &no_channel)]).unwrap_err();

        assert_eq!(fatal_kind(&refused), ErrorKind::Auth);
        assert_eq!(fatal_kind(&missing), ErrorKind::TargetNotFound);
        assert_eq!(fatal_kind(&no_channel), ErrorKind::TargetNotFound);
    }

    #[test]
    fn without_a_token_the_run_ends_naming_the_variable_and_runs_no_curl() {
        let exec = Script::new([]);
        let unset = Slack {
            exec: &exec,
            token_set: false,
        };

        let failure = unset.poll(&target_of(PERMALINK), None).unwrap_err();

        let envelope = serde_json::to_value(Envelope::from_failure(failure, PERMALINK)).unwrap();
        assert_eq!(envelope["outcome"], "error");
        assert_eq!(envelope["error"]["kind"], "auth");
        assert_eq!(envelope["error"]["forge"], "slack");
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(message.contains("BABYSIT_SLACK_TOKEN"), "{message}");
    }
}
