//! A member's context limit, and the handoff its Stop hook asks for once the
//! member's context is past it (D20).

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{self, ConfigFile};
use crate::continuation::{is_plain_name, live_transcript, session_in, transcript_of};
use crate::handoff;
use crate::ledger::{AgentRecord, Process};
use crate::process::{self, Procs};
use crate::report::{self, Body};
use crate::shepherd_resume;

/// The shepherd's handoff file's name; no member may take it.
pub const SHEPHERD: &str = "shepherd";
pub const DEFAULT_LIMIT: u64 = 250_000;
pub const DEFAULT_WINDOW: u64 = 200_000;
/// The largest window of a model loopherd knows.
pub const LARGEST_WINDOW: u64 = 1_000_000;
/// The smallest limit or window a config accepts; below it a member would
/// hand off before doing any work.
pub const MIN_TOKENS: u64 = 10_000;
/// The limit when a member's own can't be read: [`DEFAULT_LIMIT`] capped at
/// 80% of [`DEFAULT_WINDOW`].
const FALLBACK_LIMIT: u64 = if DEFAULT_LIMIT < DEFAULT_WINDOW / 5 * 4 {
    DEFAULT_LIMIT
} else {
    DEFAULT_WINDOW / 5 * 4
};

/// A session's limit, and the window that capped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLimit {
    pub tokens: u64,
    pub window: Window,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub tokens: u64,
    pub from: WindowFrom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowFrom {
    Config,
    Model(String),
    UnknownModel(String),
    Default,
}

impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let tokens = self.tokens;
        match &self.from {
            WindowFrom::Config => write!(f, "{tokens} from config.toml's context_window"),
            WindowFrom::Model(model) => write!(f, "{tokens} from its model {model}"),
            WindowFrom::UnknownModel(model) => {
                write!(f, "{tokens} by default, since its model {model} is unknown")
            }
            WindowFrom::Default => write!(f, "{tokens} by default, before its first turn"),
        }
    }
}

/// The context window of a Claude model id, `claude-<family>-<major>-<minor>`
/// with an optional date; before minor versions the date follows the major.
fn model_window(model: &str) -> Option<u64> {
    let mut parts = model.strip_prefix("claude-")?.split('-');
    let family = parts.next()?;
    if !matches!(family, "opus" | "sonnet" | "haiku" | "fable") {
        return None;
    }
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = match parts.next() {
        Some(minor) if minor.len() <= 2 => minor.parse().ok()?,
        _ => 0,
    };
    // A 4.6 has 1M only when picked as `[1m]`, which the transcript doesn't record.
    Some(if (major, minor) >= (4, 7) {
        LARGEST_WINDOW
    } else {
        DEFAULT_WINDOW
    })
}

/// `own` (a template's or `[lead]`'s `context_limit`), else `config`'s
/// `default_context_limit`, else [`DEFAULT_LIMIT`]; never over 80% of the
/// window: `config`'s `context_window`, else `model`'s, else [`DEFAULT_WINDOW`].
pub fn effective_limit(
    own: Option<Limit>,
    config: &Path,
    model: Option<&str>,
) -> Result<SessionLimit> {
    let file = config::read(config)?;
    let window = file.window(model);
    Ok(SessionLimit {
        tokens: file.limit(own, window.tokens),
        window,
    })
}

/// The shepherd's limit: `config`'s `[shepherd] context_limit` in place of a
/// member's own, resolved as [`effective_limit`].
pub fn shepherd_limit(config: &Path, model: Option<&str>) -> Result<u64> {
    let file = config::read(config)?;
    let own = file.shepherd.as_ref().and_then(|s| s.context_limit);
    Ok(file.limit(own, file.window(model).tokens))
}

/// The highest limit a member may take: 80% of `config`'s `context_window`,
/// else of [`LARGEST_WINDOW`], since each session's model caps it again.
pub struct Cap {
    pub limit: u64,
    pub window: u64,
}

pub fn limit_cap(config: &Path) -> Result<Cap> {
    let window = config::read(config)?
        .context_window
        .unwrap_or(LARGEST_WINDOW);
    Ok(Cap {
        limit: window / 5 * 4,
        window,
    })
}

impl ConfigFile {
    fn limit(&self, own: Option<Limit>, window: u64) -> u64 {
        own.or(self.default_context_limit)
            .map_or(DEFAULT_LIMIT, |limit| limit.of(window))
            .min(window / 5 * 4)
    }

    fn window(&self, model: Option<&str>) -> Window {
        let (tokens, from) = match (self.context_window, model) {
            (Some(tokens), _) => (tokens, WindowFrom::Config),
            (None, Some(model)) => match model_window(model) {
                Some(tokens) => (tokens, WindowFrom::Model(model.into())),
                None => (DEFAULT_WINDOW, WindowFrom::UnknownModel(model.into())),
            },
            (None, None) => (DEFAULT_WINDOW, WindowFrom::Default),
        };
        Window { tokens, from }
    }
}

/// A limit of `tokens` for every model, under the default window.
#[cfg(test)]
pub fn fixed_limit(tokens: u64) -> impl Fn(Option<&str>) -> Result<SessionLimit> {
    move |_| {
        Ok(SessionLimit {
            tokens,
            window: Window {
                tokens: DEFAULT_WINDOW,
                from: WindowFrom::Default,
            },
        })
    }
}

/// The token counts a context value may be written as.
pub const TOKEN_FORMS: &str = "250000, 250k, 1.5M or 1_000_000";

/// A context limit: a token count, or a percentage of the context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Tokens(u64),
    Percent(u8),
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Limit, D::Error> {
        deserializer.deserialize_any(LimitVisitor)
    }
}

struct LimitVisitor;

impl serde::de::Visitor<'_> for LimitVisitor {
    type Value = Limit;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "a whole number of tokens like {TOKEN_FORMS}, or a percentage of context_window like \"65%\""
        )
    }

    fn visit_u64<E: serde::de::Error>(self, tokens: u64) -> Result<Limit, E> {
        Ok(Limit::Tokens(tokens))
    }

    fn visit_i64<E: serde::de::Error>(self, tokens: i64) -> Result<Limit, E> {
        u64::try_from(tokens)
            .map(Limit::Tokens)
            .map_err(|_| E::invalid_value(serde::de::Unexpected::Signed(tokens), &self))
    }

    fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Limit, E> {
        parse_limit(text).map_err(|e| E::custom(format!("a limit {e}")))
    }
}

impl Limit {
    /// The token count this limit comes to in a `window`-token context window;
    /// a percentage never comes to less than [`MIN_TOKENS`].
    pub fn of(self, window: u64) -> u64 {
        match self {
            Limit::Tokens(tokens) => tokens,
            Limit::Percent(percent) => {
                (window.saturating_mul(u64::from(percent)) / 100).max(MIN_TOKENS)
            }
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Limit::Tokens(tokens) => write!(f, "{tokens}"),
            Limit::Percent(percent) => write!(f, "{percent}%"),
        }
    }
}

/// `text` as a token count in one of [`TOKEN_FORMS`], at least [`MIN_TOKENS`].
pub fn tokens(key: &str, text: &str) -> Result<u64> {
    let tokens = whole_tokens(text).ok_or_else(|| {
        anyhow!("{key} must be a whole number of tokens like {TOKEN_FORMS}, got {text:?}")
    })?;
    check_tokens(key, Some(tokens))?;
    Ok(tokens)
}

/// `text` as a token count, as [`tokens`], or a percentage of the window like `65%`.
pub fn limit(key: &str, text: &str) -> Result<Limit> {
    let limit = parse_limit(text).map_err(|e| anyhow!("{key} {e}"))?;
    check_limit(key, Some(limit))?;
    Ok(limit)
}

fn parse_limit(text: &str) -> Result<Limit, String> {
    if let Some(percent) = text.trim().strip_suffix('%') {
        let percent = percent.trim();
        if percent.is_empty() || !percent.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "must be a whole percentage of context_window like 65%, got {text:?}"
            ));
        }
        return match percent.parse::<u8>() {
            Ok(percent @ 1..=100) => Ok(Limit::Percent(percent)),
            _ => Err(format!(
                "must be between 1% and 100% of context_window, got {text:?}"
            )),
        };
    }
    whole_tokens(text).map(Limit::Tokens).ok_or_else(|| {
        format!(
            "must be a whole number of tokens like {TOKEN_FORMS}, or a percentage of \
             context_window like 65%, got {text:?}"
        )
    })
}

/// Digits, with `_` or `,` between groups of three and optional decimals,
/// then an optional `k` or `m`; `None` for anything else or a fraction of a token.
fn whole_tokens(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, scale) = match text.as_bytes().last() {
        Some(b'k' | b'K') => (&text[..text.len() - 1], 1_000),
        Some(b'm' | b'M') => (&text[..text.len() - 1], 1_000_000),
        _ => (text, 1),
    };
    let (whole, fraction) = match number.split_once('.') {
        Some((_, "")) => return None,
        Some(parts) => parts,
        None => (number, ""),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mut groups = whole.split(['_', ',']);
    let first = groups.next()?;
    let mut rest = groups.peekable();
    let grouped =
        rest.peek().is_none() || (first.len() <= 3 && rest.all(|g| g.len() == 3 && digits(g)));
    if !digits(first)
        || !grouped
        || !(fraction.is_empty() || digits(fraction))
        || fraction.len() > 6
    {
        return None;
    }
    let whole: String = whole.chars().filter(char::is_ascii_digit).collect();
    let places = 10u128.pow(fraction.len() as u32);
    let fraction: u128 = fraction.parse().unwrap_or(0);
    let scaled = whole
        .parse::<u128>()
        .ok()?
        .checked_mul(places)?
        .checked_add(fraction)?
        .checked_mul(scale)?;
    scaled
        .is_multiple_of(places)
        .then_some(scaled / places)?
        .try_into()
        .ok()
}

/// Fails when `value` is under [`MIN_TOKENS`], naming `key`.
pub fn check_tokens(key: &str, value: Option<u64>) -> Result<()> {
    match value {
        Some(v) if v < MIN_TOKENS => anyhow::bail!("{key} must be at least {MIN_TOKENS}, got {v}"),
        _ => Ok(()),
    }
}

/// Fails when `limit` is a token count under [`MIN_TOKENS`], naming `key`.
pub fn check_limit(key: &str, limit: Option<Limit>) -> Result<()> {
    match limit {
        Some(Limit::Tokens(tokens)) => check_tokens(key, Some(tokens)),
        _ => Ok(()),
    }
}

/// What the Stop hook learned about a member's context on one stop.
#[derive(Debug)]
pub struct Check {
    /// `Some` when the stop is to be blocked to ask for a handoff. An error
    /// in asking leaves `measured` intact.
    pub reason: Result<Option<String>>,
    /// `None` when the transcript is missing or has no usage yet.
    pub measured: Option<Measured>,
    /// The window the limit was capped at; `None` when the limit failed.
    pub window: Option<Window>,
}

impl Default for Check {
    fn default() -> Self {
        Check {
            reason: Ok(None),
            measured: None,
            window: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub tokens: u64,
    pub limit: u64,
}

/// Measures the context of the stop `input` describes, and gives the reason
/// to block it when `member`'s context is over `limit`: once per session,
/// never on a stop a Stop hook already continued. Before asking, moves any
/// handoff from last time to its `prev.md`. An attended member is only
/// measured. `limit` is asked with the model of the transcript's last turn.
/// A failed `limit` falls back to the built-in one, and it or an unknown
/// model is passed to `note` once per session.
pub fn handoff_block(
    member: &AgentRecord,
    input: &[u8],
    limit: &dyn Fn(Option<&str>) -> Result<SessionLimit>,
    state: &Path,
    note: &dyn Fn(String) -> Result<()>,
) -> Check {
    let Ok(input) = serde_json::from_slice::<Value>(input) else {
        return Check::default();
    };
    let live = input["transcript_path"]
        .as_str()
        .map(|t| live_transcript(Path::new(t)));
    let turn = live.as_deref().and_then(last_turn);
    let tokens = turn.as_ref().map(|t| t.tokens);
    let limit = limit(turn.as_ref().and_then(|t| t.model.as_deref()));
    let window = limit.as_ref().ok().map(|l| l.window.clone());
    let limit = limit.map(|l| l.tokens);
    let measured = |limit| tokens.map(|tokens| Measured { tokens, limit });
    let session = input["session_id"].as_str().filter(|s| is_plain_name(s));
    // The human sits in an attended member's pane, so its context is theirs to manage.
    let askable = input["hook_event_name"] == "Stop"
        && input["stop_hook_active"] != true
        && member.attended.is_none();
    let Some(session) = session.filter(|_| askable) else {
        return Check {
            reason: Ok(None),
            measured: measured(*limit.as_ref().unwrap_or(&FALLBACK_LIMIT)),
            window,
        };
    };
    let session = match &live {
        Some(live) if input["transcript_path"].as_str() != live.to_str() => session_in(live),
        _ => session.to_string(),
    };
    let session = session.as_str();
    let (limit, noted) = match limit {
        Ok(limit) => (
            limit,
            note_unknown_model(state, session, window.as_ref(), note),
        ),
        Err(e) => (FALLBACK_LIMIT, note_failed_limit(state, session, &e, note)),
    };
    // A failed note must not skip the check; it surfaces only when nothing is asked.
    let deferred = |t: u64| subagents_running(&input) && t * 2 <= limit * 3;
    let reason = match tokens.filter(|&t| t > limit && !deferred(t)) {
        Some(tokens) => ask(member, tokens, session, limit, state),
        None => Ok(None),
    };
    let reason = match reason {
        Ok(None) => noted.map(|()| None),
        reason => reason,
    };
    Check {
        reason,
        measured: measured(limit),
        window,
    }
}

/// A handoff kills in-flight subagents, so the ask waits for a stop without
/// them unless the context is past 1.5x the limit (D45).
fn subagents_running(input: &Value) -> bool {
    input["background_tasks"]
        .as_array()
        .is_some_and(|tasks| tasks.iter().any(|t| t["type"] == "subagent"))
}

fn ask(
    member: &AgentRecord,
    tokens: u64,
    session: &str,
    limit: u64,
    state: &Path,
) -> Result<Option<String>> {
    let asked = state.join("handoff-asked");
    let marker = asked.join(session);
    if marker.exists() {
        return Ok(None);
    }
    // Rotate before marking, so a failed rotation leaves the ask for the next stop.
    let moved = match handoff::find(state, &member.name) {
        Some(last) if !written_this_run(&last, state, &member.name)? => {
            let aside = handoff::set_aside(state, &member.name, &last)?;
            if let Some(warning) = aside.warning {
                eprintln!("loopherd stop-hook: warning: {warning}");
            }
            Some(aside.prev)
        }
        _ => None,
    };
    let dir = handoff::dir(state, &member.name);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let handoff = handoff::path(state, &member.name);
    fs::create_dir_all(&asked).with_context(|| format!("create {}", asked.display()))?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("create {}", marker.display())),
    }
    Ok(Some(reason(
        member,
        tokens,
        limit,
        &handoff,
        moved.as_deref(),
    )))
}

/// Whether `handoff` was written after `name`'s last context boundary in its
/// mailbox: a `start` with a fresh context (not a revive's) or a `handoff`
/// report, which consumed the file before it. Without one, it is last time's.
fn written_this_run(handoff: &Path, state: &Path, name: &str) -> Result<bool> {
    let (entries, _) = report::history(&state.join("reports"), name)?;
    let boundary = entries
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.body,
                Body::Start { resumed: false } | Body::Handoff { .. }
            )
        })
        .and_then(|e| humantime::parse_rfc3339(&e.at).ok());
    let Some(boundary) = boundary else {
        return Ok(false);
    };
    let written = fs::metadata(handoff)
        .and_then(|m| m.modified())
        .with_context(|| format!("read {}'s modified time", handoff.display()))?;
    // A tie counts as last time's: restart then refuses rather than resuming a stale file.
    Ok(written > boundary)
}

fn note_failed_limit(
    state: &Path,
    session: &str,
    error: &anyhow::Error,
    note: &dyn Fn(String) -> Result<()>,
) -> Result<()> {
    let error = format!("{error:#}");
    let error: Vec<&str> = error
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !is_snippet(l))
        .collect();
    let text = format!(
        "could not read my context limit, so it is {FALLBACK_LIMIT} until fixed: {}",
        error.join(" ")
    );
    note_once(&state.join("limit-noted"), session, text, note)
}

fn note_unknown_model(
    state: &Path,
    session: &str,
    window: Option<&Window>,
    note: &dyn Fn(String) -> Result<()>,
) -> Result<()> {
    let Some(Window {
        tokens,
        from: WindowFrom::UnknownModel(model),
    }) = window
    else {
        return Ok(());
    };
    let text = format!(
        "my model {model} has no known context window, so my limit assumes {tokens}; \
         set the right one with `loopherd config set context_window <tokens>`"
    );
    note_once(&state.join("window-noted"), session, text, note)
}

fn note_once(
    noted: &Path,
    session: &str,
    text: String,
    note: &dyn Fn(String) -> Result<()>,
) -> Result<()> {
    fs::create_dir_all(noted).with_context(|| format!("create {}", noted.display()))?;
    let marker = noted.join(session);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("create {}", marker.display())),
    }
    note(text).inspect_err(|_| {
        // Unmarked, so the next stop tries the note again.
        let _ = fs::remove_file(&marker);
    })
}

/// A line of a TOML error's source excerpt: `|`, `1 | repo =`, `|   ^`.
fn is_snippet(line: &str) -> bool {
    let gutter = line.trim_start_matches(|c: char| c.is_ascii_digit());
    gutter.starts_with('|') || gutter.starts_with(" |")
}

fn reason(
    member: &AgentRecord,
    tokens: u64,
    limit: u64,
    handoff: &Path,
    moved: Option<&Path>,
) -> String {
    // Any background shell left running, its watch or not, makes a lead's
    // stop read paused, and restart refuses it.
    let (save, how, plan, restart) = match member.is_lead() {
        true => (
            "stop every background shell and Monitor you started (TaskStop each), \
             your `loopherd watch` among them, so your stop reads finished, then ",
            " with `loopherd handoff <<'EOF'`, since your sandbox refuses your file tools there",
            "your plan as next steps, re-arming your watch first",
            ", then `loopherd restart --self`, and end your turn. It restarts you fresh from that file \
             in this pane once this turn ends.",
        ),
        false => (
            "commit and push your branch, comment your progress on your task in the project's tracker, then ",
            "",
            "your plan as next steps",
            ", and end your turn. Whoever reads your reports restarts you fresh from that file; \
             don't restart yourself.",
        ),
    };
    let moved = moved
        .map(|prev| {
            format!(
                " The handoff that was there is from before your last start or handoff, \
                 so it was moved to {}; write a new one even if you think it is current.",
                prev.display()
            )
        })
        .unwrap_or_default();
    format!(
        "loopherd: your context is at {tokens} tokens, past your limit of {limit}. \
         Stop your current work: {save}write your handoff to {}{how} (your state, {plan}, \
         gotchas; nothing already in the tracker or the ledger), run \
         `loopherd report --kind handoff \"<one line on where you are>\"`{restart}{moved}",
        handoff.display()
    )
}

/// What the shepherd's watch needs to find its caller's Claude session.
pub struct ShepherdCheck<'a> {
    pub procs: &'a dyn Procs,
    /// `$CLAUDE_PID`, counted only when it is an ancestor of `me`.
    pub claude_pid: Option<&'a str>,
    pub me: u32,
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_dir: &'a Path,
    pub state: &'a Path,
    /// The limit for the session's model; `None` skips the check.
    pub limit: &'a dyn Fn(Option<&str>) -> Option<u64>,
    /// `$HERDR_PANE_ID` and `$HERDR_SOCKET_PATH`, recorded with the handoff
    /// so a `/clear` there resumes the shepherd.
    pub pane: Option<&'a str>,
    pub socket: Option<&'a Path>,
    /// herdr's shell pid for `pane`, asked only when a handoff is recorded.
    pub shell_pid: &'a dyn Fn() -> Result<Option<u32>>,
}

/// The line telling the shepherd to hand off, due until [`Self::mark`].
pub struct ShepherdHandoff {
    pub line: String,
    /// The Claude session measured.
    pub session: String,
    marker: PathBuf,
    /// The pane's shell, or why it isn't recorded.
    resume: Option<(PathBuf, shepherd_resume::Place, Result<Process, String>)>,
}

impl ShepherdHandoff {
    #[cfg(test)]
    pub fn new(line: &str, session: &str, marker: PathBuf) -> ShepherdHandoff {
        ShepherdHandoff {
            line: line.into(),
            session: session.into(),
            marker,
            resume: None,
        }
    }

    /// Records where the shepherd runs for its resume, warning on `warn` if
    /// that fails, then remembers the session was told, so no later watch
    /// tells it again.
    pub fn mark(self, warn: &mut dyn FnMut(String)) -> Result<()> {
        if let Some((state, place, shell)) = &self.resume {
            if let Err(why) = shell {
                warn(format!(
                    "record the shepherd's pane shell, so the beside-shepherd view checks its Claude instead: {why}"
                ));
            }
            let shell = shell.as_ref().ok().copied();
            if let Err(e) =
                shepherd_resume::record(state, place, shell, &self.session, SystemTime::now())
            {
                warn(format!(
                    "record the shepherd's resume, so paste the handoff line after /clear: {e:#}"
                ));
            }
        }
        let dir = self.marker.parent().expect("a marker is in handoff-asked");
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        fs::write(&self.marker, "").with_context(|| format!("create {}", self.marker.display()))
    }
}

/// `shepherd handoff <n>k over <limit>k` when the caller's Claude session is
/// past `limit` and hasn't been told yet; nothing when the session or its
/// transcript can't be found.
pub fn shepherd_handoff(check: &ShepherdCheck) -> Option<ShepherdHandoff> {
    let pid = process::claude_pid(check.procs, check.claude_pid, check.me)?;
    let session = session_of(check.procs, pid, check.claude_dir)?;
    let marker = check.state.join("handoff-asked").join(&session);
    if marker.exists() {
        return None;
    }
    let turn = last_turn(&transcript_of(check.claude_dir, &session)?)?;
    let limit = (check.limit)(turn.model.as_deref())?;
    let tokens = turn.tokens;
    let place = || {
        Some(shepherd_resume::Place {
            pane: check.pane?.to_string(),
            socket: check.socket?.to_path_buf(),
            process: Process {
                pid,
                start: check.procs.start(pid)?,
            },
        })
    };
    (tokens > limit).then(|| ShepherdHandoff {
        line: format!("shepherd handoff {}k over {}k", tokens / 1000, limit / 1000),
        session,
        marker,
        resume: place().map(|place| {
            let shell = pane_shell(check, pid);
            (check.state.to_path_buf(), place, shell)
        }),
    })
}

/// The pane's shell, when it is `claude` or an ancestor of it; a live
/// handoff of herdr leaves shells that aren't the server's children.
fn pane_shell(check: &ShepherdCheck, claude: u32) -> Result<Process, String> {
    let pid = (check.shell_pid)()
        .map_err(|e| format!("{e:#}"))?
        .ok_or("herdr names no shell for its pane")?;
    if pid != claude && !process::is_ancestor(check.procs, pid, claude) {
        return Err(format!(
            "herdr's shell pid {pid} isn't the shepherd's Claude (pid {claude}) or above it"
        ));
    }
    let start = check
        .procs
        .start(pid)
        .ok_or_else(|| format!("can't read its shell's process (pid {pid})"))?;
    Ok(Process { pid, start })
}

/// The session in Claude's `sessions/<pid>.json`, which follows `/clear`; its
/// `procStart` must be `pid`'s, so a reused PID's stale file doesn't count.
fn session_of(procs: &dyn Procs, pid: u32, claude_dir: &Path) -> Option<String> {
    let path = claude_dir.join("sessions").join(format!("{pid}.json"));
    let file: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let start = procs.start(pid)?.to_string();
    if file["procStart"].as_str() != Some(start.as_str()) {
        return None;
    }
    let session = file["sessionId"].as_str()?;
    is_plain_name(session).then(|| session.to_string())
}

struct Turn {
    tokens: u64,
    model: Option<String>,
}

/// The transcript's last main-thread assistant turn with any usage: its
/// model, and its input, cache-creation and cache-read tokens.
fn last_turn(transcript: &Path) -> Option<Turn> {
    let text = fs::read_to_string(transcript).ok()?;
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|r| r["type"] == "assistant" && r["isSidechain"] != true)
        .find_map(|r| {
            let usage = r.pointer("/message/usage")?;
            let count = |key: &str| usage[key].as_u64().unwrap_or(0);
            let tokens = count("input_tokens")
                + count("cache_creation_input_tokens")
                + count("cache_read_input_tokens");
            // A zero is a placeholder record, not a turn the API answered.
            (tokens > 0).then(|| Turn {
                tokens,
                model: r
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .map(Into::into),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Role;

    fn config(text: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        if let Some(text) = text {
            fs::write(&path, text).unwrap();
        }
        (dir, path)
    }

    #[test]
    fn without_a_config_file_the_limit_is_its_own_capped_at_80_percent_of_200k() {
        let (_dir, path) = config(None);
        assert_eq!(
            effective_limit(Some(Limit::Tokens(100_000)), &path, None)
                .unwrap()
                .tokens,
            100_000
        );
        assert_eq!(
            effective_limit(Some(Limit::Tokens(190_000)), &path, None)
                .unwrap()
                .tokens,
            160_000
        );
        assert_eq!(effective_limit(None, &path, None).unwrap().tokens, 160_000);
    }

    #[test]
    fn the_config_default_applies_only_without_an_own_limit() {
        let (_dir, path) = config(Some("default_context_limit = 90000\n"));
        assert_eq!(effective_limit(None, &path, None).unwrap().tokens, 90_000);
        assert_eq!(
            effective_limit(Some(Limit::Tokens(120_000)), &path, None)
                .unwrap()
                .tokens,
            120_000
        );
    }

    #[test]
    fn the_built_in_limit_applies_under_a_large_window() {
        let (_dir, path) = config(Some("context_window = 1000000\n"));
        assert_eq!(effective_limit(None, &path, None).unwrap().tokens, 250_000);
        assert_eq!(
            effective_limit(Some(Limit::Tokens(900_000)), &path, None)
                .unwrap()
                .tokens,
            800_000
        );
    }

    #[test]
    fn a_percentage_limit_resolves_against_the_window_and_stays_under_the_cap() {
        for (window, own, member, lead, shepherd) in [
            ("", 50, 100_000, 130_000, 140_000),
            ("context_window = 1000000\n", 50, 500_000, 650_000, 700_000),
            ("context_window = 1000000\n", 95, 800_000, 650_000, 700_000),
        ] {
            let (_dir, path) = config(Some(&format!(
                "{window}default_context_limit = \"65%\"\n[shepherd]\ncontext_limit = \"70%\"\n"
            )));
            let own = Some(Limit::Percent(own));
            assert_eq!(
                effective_limit(own, &path, None).unwrap().tokens,
                member,
                "{window}"
            );
            assert_eq!(
                effective_limit(None, &path, None).unwrap().tokens,
                lead,
                "{window}"
            );
            assert_eq!(shepherd_limit(&path, None).unwrap(), shepherd, "{window}");
        }
    }

    #[test]
    fn a_small_percentage_is_floored_at_the_minimum_whatever_the_window() {
        for (window, limit) in [("", 10_000), ("context_window = 1000000\n", 10_000)] {
            let (_dir, path) = config(Some(&format!(
                "{window}default_context_limit = \"1%\"\n[shepherd]\ncontext_limit = \"1%\"\n"
            )));
            assert_eq!(
                effective_limit(None, &path, None).unwrap().tokens,
                limit,
                "{window}"
            );
            assert_eq!(shepherd_limit(&path, None).unwrap(), limit, "{window}");
        }
        let (_dir, path) = config(Some("context_window = 2000000\n"));
        assert_eq!(
            effective_limit(Some(Limit::Percent(1)), &path, None)
                .unwrap()
                .tokens,
            20_000
        );
    }

    #[test]
    fn a_limit_of_the_wrong_type_names_the_accepted_forms() {
        for limit in ["-5", "1.5", "true", "\"lots\""] {
            let (_dir, path) = config(Some(&format!("default_context_limit = {limit}\n")));
            let err = format!("{:#}", effective_limit(None, &path, None).unwrap_err());
            assert!(err.contains(TOKEN_FORMS), "{limit}: {err}");
        }
    }

    #[test]
    fn an_invalid_config_file_is_an_error_naming_it() {
        for text in [
            "context_windw = 1",
            "context_window = \"big\"",
            "= =",
            "context_window = 0",
            "default_context_limit = 9999",
        ] {
            let (_dir, path) = config(Some(text));
            let err = format!("{:#}", effective_limit(None, &path, None).unwrap_err());
            assert!(
                err.starts_with(&path.display().to_string()),
                "{text}: {err}"
            );
        }
    }

    #[test]
    fn the_shepherds_limit_is_its_own_else_the_default_capped_at_80_percent() {
        let (_dir, path) = config(None);
        assert_eq!(shepherd_limit(&path, None).unwrap(), 160_000);
        let (_dir, path) = config(Some("[shepherd]\ncontext_limit = 120000\n"));
        assert_eq!(shepherd_limit(&path, None).unwrap(), 120_000);
        let (_dir, path) = config(Some(
            "default_context_limit = 90000\ncontext_window = 1000000\n",
        ));
        assert_eq!(shepherd_limit(&path, None).unwrap(), 90_000);
        let (_dir, path) = config(Some(
            "default_context_limit = 90000\ncontext_window = 1000000\n[shepherd]\ncontext_limit = 900000\n",
        ));
        assert_eq!(shepherd_limit(&path, None).unwrap(), 800_000);
    }

    #[test]
    fn a_notify_value_of_any_shape_leaves_the_limits_readable() {
        for notify in ["notify = true", "[notify]\nkinds = [\"done\"]"] {
            let (_dir, path) = config(Some(&format!("context_window = 1000000\n{notify}\n")));
            assert_eq!(
                effective_limit(None, &path, None).unwrap().tokens,
                250_000,
                "{notify}"
            );
        }
    }

    #[test]
    fn a_members_limit_ignores_the_shepherds() {
        let (_dir, path) = config(Some("[shepherd]\ncontext_limit = 120000\n"));
        assert_eq!(effective_limit(None, &path, None).unwrap().tokens, 160_000);
    }

    #[test]
    fn an_invalid_shepherd_section_is_an_error_naming_the_file() {
        for text in [
            "[shepherd]\ncontext_limt = 120000\n",
            "[shepherd]\ncontext_limit = 9999\n",
            "shepherd = 1\n",
        ] {
            let (_dir, path) = config(Some(text));
            for err in [
                shepherd_limit(&path, None).unwrap_err(),
                effective_limit(None, &path, None).unwrap_err(),
            ] {
                let err = format!("{err:#}");
                assert!(
                    err.starts_with(&path.display().to_string()),
                    "{text}: {err}"
                );
            }
        }
        let (_dir, path) = config(Some("[shepherd]\ncontext_limit = 9999\n"));
        let err = format!("{:#}", shepherd_limit(&path, None).unwrap_err());
        assert!(
            err.ends_with("shepherd.context_limit must be at least 10000, got 9999"),
            "{err}"
        );
    }

    fn member(name: &str, role: Option<Role>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
            session_id: None,
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            outcome: None,
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: None,
            process: None,
            role,
            attended: None,
            no_slot: false,
            after_merge: None,
            unsandboxed: None,
            sockets_unblocked: None,
            earlier: vec![],
            unknown: Default::default(),
        }
    }

    fn assistant(input: u64, creation: u64, read: u64, sidechain: bool) -> String {
        serde_json::json!({
            "type": "assistant",
            "isSidechain": sidechain,
            "message": {"usage": {
                "input_tokens": input,
                "cache_creation_input_tokens": creation,
                "cache_read_input_tokens": read,
                "output_tokens": 999_999,
            }},
        })
        .to_string()
    }

    struct World {
        dir: tempfile::TempDir,
    }

    impl World {
        fn new() -> World {
            World {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn transcript(&self, lines: &[String]) -> PathBuf {
            let path = self.dir.path().join("t.jsonl");
            fs::write(&path, lines.join("\n") + "\n").unwrap();
            path
        }

        fn stop(&self, session: &str, transcript: &Path, extra: &str) -> Vec<u8> {
            format!(
                r#"{{"hook_event_name":"Stop","session_id":"{session}","transcript_path":"{}"{extra}}}"#,
                transcript.display()
            )
            .into_bytes()
        }

        fn block(&self, who: &AgentRecord, input: &[u8], limit: u64) -> Option<String> {
            handoff_block(who, input, &fixed_limit(limit), &self.state(), &|_| Ok(()))
                .reason
                .unwrap()
        }
    }

    #[test]
    fn over_the_limit_the_first_stop_of_a_session_is_blocked_with_the_handoff_steps() {
        let w = World::new();
        let t = w.transcript(&[
            assistant(10, 20, 100_000, false),
            r#"{"type":"user"}"#.into(),
        ]);
        let reason = w
            .block(&member("lh-1", None), &w.stop("s1", &t, ""), 100_000)
            .unwrap();
        let handoff = w.state().join("handoffs/lh-1/handoff.md");
        assert_eq!(
            reason,
            format!(
                "loopherd: your context is at 100030 tokens, past your limit of 100000. \
                 Stop your current work: commit and push your branch, comment your progress on \
                 your task in the project's tracker, then write your handoff to {} (your state, \
                 your plan as next steps, gotchas; nothing already in the tracker or the ledger), \
                 run `loopherd report --kind handoff \"<one line on where you are>\"`, and end \
                 your turn. Whoever reads your reports restarts you fresh from that file; don't \
                 restart yourself.",
                handoff.display()
            )
        );
        assert!(handoff.parent().unwrap().is_dir());
    }

    #[test]
    fn a_continued_session_is_measured_and_asked_once_in_the_session_it_continued_in() {
        let w = World::new();
        let stub = w.dir.path().join("stub.jsonl");
        fs::write(
            &stub,
            r#"{"type":"continued-in","sessionId":"stub","continuedInSessionId":"live"}"#,
        )
        .unwrap();
        fs::write(
            w.dir.path().join("live.jsonl"),
            assistant(0, 0, 195_000, false),
        )
        .unwrap();
        let lead = member("lh-1", None);
        assert!(
            w.block(&lead, &w.stop("stub", &stub, ""), 150_000)
                .is_some()
        );
        assert!(w.state().join("handoff-asked/live").exists());
        assert_eq!(w.block(&lead, &w.stop("live", &stub, ""), 150_000), None);
    }

    #[test]
    fn a_leads_reason_has_it_stop_every_background_shell_not_push_a_branch() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let reason = w
            .block(
                &member("lead-cards", Some(Role::Lead)),
                &w.stop("s1", &t, ""),
                100,
            )
            .unwrap();
        assert!(
            reason.contains(
                "Stop your current work: stop every background shell and Monitor you started \
                 (TaskStop each), your `loopherd watch` among them, so your stop \
                 reads finished, then write your handoff to"
            ),
            "{reason}"
        );
        assert!(!reason.contains("branch"), "{reason}");
        assert!(!reason.contains("kill"), "{reason}");
        let handoff = w.state().join("handoffs/lead-cards/handoff.md");
        assert!(
            reason.contains(&format!(
                "write your handoff to {} with `loopherd handoff <<'EOF'`, since your sandbox \
                 refuses your file tools there (your state,",
                handoff.display()
            )),
            "{reason}"
        );
        assert!(
            reason.contains("your plan as next steps, re-arming your watch first"),
            "{reason}"
        );
        assert!(
            reason.ends_with(
                "run `loopherd report --kind handoff \"<one line on where you are>\"`, then \
                 `loopherd restart --self`, and end your turn. It restarts you fresh from that \
                 file in this pane once this turn ends."
            ),
            "{reason}"
        );
    }

    #[test]
    fn a_session_is_blocked_only_once_and_a_new_session_again() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let who = member("lh-1", None);
        assert!(w.block(&who, &w.stop("s1", &t, ""), 100).is_some());
        assert_eq!(w.block(&who, &w.stop("s1", &t, ""), 100), None);
        assert!(w.block(&who, &w.stop("s2", &t, ""), 100).is_some());
    }

    #[test]
    fn at_or_under_the_limit_nothing_is_blocked_or_remembered() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 100, false)]);
        let who = member("lh-1", None);
        assert_eq!(w.block(&who, &w.stop("s1", &t, ""), 100), None);
        assert!(w.block(&who, &w.stop("s1", &t, ""), 99).is_some());
    }

    #[test]
    fn an_attended_member_is_measured_but_never_blocked() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 150, false)]);
        let mut who = member("grill-1", None);
        who.attended = Some(Default::default());

        let check = handoff_block(
            &who,
            &w.stop("s1", &t, ""),
            &fixed_limit(100),
            &w.state(),
            &|_| Ok(()),
        );

        assert_eq!(check.reason.unwrap(), None);
        assert_eq!(check.measured.map(|m| m.tokens), Some(150));
        assert!(!w.state().join("handoff-asked/s1").exists());
    }

    const SUBAGENT: &str = r#","background_tasks":[{"type":"subagent","status":"running"}]"#;

    #[test]
    fn running_subagents_defer_the_ask_until_a_stop_without_them() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 150, false)]);
        let who = member("lh-1", None);
        assert_eq!(w.block(&who, &w.stop("s1", &t, SUBAGENT), 100), None);
        assert!(!w.state().join("handoff-asked/s1").exists());
        assert!(w.block(&who, &w.stop("s1", &t, ""), 100).is_some());
    }

    #[test]
    fn running_subagents_dont_defer_the_ask_past_one_and_a_half_times_the_limit() {
        let w = World::new();
        let who = member("lh-1", None);
        let at = w.transcript(&[assistant(0, 0, 150, false)]);
        assert_eq!(w.block(&who, &w.stop("s1", &at, SUBAGENT), 100), None);
        let past = w.transcript(&[assistant(0, 0, 151, false)]);
        assert!(w.block(&who, &w.stop("s1", &past, SUBAGENT), 100).is_some());
    }

    #[test]
    fn background_shells_alone_dont_defer_the_ask() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 101, false)]);
        let shells = r#","background_tasks":[{"type":"shell","status":"running"}]"#;
        let lead = member("lead-cards", Some(Role::Lead));
        assert!(w.block(&lead, &w.stop("s1", &t, shells), 100).is_some());
    }

    #[test]
    fn a_stop_a_hook_already_continued_is_never_blocked() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let who = member("lh-1", None);
        let active = w.stop("s1", &t, r#","stop_hook_active":true"#);
        assert_eq!(w.block(&who, &active, 100), None);
        assert!(w.block(&who, &w.stop("s1", &t, ""), 100).is_some());
    }

    #[test]
    fn the_measure_is_the_last_main_thread_assistant_turn() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[
            assistant(0, 0, 500, false),
            assistant(0, 0, 50, false),
            assistant(0, 0, 900, true),
        ]);
        assert_eq!(w.block(&who, &w.stop("s1", &t, ""), 100), None);
        let t = w.transcript(&[assistant(0, 0, 50, false), assistant(0, 0, 500, false)]);
        assert!(w.block(&who, &w.stop("s2", &t, ""), 100).is_some());
        let t = w.transcript(&[assistant(0, 0, 500, false), assistant(0, 0, 0, false)]);
        assert!(w.block(&who, &w.stop("s3", &t, ""), 100).is_some());
    }

    #[test]
    fn unusable_input_or_transcript_blocks_nothing() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 500, false)]);
        let missing = w.dir.path().join("missing.jsonl");
        let empty = w.transcript(&[]);
        for input in [
            b"not json".to_vec(),
            format!(
                r#"{{"hook_event_name":"Stop","transcript_path":"{}"}}"#,
                t.display()
            )
            .into_bytes(),
            br#"{"hook_event_name":"Stop","session_id":"s1"}"#.to_vec(),
            format!(
                r#"{{"hook_event_name":"StopFailure","session_id":"s1","transcript_path":"{}"}}"#,
                t.display()
            )
            .into_bytes(),
            w.stop("s1", &missing, ""),
            w.stop("s1", &empty, ""),
        ] {
            assert_eq!(
                w.block(&who, &input, 100),
                None,
                "{}",
                String::from_utf8_lossy(&input)
            );
        }
    }

    use crate::report::Body;

    fn posted(state: &Path, name: &str, at: &str, body: Body) {
        let at = humantime::parse_rfc3339(at).unwrap();
        crate::report::append(
            &state.join("reports"),
            name,
            &crate::report::Entry::new(at, body),
        )
        .unwrap();
    }

    fn fresh_start() -> Body {
        Body::Start { resumed: false }
    }

    fn written_at(path: &Path, text: &str, at: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        let at = humantime::parse_rfc3339(at).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    #[test]
    fn a_handoff_written_since_the_members_last_start_is_kept_when_its_first_ask_comes() {
        let w = World::new();
        let lead = member("lead-loopherd-cards", Some(Role::Lead));
        let handoffs = w.state().join("handoffs");
        posted(
            &w.state(),
            &lead.name,
            "2026-11-17T02:00:00Z",
            fresh_start(),
        );
        written_at(
            &handoffs.join("lead-loopherd-cards/handoff.md"),
            "## State\nfresh",
            "2026-11-17T05:01:02Z",
        );
        let t = w.transcript(&[assistant(1, 367, 320_000, false)]);
        let stop = w.stop(
            "add790dc-d78c-4aad-9c9d-929c7ca8f0ec",
            &t,
            r#","background_tasks":[{"type":"shell","status":"running"}],"session_crons":[]"#,
        );

        let reason = w.block(&lead, &stop, 300_000).unwrap();

        assert!(reason.contains("320368 tokens"), "{reason}");
        assert!(!reason.contains("moved"), "{reason}");
        assert_eq!(
            fs::read_to_string(handoffs.join("lead-loopherd-cards/handoff.md")).unwrap(),
            "## State\nfresh"
        );
        assert!(!handoffs.join("lead-loopherd-cards/prev.md").exists());
    }

    #[test]
    fn a_handoff_from_before_the_last_fresh_start_or_handoff_report_moves_to_prev() {
        let handoff = || Body::Handoff {
            text: "at step 3".into(),
        };
        let resumed = || Body::Start { resumed: true };
        type Mailbox = Vec<(&'static str, Body)>;
        let cases: [(&str, Mailbox, &str, bool); 7] = [
            ("no boundary in the mailbox", vec![], "02:00:00", true),
            (
                "written before a fresh start",
                vec![("02:00:00", fresh_start())],
                "01:00:00",
                true,
            ),
            (
                "written at the same instant as a fresh start",
                vec![("02:00:00", fresh_start())],
                "02:00:00",
                true,
            ),
            (
                "written then handed off, with no start since",
                vec![("01:00:00", fresh_start()), ("03:00:00", handoff())],
                "02:00:00",
                true,
            ),
            (
                "rewritten after the last handoff report",
                vec![("01:00:00", fresh_start()), ("03:00:00", handoff())],
                "04:00:00",
                false,
            ),
            (
                "written before a revive's start",
                vec![("01:00:00", fresh_start()), ("03:00:00", resumed())],
                "02:00:00",
                false,
            ),
            (
                "written after a fresh start",
                vec![("01:00:00", fresh_start())],
                "02:00:00",
                false,
            ),
        ];
        // The flat file is where an older release wrote the handoff.
        let cases = cases
            .into_iter()
            .flat_map(|c| ["lh-1/handoff.md", "lh-1.md"].map(|file| (c.clone(), file)));
        for ((case, mailbox, written, moved), file) in cases {
            let w = World::new();
            let who = member("lh-1", None);
            let handoffs = w.state().join("handoffs");
            for (at, body) in mailbox {
                posted(&w.state(), "lh-1", &format!("2026-11-17T{at}Z"), body);
            }
            written_at(
                &handoffs.join(file),
                "earlier",
                &format!("2026-11-17T{written}Z"),
            );
            let t = w.transcript(&[assistant(0, 0, 200, false)]);

            let reason = w.block(&who, &w.stop("s1", &t, ""), 100).unwrap();

            let case = format!("{case}, in {file}");
            let prev = handoffs.join("lh-1/prev.md");
            assert_eq!(prev.exists(), moved, "{case}");
            assert_eq!(handoffs.join(file).exists(), !moved, "{case}");
            let told = format!(
                "The handoff that was there is from before your last start or handoff, \
                 so it was moved to {}; write a new one even if you think it is current.",
                prev.display()
            );
            assert_eq!(reason.ends_with(&told), moved, "{case}: {reason}");
        }
    }

    #[test]
    fn a_failed_rotation_leaves_the_ask_for_the_next_stop() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let handoffs = w.state().join("handoffs");
        fs::create_dir_all(handoffs.join("lh-1/prev.md/x")).unwrap();
        fs::write(handoffs.join("lh-1/handoff.md"), "first").unwrap();
        assert!(
            handoff_block(
                &who,
                &w.stop("s1", &t, ""),
                &fixed_limit(100),
                &w.state(),
                &|_| Ok(())
            )
            .reason
            .is_err()
        );
        fs::remove_dir_all(handoffs.join("lh-1/prev.md")).unwrap();
        assert!(w.block(&who, &w.stop("s1", &t, ""), 100).is_some());
    }

    #[test]
    fn a_failed_limit_still_blocks_when_its_note_fails_and_the_next_stop_notes_again() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 160_001, false)]);
        let broken = |_: Option<&str>| Err(anyhow::anyhow!("config.toml: TOML parse error"));
        let reason = handoff_block(&who, &w.stop("s1", &t, ""), &broken, &w.state(), &|_| {
            anyhow::bail!("mailbox full")
        })
        .reason
        .unwrap()
        .unwrap();
        assert!(reason.contains("past your limit of 160000"), "{reason}");

        let notes = std::cell::RefCell::new(vec![]);
        let note = |text: String| {
            notes.borrow_mut().push(text);
            Ok(())
        };
        let t = w.transcript(&[assistant(0, 0, 100, false)]);
        for _ in 0..2 {
            let stop = w.stop("s1", &t, "");
            assert_eq!(
                handoff_block(&who, &stop, &broken, &w.state(), &note)
                    .reason
                    .unwrap(),
                None
            );
        }
        assert_eq!(
            *notes.borrow(),
            [
                "could not read my context limit, so it is 160000 until fixed: config.toml: TOML parse error"
            ]
        );
    }

    #[test]
    fn a_failing_note_under_the_limit_is_the_checks_error() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 100, false)]);
        let err = handoff_block(
            &who,
            &w.stop("s1", &t, ""),
            &|_| Err(anyhow::anyhow!("bad")),
            &w.state(),
            &|_| anyhow::bail!("mailbox full"),
        )
        .reason
        .unwrap_err();
        assert_eq!(format!("{err:#}"), "mailbox full");
    }

    #[test]
    fn a_failing_note_or_ask_keeps_the_measurement() {
        let w = World::new();
        let who = member("lh-1", None);
        let at = Some(Measured {
            tokens: 100,
            limit: FALLBACK_LIMIT,
        });
        let t = w.transcript(&[assistant(0, 0, 100, false)]);
        let noted = handoff_block(
            &who,
            &w.stop("s1", &t, ""),
            &|_| Err(anyhow::anyhow!("bad")),
            &w.state(),
            &|_| anyhow::bail!("mailbox full"),
        );
        assert!(noted.reason.is_err());
        assert_eq!(noted.measured, at);

        let handoffs = w.state().join("handoffs");
        fs::create_dir_all(handoffs.join("lh-1/prev.md/x")).unwrap();
        fs::write(handoffs.join("lh-1/handoff.md"), "first").unwrap();
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let asked = handoff_block(
            &who,
            &w.stop("s2", &t, ""),
            &fixed_limit(100),
            &w.state(),
            &|_| Ok(()),
        );
        assert!(asked.reason.is_err());
        assert_eq!(
            asked.measured,
            Some(Measured {
                tokens: 200,
                limit: 100
            })
        );
    }

    #[test]
    fn every_stop_with_a_transcript_is_measured_even_one_not_asked_about() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 120_000, false)]);
        let measure = |input: &[u8], limit: &dyn Fn(Option<&str>) -> Result<SessionLimit>| {
            handoff_block(&who, input, limit, &w.state(), &|_| Ok(())).measured
        };
        let at = |limit| {
            Some(Measured {
                tokens: 120_000,
                limit,
            })
        };
        assert_eq!(
            measure(&w.stop("s1", &t, ""), &fixed_limit(150_000)),
            at(150_000)
        );
        let continued = w.stop("s1", &t, r#","stop_hook_active":true"#);
        assert_eq!(measure(&continued, &fixed_limit(150_000)), at(150_000));
        assert_eq!(
            measure(&continued, &|_| Err(anyhow::anyhow!("bad"))),
            at(160_000)
        );
        let untranscribed = br#"{"hook_event_name":"Stop","session_id":"s1"}"#;
        assert_eq!(measure(untranscribed, &fixed_limit(150_000)), None);
    }

    fn turn(model: &str, tokens: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "isSidechain": false,
            "message": {"model": model, "usage": {"input_tokens": tokens}},
        })
        .to_string()
    }

    #[test]
    fn the_window_comes_from_the_last_turns_model_unless_config_sets_one() {
        let model = |m: &str| WindowFrom::Model(m.into());
        for (config_text, model_id, window, from) in [
            (None, "claude-opus-5-5", 1_000_000, model("claude-opus-5-5")),
            (
                None,
                "claude-sonnet-5-5",
                1_000_000,
                model("claude-sonnet-5-5"),
            ),
            (
                None,
                "claude-haiku-5-5",
                1_000_000,
                model("claude-haiku-5-5"),
            ),
            (
                None,
                "claude-fable-5-2",
                1_000_000,
                model("claude-fable-5-2"),
            ),
            (None, "claude-opus-4-7", 1_000_000, model("claude-opus-4-7")),
            (None, "claude-opus-4-6", 200_000, model("claude-opus-4-6")),
            (
                None,
                "claude-haiku-4-5-20251001",
                200_000,
                model("claude-haiku-4-5-20251001"),
            ),
            (
                None,
                "claude-opus-4-20250514",
                200_000,
                model("claude-opus-4-20250514"),
            ),
            (
                Some("context_window = 300000\n"),
                "claude-opus-5-5",
                300_000,
                WindowFrom::Config,
            ),
        ] {
            let w = World::new();
            let (_dir, path) = config(config_text);
            let t = w.transcript(&[
                turn("claude-haiku-4-5", 10),
                turn(model_id, 120_000),
                turn("<synthetic>", 0),
            ]);
            let limit = |m: Option<&str>| effective_limit(Some(Limit::Tokens(900_000)), &path, m);
            let check = handoff_block(
                &member("lh-1", None),
                &w.stop("s1", &t, ""),
                &limit,
                &w.state(),
                &|_| Ok(()),
            );
            assert_eq!(
                check.measured.map(|m| m.limit),
                Some(window / 5 * 4),
                "{model_id}"
            );
            assert_eq!(
                check.window,
                Some(Window {
                    tokens: window,
                    from
                })
            );
        }
    }

    #[test]
    fn an_unknown_model_gets_the_default_window_and_one_note_per_session() {
        let w = World::new();
        let (_dir, path) = config(None);
        let t = w.transcript(&[turn("claude-mystery-9", 100)]);
        let limit = |m: Option<&str>| effective_limit(None, &path, m);
        let noted = std::cell::RefCell::new(vec![]);
        let stop = |session| {
            handoff_block(
                &member("lh-1", None),
                &w.stop(session, &t, ""),
                &limit,
                &w.state(),
                &|text| {
                    noted.borrow_mut().push(text);
                    Ok(())
                },
            )
        };
        let check = stop("s1");
        stop("s1");
        assert_eq!(check.measured.map(|m| m.limit), Some(160_000));
        assert_eq!(
            *noted.borrow(),
            [
                "my model claude-mystery-9 has no known context window, so my limit assumes \
              200000; set the right one with `loopherd config set context_window <tokens>`"
            ]
        );
        stop("s2");
        assert_eq!(noted.borrow().len(), 2);
    }

    #[test]
    fn a_configured_window_leaves_an_unknown_model_unnoted() {
        let w = World::new();
        let (_dir, path) = config(Some("context_window = 500000\n"));
        let t = w.transcript(&[turn("claude-mystery-9", 100)]);
        let limit = |m: Option<&str>| effective_limit(None, &path, m);
        let noted = std::cell::RefCell::new(vec![]);
        handoff_block(
            &member("lh-1", None),
            &w.stop("s1", &t, ""),
            &limit,
            &w.state(),
            &|text| {
                noted.borrow_mut().push(text);
                Ok(())
            },
        );
        assert!(noted.borrow().is_empty());
    }

    #[test]
    fn a_percentage_limit_resolves_against_the_models_window_floored_and_capped() {
        let limit = |percent: &str, model| {
            let (_dir, path) = config(Some(&format!("default_context_limit = \"{percent}\"\n")));
            effective_limit(None, &path, Some(model)).unwrap().tokens
        };
        assert_eq!(limit("50%", "claude-opus-5-5"), 500_000);
        assert_eq!(limit("50%", "claude-haiku-4-5"), 100_000);
        assert_eq!(limit("1%", "claude-opus-5-5"), 10_000);
        assert_eq!(limit("95%", "claude-opus-5-5"), 800_000);
        assert_eq!(limit("95%", "claude-haiku-4-5"), 160_000);
    }

    #[test]
    fn the_shepherds_limit_follows_its_sessions_model() {
        let (_dir, path) = config(None);
        assert_eq!(
            shepherd_limit(&path, Some("claude-opus-5-5")).unwrap(),
            250_000
        );
        assert_eq!(
            shepherd_limit(&path, Some("claude-haiku-4-5")).unwrap(),
            160_000
        );
    }

    #[test]
    fn a_failed_limit_is_noted_even_without_a_transcript() {
        let w = World::new();
        let noted = std::cell::RefCell::new(vec![]);
        let input = br#"{"hook_event_name":"Stop","session_id":"s1"}"#;
        let blocked = handoff_block(
            &member("lh-1", None),
            input,
            &|_| Err(anyhow::anyhow!("bad")),
            &w.state(),
            &|text| {
                noted.borrow_mut().push(text);
                Ok(())
            },
        );
        assert_eq!(blocked.reason.unwrap(), None);
        assert_eq!(noted.borrow().len(), 1);
    }

    #[test]
    fn a_session_id_that_isnt_a_plain_file_name_blocks_nothing() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        for session in ["", "../x", "/tmp/x", ".hidden"] {
            assert_eq!(
                w.block(&who, &w.stop(session, &t, ""), 100),
                None,
                "{session}"
            );
        }
        assert!(!w.state().join("x").exists());
    }
}

#[cfg(test)]
mod shepherd_tests {
    use super::*;
    use crate::process::FakeProcs;

    const SESSION: &str = "d7acc609-7ff5-45ff-84c9-c4b088fb954e";

    /// The shepherd's Claude (30, started at 300) in a herdr pane, running
    /// `sh` (35) running the watch (36), with its session file and transcript.
    struct Shepherd {
        dir: tempfile::TempDir,
        procs: FakeProcs,
        /// herdr's shell pid for the pane: bash, 20.
        shell_pid: Option<u32>,
    }

    impl Shepherd {
        fn new(tokens: u64) -> Shepherd {
            let mut procs = FakeProcs::pane();
            procs.add(35, "/bin/sh", 30, 350);
            procs.add(36, "/usr/bin/loopherd", 35, 360);
            let s = Shepherd {
                dir: tempfile::tempdir().unwrap(),
                procs,
                shell_pid: Some(20),
            };
            s.session_file(30, SESSION, "300");
            s.transcript(SESSION, tokens);
            s
        }

        fn claude(&self) -> PathBuf {
            self.dir.path().join("claude")
        }

        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn session_file(&self, pid: u32, session: &str, start: &str) {
            let sessions = self.claude().join("sessions");
            fs::create_dir_all(&sessions).unwrap();
            let body = serde_json::json!({"pid": pid, "sessionId": session, "procStart": start});
            fs::write(sessions.join(format!("{pid}.json")), body.to_string()).unwrap();
        }

        fn transcript(&self, session: &str, tokens: u64) {
            let project = self.claude().join("projects/-home-u-repo");
            fs::create_dir_all(&project).unwrap();
            let line = serde_json::json!({
                "type": "assistant",
                "message": {"usage": {"input_tokens": 5, "cache_read_input_tokens": tokens - 5}},
            });
            fs::write(
                project.join(format!("{session}.jsonl")),
                format!("{line}\n"),
            )
            .unwrap();
        }

        fn check_from(&self, env: Option<&str>, limit: u64) -> Option<String> {
            let handoff = shepherd_handoff(&ShepherdCheck {
                procs: &self.procs,
                claude_pid: env,
                me: 36,
                claude_dir: &self.claude(),
                state: &self.state(),
                limit: &|_| Some(limit),
                pane: Some("w1:p1"),
                socket: Some(Path::new("/run/herdr.sock")),
                shell_pid: &|| Ok(self.shell_pid),
            })?;
            let line = handoff.line.clone();
            handoff.mark(&mut |w| panic!("{w}")).unwrap();
            Some(line)
        }

        fn check(&self, limit: u64) -> Option<String> {
            self.check_from(Some("30"), limit)
        }
    }

    #[test]
    fn over_the_limit_it_tells_the_shepherd_to_hand_off_once_per_session() {
        let s = Shepherd::new(171_400);
        assert_eq!(
            s.check(160_000).as_deref(),
            Some("shepherd handoff 171k over 160k")
        );
        assert_eq!(s.check(160_000), None);
        s.session_file(30, "after-clear", "300");
        s.transcript("after-clear", 171_400);
        assert!(s.check(160_000).is_some());
    }

    #[test]
    fn telling_the_shepherd_records_its_pane_socket_session_claude_and_shell() {
        let s = Shepherd::new(171_400);
        assert!(s.check(160_000).is_some());
        let record: Value =
            serde_json::from_str(&fs::read_to_string(s.state().join("shepherd.json")).unwrap())
                .unwrap();
        assert_eq!(record["pane"], "w1:p1");
        assert_eq!(record["socket"], "/run/herdr.sock");
        assert_eq!(record["session"], SESSION);
        assert_eq!(
            record["process"],
            serde_json::json!({"pid": 30, "start": 300})
        );
        assert_eq!(
            record["shell"],
            serde_json::json!({"pid": 20, "start": 200})
        );
        assert_eq!(record["used"], false);
        assert!(humantime::parse_rfc3339(record["at"].as_str().unwrap()).is_ok());
    }

    fn recorded_shell(s: &Shepherd) -> Value {
        assert!(s.check(160_000).is_some());
        let record: Value =
            serde_json::from_str(&fs::read_to_string(s.state().join("shepherd.json")).unwrap())
                .unwrap();
        record["shell"].clone()
    }

    #[test]
    fn a_shell_orphaned_by_a_herdr_handoff_is_still_recorded() {
        let mut s = Shepherd::new(171_400);
        s.procs.add(20, "/bin/bash", 1, 200);
        assert_eq!(
            recorded_shell(&s),
            serde_json::json!({"pid": 20, "start": 200})
        );
    }

    #[test]
    fn a_claude_execed_as_the_panes_shell_is_recorded_as_its_shell() {
        let mut s = Shepherd::new(171_400);
        s.shell_pid = Some(30);
        assert_eq!(
            recorded_shell(&s),
            serde_json::json!({"pid": 30, "start": 300})
        );
    }

    #[test]
    fn a_shell_it_cant_record_warns_and_still_records_the_handoff() {
        type ShellPid = fn() -> Result<Option<u32>>;
        let cases: [(ShellPid, &str); 3] = [
            (
                || Ok(Some(99)),
                "herdr's shell pid 99 isn't the shepherd's Claude (pid 30)",
            ),
            (|| Ok(None), "herdr names no shell for its pane"),
            (
                || Err(anyhow::anyhow!("herdr pane process-info failed")),
                "herdr pane process-info failed",
            ),
        ];
        for (shell_pid, said) in cases {
            let mut s = Shepherd::new(171_400);
            s.procs.add(99, "/bin/bash", 10, 990);
            let (claude, state) = (s.claude(), s.state());
            let check = ShepherdCheck {
                procs: &s.procs,
                claude_pid: Some("30"),
                me: 36,
                claude_dir: &claude,
                state: &state,
                limit: &|_| Some(160_000),
                pane: Some("w1:p1"),
                socket: Some(Path::new("/run/herdr.sock")),
                shell_pid: &shell_pid,
            };
            let mut warned = vec![];
            shepherd_handoff(&check)
                .unwrap()
                .mark(&mut |w| warned.push(w))
                .unwrap();

            let [warning] = warned.as_slice() else {
                panic!("{warned:?}");
            };
            assert!(warning.contains(said), "{warning}");
            let record: Value =
                serde_json::from_str(&fs::read_to_string(state.join("shepherd.json")).unwrap())
                    .unwrap();
            assert_eq!(record["pane"], "w1:p1");
            assert_eq!(record["shell"], Value::Null);
        }
    }

    #[test]
    fn without_a_pane_or_socket_there_is_no_record() {
        let s = Shepherd::new(171_400);
        let (claude, state) = (s.claude(), s.state());
        for (pane, socket) in [
            (None, Some(Path::new("/run/herdr.sock"))),
            (Some("w1:p1"), None),
        ] {
            let check = ShepherdCheck {
                procs: &s.procs,
                claude_pid: Some("30"),
                me: 36,
                claude_dir: &claude,
                state: &state,
                limit: &|_| Some(100_000),
                pane,
                socket,
                shell_pid: &|| Ok(Some(20)),
            };
            shepherd_handoff(&check)
                .unwrap()
                .mark(&mut |w| panic!("{w}"))
                .unwrap();
            assert!(!state.join("shepherd.json").exists());
            fs::remove_dir_all(state.join("handoff-asked")).unwrap();
        }
    }

    #[test]
    fn a_record_that_cant_be_written_warns_and_still_marks_the_session_told() {
        let s = Shepherd::new(171_400);
        fs::create_dir_all(s.state().join("shepherd.json")).unwrap();
        let (claude, state) = (s.claude(), s.state());
        let check = ShepherdCheck {
            procs: &s.procs,
            claude_pid: Some("30"),
            me: 36,
            claude_dir: &claude,
            state: &state,
            limit: &|_| Some(160_000),
            pane: Some("w1:p1"),
            socket: Some(Path::new("/run/herdr.sock")),
            shell_pid: &|| Ok(Some(20)),
        };
        let mut warned = vec![];
        shepherd_handoff(&check)
            .unwrap()
            .mark(&mut |w| warned.push(w))
            .unwrap();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("shepherd.json"), "{warned:?}");
        assert!(shepherd_handoff(&check).is_none());
    }

    #[test]
    fn at_or_under_the_limit_it_says_nothing_and_remembers_nothing() {
        let s = Shepherd::new(160_000);
        assert_eq!(s.check(160_000), None);
        assert!(s.check(159_999).is_some());
    }

    #[test]
    fn an_unmarked_line_comes_back() {
        let s = Shepherd::new(200_000);
        let (claude, state) = (s.claude(), s.state());
        let check = ShepherdCheck {
            procs: &s.procs,
            claude_pid: Some("30"),
            me: 36,
            claude_dir: &claude,
            state: &state,
            limit: &|_| Some(100_000),
            pane: None,
            socket: None,
            shell_pid: &|| Ok(Some(20)),
        };
        assert!(shepherd_handoff(&check).is_some());
        assert!(shepherd_handoff(&check).is_some());
    }

    #[test]
    fn a_session_or_transcript_it_cant_find_says_nothing() {
        let s = Shepherd::new(200_000);
        assert_eq!(s.check_from(None, 100), None);
        assert_eq!(s.check_from(Some("x"), 100), None);
        // A Claude that isn't the watch's ancestor.
        let mut other = Shepherd::new(200_000);
        other.procs.add(99, crate::process::CLAUDE, 1, 990);
        other.session_file(99, SESSION, "990");
        assert_eq!(other.check_from(Some("99"), 100), None);

        let reused_pid = Shepherd::new(200_000);
        reused_pid.session_file(30, SESSION, "301");
        assert_eq!(reused_pid.check(100), None);

        let no_session_file = Shepherd::new(200_000);
        fs::remove_file(no_session_file.claude().join("sessions/30.json")).unwrap();
        assert_eq!(no_session_file.check(100), None);

        let no_transcript = Shepherd::new(200_000);
        no_transcript.session_file(30, "fresh", "300");
        assert_eq!(no_transcript.check(100), None);

        for session in ["", "../x", ".hidden"] {
            let odd = Shepherd::new(200_000);
            odd.session_file(30, session, "300");
            assert_eq!(odd.check(100), None, "{session}");
        }
    }
}
