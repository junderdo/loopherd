//! Telling the human, outside the shepherd's pane, that something waits on
//! them: a `question` report or a `blocked` member, per `config.toml`'s
//! `[notify]` table.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::lock::Lock;
use crate::report::{self, Body, Entry};
use crate::shell;
use crate::template::Template;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Question,
    Blocked,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Question => "question",
            Kind::Blocked => "blocked",
        }
    }
}

const KEYS: [&str; 4] = ["herd", "member", "kind", "text"];

#[derive(Deserialize)]
struct File {
    notify: Option<toml::Table>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    #[serde(default)]
    enabled: bool,
    kinds: Option<Vec<Kind>>,
    herdr: Option<bool>,
    command: Option<CommandLine>,
}

/// Words split on whitespace, or an exact argument list.
#[derive(Deserialize)]
#[serde(untagged)]
enum CommandLine {
    Line(String),
    Args(Vec<String>),
}

#[derive(Debug)]
pub struct Config {
    kinds: Vec<Kind>,
    herdr: bool,
    command: Vec<String>,
}

/// `config`'s `[notify]` table; `None` when it is missing or not enabled.
/// A table with an unknown key, kind or placeholder is an error, enabled or not.
pub fn load(config: &Path) -> Result<Option<Config>> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", config.display())),
    };
    let file: File = toml::from_str(&text).with_context(|| config.display().to_string())?;
    let Some(table) = file.notify else {
        return Ok(None);
    };
    Table::deserialize(table)
        .map_err(anyhow::Error::from)
        .and_then(check)
        .with_context(|| format!("{}: [notify]", config.display()))
}

fn check(table: Table) -> Result<Option<Config>> {
    let given = table.command.is_some();
    let command: Vec<String> = match table.command {
        None => vec![],
        Some(CommandLine::Line(line)) if line.contains(['\'', '"', '\\']) => {
            bail!("command: quotes are not parsed; give arguments as an array")
        }
        Some(CommandLine::Line(line)) => line.split_whitespace().map(str::to_string).collect(),
        Some(CommandLine::Args(args)) => args,
    };
    if given
        && command
            .first()
            .is_none_or(|program| program.trim().is_empty())
    {
        bail!("command is empty");
    }
    for word in &command {
        Template::parse(word)
            .and_then(|t| t.check(KEYS))
            .context("command")?;
    }
    Ok(table.enabled.then(|| Config {
        kinds: table.kinds.unwrap_or(vec![Kind::Question, Kind::Blocked]),
        herdr: table.herdr.unwrap_or(true),
        command,
    }))
}

/// Something waiting on the human that a watch just read.
pub struct Waiting {
    pub herd: Option<String>,
    pub member: String,
    pub kind: Kind,
    /// The question; empty for `blocked`.
    pub text: String,
    /// The question's mailbox time, telling it from the member's others.
    pub at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Sender {
    Herdr,
    Command,
}

/// One notification to run detached: `args[0]` with the rest as its arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    pub sender: Sender,
    /// Whose mailbox gets the note if it fails.
    pub member: String,
    pub args: Vec<String>,
}

/// Hands `launch` herdr's toast and the command for each event `config`
/// covers, skipping a question already marked notified in `mailboxes` and
/// marking it first. A question that can't be marked is skipped; every
/// other launch is tried, and the first failure is returned.
pub fn send(
    config: &Config,
    mailboxes: &Path,
    events: &[Waiting],
    launch: &mut dyn FnMut(Launch) -> Result<()>,
) -> Result<()> {
    let mut failed = None;
    for event in events.iter().filter(|e| config.kinds.contains(&e.kind)) {
        if let Some(at) = &event.at {
            match first_to_notify(mailboxes, &event.member, at) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    failed.get_or_insert(e);
                    continue;
                }
            }
        }
        let text = match event.kind {
            Kind::Question => summary(&event.text),
            Kind::Blocked => Kind::Blocked.as_str().to_string(),
        };
        let herd = event.herd.as_deref().unwrap_or_default();
        let mut launches = vec![];
        if config.herdr {
            let title = match herd {
                "" => event.member.clone(),
                herd => format!("{herd}: {}", event.member),
            };
            // `--body=`, so a text starting with `-` isn't read as a flag.
            let body = format!("--body={text}");
            let toast = ["herdr", "notification", "show", &title, &body];
            let args = toast.iter().chain(&["--sound", "request"]);
            launches.push((Sender::Herdr, args.map(|a| a.to_string()).collect()));
        }
        if !config.command.is_empty() {
            let values = HashMap::from([
                ("herd", herd),
                ("member", event.member.as_str()),
                ("kind", event.kind.as_str()),
                ("text", text.as_str()),
            ]);
            let args = config.command.iter().map(|word| {
                Template::parse(word)
                    .and_then(|t| t.render(&values))
                    .expect("load checked the command's placeholders")
            });
            launches.push((Sender::Command, args.collect()));
        }
        for (sender, args) in launches {
            let member = event.member.clone();
            if let Err(e) = launch(Launch {
                sender,
                member,
                args,
            }) {
                failed.get_or_insert(e);
            }
        }
    }
    failed.map_or(Ok(()), Err)
}

/// How long one notification may run.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `launch` within `timeout`. A failure appends a note to its member's
/// mailbox in `mailboxes`, once until that sender next succeeds.
pub fn run(launch: &Launch, mailboxes: &Path, timeout: Duration, now: SystemTime) -> Result<()> {
    let (program, args) = launch.args.split_first().context("no program")?;
    let mut command = Command::new(program);
    command.args(args);
    let problem = match shell::run_within(&mut command, timeout) {
        Err(e) => Some(format!("could not start {program}: {e}")),
        Ok(None) => Some(format!(
            "timed out after {}",
            humantime::format_duration(timeout)
        )),
        Ok(Some(out)) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let code = out.status.code().map_or("signal".into(), |c| c.to_string());
            Some(match stderr.lines().next().map(str::trim) {
                Some(line) if !line.is_empty() => format!("exit {code}: {}", summary(line)),
                _ => format!("exit {code}"),
            })
        }
        Ok(Some(_)) => None,
    };
    match problem {
        Some(problem) => failed(launch, mailboxes, &problem, now),
        None => match fs::remove_file(marker(mailboxes, launch.sender)) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e).context("clear the failure mark"),
            _ => Ok(()),
        },
    }
}

fn marker(mailboxes: &Path, sender: Sender) -> std::path::PathBuf {
    mailboxes.join(match sender {
        Sender::Herdr => ".notify-herdr.failed",
        Sender::Command => ".notify-command.failed",
    })
}

/// Notes in its member's mailbox that `launch` failed with `problem`, unless
/// its sender failed since it last succeeded.
pub fn failed(launch: &Launch, mailboxes: &Path, problem: &str, now: SystemTime) -> Result<()> {
    let what = match launch.sender {
        Sender::Herdr => "herdr's toast",
        Sender::Command => "notify command",
    };
    let mark = marker(mailboxes, launch.sender);
    fs::create_dir_all(mailboxes).with_context(|| format!("create {}", mailboxes.display()))?;
    match OpenOptions::new().write(true).create_new(true).open(&mark) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(()),
        opened => opened.with_context(|| format!("create {}", mark.display()))?,
    };
    let text =
        format!("{what} failed ({problem}); further failures go unnoted until it next succeeds");
    let noted = report::append(
        mailboxes,
        &launch.member,
        &Entry::new(now, Body::Note { text }),
    );
    if noted.is_err() {
        // Unmarked, so the next failure tries the note again.
        let _ = fs::remove_file(&mark);
    }
    noted
}

/// Marks `member`'s question at `at` notified unless a watch already did.
fn first_to_notify(mailboxes: &Path, member: &str, at: &str) -> Result<bool> {
    fs::create_dir_all(mailboxes).with_context(|| format!("create {}", mailboxes.display()))?;
    let path = mailboxes.join(format!("{member}.notified"));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut marks = Lock::wait(file).with_context(|| format!("lock {}", path.display()))?;
    let mark = format!("question {at}");
    let mut text = String::new();
    marks
        .read_to_string(&mut text)
        .with_context(|| format!("read {}", path.display()))?;
    if text.lines().any(|line| line == mark) {
        return Ok(false);
    }
    writeln!(marks, "{mark}").with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

const MAX_TEXT: usize = 200;

/// The question's first non-blank line, cut to [`MAX_TEXT`] characters, with each word
/// that looks like a token, carries one or follows a key such as
/// `password:` replaced by `[redacted]`, and a URL's userinfo hidden.
/// Best-effort: a secret in a shape it doesn't know gets through.
fn summary(question: &str) -> String {
    let line = question
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let mut after_key = false;
    let words: Vec<String> = line
        .split_whitespace()
        .map(|word| {
            let norm = normalized(word);
            if after_key && FILLERS.contains(&norm.as_str()) {
                return word.to_string();
            }
            let hidden = after_key || secret(word);
            after_key = match after_key {
                true => AUTH_SCHEMES.contains(&norm.as_str()),
                false => names_a_secret(word),
            };
            match hidden {
                true => "[redacted]".to_string(),
                false => without_userinfo(word),
            }
        })
        .collect();
    let line = words.join(" ");
    match line.char_indices().nth(MAX_TEXT) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

const SECRET_KEYS: [&str; 14] = [
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "api_key",
    "apikey",
    "access_key",
    "auth",
    "authorization",
    "bearer",
    "credential",
    "credentials",
    "key",
];
/// Keys too common in plain speech to hide the bare word after them.
const PROSE_KEYS: [&str; 1] = ["key"];
/// Kept between a key and its value: `the password is hunter2`.
const FILLERS: [&str; 5] = ["is", "was", "to", ":", "="];
/// After `Authorization:`, the scheme and the credential both go.
const AUTH_SCHEMES: [&str; 3] = ["bearer", "basic", "digest"];
const TOKEN_PREFIXES: [&str; 11] = [
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "sk-",
    "AKIA",
];

/// Lowercase, with `-` spelled `_`, so `X-Api-Key` reads as `x_api_key`.
fn normalized(word: &str) -> String {
    word.to_ascii_lowercase().replace('-', "_")
}

/// `password:`, `token=`, `--password`, `PASSWORD` or `Bearer`: the next
/// word is the secret.
fn names_a_secret(word: &str) -> bool {
    let flag = word.trim_start_matches('-');
    let lower = normalized(flag);
    let key = match lower.strip_suffix([':', '=']) {
        Some(key) => key,
        None if flag.len() < word.len() => &lower,
        None => {
            return SECRET_KEYS
                .iter()
                .any(|k| *k == lower && !PROSE_KEYS.contains(k));
        }
    };
    SECRET_KEYS.iter().any(|k| key.ends_with(k))
}

/// `word` with the `user:password@` of a URL in it replaced.
fn without_userinfo(word: &str) -> String {
    let Some((scheme, rest)) = word.split_once("://") else {
        return word.to_string();
    };
    let authority = rest.split('/').next().unwrap_or_default();
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://[redacted]@{host}{}", &rest[authority.len()..]),
        None => word.to_string(),
    }
}

fn secret(word: &str) -> bool {
    let lower = normalized(word);
    let carries_value = |sep| {
        SECRET_KEYS.iter().any(|key| {
            lower
                .split_once(&format!("{key}{sep}"))
                .is_some_and(|(_, value)| !value.is_empty())
        })
    };
    if carries_value(':') || carries_value('=') {
        return true;
    }
    let token_char = |c: char| c.is_ascii_alphanumeric() || "_-+=".contains(c);
    word.split(|c| !token_char(c)).any(|part| {
        let prefixed = TOKEN_PREFIXES.iter().any(|p| part.starts_with(p)) && part.len() >= 12;
        let random = part.len() >= 24
            && part.contains(|c: char| c.is_ascii_digit())
            && part.contains(|c: char| c.is_ascii_alphabetic());
        prefixed || random
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(extra: &str) -> Config {
        load_text(&format!("[notify]\nenabled = true\n{extra}"))
            .unwrap()
            .unwrap()
    }

    fn question(member: &str, text: &str, at: &str) -> Waiting {
        Waiting {
            herd: Some("cards".into()),
            member: member.into(),
            kind: Kind::Question,
            text: text.into(),
            at: Some(at.into()),
        }
    }

    fn blocked(member: &str) -> Waiting {
        Waiting {
            herd: Some("cards".into()),
            member: member.into(),
            kind: Kind::Blocked,
            text: String::new(),
            at: None,
        }
    }

    fn sent(config: &Config, mailboxes: &Path, events: &[Waiting]) -> Vec<Launch> {
        let mut launched = vec![];
        send(config, mailboxes, events, &mut |l| {
            launched.push(l);
            Ok(())
        })
        .unwrap();
        launched
    }

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn stub(script: &str) -> Launch {
        Launch {
            sender: Sender::Command,
            member: "lh-3".into(),
            args: args(&["sh", "-c", script, "stub", "Which way?"]),
        }
    }

    fn mailbox(dir: &Path) -> Vec<crate::report::Body> {
        let text = fs::read_to_string(dir.join("lh-3.jsonl")).unwrap_or_default();
        text.lines()
            .map(|l| {
                serde_json::from_str::<crate::report::Entry>(l)
                    .unwrap()
                    .body
            })
            .collect()
    }

    fn note(text: &str) -> crate::report::Body {
        crate::report::Body::Note { text: text.into() }
    }

    const SOON: Duration = Duration::from_secs(10);

    #[test]
    fn a_command_that_succeeds_gets_each_argument_whole_and_leaves_no_note() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let script = format!("printf '%s|' \"$@\" > {}", out.display());

        run(&stub(&script), dir.path(), SOON, SystemTime::now()).unwrap();

        assert_eq!(fs::read_to_string(out).unwrap(), "Which way?|");
        assert!(mailbox(dir.path()).is_empty());
    }

    #[test]
    fn failures_are_noted_once_until_the_sender_next_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let fail = stub("echo nope >&2; exit 3");

        run(&fail, dir.path(), SOON, SystemTime::now()).unwrap();
        run(&fail, dir.path(), SOON, SystemTime::now()).unwrap();
        run(&stub("true"), dir.path(), SOON, SystemTime::now()).unwrap();
        run(&fail, dir.path(), SOON, SystemTime::now()).unwrap();

        let failed = note(
            "notify command failed (exit 3: nope); \
             further failures go unnoted until it next succeeds",
        );
        assert_eq!(mailbox(dir.path()), [failed.clone(), failed]);
    }

    #[test]
    fn a_hung_command_is_killed_at_the_timeout_and_noted() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();

        let hung = stub("sleep 30");
        run(
            &hung,
            dir.path(),
            Duration::from_millis(200),
            SystemTime::now(),
        )
        .unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            mailbox(dir.path()),
            [note(
                "notify command failed (timed out after 200ms); \
                 further failures go unnoted until it next succeeds"
            )]
        );
    }

    #[test]
    fn a_missing_program_is_noted_per_sender() {
        let dir = tempfile::tempdir().unwrap();
        let missing = |sender| Launch {
            sender,
            member: "lh-3".into(),
            args: args(&["/nonexistent/herdr"]),
        };

        run(&missing(Sender::Herdr), dir.path(), SOON, SystemTime::now()).unwrap();
        run(
            &missing(Sender::Command),
            dir.path(),
            SOON,
            SystemTime::now(),
        )
        .unwrap();

        let notes = mailbox(dir.path());
        assert_eq!(notes.len(), 2);
        assert!(format!("{:?}", notes[0]).contains("herdr's toast failed (could not start"));
        assert!(format!("{:?}", notes[1]).contains("notify command failed (could not start"));
    }

    #[test]
    fn a_question_goes_to_herdrs_toast_and_the_command_each_placeholder_one_argument() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("command = 'notify {herd} {member} {kind} {text}'\n");

        let launched = sent(
            &config,
            dir.path(),
            &[question("lh-3", "Merge PR #107?\nIt adds [notify].", "t1")],
        );

        assert_eq!(
            launched,
            [
                Launch {
                    sender: Sender::Herdr,
                    member: "lh-3".into(),
                    args: args(&[
                        "herdr",
                        "notification",
                        "show",
                        "cards: lh-3",
                        "--body=Merge PR #107?",
                        "--sound",
                        "request",
                    ]),
                },
                Launch {
                    sender: Sender::Command,
                    member: "lh-3".into(),
                    args: args(&["notify", "cards", "lh-3", "question", "Merge PR #107?"]),
                },
            ]
        );
    }

    #[test]
    fn a_blocked_member_is_sent_with_blocked_as_its_text() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("herdr = false\ncommand = ['say', '{member} is {text}']\n");

        let launched = sent(&config, dir.path(), &[blocked("lh-4")]);

        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].args, args(&["say", "lh-4 is blocked"]));
    }

    #[test]
    fn a_question_is_sent_once_however_many_watches_read_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("");
        let first = question("lh-3", "Which way?", "t1");

        assert_eq!(sent(&config, dir.path(), &[first]).len(), 1);
        let again = question("lh-3", "Which way?", "t1");
        let next = question("lh-3", "Which way now?", "t2");
        let launched = sent(&config, dir.path(), &[again, next]);

        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].args[4], "--body=Which way now?");
    }

    #[test]
    fn kinds_narrow_what_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("kinds = ['blocked']\n");

        let launched = sent(
            &config,
            dir.path(),
            &[question("lh-3", "Which way?", "t1"), blocked("lh-4")],
        );

        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].args[3], "cards: lh-4");
    }

    #[test]
    fn a_member_in_no_herd_is_titled_by_name_alone() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("command = ['n', '{herd}', '{member}']\n");
        let stray = Waiting {
            herd: None,
            ..blocked("solo")
        };

        let launched = sent(&config, dir.path(), &[stray]);

        assert_eq!(launched[0].args[3], "solo");
        assert_eq!(launched[1].args, args(&["n", "", "solo"]));
    }

    #[test]
    fn the_text_carries_no_token_from_the_question() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("herdr = false\ncommand = ['n', '{text}']\n");
        let text = "Use ghp_abcdefghijklmnopqrstuvwxyz0123456789 or \
                    https://x.example/hook?token=s3cr3t for https://github.com/o/r/pull/107?";

        let launched = sent(&config, dir.path(), &[question("lh-3", text, "t1")]);

        assert_eq!(
            launched[0].args[1],
            "Use [redacted] or [redacted] for https://github.com/o/r/pull/107?"
        );
    }

    #[test]
    fn a_failed_launch_still_marks_the_question_and_sends_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("command = ['n']\n");
        let mut tried = 0;

        let result = send(
            &config,
            dir.path(),
            &[question("lh-3", "Which way?", "t1"), blocked("lh-4")],
            &mut |_| {
                tried += 1;
                anyhow::bail!("no exe")
            },
        );

        assert!(result.is_err());
        assert_eq!(tried, 4);
        assert!(sent(&config, dir.path(), &[question("lh-3", "Which way?", "t1")]).is_empty());
    }

    #[test]
    fn a_question_starting_with_a_dash_stays_the_toasts_body() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("");

        let launched = sent(
            &config,
            dir.path(),
            &[question("lh-3", "-- merge it?", "t1")],
        );

        assert_eq!(launched[0].args[4], "--body=-- merge it?");
    }

    fn text_sent(question_text: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("herdr = false\ncommand = ['n', '{text}']\n");
        let launched = sent(
            &config,
            dir.path(),
            &[question("lh-3", question_text, "t1")],
        );
        launched[0].args[1].clone()
    }

    #[test]
    fn secrets_after_a_key_in_a_url_or_in_an_authorization_header_are_redacted() {
        for (question, text) in [
            ("password: hunter2 ok?", "password: [redacted] ok?"),
            ("token:abc ok?", "[redacted] ok?"),
            ("--password pw2 ok?", "--password [redacted] ok?"),
            ("auth=q ok?", "[redacted] ok?"),
            ("credential: c1 ok?", "credential: [redacted] ok?"),
            ("Bearer xyz ok?", "Bearer [redacted] ok?"),
            (
                "Authorization: Bearer xyz ok?",
                "Authorization: [redacted] [redacted] ok?",
            ),
            (
                "Authorization: Basic dXNlcjpwYXNz ok?",
                "Authorization: [redacted] [redacted] ok?",
            ),
            (
                "clone https://ci:hunter2pass@git.example.com/r.git ok?",
                "clone https://[redacted]@git.example.com/r.git ok?",
            ),
            (
                "use postgres://admin:s3cretpw@db/x ok?",
                "use postgres://[redacted]@db/x ok?",
            ),
            ("x-api-key: abc123def ok?", "x-api-key: [redacted] ok?"),
            ("api-key=abc ok?", "[redacted] ok?"),
            (
                "the password is hunter2 ok?",
                "the password is [redacted] ok?",
            ),
            ("PASSWORD hunter2 ok?", "PASSWORD [redacted] ok?"),
            ("key=AIzaSyA1 ok?", "[redacted] ok?"),
            ("press any key to go on?", "press any key to go on?"),
            (
                "see https://github.com/o/r/pull/107 ok?",
                "see https://github.com/o/r/pull/107 ok?",
            ),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn a_question_that_cannot_be_marked_is_skipped_and_the_rest_are_sent() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("lh-3.notified")).unwrap();
        let config = enabled("herdr = false\ncommand = ['n', '{member}']\n");
        let mut launched = vec![];

        let result = send(
            &config,
            dir.path(),
            &[question("lh-3", "Which way?", "t1"), blocked("lh-4")],
            &mut |l| {
                launched.push(l.args);
                Ok(())
            },
        );

        assert!(format!("{:#}", result.unwrap_err()).contains("lh-3.notified"));
        assert_eq!(launched, [args(&["n", "lh-4"])]);
    }

    #[test]
    fn a_launch_that_never_ran_is_noted_like_a_failed_run() {
        let dir = tempfile::tempdir().unwrap();

        failed(
            &stub("true"),
            dir.path(),
            "could not start loopherd",
            SystemTime::now(),
        )
        .unwrap();
        failed(
            &stub("true"),
            dir.path(),
            "could not start loopherd",
            SystemTime::now(),
        )
        .unwrap();

        assert_eq!(
            mailbox(dir.path()),
            [note(
                "notify command failed (could not start loopherd); \
                 further failures go unnoted until it next succeeds"
            )]
        );
    }

    #[test]
    fn the_text_is_the_first_line_that_isnt_blank() {
        assert_eq!(text_sent("\n  \nWhich way?\nMore."), "Which way?");
    }

    #[test]
    fn a_note_that_cannot_be_written_leaves_the_next_failure_noted() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("lh-3.jsonl")).unwrap();
        let fail = stub("exit 3");

        assert!(run(&fail, dir.path(), SOON, SystemTime::now()).is_err());
        fs::remove_dir(dir.path().join("lh-3.jsonl")).unwrap();
        run(&fail, dir.path(), SOON, SystemTime::now()).unwrap();

        assert_eq!(
            mailbox(dir.path()),
            [note(
                "notify command failed (exit 3); \
                 further failures go unnoted until it next succeeds"
            )]
        );
    }

    fn load_text(text: &str) -> Result<Option<Config>> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        load(&path)
    }

    fn error(text: &str) -> String {
        format!("{:#}", load_text(text).expect_err("an error"))
    }

    #[test]
    fn no_file_no_table_or_enabled_false_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("config.toml")).unwrap().is_none());
        assert!(load_text("context_window = 200000\n").unwrap().is_none());
        assert!(load_text("[notify]\n").unwrap().is_none());
        assert!(
            load_text("[notify]\nenabled = false\ncommand = 'x {text}'\n")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unknown_keys_kinds_and_placeholders_are_errors() {
        assert!(error("[notify]\nenabled = true\nsound = 1\n").contains("sound"));
        assert!(error("[notify]\nenabled = true\nkinds = ['done']\n").contains("done"));
        assert!(error("[notify]\nenabled = true\ncommand = 'x {who}'\n").contains("{who}"));
        assert!(error("[notify]\nenabled = true\ncommand = ''\n").contains("command"));
    }

    #[test]
    fn a_broken_table_is_an_error_even_when_disabled() {
        assert!(error("[notify]\nenabled = false\nkinds = ['nope']\n").contains("nope"));
    }
}
