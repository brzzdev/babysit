use crate::diff::{Detail, Event};
use crate::forge::Account;

const COLOURS: [&str; 3] = ["amber", "green", "red"];

/// Every event kind in v1 with the fields it carries, including kinds no backend reports yet.
const KINDS: [Kind; 15] = [
    ("ci.settled", &[Field::Colour]),
    ("comment.created", &[Field::Actor, Field::Body]),
    ("comment.edited", &[Field::Actor, Field::Body]),
    ("head.pushed", &[]),
    ("merge.behind", &[]),
    ("merge.conflicted", &[]),
    (
        "review.submitted",
        &[Field::Actor, Field::Body, Field::State],
    ),
    ("review.withdrawn", &[Field::Actor, Field::Subject]),
    ("state.closed", &[Field::Actor]),
    ("state.merged", &[Field::Actor]),
    ("state.reopened", &[Field::Actor]),
    ("thread.opened", &[Field::Actor, Field::Body]),
    ("thread.replied", &[Field::Actor, Field::Body]),
    ("thread.resolved", &[Field::Actor]),
    ("thread.unresolved", &[]),
];

const QUOTE: char = '"';

const STATES: [&str; 3] = ["approved", "changes_requested", "commented"];

type Kind = (&'static str, &'static [Field]);

/// A caller's condition on events: the kinds its pattern covers, then terms that must all hold.
#[derive(Clone, Debug)]
pub struct Rule {
    kinds: Vec<Kind>,
    terms: Vec<Term>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Actor,
    Body,
    Colour,
    State,
    Subject,
}

/// One `field=value[,value…]` term, holding when the event's field matches any value.
#[derive(Clone, Debug)]
enum Term {
    Accounts(Field, Vec<Selector>),
    BodyPrefixes(Vec<String>),
    Words(Field, Vec<&'static str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Selector {
    Bot,
    Human,
    Login(String),
    Me,
}

pub fn parse(given: &str) -> Result<Rule, String> {
    let mut words = split(given)?.into_iter().peekable();
    let pattern = words.next_if(|word| !word.contains('='));
    if let Some(pattern) = pattern
        && pattern.contains(QUOTE)
    {
        return Err(outside_a_value(pattern));
    }
    let kinds = kinds(pattern)?;
    let mut terms: Vec<Term> = Vec::new();
    for word in words {
        let term = term(word)?;
        let field = term.field();
        if terms.iter().any(|seen| seen.field() == field) {
            return Err(format!("`{}` appears twice in one rule", field.name()));
        }
        if let Some(pattern) = pattern
            && !kinds.iter().any(|(_, fields)| fields.contains(&field))
        {
            return Err(format!("no `{pattern}` event carries `{}`", field.name()));
        }
        terms.push(term);
    }
    if pattern.is_none() && terms.is_empty() {
        return Err("a rule needs a kind pattern or a `field=value` term".to_owned());
    }
    Ok(Rule { kinds, terms })
}

/// The kinds an exact or `noun.*` pattern covers; every kind when there is no pattern.
fn kinds(pattern: Option<&str>) -> Result<Vec<Kind>, String> {
    let Some(pattern) = pattern else {
        return Ok(KINDS.to_vec());
    };
    let covered: Vec<Kind> = KINDS
        .into_iter()
        .filter(|(kind, _)| match pattern.strip_suffix(".*") {
            Some(noun) => kind.split_once('.').is_some_and(|(seen, _)| seen == noun),
            None => *kind == pattern,
        })
        .collect();
    if covered.is_empty() {
        return Err(format!(
            "`{pattern}` is not an event kind or a `field=value` term"
        ));
    }
    Ok(covered)
}

/// The rule's whitespace-separated words, a double-quoted run staying whole inside its word.
fn split(given: &str) -> Result<Vec<&str>, String> {
    let mut words = Vec::new();
    let mut start = None;
    let mut quoted = false;
    let mut chars = given.char_indices();
    while let Some((at, char)) = chars.next() {
        if quoted {
            match char {
                '\\' => _ = chars.next(),
                QUOTE => quoted = false,
                _ => {}
            }
            continue;
        }
        if char.is_whitespace() {
            if let Some(start) = start.take() {
                words.push(&given[start..at]);
            }
            continue;
        }
        start.get_or_insert(at);
        quoted = char == QUOTE;
    }
    if quoted {
        return Err(format!("`{given}` has an unterminated quote"));
    }
    words.extend(start.map(|start| &given[start..]));
    Ok(words)
}

fn term(word: &str) -> Result<Term, String> {
    if word
        .split_once('=')
        .map_or(word, |(name, _)| name)
        .contains(QUOTE)
    {
        return Err(outside_a_value(word));
    }
    let (name, given) = word.split_once('=').ok_or_else(|| {
        format!("`{word}` is not a `field=value` term; a kind pattern comes first")
    })?;
    let (name, is_prefix) = name
        .strip_suffix('^')
        .map_or((name, false), |name| (name, true));
    let field = match name {
        "actor" => Field::Actor,
        "body" => Field::Body,
        "colour" => Field::Colour,
        "state" => Field::State,
        "subject" => Field::Subject,
        _ => {
            return Err(format!(
                "`{name}` is not a field: use actor, body, colour, state or subject"
            ));
        }
    };
    match (field == Field::Body, is_prefix) {
        (false, true) => return Err(format!("`{name}` takes `=`; only `body` takes `^=`")),
        (true, false) => return Err("`body` takes `^=`, a prefix, not `=`".to_owned()),
        _ => {}
    }
    let values = values(word, given)?;
    let values = values.iter().map(String::as_str);
    Ok(match field {
        Field::Actor | Field::Subject => {
            Term::Accounts(field, values.map(selector).collect::<Result<_, _>>()?)
        }
        Field::Body => Term::BodyPrefixes(values.map(body_prefix).collect::<Result<_, _>>()?),
        Field::Colour => Term::Words(field, words(values, &COLOURS, field)?),
        Field::State => Term::Words(field, words(values, &STATES, field)?),
    })
}

/// A term's comma-separated values, each bare or double-quoted with `\"` and `\\` as escapes.
fn values(word: &str, given: &str) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    let mut rest = given;
    loop {
        let (value, after) = if let Some(quoted) = rest.strip_prefix(QUOTE) {
            unquote(quoted)
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let (value, after) = rest.split_at(end);
            if value.contains(QUOTE) {
                return Err(outside_a_value(word));
            }
            (value.to_owned(), after)
        };
        values.push(value);
        if after.is_empty() {
            return Ok(values);
        }
        rest = after
            .strip_prefix(',')
            .ok_or_else(|| outside_a_value(word))?;
    }
}

/// The quoted value opening `given`, with what follows its closing quote, which `split` ensures.
fn unquote(given: &str) -> (String, &str) {
    let mut value = String::new();
    let mut chars = given.char_indices().peekable();
    while let Some((at, char)) = chars.next() {
        match char {
            QUOTE => return (value, &given[at + 1..]),
            '\\' => value.push(
                chars
                    .next_if(|(_, next)| matches!(*next, QUOTE | '\\'))
                    .map_or(char, |(_, next)| next),
            ),
            _ => value.push(char),
        }
    }
    (value, "")
}

fn outside_a_value(word: &str) -> String {
    format!("`{word}` has a quote outside a value; quote a whole value after `=`")
}

fn body_prefix(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("a body prefix is empty".to_owned());
    }
    Ok(value.to_owned())
}

fn selector(value: &str) -> Result<Selector, String> {
    match value {
        "" => Err("an account value is empty".to_owned()),
        "@bot" => Ok(Selector::Bot),
        "@human" => Ok(Selector::Human),
        "@me" => Ok(Selector::Me),
        _ if value.starts_with('@') => Err(format!(
            "`{value}` is not a selector: use @bot, @human or @me"
        )),
        _ if value.contains('=') => Err(format!("`{value}` is not a login")),
        _ => Ok(Selector::Login(value.to_owned())),
    }
}

fn words<'a>(
    values: impl Iterator<Item = &'a str>,
    known: &[&'static str],
    field: Field,
) -> Result<Vec<&'static str>, String> {
    values
        .map(|value| {
            known
                .iter()
                .copied()
                .find(|word| *word == value)
                .ok_or_else(|| {
                    format!(
                        "`{value}` is not a {}: use {}",
                        field.name(),
                        known.join(", ")
                    )
                })
        })
        .collect()
}

/// Whether `event` wakes the run: it matches at least one `on` rule (or there are none) and no
/// `ignore` rule. `me` is the viewer's account id on the event's forge.
pub fn wakes(on: &[Rule], ignore: &[Rule], event: &Event, me: Option<&str>) -> bool {
    let matches = |rule: &Rule| rule.matches(event, me);
    (on.is_empty() || on.iter().any(matches)) && !ignore.iter().any(matches)
}

/// Whether any of `rules` names `@me`.
pub fn names_me<'a>(rules: impl IntoIterator<Item = &'a Rule>) -> bool {
    rules.into_iter().flat_map(|rule| &rule.terms).any(
        |term| matches!(term, Term::Accounts(_, selectors) if selectors.contains(&Selector::Me)),
    )
}

impl Rule {
    fn matches(&self, event: &Event, me: Option<&str>) -> bool {
        let kind = event.detail.kind();
        self.kinds.iter().any(|(covered, _)| *covered == kind)
            && self.terms.iter().all(|term| term.matches(event, me))
    }
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Self::Actor => "actor",
            Self::Body => "body",
            Self::Colour => "colour",
            Self::State => "state",
            Self::Subject => "subject",
        }
    }
}

impl Term {
    fn field(&self) -> Field {
        match self {
            Self::Accounts(field, _) | Self::Words(field, _) => *field,
            Self::BodyPrefixes(_) => Field::Body,
        }
    }

    fn matches(&self, event: &Event, me: Option<&str>) -> bool {
        match self {
            Self::Accounts(field, selectors) => account(event, *field).is_some_and(|account| {
                selectors
                    .iter()
                    .any(|selector| selector.matches(account, me))
            }),
            Self::BodyPrefixes(prefixes) => body(&event.detail)
                .is_some_and(|body| prefixes.iter().any(|prefix| body.starts_with(prefix))),
            Self::Words(field, words) => {
                word(&event.detail, *field).is_some_and(|word| words.contains(&word.as_str()))
            }
        }
    }
}

impl Selector {
    fn matches(&self, account: &Account, me: Option<&str>) -> bool {
        match self {
            Self::Bot => account.bot == Some(true),
            Self::Human => account.bot == Some(false),
            Self::Login(login) => account.login.eq_ignore_ascii_case(login),
            Self::Me => me.is_some_and(|me| account.id.as_deref() == Some(me)),
        }
    }
}

fn account(event: &Event, field: Field) -> Option<&Account> {
    match field {
        Field::Actor => event.actor.as_ref(),
        Field::Subject => event.subject.as_ref(),
        Field::Body | Field::Colour | Field::State => None,
    }
}

fn body(detail: &Detail) -> Option<&str> {
    match detail {
        Detail::CommentCreated { body, .. }
        | Detail::CommentEdited { body, .. }
        | Detail::ReviewSubmitted { body, .. }
        | Detail::ThreadOpened { body, .. }
        | Detail::ThreadReplied { body, .. } => Some(body),
        _ => None,
    }
}

/// The field's value in its printed form.
fn word(detail: &Detail, field: Field) -> Option<String> {
    let value = match (field, detail) {
        (Field::Colour, Detail::CiSettled { colour, .. }) => serde_json::to_value(colour),
        (Field::State, Detail::ReviewSubmitted { state, .. }) => serde_json::to_value(state),
        _ => return None,
    };
    value.ok()?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{Colour, Position, ReviewState};

    fn event(detail: Detail) -> Event {
        Event {
            detail,
            url: "https://github.com/o/r/pull/1".to_owned(),
            at: None,
            actor: None,
            subject: None,
        }
    }

    fn created() -> Event {
        event(Detail::CommentCreated {
            comment_id: "1".to_owned(),
            body: "hi".to_owned(),
            reply_to: None,
        })
    }

    fn resolved() -> Event {
        event(Detail::ThreadResolved {
            thread_id: "PRRT_1".to_owned(),
            reply_to: "10".to_owned(),
        })
    }

    fn settled(colour: Colour) -> Event {
        event(Detail::CiSettled {
            colour,
            head_sha: "abc123".to_owned(),
            checks: Vec::new(),
            warnings: Vec::new(),
        })
    }

    fn rules(given: &[&str]) -> Vec<Rule> {
        given.iter().map(|rule| parse(rule).unwrap()).collect()
    }

    fn woken(on: &[&str], ignore: &[&str], event: &Event) -> bool {
        wakes(&rules(on), &rules(ignore), event, None)
    }

    #[test]
    fn every_reported_kind_is_named_as_printed_and_known_to_rules() {
        let thread_id = || "PRRT_1".to_owned();
        let reply_to = || "10".to_owned();
        for detail in [
            settled(Colour::Red).detail,
            created().detail,
            Detail::CommentEdited {
                comment_id: "1".to_owned(),
                body: String::new(),
                thread_id: None,
                reply_to: None,
            },
            Detail::HeadPushed {
                old_sha: "a".to_owned(),
                new_sha: "b".to_owned(),
            },
            Detail::MergeBehind,
            Detail::MergeConflicted,
            submitted(ReviewState::Approved).detail,
            Detail::ReviewWithdrawn {
                review_id: Some("500".to_owned()),
            },
            Detail::StateClosed,
            Detail::StateMerged,
            Detail::StateReopened,
            Detail::ThreadOpened {
                thread_id: thread_id(),
                comment_id: "10".to_owned(),
                reply_to: reply_to(),
                body: String::new(),
                position: Some(Position {
                    line: None,
                    path: "src/run.rs".to_owned(),
                }),
                review_id: None,
            },
            Detail::ThreadReplied {
                thread_id: thread_id(),
                comment_id: "11".to_owned(),
                reply_to: reply_to(),
                body: String::new(),
                review_id: None,
            },
            resolved().detail,
            Detail::ThreadUnresolved {
                thread_id: thread_id(),
                reply_to: reply_to(),
            },
        ] {
            let printed = serde_json::to_value(&detail).unwrap()["kind"].clone();
            assert_eq!(printed, detail.kind());
            assert!(
                KINDS.iter().any(|(kind, _)| *kind == detail.kind()),
                "{printed}"
            );
        }
    }

    #[test]
    fn every_event_wakes_without_rules() {
        assert!(woken(&[], &[], &created()));
    }

    #[test]
    fn kind_patterns_match_exactly_or_by_whole_noun() {
        assert!(woken(&["comment.created"], &[], &created()));
        assert!(woken(&["comment.*"], &[], &created()));
        assert!(!woken(&["comment.edited"], &[], &created()));
        assert!(!woken(&["thread.*"], &[], &created()));
        assert!(woken(&["thread.*", "comment.created"], &[], &resolved()));
    }

    fn account(login: &str, bot: Option<bool>) -> Account {
        Account {
            bot,
            id: None,
            login: login.to_owned(),
        }
    }

    fn by(actor: Account, event: Event) -> Event {
        Event {
            actor: Some(actor),
            ..event
        }
    }

    fn submitted(state: ReviewState) -> Event {
        event(Detail::ReviewSubmitted {
            review_id: Some("500".to_owned()),
            state,
            body: String::new(),
        })
    }

    #[test]
    fn logins_match_case_insensitively_with_commas_as_or() {
        let gitar = by(account("gitar-bot[bot]", Some(true)), created());

        assert!(woken(&["actor=Gitar-Bot[bot]"], &[], &gitar));
        assert!(woken(&["actor=octocat,gitar-bot[bot]"], &[], &gitar));
        assert!(!woken(&["actor=gitar-bot"], &[], &gitar));
    }

    #[test]
    fn bot_and_human_follow_the_forges_flag_and_an_unknown_kind_matches_neither() {
        let bot = by(account("gitar-bot[bot]", Some(true)), created());
        let human = by(account("octocat", Some(false)), created());
        let unknown = by(account("ghost", None), created());

        assert!(woken(&["actor=@bot"], &[], &bot));
        assert!(!woken(&["actor=@bot"], &[], &human));
        assert!(woken(&["actor=@human"], &[], &human));
        assert!(!woken(&["actor=@human"], &[], &bot));
        assert!(!woken(&["actor=@bot,@human"], &[], &unknown));
    }

    #[test]
    fn terms_are_anded_and_a_missing_field_never_matches() {
        let approved = by(
            account("octocat", Some(false)),
            submitted(ReviewState::Approved),
        );

        assert!(woken(
            &["review.submitted state=approved actor=@human"],
            &[],
            &approved
        ));
        assert!(!woken(&["state=approved actor=@bot"], &[], &approved));
        assert!(woken(
            &["review.* state=changes_requested,approved"],
            &[],
            &approved
        ));
        assert!(!woken(&["state=commented"], &[], &approved));
        assert!(!woken(&["actor=@human"], &[], &resolved()));
        assert!(!woken(&["subject=@human"], &[], &approved));
        assert!(!woken(&["colour=red"], &[], &approved));
    }

    #[test]
    fn colour_matches_the_settled_colour() {
        assert!(woken(
            &["ci.settled colour=red,amber"],
            &[],
            &settled(Colour::Amber)
        ));
        assert!(!woken(&["colour=red"], &[], &settled(Colour::Green)));
    }

    #[test]
    fn subject_matches_the_withdrawn_reviewer() {
        let withdrawn = Event {
            subject: Some(account("octocat", Some(false))),
            ..by(
                account("hubot", Some(true)),
                event(Detail::ReviewWithdrawn {
                    review_id: Some("500".to_owned()),
                }),
            )
        };

        assert!(woken(&["subject=octocat actor=@bot"], &[], &withdrawn));
        assert!(!woken(&["subject=hubot"], &[], &withdrawn));
    }

    #[test]
    fn me_matches_the_viewers_account_id_not_its_login() {
        let mine = by(
            Account {
                id: Some("U_1".to_owned()),
                ..account("octocat", Some(false))
            },
            created(),
        );
        let me = rules(&["actor=@me"]);

        assert!(wakes(&me, &[], &mine, Some("U_1")));
        assert!(!wakes(&me, &[], &mine, Some("U_2")));
        assert!(!wakes(&me, &[], &mine, None));
        assert!(!wakes(
            &me,
            &[],
            &by(account("octocat", Some(false)), created()),
            Some("U_1")
        ));
    }

    fn replied(body: &str) -> Event {
        event(Detail::ThreadReplied {
            thread_id: "PRRT_1".to_owned(),
            comment_id: "11".to_owned(),
            reply_to: "10".to_owned(),
            body: body.to_owned(),
            review_id: None,
        })
    }

    #[test]
    fn a_body_prefix_tells_apart_two_agents_sharing_an_account() {
        let ignore = rules(&[r#"thread.* actor=@me body^="Paul's Claude:""#]);
        let me = || Account {
            id: Some("U_1".to_owned()),
            ..account("octocat", Some(false))
        };
        let claude = by(me(), replied("Paul's Claude: done"));
        let codex = by(
            me(),
            event(Detail::ThreadOpened {
                thread_id: "PRRT_2".to_owned(),
                comment_id: "20".to_owned(),
                reply_to: "20".to_owned(),
                body: "Paul's Codex: found".to_owned(),
                position: None,
                review_id: None,
            }),
        );

        assert!(!wakes(&[], &ignore, &claude, Some("U_1")));
        assert!(wakes(&[], &ignore, &codex, Some("U_1")));
    }

    #[test]
    fn quoted_values_keep_spaces_commas_and_escapes_and_commas_between_them_are_or() {
        let either = [r#"body^="A","B""#];
        let literal = [r#"body^="a, b \"c\" \\ \d""#];

        assert!(woken(&either, &[], &replied("A1")));
        assert!(woken(&either, &[], &replied("B2")));
        assert!(!woken(&either, &[], &replied("C3")));
        assert!(woken(&literal, &[], &replied(r#"a, b "c" \ \d!"#)));
        assert!(!woken(&literal, &[], &replied("a")));
    }

    #[test]
    fn a_body_prefix_never_matches_an_empty_or_missing_body() {
        assert!(!woken(&["body^=x"], &[], &replied("")));
        assert!(!woken(&["body^=x"], &[], &resolved()));
    }

    #[test]
    fn a_quoted_login_matches_as_a_bare_one() {
        let bot = by(account("some-bot[bot]", Some(true)), created());

        assert!(woken(&[r#"actor="some-bot[bot]""#], &[], &bot));
        assert!(woken(&[r#"actor=octocat,"some-bot[bot]""#], &[], &bot));
    }

    #[test]
    fn only_rules_naming_me_need_the_viewer() {
        assert!(!names_me(&rules(&["actor=@bot,octocat", "colour=red"])));
        assert!(names_me(&rules(&["colour=red", "subject=octocat,@me"])));
    }

    #[test]
    fn accepts_the_specs_examples() {
        for given in [
            "ci.settled colour=red,amber",
            "thread.* actor=@human",
            "review.submitted",
            "review.submitted state=approved actor=gitar-bot[bot]",
            "review.* state=approved",
            "colour=red",
            "subject=@me",
        ] {
            assert!(parse(given).is_ok(), "{given}");
        }
    }

    #[test]
    fn rejects_every_usage_error_naming_the_culprit() {
        for (given, culprit) in [
            ("", "a kind pattern"),
            ("thread.open", "`thread.open`"),
            ("label.*", "`label.*`"),
            ("thread*", "`thread*`"),
            ("*", "`*`"),
            ("thread.* review.submitted", "`review.submitted`"),
            ("author=octocat", "`author`"),
            ("colour=blue", "`blue`"),
            ("state=dismissed", "`dismissed`"),
            ("actor=", "empty"),
            ("actor=octocat,", "empty"),
            ("actor=@admin", "`@admin`"),
            ("actor=a=b", "`a=b`"),
            ("actor=@bot actor=@human", "`actor` appears twice"),
            ("thread.opened colour=red", "`thread.opened`"),
            ("thread.* state=approved", "`thread.*`"),
            ("head.* actor=@me", "`head.*`"),
            ("head.pushed actor=@me", "`head.pushed`"),
            ("merge.behind actor=@me", "`merge.behind`"),
            ("thread.unresolved actor=@human", "`thread.unresolved`"),
            ("ci.settled body^=x", "`ci.settled`"),
            ("thread.resolved body^=x", "`thread.resolved`"),
            ("body=x", "`^=`"),
            ("actor^=octocat", "`actor` takes `=`"),
            ("body^=x body^=y", "`body` appears twice"),
            (r#"body^="""#, "empty"),
            ("body^=", "empty"),
            ("body^=x,", "empty"),
            (r#"body^="x"#, "unterminated"),
            (r#"body^="x\""#, "unterminated"),
            (r#""thread.*" body^=x"#, "`\"thread.*\"`"),
            (r#"thread.* "body"^=x"#, "outside a value"),
            (r#"thread.* "x""#, "outside a value"),
            (r#"body^=x"y""#, "outside a value"),
            (r#"body^="x"y"#, "outside a value"),
        ] {
            let error = parse(given).expect_err(given);
            assert!(error.contains(culprit), "{given}: {error}");
        }
    }

    #[test]
    fn an_ignore_wins_and_drops_only_what_it_matches() {
        assert!(!woken(&["comment.*"], &["comment.created"], &created()));
        assert!(!woken(&[], &["comment.*"], &created()));
        assert!(woken(&[], &["comment.*"], &resolved()));
    }
}
