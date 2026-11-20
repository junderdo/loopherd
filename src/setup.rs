//! `loopherd setup`: config.toml's machine-wide settings, the lead defaults
//! and the handoff limits, asked on a terminal or set by flags (D39).

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{self, ConfigFile};
use crate::context::{DEFAULT_LIMIT, DEFAULT_WINDOW, check_tokens};
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

const SWITCH_HELP: [(&str, &str); 6] = [
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
];

const LEAD_FLAG: &str = "--lead-defaults <switches|none>";
const CONTEXT_FLAG: &str = "--context window=<n>,limit=<n>,shepherd=<n>";

#[derive(Default)]
struct Limits {
    window: Option<u64>,
    limit: Option<u64>,
    shepherd: Option<u64>,
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

fn shepherd(file: &ConfigFile) -> Option<u64> {
    file.shepherd.as_ref().and_then(|s| s.context_limit)
}

fn cap(window: u64) -> u64 {
    window / 5 * 4
}

fn over_cap(limit: u64, window: u64) -> Option<String> {
    (limit > cap(window)).then(|| {
        format!(
            "{limit} is over the cap of {}, 80% of the {window}-token window; give {} or less, or a bigger window",
            cap(window),
            cap(window)
        )
    })
}

fn tokens(key: &str, text: &str) -> Result<u64> {
    let value = text
        .trim()
        .replace('_', "")
        .parse()
        .with_context(|| format!("{key} must be a whole number of tokens, got {text:?}"))?;
    check_tokens(key, Some(value))?;
    Ok(value)
}

/// `--context`'s `window=<n>,limit=<n>,shepherd=<n>`, any subset, each checked.
fn context_flag(flag: &str, file: &ConfigFile) -> Result<Limits> {
    let mut context = Limits::default();
    for part in flag.split(',') {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        let (slot, key) = match name.trim() {
            "window" => (&mut context.window, "context_window"),
            "limit" => (&mut context.limit, "default_context_limit"),
            "shepherd" => (&mut context.shepherd, "shepherd.context_limit"),
            other => bail!(
                "unknown --context name {other:?}: give window=<n>,limit=<n>,shepherd=<n>, any of them"
            ),
        };
        *slot = Some(tokens(key, value)?);
    }
    let window = context
        .window
        .or(file.context_window)
        .unwrap_or(DEFAULT_WINDOW);
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
        ("context_window", context.window),
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

/// Asks one token count until it's valid and, for a limit, under `cap_of`'s
/// cap; `None` when Enter keeps `shown`.
fn ask_tokens(
    term: &mut Terminal,
    key: &str,
    help: &str,
    shown: u64,
    cap_of: Option<u64>,
) -> Result<Option<u64>> {
    loop {
        let text = term.answer(&format!("  {key}: {help} [{shown}] "))?;
        if text.is_empty() {
            return Ok(None);
        }
        let problem = match tokens(key, &text) {
            Ok(value) => match cap_of.and_then(|window| over_cap(value, window)) {
                None => return Ok(Some(value)),
                Some(over) => over,
            },
            Err(e) => format!("{e:#}"),
        };
        term.say(&format!("  {problem}"))?;
    }
}

fn ask_context_values(term: &mut Terminal, file: &ConfigFile) -> Result<Limits> {
    term.say("Handoffs: past its handoff limit, a member, lead or the shepherd writes a handoff and starts a fresh session. Enter keeps the value shown.")?;
    let window = ask_tokens(
        term,
        "context_window",
        "tokens in your model's context window; no limit goes over 80% of it",
        file.context_window.unwrap_or(DEFAULT_WINDOW),
        None,
    )?;
    let effective_window = window.or(file.context_window).unwrap_or(DEFAULT_WINDOW);
    let limit = ask_tokens(
        term,
        "default_context_limit",
        "tokens a member or lead uses before handing off, unless its herd file sets its own",
        file.default_context_limit
            .unwrap_or(DEFAULT_LIMIT)
            .min(cap(effective_window)),
        Some(effective_window),
    )?;
    let effective_limit = limit.or(file.default_context_limit);
    let shepherd = ask_tokens(
        term,
        "shepherd.context_limit",
        "tokens the shepherd uses before handing off",
        shepherd(file)
            .or(effective_limit)
            .unwrap_or(DEFAULT_LIMIT)
            .min(cap(effective_window)),
        Some(effective_window),
    )?;
    Ok(Limits {
        window,
        limit,
        shepherd,
    })
}
