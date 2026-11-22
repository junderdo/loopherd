//! `loopherd setup`: config.toml's machine-wide settings, the lead defaults
//! and the handoff limits, asked on a terminal or set by flags (D39).

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{self, ConfigFile};
use crate::context::{self, DEFAULT_LIMIT, LARGEST_WINDOW, Limit};
use crate::herd::SwitchValues;

/// One group of settings: `None` to ask it only while unset (all of them
/// under `again`), `Some(None)` to ask it, `Some(Some(value))` to set it.
pub struct Request<'a> {
    pub config: &'a Path,
    pub again: bool,
    pub lead_defaults: Option<Option<String>>,
    pub context: Option<Option<String>>,
}

/// The terminal questions go to and answers come from.
pub struct Tty<'a> {
    pub input: &'a mut dyn BufRead,
    pub output: &'a mut dyn Write,
}

const SWITCH_HELP: [(&str, &str); 7] = [
    ("merge", "merge a PR its reviewer verified"),
    ("install", "run scripts/install and loopherd install-hook"),
    ("rebase", "have a member rebase and force-push"),
    (
        "new_cards",
        "create cards for minor or out-of-scope findings",
    ),
    (
        "design_escalation",
        "settle a design question a member raises",
    ),
    ("blocked_dialogs", "answer a member's blocking dialog"),
    (
        "follow_ups_ready",
        "file a qualifying review follow-up ready to claim",
    ),
];

const LEAD_FLAG: &str = "--lead-defaults <switches|none>";
const CONTEXT_FLAG: &str = "--context window=<n>,limit=<n>,shepherd=<n>";

#[derive(Default)]
struct Limits {
    window: Option<u64>,
    limit: Option<Limit>,
    shepherd: Option<Limit>,
}

/// Sets what the flags give, then asks the groups `req` wants on `tty`, or
/// without one names the flags for those still unset. Returns lines to print.
pub fn setup(req: Request, tty: Option<Tty>) -> Result<Vec<String>> {
    let file = config::read(req.config)?;
    let given = req.lead_defaults.is_some() || req.context.is_some();
    let wants = |group: &Option<Option<String>>, unset: bool| match group {
        Some(value) => value.is_none(),
        None => !given && (req.again || unset),
    };
    let context_unset = file.context_window.is_none()
        && file.default_context_limit.is_none()
        && shepherd(&file).is_none();
    let switches_unset = !file
        .lead_defaults
        .as_ref()
        .is_some_and(SwitchValues::sets_a_switch);
    let ask_lead = wants(&req.lead_defaults, switches_unset);
    let ask_context = wants(&req.context, context_unset);

    let context = match &req.context {
        Some(Some(flag)) => Some(context_flag(flag, &file)?),
        _ => None,
    };
    let mut lines = vec![];
    if let Some(Some(list)) = &req.lead_defaults {
        config::set_lead_defaults(req.config, list)?;
        lines.push(format!("set the lead defaults: {}", list.trim()));
    }
    if let Some(context) = context {
        lines.extend(write_context(req.config, &context)?);
    }

    let Some(tty) = tty else {
        let mut flags = vec![];
        if switches_unset && !matches!(req.lead_defaults, Some(Some(_))) {
            flags.push(LEAD_FLAG);
        }
        if context_unset && !matches!(req.context, Some(Some(_))) {
            flags.push(CONTEXT_FLAG);
        }
        if !flags.is_empty() {
            lines.push(format!(
                "setup asked nothing (no terminal); set what's unset with {}",
                flags.join(" ")
            ));
        }
        return Ok(lines);
    };
    let term = &mut Terminal { tty, ended: false };
    if ask_lead {
        let on = ask_lead_defaults(term, file.lead_defaults.as_ref())?;
        let list = if on.is_empty() {
            "none".to_string()
        } else {
            on.join(",")
        };
        config::set_lead_defaults(req.config, &list)?;
        lines.push(format!("set the lead defaults: {list}"));
    }
    if ask_context {
        let context = ask_context_values(term, &file)?;
        lines.extend(write_context(req.config, &context)?);
    }
    Ok(lines)
}

fn shepherd(file: &ConfigFile) -> Option<Limit> {
    file.shepherd.as_ref().and_then(|s| s.context_limit)
}

fn cap(window: u64) -> u64 {
    window / 5 * 4
}

fn over_cap(limit: Limit, window: u64) -> Option<String> {
    (limit.of(window) > cap(window)).then(|| {
        format!(
            "{limit} is over the cap of {}, 80% of the {window}-token window; give {} or 80% or less, or a bigger window",
            cap(window),
            cap(window)
        )
    })
}

/// `--context`'s `window=<n>,limit=<n>,shepherd=<n>`, any subset, each checked.
fn context_flag(flag: &str, file: &ConfigFile) -> Result<Limits> {
    let mut context = Limits::default();
    // A comma inside a value, as in `window=1,000,000`, is a separator, not the next name.
    let mut parts: Vec<String> = vec![];
    for piece in flag.split(',') {
        match parts.last_mut() {
            Some(last) if piece.starts_with(|c: char| c.is_ascii_digit()) => {
                last.push(',');
                last.push_str(piece);
            }
            _ => parts.push(piece.to_string()),
        }
    }
    for part in &parts {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        match name.trim() {
            "window" => context.window = Some(context::tokens("context_window", value)?),
            "limit" => context.limit = Some(context::limit("default_context_limit", value)?),
            "shepherd" => context.shepherd = Some(context::limit("shepherd.context_limit", value)?),
            other => bail!(
                "unknown --context name {other:?}: give window=<n>,limit=<n>,shepherd=<n>, any of them"
            ),
        }
    }
    let window = context
        .window
        .or(file.context_window)
        .unwrap_or(LARGEST_WINDOW);
    for (key, limit) in [
        (
            "default_context_limit",
            context.limit.or(file.default_context_limit),
        ),
        (
            "shepherd.context_limit",
            context.shepherd.or(shepherd(file)),
        ),
    ] {
        if let Some(over) = limit.and_then(|limit| over_cap(limit, window)) {
            bail!("--context: {key} {over}");
        }
    }
    Ok(context)
}

fn write_context(config: &Path, context: &Limits) -> Result<Vec<String>> {
    let mut lines = vec![];
    for (key, value) in [
        ("context_window", context.window.map(Limit::Tokens)),
        ("default_context_limit", context.limit),
        ("shepherd.context_limit", context.shepherd),
    ] {
        if let Some(value) = value {
            config::set(config, key, &value.to_string())?;
            lines.push(format!("set {key} = {value}"));
        }
    }
    Ok(lines)
}

struct Terminal<'a> {
    tty: Tty<'a>,
    ended: bool,
}

impl Terminal<'_> {
    /// A line typed in answer to `question`. End of input counts as Enter
    /// for this question and every later one, since it may not come twice.
    fn answer(&mut self, question: &str) -> Result<String> {
        write!(self.tty.output, "{question}")?;
        self.tty.output.flush()?;
        if self.ended {
            writeln!(self.tty.output)?;
            return Ok(String::new());
        }
        let mut line = String::new();
        self.ended = self
            .tty
            .input
            .read_line(&mut line)
            .context("read the terminal")?
            == 0;
        Ok(line.trim().to_string())
    }

    fn say(&mut self, line: &str) -> Result<()> {
        Ok(writeln!(self.tty.output, "{line}")?)
    }
}

fn ask_lead_defaults(
    term: &mut Terminal,
    current: Option<&SwitchValues>,
) -> Result<Vec<&'static str>> {
    let current = current.map(|c| c.over(&Default::default()));
    term.say("Lead defaults: what a herd's lead may do on its own wherever its herd file's [lead] doesn't say. Off means the human does it.")?;
    let mut on = vec![];
    for (name, help) in SWITCH_HELP {
        let default = current
            .as_ref()
            .is_some_and(|c| c.named().contains(&(name, true)));
        let options = if default { "[Y/n]" } else { "[y/N]" };
        let yes = match term
            .answer(&format!("  {name}: {help}? {options} "))?
            .chars()
            .next()
        {
            Some('y' | 'Y') => true,
            Some('n' | 'N') => false,
            _ => default,
        };
        if yes {
            on.push(name);
        }
    }
    Ok(on)
}

/// One value to ask until `parse` takes it and, when `capped`, it's under the
/// cap of `window`; Enter keeps `shown`, which comes to `kept` tokens.
struct Question<'a> {
    key: &'a str,
    help: &'a str,
    shown: Limit,
    kept: u64,
    window: u64,
    capped: bool,
    parse: fn(&str, &str) -> Result<Limit>,
}

/// The answer to `q` once it's valid, after saying what it comes to; `None` for Enter.
fn ask(term: &mut Terminal, q: Question) -> Result<Option<Limit>> {
    loop {
        let text = term.answer(&format!("  {}: {} [{}] ", q.key, q.help, q.shown))?;
        if text.is_empty() {
            term.say(&format!("  = {} tokens", grouped(q.kept)))?;
            return Ok(None);
        }
        let value = match (q.parse)(q.key, &text) {
            Ok(value) => value,
            Err(e) => {
                term.say(&format!("  {e:#}"))?;
                continue;
            }
        };
        if let Some(over) = q.capped.then(|| over_cap(value, q.window)).flatten() {
            term.say(&format!("  {over}"))?;
            continue;
        }
        let read = match value {
            Limit::Tokens(tokens) => format!("{} tokens", grouped(tokens)),
            Limit::Percent(percent) => format!(
                "{} tokens ({percent}% of {})",
                grouped(value.of(q.window)),
                grouped(q.window)
            ),
        };
        term.say(&format!("  = {read}"))?;
        return Ok(Some(value));
    }
}

/// `n` with a comma between each group of three digits.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// What an unset limit comes to, shown as the whole percentage of `window`
/// nearest under it.
fn percent_shown(limit: Option<Limit>, fallback: u64, window: u64) -> (Limit, u64) {
    let kept = limit.map_or(fallback, |l| l.of(window)).min(cap(window));
    let shown = match limit {
        Some(Limit::Tokens(tokens)) if tokens > cap(window) => Limit::Tokens(kept),
        Some(Limit::Percent(percent)) if percent > 80 => Limit::Percent(80),
        Some(limit) => limit,
        None => Limit::Percent((kept * 100 / window).clamp(1, 100) as u8),
    };
    (shown, kept)
}

fn ask_context_values(term: &mut Terminal, file: &ConfigFile) -> Result<Limits> {
    term.say("Handoffs: past its handoff limit, a member, lead or the shepherd writes a handoff and starts a fresh session. Give tokens as 250000, 250k or 1.5M, and a limit also as a percentage of the window like 65%. Enter keeps the value shown.")?;
    // Unset, the window comes from each session's model.
    let window = match file.context_window {
        Some(current) => ask(
            term,
            Question {
                key: "context_window",
                help: "tokens in your model's context window; no limit goes over 80% of it",
                shown: Limit::Tokens(current),
                kept: current,
                window: current,
                capped: false,
                parse: |key, text| context::tokens(key, text).map(Limit::Tokens),
            },
        )?
        .map(|w| w.of(current)),
        None => {
            term.say(&format!("  The context window comes from each session's model; a percentage is of that, shown here for the largest, {}.", grouped(LARGEST_WINDOW)))?;
            None
        }
    };
    let effective_window = window.or(file.context_window).unwrap_or(LARGEST_WINDOW);
    let shown_for = |limit: Option<Limit>| {
        let (shown, kept) = percent_shown(limit, DEFAULT_LIMIT, effective_window);
        match (limit, file.context_window.or(window)) {
            (None, None) => (Limit::Tokens(kept), kept),
            _ => (shown, kept),
        }
    };
    let (shown, kept) = shown_for(file.default_context_limit);
    let limit = ask(
        term,
        Question {
            key: "default_context_limit",
            help: "tokens a member or lead uses before handing off, unless its herd file sets its own",
            shown,
            kept,
            window: effective_window,
            capped: true,
            parse: context::limit,
        },
    )?;
    let effective_limit = limit.or(file.default_context_limit);
    let (shown, kept) = shown_for(shepherd(file).or(effective_limit));
    let shepherd = ask(
        term,
        Question {
            key: "shepherd.context_limit",
            help: "tokens the shepherd uses before handing off",
            shown,
            kept,
            window: effective_window,
            capped: true,
            parse: context::limit,
        },
    )?;
    Ok(Limits {
        window,
        limit,
        shepherd,
    })
}
