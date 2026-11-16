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

impl Config {
    /// Enabled with herdr's toast off and no command.
    pub fn sends_nothing(&self) -> bool {
        !self.herdr && self.command.is_empty()
    }
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
        // Before the mark, so a text that can't be built never leaves a question marked.
        let text = match event.kind {
            Kind::Question => summary(&event.text),
            Kind::Blocked => Kind::Blocked.as_str().to_string(),
        };
        let marked = match &event.at {
            Some(at) => first_to_notify(mailboxes, &event.member, at),
            None => first_to_notify_blocked(mailboxes, &event.member),
        };
        match marked {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                failed.get_or_insert(e);
                continue;
            }
        }
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

/// Marks `member` notified as blocked unless a watch already did since it
/// was last seen unblocked.
fn first_to_notify_blocked(mailboxes: &Path, member: &str) -> Result<bool> {
    fs::create_dir_all(mailboxes).with_context(|| format!("create {}", mailboxes.display()))?;
    let path = mailboxes.join(format!("{member}.blocked"));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
    }
}

/// The `<member>.blocked` marks in a mailboxes directory as they stood
/// before a watch read statuses.
pub struct BlockMarks(HashMap<String, (u64, SystemTime)>);

impl BlockMarks {
    pub fn read(mailboxes: &Path) -> Result<BlockMarks> {
        let mut marks = HashMap::new();
        let entries = match fs::read_dir(mailboxes) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BlockMarks(marks)),
            entries => entries.with_context(|| format!("read {}", mailboxes.display()))?,
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("read {}", mailboxes.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(member) = name.strip_suffix(".blocked") else {
                continue;
            };
            if let Some(id) = mark_id(&entry.path())? {
                marks.insert(member.to_string(), id);
            }
        }
        Ok(BlockMarks(marks))
    }

    /// Clears `member`'s blocked mark, as a watch saw it not blocked, so its
    /// next block is sent. A mark made since [`BlockMarks::read`] stays: it
    /// is for a block that status read predates.
    pub fn unblocked(&self, mailboxes: &Path, member: &str) -> Result<()> {
        let path = mailboxes.join(format!("{member}.blocked"));
        let Some(seen) = self.0.get(member) else {
            return Ok(());
        };
        if mark_id(&path)?.as_ref() != Some(seen) {
            return Ok(());
        }
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(e).with_context(|| format!("remove {}", path.display()))
            }
            _ => Ok(()),
        }
    }
}

/// A mark's inode and mtime, so a mark removed and made again reads as new.
fn mark_id(path: &Path) -> Result<Option<(u64, SystemTime)>> {
    use std::os::unix::fs::MetadataExt;
    match fs::metadata(path) {
        Ok(meta) => Ok(Some((
            meta.ino(),
            meta.modified()
                .with_context(|| format!("read {}'s mtime", path.display()))?,
        ))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
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
/// that looks like a token, carries one (`token=abc`) or is a key's value replaced by
/// `[redacted]`, and a URL's userinfo hidden. A key with a separator (`password:`,
/// `password =`, `--password`) hides its value outright, to the end of its clause; any
/// other mention of a key hides the next word only if it [looks like a
/// secret](looks_like_a_secret), so a merge question naming `GITHUB_TOKEN` or "the auth
/// flow" survives. Best-effort: a secret in a shape it doesn't know gets through.
fn summary(question: &str) -> String {
    let first = question
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let mut next = Next::Free;
    let mut line = String::new();
    for word in first.split_whitespace() {
        let shown;
        (shown, next) = match next {
            Next::Clause => match clause_end(word) {
                Some(end) => (Shown::Glued(end), Next::Free),
                None => (Shown::Glued(""), Next::Clause),
            },
            Next::ClauseStart => clause_start(word),
            Next::Closing(closer) => (Shown::redacted(), Next::closed_by(word, closer)),
            Next::Value if is_separator(word) => (Shown::Word(word.to_string()), Next::ClauseStart),
            Next::Value if FILLERS.contains(&normalized(word).as_str()) => {
                (Shown::Word(word.to_string()), next)
            }
            Next::Value => (Shown::redacted(), Next::after_value(word)),
            Next::Key(key) if is_separator(word) => (Shown::Word(word.to_string()), key.value()),
            Next::Key(_) if FILLERS.contains(&normalized(word).as_str()) => {
                (Shown::Word(word.to_string()), next)
            }
            Next::Key(_) | Next::MaybeValue if looks_like_a_secret(word) => {
                (Shown::redacted(), Next::after_value(word))
            }
            Next::Free | Next::Key(_) | Next::MaybeValue => match carried_value(word) {
                Some((true, value)) => clause_start(value),
                Some((false, value)) => (Shown::redacted(), Next::after_value(value)),
                None if token_shaped(word) => (Shown::redacted(), key_named_by(word)),
                None => (Shown::Word(without_userinfo(word)), key_named_by(word)),
            },
        };
        match shown {
            Shown::Word(shown) => {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&shown);
            }
            Shown::Glued(end) => line.push_str(end),
        }
    }
    match line.char_indices().nth(MAX_TEXT) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// A word as [`summary`] shows it: whole, or only a clause's end glued to the
/// `[redacted]` before it.
enum Shown<'a> {
    Word(String),
    Glued(&'a str),
}

impl Shown<'_> {
    fn redacted() -> Self {
        Shown::Word("[redacted]".to_string())
    }
}

/// Trimmed from after a key: `"password":`, `‘token’=`.
const QUOTES: [char; 7] = ['"', '\'', '“', '”', '‘', '’', '»'];
/// Open a quoted or bracketed value, and are trimmed from before a key. `”` and `’`
/// open as well as close, as some languages quote.
const OPENERS: [char; 12] = ['"', '\'', '“', '‘', '”', '’', '„', '‚', '«', '[', '(', '{'];

/// The first word of a separated strong key's value, hidden with the rest of its
/// clause unless it opens a quote or bracket.
fn clause_start(value: &str) -> (Shown<'_>, Next) {
    if value.starts_with(OPENERS) {
        return (Shown::redacted(), Next::after_value(value));
    }
    match clause_end(value) {
        Some(end) => (Shown::Word(format!("[redacted]{end}")), Next::Free),
        None => (Shown::redacted(), Next::Clause),
    }
}

/// The punctuation ending `word`'s clause (`,` `;` `?` `!` `.` or a closing quote
/// or bracket), if it ends one. A cookie's `a=b;` doesn't, so its next pair is hidden.
fn clause_end(word: &str) -> Option<&str> {
    let value = word.trim_end_matches(|c: char| ",;?!.)]}\"'“”‘’»".contains(c));
    let end = &word[value.len()..];
    let cookie_pair = end.starts_with(';') && value.contains('=');
    (!end.is_empty() && !cookie_pair).then_some(end)
}

/// What the next word is to [`summary`].
#[derive(Clone, Copy)]
enum Next {
    Free,
    /// Hidden outright.
    Value,
    /// A separated strong key's value: hidden to the end of its clause.
    ClauseStart,
    /// Inside a [`ClauseStart`](Next::ClauseStart) value, already shown as `[redacted]`.
    Clause,
    /// Hidden if it looks like a secret.
    MaybeValue,
    /// After a key with no separator yet: `password`, `the token is`.
    Key(Key),
    /// Inside a quoted or bracketed value, until the word that closes it.
    Closing(char),
}

#[derive(Clone, Copy, PartialEq)]
enum Key {
    /// `password`, `GITHUB_TOKEN`, `privateKey`.
    Strong,
    /// A short key alone (`key`, `auth`, `session`): `Auth: merge #107 now?`.
    Weak,
}

impl Key {
    /// The next word, once a separator follows this key.
    fn value(self) -> Next {
        match self {
            Key::Strong => Next::ClauseStart,
            Key::Weak => Next::MaybeValue,
        }
    }
}

impl Next {
    /// After a hidden `value`: the rest of an unclosed quote or bracket, or the
    /// next `a=b;` pair.
    fn after_value(value: &str) -> Next {
        let opener = value.chars().next().unwrap_or_default();
        let closer = match opener {
            '"' => Some('"'),
            '\'' => Some('\''),
            '“' => Some('”'),
            '‘' | '’' => Some('’'),
            '”' => Some('”'),
            '„' => Some('“'),
            '‚' => Some('‘'),
            '«' => Some('»'),
            '[' => Some(']'),
            '(' => Some(')'),
            '{' => Some('}'),
            _ => None,
        };
        match closer {
            Some(closer) => Next::closed_by(&value[opener.len_utf8()..], closer),
            None if value.ends_with(';') => Next::Value,
            None => Next::Free,
        }
    }

    /// A closer followed by a letter or digit is an apostrophe (`it’s`), not a close.
    fn closed_by(word: &str, closer: char) -> Next {
        let closes = word.match_indices(closer).any(|(at, _)| {
            !word[at + closer.len_utf8()..].starts_with(|c: char| c.is_alphanumeric())
        });
        match closes {
            true => Next::Free,
            false => Next::Closing(closer),
        }
    }
}

/// Matched at the end of a key, so `accessToken`, `PGPASSWORD` and `x-api-key` count.
const SECRET_KEYS: [&str; 10] = [
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "apikey",
    "authorization",
    "bearer",
    "credential",
    "cookie",
];
/// Matched only whole or after a `_`/`-`/case boundary, so `monkey` and `hotkey` don't.
const SHORT_KEYS: [&str; 4] = ["key", "auth", "pwd", "session"];
/// Keys only whole and right before `:` or `=`, so "pass the test" and "pwd shows" stay.
const PASSWORD_SHORTHANDS: [&str; 3] = ["pwd", "pw", "pass"];
/// Kept between a key and its value: `the password is hunter2`.
const FILLERS: [&str; 3] = ["is", "was", "to"];
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

/// Lowercase, with `-` and a lower-to-upper case change spelled `_`, so
/// `X-Api-Key` and `privateKey` read as `x_api_key` and `private_key`.
fn normalized(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut lower_before = false;
    for c in word.chars() {
        if lower_before && c.is_ascii_uppercase() {
            out.push('_');
        }
        lower_before = c.is_ascii_lowercase();
        out.push(match c {
            '-' => '_',
            c => c.to_ascii_lowercase(),
        });
    }
    out
}

/// `name` as a secret key, if it is one; `separated` when `:` or `=` follows it.
fn secret_key(name: &str, separated: bool) -> Option<Key> {
    let name = normalized(name);
    if PASSWORD_SHORTHANDS.contains(&name.as_str()) {
        return separated.then_some(Key::Strong);
    }
    if SHORT_KEYS.contains(&name.as_str()) {
        return Some(Key::Weak);
    }
    let short = SHORT_KEYS
        .iter()
        .any(|key| name.ends_with(&format!("_{key}")));
    (short || SECRET_KEYS.iter().any(|key| name.ends_with(key))).then_some(Key::Strong)
}

/// `:`, `=`, or a filler carrying one: `password = x`, `the token is: x`.
fn is_separator(word: &str) -> bool {
    word.strip_suffix([':', '='])
        .is_some_and(|rest| rest.is_empty() || FILLERS.contains(&normalized(rest).as_str()))
}

/// What `word` makes the next one: a value after `password:`, `"token":` or
/// `--password`; one after a whole capitalised key (`TOKEN`), read as a label;
/// else a key awaiting its separator.
fn key_named_by(word: &str) -> Next {
    let word = word.trim_start_matches(OPENERS);
    let flag = word.trim_start_matches('-');
    let (name, separated) = match flag.strip_suffix([':', '=']) {
        Some(name) => (name, true),
        None => (flag, false),
    };
    let name = name.trim_end_matches(QUOTES);
    let Some(key) = secret_key(name, separated) else {
        return Next::Free;
    };
    let label = name.chars().all(|c| c.is_ascii_uppercase()) && key == Key::Strong;
    match () {
        _ if separated || flag.len() < word.len() => key.value(),
        _ if label => Next::Value,
        _ => Next::Key(key),
    }
}

/// Six or more characters with a digit, or eight or more with no vowel:
/// `hunter2`, `xkcdqwrtz`, but not `v2`, `#107`, `flow` or `reset`.
fn looks_like_a_secret(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    (word.len() >= 6 && word.contains(|c: char| c.is_ascii_digit()))
        || (word.len() >= 8 && !word.contains(|c: char| "aeiouAEIOU".contains(c)))
}

/// The value in a `key=value` or `key:value` word whose key is a secret key, quoted
/// or not (`token=abc`, `{"token":"abc"}`, `Authorization:Bearer`), and whether it
/// runs to the end of its clause: a strong key starting the word, not in a URL's query.
fn carried_value(word: &str) -> Option<(bool, &str)> {
    word.match_indices([':', '=']).find_map(|(at, _)| {
        let left = word[..at].trim_end_matches(QUOTES);
        let key_start = left
            .char_indices()
            .rev()
            .find(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let value = &word[at + 1..];
        let key = secret_key(&left[key_start..], true)?;
        let starts_word = left[..key_start]
            .trim_start_matches(|c| OPENERS.contains(&c) || c == '-')
            .is_empty();
        (!value.is_empty()).then_some((starts_word && key == Key::Strong, value))
    })
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

/// A known token prefix, or a long run mixing letters and digits.
fn token_shaped(word: &str) -> bool {
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
    fn a_block_marked_after_a_watchs_status_read_outlives_that_watch_seeing_it_unblocked() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("herdr = false\ncommand = ['n']\n");
        let stale = BlockMarks::read(dir.path()).unwrap();
        assert_eq!(sent(&config, dir.path(), &[blocked("lh-4")]).len(), 1);

        stale.unblocked(dir.path(), "lh-4").unwrap();
        assert!(sent(&config, dir.path(), &[blocked("lh-4")]).is_empty());

        let fresh = BlockMarks::read(dir.path()).unwrap();
        fresh.unblocked(dir.path(), "lh-4").unwrap();
        assert_eq!(sent(&config, dir.path(), &[blocked("lh-4")]).len(), 1);
    }

    #[test]
    fn a_mark_made_again_since_the_read_outlives_unblocked() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("herdr = false\ncommand = ['n']\n");
        let mark = dir.path().join("lh-4.blocked");
        sent(&config, dir.path(), &[blocked("lh-4")]);
        let before = BlockMarks::read(dir.path()).unwrap();

        // Made again on top, so it has another inode.
        let again = dir.path().join("lh-4.blocked.new");
        fs::write(&again, "").unwrap();
        fs::rename(&again, &mark).unwrap();
        before.unblocked(dir.path(), "lh-4").unwrap();
        assert!(mark.exists());

        // An inode reused by a mark made again reads as new by its mtime.
        let before = BlockMarks::read(dir.path()).unwrap();
        let later = fs::metadata(&mark).unwrap().modified().unwrap() + Duration::from_secs(5);
        fs::File::options()
            .write(true)
            .open(&mark)
            .unwrap()
            .set_modified(later)
            .unwrap();
        before.unblocked(dir.path(), "lh-4").unwrap();
        assert!(mark.exists());
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
            ("password: hunter2 ok?", "password: [redacted]?"),
            ("token:abc ok?", "[redacted]?"),
            ("--password pw2 ok?", "--password [redacted]?"),
            ("auth=q ok?", "[redacted] ok?"),
            ("credential: c1 ok?", "credential: [redacted]?"),
            ("Bearer abc123def ok?", "Bearer [redacted] ok?"),
            (
                "Authorization: Bearer xyz ok?",
                "Authorization: [redacted]?",
            ),
            (
                "Authorization: Basic dXNlcjpwYXNz ok?",
                "Authorization: [redacted]?",
            ),
            (
                "clone https://ci:hunter2pass@git.example.com/r.git ok?",
                "clone https://[redacted]@git.example.com/r.git ok?",
            ),
            (
                "use postgres://admin:s3cretpw@db/x ok?",
                "use postgres://[redacted]@db/x ok?",
            ),
            ("x-api-key: abc123def ok?", "x-api-key: [redacted]?"),
            ("api-key=abc ok?", "[redacted]?"),
            (
                "the password is hunter2 ok?",
                "the password is [redacted] ok?",
            ),
            ("PASSWORD hunter2 ok?", "PASSWORD [redacted] ok?"),
            ("key=AIzaSyA1 ok?", "[redacted] ok?"),
            ("Authorization:Bearer xyz ok?", "[redacted]?"),
            ("Authorization=Bearer xyz ok?", "[redacted]?"),
            (
                "Authorization: Token abc123 ok?",
                "Authorization: [redacted]?",
            ),
            ("Authorization: Token hunter", "Authorization: [redacted]"),
            (
                r#""password": "hunter2" ok?"#,
                r#""password": [redacted] ok?"#,
            ),
            (r#"{"token":"abc"} ok?"#, "[redacted] ok?"),
            ("GITHUB_TOKEN abc123 ok?", "GITHUB_TOKEN [redacted] ok?"),
            ("API-KEY abc123 ok?", "API-KEY [redacted] ok?"),
            (
                r#"password: "hunter 2" ok?"#,
                "password: [redacted] [redacted] ok?",
            ),
            ("passphrase=abc ok?", "[redacted]?"),
            ("session=abc ok?", "[redacted] ok?"),
            ("Cookie: sid=abc ok?", "Cookie: [redacted]?"),
            ("the secret xkcdqwrtz ok?", "the secret [redacted] ok?"),
            ("accessToken=abc ok?", "[redacted]?"),
            ("clientSecret: abc ok?", "clientSecret: [redacted]?"),
            ("PGPASSWORD=abc ok?", "[redacted]?"),
            ("secretKey=hunter ok?", "[redacted]?"),
            ("privateKey: hunter ok?", "privateKey: [redacted]?"),
            ("dbPassword: hunter ok?", "dbPassword: [redacted]?"),
            ("password = hunter ok?", "password = [redacted]?"),
            ("password : x ok?", "password : [redacted]?"),
            (r#"password = "hunter" ok?"#, "password = [redacted] ok?"),
            (
                r#""password" : "hunter" ok?"#,
                r#""password" : [redacted] ok?"#,
            ),
            (
                r#""password" = "hunter" ok?"#,
                r#""password" = [redacted] ok?"#,
            ),
            ("the token is: hunter ok?", "the token is: [redacted]?"),
            ("Cookie: sid=abc; other=def ok?", "Cookie: [redacted]?"),
            ("password=[abc def] ok?", "[redacted] [redacted] ok?"),
            ("TOKEN hunter ok?", "TOKEN [redacted] ok?"),
            ("PASSWORD hunter ok?", "PASSWORD [redacted] ok?"),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn a_separated_strong_keys_value_is_hidden_to_the_end_of_its_clause() {
        for (question, text) in [
            (
                "password: correct horse battery staple",
                "password: [redacted]",
            ),
            ("passphrase=correct horse battery staple", "[redacted]"),
            (
                "passphrase: correct horse battery staple, ok?",
                "passphrase: [redacted], ok?",
            ),
            (
                "password: two words; merge?",
                "password: [redacted]; merge?",
            ),
            (
                r#"curl -H "Authorization: Bearer abc" now and later?"#,
                r#"curl -H "Authorization: [redacted]" now and later?"#,
            ),
            (
                "password: two words. Merge?",
                "password: [redacted]. Merge?",
            ),
            (
                "password: two words! Merge?",
                "password: [redacted]! Merge?",
            ),
            (
                "(password: two words) merge?",
                "(password: [redacted]) merge?",
            ),
            ("--password two words, ok?", "--password [redacted], ok?"),
            ("--auth abc123 ok?", "--auth [redacted] ok?"),
            ("password: a.b,c d, ok?", "password: [redacted], ok?"),
            ("password: hunter2, ok?", "password: [redacted], ok?"),
            (
                r#"Is "passphrase=correct horse battery staple" right?"#,
                r#"Is [redacted]" right?"#,
            ),
            (
                "Is ('password=correct horse battery') right?",
                "Is [redacted]') right?",
            ),
            (
                r#"password: "two words" ok?"#,
                "password: [redacted] [redacted] ok?",
            ),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn pwd_pw_and_pass_are_keys_only_right_before_a_separator() {
        for (question, text) in [
            ("pwd: hunter", "pwd: [redacted]"),
            ("pw: hunter2", "pw: [redacted]"),
            ("pw: Summer2024!", "pw: [redacted]!"),
            ("pw=Summer2024!", "[redacted]!"),
            ("pass: Summer2024!", "pass: [redacted]!"),
            ("pass=hunter", "[redacted]"),
            ("Did the tests pass?", "Did the tests pass?"),
            ("pass the flag through", "pass the flag through"),
            ("PASS the build?", "PASS the build?"),
            ("pw hunter2 ok?", "pw hunter2 ok?"),
            ("pwd shows the worktree?", "pwd shows the worktree?"),
            ("Is db_pwd hunter2 ok?", "Is db_pwd [redacted] ok?"),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn ordinary_words_after_a_key_word_or_inside_one_stay() {
        for question in [
            "press any key to go on?",
            "see https://github.com/o/r/pull/107 ok?",
            "Merge #107, which fixes the auth flow?",
            "Rotate the token now or after merge?",
            "Should the password reset ship?",
            "Which hotkey: ctrl or alt?",
            "Merge #12 (monkey=patch fix)?",
            "Is the session over?",
            "Merge #107, the auth-token refresh fix?",
            "Merge #107 (x-api-key header)?",
            "Merge #110 that renames GITHUB_TOKEN handling?",
            "Merge the session-cookie fix?",
            "Auth: merge #107 now?",
            "Key: which of #107 or #108 first?",
            "Update auth to v2?",
            "Is the token 2 days old?",
            "Is the bearer token flow done?",
            "Rotate the token by Fri?",
            "Run it with --auth disabled?",
            "Is AUTH broken?",
            "KEY rotation next?",
        ] {
            assert_eq!(text_sent(question), question);
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
    fn a_question_with_non_ascii_text_is_sent() {
        for (question, text) in [
            (
                "Reviewer said “done”: merge?",
                "Reviewer said “done”: merge?",
            ),
            ("Is the café: open?", "Is the café: open?"),
            ("Run git rev-parse …: fatal?", "Run git rev-parse …: fatal?"),
            (
                "Docs say “token: abc” ok?",
                "Docs say “token: [redacted]” ok?",
            ),
            ("Merge? naïve=ok", "Merge? naïve=ok"),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn curly_quotes_are_read_like_straight_ones() {
        for (question, text) in [
            (
                "Set “password”: hunter2 then merge?",
                "Set “password”: [redacted]?",
            ),
            (
                "Is “GITHUB_TOKEN”: s3cretvalue right?",
                "Is “GITHUB_TOKEN”: [redacted]?",
            ),
            ("Use ‘token’=abc123xyz ok?", "Use [redacted]?"),
            ("{“password”:“hunter2”} ok?", "[redacted] ok?"),
            ("“password”:hunter2 ok?", "[redacted]?"),
            (
                "session: “abc123 def456” ok?",
                "session: [redacted] [redacted] ok?",
            ),
            (
                "auth=“abc123 def456 ghi789” ok?",
                "[redacted] [redacted] [redacted] ok?",
            ),
            (
                "password: ‘two words’ ok?",
                "password: [redacted] [redacted] ok?",
            ),
            ("Set “pwd”: hunter2 then merge?", "Set “pwd”: [redacted]?"),
            ("“--password” correct horse ok?", "“--password” [redacted]?"),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn any_quote_before_a_key_or_a_value_is_read_like_a_straight_one() {
        for (question, text) in [
            ("Set ”token”:Bearer abc123def456 ok?", "Set [redacted]?"),
            (
                "Set „Authorization“:Bearer abc123def456 ok?",
                "Set [redacted]?",
            ),
            (
                "Set ”password”:correct horse battery staple ok?",
                "Set [redacted]?",
            ),
            ("Set ”pwd”: hunter2 then merge?", "Set ”pwd”: [redacted]?"),
            ("Set ’pwd’: hunter2 then merge?", "Set ’pwd’: [redacted]?"),
            ("Set ‚pwd‘: hunter2 then merge?", "Set ‚pwd‘: [redacted]?"),
            ("Set «pwd»: hunter2 then merge?", "Set «pwd»: [redacted]?"),
            ("Set ”key”: abc123 then?", "Set ”key”: [redacted] then?"),
            (
                "Set «password»: hunter2 then merge?",
                "Set «password»: [redacted]?",
            ),
            (
                "session: ”abc123 def456” ok?",
                "session: [redacted] [redacted] ok?",
            ),
            (
                "auth=”abc123 def456 ghi789” ok?",
                "[redacted] [redacted] [redacted] ok?",
            ),
            (
                "session: „abc123 def456“ ok?",
                "session: [redacted] [redacted] ok?",
            ),
            (
                "session: «abc123 def456» ok?",
                "session: [redacted] [redacted] ok?",
            ),
            (
                "session: ’abc123 def456’ ok?",
                "session: [redacted] [redacted] ok?",
            ),
            (
                "session: ‚abc123 def456‘ ok?",
                "session: [redacted] [redacted] ok?",
            ),
            (
                "Docs say „token: abc“ ok?",
                "Docs say „token: [redacted]“ ok?",
            ),
            (
                "Docs say «token: abc» ok?",
                "Docs say «token: [redacted]» ok?",
            ),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn a_separator_or_filler_after_a_capitalised_key_is_kept_and_its_value_hidden() {
        for (question, text) in [
            (
                "Set ”TOKEN” = hunter22 then merge?",
                "Set ”TOKEN” = [redacted]?",
            ),
            (
                "Set „PASSWORD“ is hunter22 then merge?",
                "Set „PASSWORD“ is [redacted] then merge?",
            ),
            (
                "Set «TOKEN» = hunter22 then merge?",
                "Set «TOKEN» = [redacted]?",
            ),
            (
                "The PASSWORD is hunter22, merge #107?",
                "The PASSWORD is [redacted] merge #107?",
            ),
            (
                "PGPASSWORD = hunter22 then merge #107?",
                "PGPASSWORD = [redacted]?",
            ),
            ("Is SECRET = abc123def456 right?", "Is SECRET = [redacted]?"),
            ("TOKEN abc123 ok?", "TOKEN [redacted] ok?"),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn an_apostrophe_inside_a_quoted_value_does_not_close_it() {
        for (question, text) in [
            (
                "passphrase: ‘it’s a long passphrase’ ok?",
                "passphrase: [redacted] [redacted] [redacted] [redacted] ok?",
            ),
            (
                "pw: ‘don’t panic2 now’",
                "pw: [redacted] [redacted] [redacted]",
            ),
            (
                "Use ‘password’: ‘don’t tell anyone’ ok?",
                "Use ‘password’: [redacted] [redacted] [redacted] ok?",
            ),
            (
                "Use 'password': 'don't tell anyone' ok?",
                "Use 'password': [redacted] [redacted] [redacted] ok?",
            ),
            (
                r#"{"token":"abc","x":1} then merge?"#,
                "[redacted] then merge?",
            ),
        ] {
            assert_eq!(text_sent(question), text, "{question}");
        }
    }

    #[test]
    fn a_failing_commands_non_ascii_stderr_is_noted() {
        let dir = tempfile::tempdir().unwrap();

        run(
            &stub("echo 'naïve=ok: café' >&2; exit 3"),
            dir.path(),
            SOON,
            SystemTime::now(),
        )
        .unwrap();

        assert_eq!(
            mailbox(dir.path()),
            [note(
                "notify command failed (exit 3: naïve=ok: café); \
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

    #[test]
    fn watches_marking_the_same_question_at_once_send_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("");
        let start = std::sync::Barrier::new(16);

        let sends: usize = std::thread::scope(|scope| {
            let watches: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        sent(&config, dir.path(), &[question("lh-3", "Which way?", "t1")]).len()
                    })
                })
                .collect();
            watches.into_iter().map(|w| w.join().unwrap()).sum()
        });

        assert_eq!(sends, 1);
    }

    #[test]
    fn placeholders_inside_the_question_stay_literal() {
        let dir = tempfile::tempdir().unwrap();
        let config = enabled("command = ['n', '{text}', '{member}']\n");

        let launched = sent(
            &config,
            dir.path(),
            &[question("lh-3", "say {member} and {{x}} {text}", "t1")],
        );

        assert_eq!(launched[0].args[4], "--body=say {member} and {{x}} {text}");
        assert_eq!(
            launched[1].args,
            args(&["n", "say {member} and {{x}} {text}", "lh-3"])
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
