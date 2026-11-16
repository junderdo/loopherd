//! `loopherd tool-hook`: Claude's PreToolUse hook on Bash. Blocks a command
//! from a lead's pane that needs a switch its herd file leaves off (D17), or
//! a `loopherd restart --force` (D20), a `loopherd pause` or `resume`
//! (D23), a `loopherd herd set` or `unset` (D29), a write of the lead
//! defaults (D31), a `loopherd config set`, `unset` or `setup` (D38), cargo run through an alias it can't read, or a `$(…)`
//! whose end it can't find, which no switch allows.
//! A guardrail on the command text, not a sandbox.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::herd::Switches;
use crate::ledger::{AgentRecord, Ledger};

/// Where the hook runs: `$HERDR_PANE_ID` and `$HERDR_SOCKET_PATH`.
pub struct Pane<'a> {
    pub id: &'a str,
    pub socket: Option<&'a Path>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    Merge,
    Install,
    Rebase,
    /// Not a switch: always the human's.
    ForceRestart,
    /// `loopherd pause` or `resume`. Not a switch: always the human's.
    Pause,
    /// `loopherd herd set` or `unset`, which could turn a lead's own switches
    /// on. Not a switch: always the human's.
    HerdEdit,
    /// `loopherd lead-defaults --set` or `scripts/install --lead-defaults`,
    /// which could turn a lead's own switches on. Not a switch: always the human's.
    LeadDefaults,
    /// `loopherd config set` or `unset`, `loopherd setup`, or `scripts/install
    /// --context`, which could raise a lead's own handoff limit or switches.
    /// Not a switch: always the human's.
    Config,
    /// `cargo` run through a `CARGO_ALIAS_*` whose value the hook can't read.
    /// Not a switch: always the human's.
    OpaqueCargoAlias,
    /// A command the hook may read differently from bash: one in a form
    /// [`unreadable`] lists, or a `$(…)` whose end it can't find. Not a
    /// switch: always the human's.
    Unreadable,
}

impl Switch {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Switch::Merge => "merge",
            Switch::Install => "install",
            Switch::Rebase => "rebase",
            Switch::ForceRestart => "restart --force",
            Switch::Pause => "pause",
            Switch::HerdEdit => "herd set",
            Switch::LeadDefaults => "lead-defaults",
            Switch::Config => "config",
            Switch::OpaqueCargoAlias => "cargo alias",
            Switch::Unreadable => "unreadable command",
        }
    }

    fn humans_part(self) -> &'static str {
        match self {
            Switch::Merge => "the human merges PRs",
            Switch::Install => "the human runs scripts/install and install-hook",
            Switch::Rebase => "the human rebases and force-pushes",
            Switch::ForceRestart => "the human forces a restart",
            Switch::Pause => "the human pauses and resumes herds",
            Switch::HerdEdit => "the human edits herd files",
            Switch::LeadDefaults => "the human sets lead defaults",
            Switch::Config => "the human sets loopherd's config",
            Switch::OpaqueCargoAlias => "the human runs cargo through such aliases",
            Switch::Unreadable => "the human runs commands the hook can't read",
        }
    }

    pub(crate) fn on(self, switches: &Switches) -> bool {
        match self {
            Switch::Merge => switches.merge,
            Switch::Install => switches.install,
            Switch::Rebase => switches.rebase,
            Switch::ForceRestart
            | Switch::Pause
            | Switch::HerdEdit
            | Switch::LeadDefaults
            | Switch::Config
            | Switch::OpaqueCargoAlias
            | Switch::Unreadable => false,
        }
    }
}

/// The message to block `input`'s Bash command with, when the pane is a lead's
/// and the command needs a switch that lead has off. Any Claude in the pane is
/// bound, nested ones included. `switches` reads a herd's `[lead]`; when it
/// fails, every switch counts as off. Reads the ledger without its lock.
pub fn tool_hook(
    ledger: &dyn Ledger,
    pane: &Pane,
    input: &[u8],
    switches: &dyn Fn(&str) -> Result<Switches>,
) -> Result<Option<String>> {
    decide(ledger, pane, input, switches, &needs)
}

/// [`tool_hook`] with the matcher passed in. A panicking matcher counts as
/// needing every switch, since a panic would otherwise print no deny.
fn decide(
    ledger: &dyn Ledger,
    pane: &Pane,
    input: &[u8],
    switches: &dyn Fn(&str) -> Result<Switches>,
    needs: &dyn Fn(&str) -> Vec<Switch>,
) -> Result<Option<String>> {
    if pane.socket.is_none() {
        return Ok(None);
    }
    let Some(command) = bash_command(input) else {
        return Ok(None);
    };
    let needed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| needs(&command)))
        .unwrap_or_else(|_| {
            vec![
                Switch::Merge,
                Switch::Install,
                Switch::Rebase,
                Switch::ForceRestart,
                Switch::Pause,
                Switch::HerdEdit,
                Switch::LeadDefaults,
                Switch::Config,
                Switch::OpaqueCargoAlias,
                Switch::Unreadable,
            ]
        });
    if needed.is_empty() {
        return Ok(None);
    }
    let Some(lead) = lead_in(ledger, pane)? else {
        return Ok(None);
    };
    let (switches, unread) = match lead.herd.as_deref().map(switches) {
        Some(Ok(switches)) => (switches, None),
        Some(Err(e)) => (Switches::default(), Some(format!("{e:#}"))),
        None => (Switches::default(), Some("no herd recorded".into())),
    };
    let Some(needed) = needed.into_iter().find(|s| !s.on(&switches)) else {
        return Ok(None);
    };
    if needed == Switch::HerdEdit {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not run `loopherd herd set` or `loopherd herd unset`, which could turn its own switches on: {}; ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    if needed == Switch::LeadDefaults {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not set lead defaults with `loopherd lead-defaults --set` or `scripts/install --lead-defaults`, which could turn its own switches on: {}; ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    if needed == Switch::Config {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not change loopherd's config with `loopherd config set`, `config unset`, `loopherd setup` or `scripts/install --context`, which could raise its own handoff limit or switches: {}; ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    if needed == Switch::OpaqueCargoAlias {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not run cargo through a `CARGO_ALIAS_*` whose value can't be read from the command: {}; spell the cargo command out, or ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    if needed == Switch::Unreadable {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not run a command the hook can't read, as with a backslash-newline, `$'…'`, a `${{…}}` holding more than a name, a `\\`` in a backtick substitution, a heredoc that leaves a quote open, a `<<` in `((…))`, or a `case` in `$(…)`: {}; rewrite it, or ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    if needed == Switch::Pause {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not run `loopherd pause` or `loopherd resume`: {}; ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    if needed == Switch::ForceRestart {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not run `loopherd restart --force`, which skips the checks that the member is finished since its handoff, so {}; restart once its stop reads finished, or ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    let mut message = format!(
        "loopherd: blocked: {}'s `{}` switch is off, so {}; ask with `loopherd report --kind question` instead",
        lead.name,
        needed.name(),
        needed.humans_part()
    );
    if let Some(why) = unread {
        message.push_str(&format!(
            " (its switches couldn't be read, so every switch counts as off: {why})"
        ));
    }
    Ok(Some(message))
}

/// The unretired lead running in `pane`, if any.
pub fn lead_in(ledger: &dyn Ledger, pane: &Pane) -> Result<Option<AgentRecord>> {
    let Some(socket) = pane.socket else {
        return Ok(None);
    };
    Ok(ledger.entries()?.into_values().find(|e| {
        !e.retired && e.is_lead() && e.pane_id == pane.id && e.socket.as_deref() == Some(socket)
    }))
}

/// The PreToolUse output that denies the Bash call, telling Claude `reason`.
pub fn deny(reason: &str) -> Value {
    serde_json::json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": reason,
    }})
}

fn bash_command(input: &[u8]) -> Option<String> {
    let input: Value = serde_json::from_slice(input).ok()?;
    if input.get("tool_name")?.as_str()? != "Bash" {
        return None;
    }
    Some(input.pointer("/tool_input/command")?.as_str()?.to_string())
}

/// Every switch the simple commands in `script` need, in order.
pub fn needs(script: &str) -> Vec<Switch> {
    needs_with(script, &mut Aliases::default())
}

/// The `CARGO_ALIAS_*` variables assigned so far in a script.
#[derive(Clone, Default)]
struct Aliases {
    /// Values by the alias's upper-case name; `None` when it can't be read.
    values: std::collections::HashMap<String, Option<String>>,
    /// Whether one was assigned under a name that can't be read, so any
    /// alias may be set.
    unreadable_name: bool,
}

impl Aliases {
    fn assign(&mut self, word: &Word) {
        if word.name_expands() {
            self.unreadable_name = true;
            return;
        }
        let Some((name, value)) = word
            .text
            .strip_prefix("CARGO_ALIAS_")
            .and_then(|rest| rest.split_once('='))
        else {
            return;
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            self.unreadable_name = true;
            return;
        }
        let readable =
            !value.trim().is_empty() && !value.contains('$') && word.substituted_at.is_none();
        self.values
            .insert(name.to_uppercase(), readable.then(|| value.to_string()));
    }
}

/// [`needs`] with the cargo aliases assigned before `script`. An assignment
/// counts for the rest of the script, exported or not.
fn needs_with(script: &str, aliases: &mut Aliases) -> Vec<Switch> {
    let Some(commands) = commands(script).filter(|_| !unreadable(script)) else {
        return vec![Switch::Unreadable];
    };
    commands
        .iter()
        .flat_map(|command| needs_one(command, aliases))
        .collect()
}

/// A word with quotes removed, and where in `text` its first `$(…)` or
/// backtick substitution stood.
#[derive(Default)]
struct Word {
    text: String,
    substituted_at: Option<usize>,
}

impl Word {
    fn mark_substituted(&mut self) {
        self.substituted_at.get_or_insert(self.text.len());
    }

    /// Whether a `$` or a substitution may build the part before the first
    /// `=`, or the whole word when it has none.
    fn name_expands(&self) -> bool {
        let name = self.text.split_once('=').map_or(&*self.text, |(n, _)| n);
        name.contains('$') || self.substituted_at.is_some_and(|at| at <= name.len())
    }
}

/// Whether `script` holds a form the lexer may read differently from bash,
/// wherever it sits, even in quotes: a backslash-newline, a `$` before a
/// quote, or a `${…}` or `$[…]` holding more than a plain name and operator.
fn unreadable(script: &str) -> bool {
    ["\\\n", "$'", "$\""]
        .iter()
        .any(|form| script.contains(form))
        || script
            .match_indices(['{', '['])
            .filter(|&(at, _)| script[..at].ends_with('$'))
            .any(|(at, _)| !plain_expansion(&script[at..]))
}

/// Whether the `${…}` or `$[…]` that `text` opens closes before any
/// character that could hide its end or a heredoc from the hook.
fn plain_expansion(text: &str) -> bool {
    let mut closers = vec![];
    for c in text.chars() {
        match c {
            '{' => closers.push('}'),
            '[' => closers.push(']'),
            _ if closers.last() == Some(&c) => {
                closers.pop();
                if closers.is_empty() {
                    return true;
                }
            }
            '(' | ')' | '<' | '>' | ';' | '&' | '|' | '\'' | '"' | '\\' | '`' => return false,
            _ if c.is_whitespace() => return false,
            _ => {}
        }
    }
    false
}

/// The simple commands in `script`, each as its words.
/// `;`, `&`, `|`, newlines and parentheses end a command; the contents of a
/// `$(…)` or backtick substitution, and heredoc bodies, are lexed as commands
/// of their own. `None` when the script ends inside a quote, backtick or
/// `$(…)`, a `$(…)` holds a `case`, a heredoc's end can't be told, or bash may
/// read a `<<` as a shift or a backtick as escaped where the hook doesn't.
fn commands(script: &str) -> Option<Vec<Vec<Word>>> {
    let mut commands = vec![];
    let mut words = vec![];
    let mut word: Option<Word> = None;
    let mut heredocs: Vec<(String, bool)> = vec![];
    let mut in_arithmetic = false;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let w = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.text.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => w.text.extend(chars.next()),
                        '$' if chars.peek() == Some(&'(') => {
                            chars.next();
                            w.mark_substituted();
                            commands.extend(self::commands(&substitution(&mut chars)?)?);
                        }
                        '`' => {
                            w.mark_substituted();
                            commands.extend(self::commands(&backticked(&mut chars)?)?);
                        }
                        c => w.text.push(c),
                    }
                }
            }
            '#' if word.is_none() => while chars.next_if(|&c| c != '\n').is_some() {},
            '\\' => word.get_or_insert_default().text.extend(chars.next()),
            // `2>&1`, `<&3` and `&>file` are redirections, not `&`.
            '&' if word.as_ref().is_some_and(|w| w.text.ends_with(['>', '<']))
                || chars.peek() == Some(&'>') =>
            {
                word.get_or_insert_default().text.push(c);
            }
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                let w = word.get_or_insert_default();
                // Bash reads `<<` in `((…))` or a `name[…]` subscript as a shift.
                if in_arithmetic || w.text.matches('[').count() > w.text.matches(']').count() {
                    return None;
                }
                w.text.push_str("<<");
                if chars.next_if_eq(&'<').is_some() {
                    w.text.push('<');
                } else {
                    heredocs.push(heredoc_operator(&mut chars, &mut w.text)?);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                word.get_or_insert_default().mark_substituted();
                commands.extend(self::commands(&substitution(&mut chars)?)?);
            }
            '`' => {
                word.get_or_insert_default().mark_substituted();
                commands.extend(self::commands(&backticked(&mut chars)?)?);
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                if chars.peek() == Some(&c) && matches!(c, '(' | ')') {
                    in_arithmetic = c == '(';
                }
                words.extend(word.take());
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
                if c == '\n' {
                    for (delimiter, tabs) in std::mem::take(&mut heredocs) {
                        let mut body = String::new();
                        heredoc_body(&mut chars, &delimiter, tabs, &mut body)?;
                        commands.extend(self::commands(&body)?);
                    }
                }
            }
            c if c.is_whitespace() => words.extend(word.take()),
            _ => word.get_or_insert_default().text.push(c),
        }
    }
    words.extend(word.take());
    if !words.is_empty() {
        commands.push(words);
    }
    Some(commands)
}

/// The text of a backtick substitution whose opening backtick was just read.
/// `None` when it isn't closed, or ends in `\`, as in ``echo `a \`b\`` ``,
/// where bash nests a substitution the hook doesn't.
fn backticked(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    let mut inner = String::new();
    loop {
        match chars.next()? {
            '`' if inner.ends_with('\\') => return None,
            '`' => return Some(inner),
            c => inner.push(c),
        }
    }
}

/// The text of a `$(…)` whose `$(` was just read, through its matching `)`,
/// skipping quotes, escapes, comments and heredoc bodies. `None` when it
/// holds a `case … in`, since a pattern's `)` would end it early, or its end
/// can't be told for some other reason.
fn substitution(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    // While in `$((…))` or `((…))`, the depth it ends at; `<<` there is a shift.
    let mut arithmetic_from = (chars.peek() == Some(&'(')).then_some(0);
    let mut depth = 1;
    let mut inner = String::new();
    let mut word = String::new();
    let mut saw_case = false;
    // Heredocs whose bodies start after this line.
    let mut heredocs: Vec<(String, bool)> = vec![];
    while let Some(c) = chars.next() {
        if separates(c) && !word.is_empty() {
            if word == "case" {
                saw_case = true;
            } else if saw_case && word == "in" {
                return None;
            }
            word.clear();
        }
        match c {
            '(' => {
                if arithmetic_from.is_none() && chars.peek() == Some(&'(') {
                    arithmetic_from = Some(depth);
                }
                depth += 1;
            }
            ')' => {
                depth -= 1;
                // `((…)` then anything but `)` may be nested subshells, not arithmetic.
                if arithmetic_from.is_some_and(|from| depth == from + 1)
                    && chars.peek() != Some(&')')
                {
                    return None;
                }
                if depth == 0 {
                    // Bash reads the body of a heredoc left open here from the lines after.
                    return heredocs.is_empty().then_some(inner);
                }
                if arithmetic_from.is_some_and(|from| depth <= from) {
                    arithmetic_from = None;
                }
            }
            '#' if word.is_empty() => {
                inner.push(c);
                while let Some(c) = chars.next_if(|&c| c != '\n') {
                    inner.push(c);
                }
                continue;
            }
            '\n' => {
                inner.push(c);
                for (delimiter, tabs) in std::mem::take(&mut heredocs) {
                    heredoc_body(chars, &delimiter, tabs, &mut inner)?;
                }
                continue;
            }
            '<' if arithmetic_from.is_none() && chars.peek() == Some(&'<') => {
                chars.next();
                inner.push_str("<<");
                if chars.next_if_eq(&'<').is_some() {
                    inner.push('<');
                } else {
                    heredocs.push(heredoc_operator(chars, &mut inner)?);
                }
                continue;
            }
            '\\' => {
                inner.push(c);
                inner.extend(chars.next());
                word.push(c);
                continue;
            }
            '\'' | '"' | '`' => {
                inner.push(c);
                quoted(chars, c, &mut inner)?;
                word.push(c);
                continue;
            }
            _ if !separates(c) => word.push(c),
            _ => {}
        }
        inner.push(c);
    }
    None
}

fn separates(c: char) -> bool {
    c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>')
}

/// Copies a quoted span whose opening `quote` was just read onto `inner`,
/// through its closing one. `None` when it isn't closed.
fn quoted(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    quote: char,
    inner: &mut String,
) -> Option<()> {
    loop {
        let c = chars.next()?;
        if quote == '"' && c == '$' && chars.next_if_eq(&'(').is_some() {
            inner.push_str("$(");
            inner.push_str(&substitution(chars)?);
            inner.push(')');
            continue;
        }
        inner.push(c);
        if c == quote {
            return Some(());
        }
        if quote == '"' && c == '`' {
            quoted(chars, c, inner)?;
        } else if c == '\\' && quote != '\'' {
            inner.extend(chars.next());
        }
    }
}

/// Copies the rest of a heredoc operator whose `<<` was just read onto
/// `text`, and returns its delimiter and whether `<<-` strips tabs. `None`
/// when the delimiter holds a `$` or a backtick.
fn heredoc_operator(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    text: &mut String,
) -> Option<(String, bool)> {
    let tabs = chars.next_if_eq(&'-').is_some();
    if tabs {
        text.push('-');
    }
    while let Some(c) = chars.next_if(|&c| c == ' ' || c == '\t') {
        text.push(c);
    }
    let mut delimiter = String::new();
    while let Some(c) = chars.next_if(|&c| !separates(c)) {
        text.push(c);
        match c {
            '$' | '`' => return None,
            '\'' | '"' => {
                for q in chars.by_ref() {
                    text.push(q);
                    if q == c {
                        break;
                    }
                    delimiter.push(q);
                }
            }
            '\\' => {
                if let Some(e) = chars.next() {
                    text.push(e);
                    delimiter.push(e);
                }
            }
            _ => delimiter.push(c),
        }
    }
    Some((delimiter, tabs))
}

/// Copies a heredoc's body onto `text` through its `delimiter` line. `None`
/// when the script ends first, or a line starts with the delimiter and holds
/// a `)`, as `EOF)` does, which bash inside `$(…)` takes as the end.
fn heredoc_body(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    delimiter: &str,
    tabs: bool,
    text: &mut String,
) -> Option<()> {
    while chars.peek().is_some() {
        let mut line = String::new();
        while let Some(c) = chars.next_if(|&c| c != '\n') {
            line.push(c);
        }
        chars.next();
        text.push_str(&line);
        text.push('\n');
        let stripped = if tabs {
            line.trim_start_matches('\t')
        } else {
            &line
        };
        if stripped == delimiter {
            return Some(());
        }
        if stripped.starts_with(delimiter) && stripped.contains(')') {
            return None;
        }
    }
    None
}

/// Words that run the command after them, with their options that take a value.
const WRAPPERS: [(&str, &[&str]); 14] = [
    ("env", &["-u", "-C", "-S"]),
    ("sudo", &["-u", "-g", "-C", "-D"]),
    ("exec", &["-a"]),
    ("command", &[]),
    ("builtin", &[]),
    ("nohup", &[]),
    ("time", &["-f", "-o"]),
    ("nice", &["-n"]),
    ("stdbuf", &["-i", "-o", "-e"]),
    ("noglob", &[]),
    ("xargs", &["-I", "-n", "-P", "-L", "-d", "-s", "-E", "-a"]),
    ("timeout", &["-s", "-k"]),
    ("setsid", &[]),
    // This repo's `scripts/herdr-dev`.
    ("herdr-dev", &[]),
];
const KEYWORDS: [&str; 10] = [
    "if", "then", "else", "elif", "do", "while", "until", "!", "{", "source",
];
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];
/// Builtins whose arguments may assign variables.
const DECLARERS: [&str; 5] = ["export", "declare", "typeset", "local", "readonly"];

fn needs_one(command: &[Word], aliases: &mut Aliases) -> Vec<Switch> {
    let words: Vec<String> = command.iter().map(|w| w.text.clone()).collect();
    let words = words.as_slice();
    let mut i = 0;
    let mut in_env = false;
    loop {
        let Some(word) = words.get(i) else {
            return vec![];
        };
        let program = file_name(word);
        if let Some(bare) = redirection(word) {
            i += if bare { 2 } else { 1 };
        } else if in_env && (word.contains('=') || word.is_empty()) && command[i].name_expands() {
            // `env` may get an assignment whose name the hook can't read.
            aliases.assign(&command[i]);
            i += 1;
        } else if word.is_empty() && command[i].substituted_at.is_some() {
            // An unquoted substitution that prints nothing leaves no word.
            i += 1;
        } else if is_assignment(word) {
            aliases.assign(&command[i]);
            i += 1;
        } else if KEYWORDS.contains(&word.as_str()) || word == "." {
            i += 1;
        } else if DECLARERS.contains(&program) {
            let args = &command[i + 1..];
            let nameref = |w: &Word| w.text.starts_with('-') && w.text.contains('n');
            if !["export", "readonly"].contains(&program) && args.iter().any(nameref) {
                // A nameref (`declare -n r=CARGO_ALIAS_X`) assigns through another name.
                aliases.unreadable_name = true;
            }
            args.iter().for_each(|w| aliases.assign(w));
            return vec![];
        } else if let Some((_, with_value)) = WRAPPERS.iter().find(|(w, _)| *w == program) {
            in_env |= program == "env";
            i = skip_options(words, i + 1, with_value);
            if program == "timeout" {
                i += 1;
            }
        } else if program == "rustup" {
            // `rustup [-q] [+toolchain] run [--install] <toolchain> [--install] [--] <command>`
            i += 1;
            while words.get(i).is_some_and(|w| w.starts_with(['-', '+'])) {
                i += 1;
            }
            if words.get(i).is_none_or(|w| w != "run") {
                return vec![];
            }
            let toolchain = skip_options(words, i + 1, &[]);
            i = skip_options(words, toolchain + 1, &[]);
        } else if SHELLS.contains(&program) {
            i += 1;
            // `-o pipefail`, `-O extglob`: the next word is the option's value.
            const WITH_VALUE: [&str; 4] = ["-o", "+o", "-O", "+O"];
            while let Some(option) = words.get(i).filter(|w| w.starts_with(['-', '+'])) {
                if option == "--" {
                    i += 1;
                    break;
                }
                if !option.starts_with("--") && option.contains('c') {
                    let script = skip_options(words, i + 1, &WITH_VALUE);
                    return words
                        .get(script)
                        .map_or(vec![], |script| needs_with(script, &mut aliases.clone()));
                }
                i += if WITH_VALUE.contains(&option.as_str()) {
                    2
                } else {
                    1
                };
            }
        } else if program == "eval" {
            return needs_with(&words[i + 1..].join(" "), aliases);
        } else if program == "cargo" {
            return cargo_needs(word, &words[i + 1..], aliases);
        } else {
            return needs_program(word, &words[i + 1..]).into_iter().collect();
        }
    }
}

fn needs_program(path: &str, args: &[String]) -> Option<Switch> {
    if Path::new(path).ends_with("scripts/install") {
        let flag = |name| args.iter().any(|a| is_flag(a, name));
        return Some(if flag("--lead-defaults") {
            Switch::LeadDefaults
        } else if flag("--context") {
            Switch::Config
        } else {
            Switch::Install
        });
    }
    let positional = positionals(args, &[]);
    match file_name(path) {
        "gh" => {
            let positional = positionals(args, &["-R", "--repo"]);
            let pr_merge = positional.starts_with(&["pr", "merge"]);
            let api_merge = positional.first() == Some(&"api")
                && args.iter().any(|a| merges(a))
                && gh_api_writes(args);
            (pr_merge || api_merge).then_some(Switch::Merge)
        }
        "curl" | "wget" => args.iter().any(|a| merges(a)).then_some(Switch::Merge),
        "git" => git_needs(args),
        "loopherd" => match positional.first() {
            Some(&"install-hook") => Some(Switch::Install),
            Some(&"lead-defaults") => sets_lead_defaults(args),
            Some(&"config") => {
                matches!(positional.get(1), Some(&"set" | &"unset")).then_some(Switch::Config)
            }
            Some(&"setup") => runs_setup(args).then_some(Switch::Config),
            Some(&"restart") => forces_restart(args),
            Some(&"pause" | &"resume") => Some(Switch::Pause),
            Some(&"herd") => {
                matches!(positional.get(1), Some(&"set" | &"unset")).then_some(Switch::HerdEdit)
            }
            _ => None,
        },
        // `cargo run -- herd set` and `pause` in a loopherd checkout, in any
        // spelling; first, since a value or reason may hold any word.
        "cargo" if edits_herd(cargo_run_args(args)) => Some(Switch::HerdEdit),
        "cargo"
            if cargo_run_args(args).iter().any(|a| a == "lead-defaults")
                && sets_lead_defaults(args).is_some() =>
        {
            Some(Switch::LeadDefaults)
        }
        // A key or value read by `herd get` may be any word.
        "cargo" if runs_herd_get(cargo_run_args(args)) => None,
        "cargo" if edits_config(cargo_run_args(args)) => Some(Switch::Config),
        "cargo"
            if cargo_run_args(args)
                .iter()
                .any(|a| a == "pause" || a == "resume") =>
        {
            Some(Switch::Pause)
        }
        // `cargo run -- install-hook` in a loopherd checkout, in any spelling.
        "cargo" if args.iter().any(|a| a == "install-hook") => Some(Switch::Install),
        "cargo" if args.iter().any(|a| a == "restart") => forces_restart(args),
        _ => None,
    }
}

/// Cargo's built-in subcommands, which ignore an alias of the same name.
/// Its short forms (`b`, `r`, `t`…) are not here: an alias overrides them.
const CARGO_BUILTINS: [&str; 39] = [
    "add",
    "bench",
    "build",
    "check",
    "clean",
    "config",
    "doc",
    "fetch",
    "fix",
    "generate-lockfile",
    "git-checkout",
    "help",
    "info",
    "init",
    "install",
    "locate-project",
    "login",
    "logout",
    "metadata",
    "new",
    "owner",
    "package",
    "pkgid",
    "publish",
    "read-manifest",
    "remove",
    "report",
    "run",
    "rustc",
    "rustdoc",
    "search",
    "test",
    "tree",
    "uninstall",
    "update",
    "vendor",
    "verify-project",
    "version",
    "yank",
];

/// What `cargo args` needs as written and after each step of expanding its
/// subcommand through `aliases`.
fn cargo_needs(path: &str, args: &[String], aliases: &Aliases) -> Vec<Switch> {
    // Past this many expansions the alias is circular, which cargo refuses too.
    const MAX_EXPANSIONS: usize = 16;
    let mut needed: Vec<Switch> = needs_program(path, args).into_iter().collect();
    let mut args = args.to_vec();
    for _ in 0..MAX_EXPANSIONS {
        let at = cargo_subcommand(&args);
        let Some(subcommand) = args
            .get(at)
            .filter(|sub| !CARGO_BUILTINS.contains(&sub.as_str()))
        else {
            return needed;
        };
        if aliases.unreadable_name {
            return vec![Switch::OpaqueCargoAlias];
        }
        let Some(value) = aliases
            .values
            .get(&subcommand.to_uppercase().replace(['-', '.'], "_"))
        else {
            return needed;
        };
        let Some(value) = value else {
            return vec![Switch::OpaqueCargoAlias];
        };
        args.splice(at..=at, value.split_whitespace().map(String::from));
        needed.extend(needs_program(path, &args).filter(|s| !needed.contains(s)));
    }
    vec![Switch::OpaqueCargoAlias]
}

/// The index of cargo's subcommand in `args`, past a `+toolchain` and global options.
fn cargo_subcommand(args: &[String]) -> usize {
    const GLOBAL_WITH_VALUE: [&str; 4] = ["--color", "--config", "-Z", "-C"];
    let toolchain = usize::from(args.first().is_some_and(|a| a.starts_with('+')));
    skip_options(args, toolchain, &GLOBAL_WITH_VALUE)
}

/// The args after `cargo run` (or `r`), options and `--` included; none for
/// other subcommands, since `cargo test pause` is a filter.
fn cargo_run_args(args: &[String]) -> &[String] {
    let at = cargo_subcommand(args);
    match args.get(at) {
        Some(run) if run == "run" || run == "r" => &args[at + 1..],
        _ => &[],
    }
}

/// Whether `run_args` run loopherd's `herd get`: every option before `herd`
/// is one cargo knows, and no `herd set` or `unset` hides among the words.
fn runs_herd_get(run_args: &[String]) -> bool {
    const WITH_VALUE: [&str; 16] = [
        "--bin",
        "-p",
        "--package",
        "--profile",
        "--features",
        "-F",
        "--target",
        "--target-dir",
        "--manifest-path",
        "-j",
        "--jobs",
        "--example",
        "--color",
        "--config",
        "-Z",
        "-C",
    ];
    const FLAGS: [&str; 10] = [
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "-r",
        "--release",
        "--offline",
        "--locked",
        "--frozen",
        "--all-features",
    ];
    let mut i = 0;
    while let Some(arg) = run_args.get(i) {
        match arg.as_str() {
            "--" => {
                i += 1;
                break;
            }
            a if WITH_VALUE.contains(&a) => i += 2,
            a if FLAGS.contains(&a) => i += 1,
            a if a.starts_with('-') => return false,
            _ => break,
        }
    }
    let rest = run_args.get(i..).unwrap_or_default();
    rest.starts_with(&["herd".to_string(), "get".to_string()]) && !edits_herd(rest)
}

/// Whether `herd` then `set` or `unset` appear in `args`, side by side.
fn edits_herd<S: AsRef<str>>(args: &[S]) -> bool {
    args.windows(2)
        .any(|w| w[0].as_ref() == "herd" && matches!(w[1].as_ref(), "set" | "unset"))
}

/// Whether `args` run `config set` or `unset`, or a `setup` that isn't just its help.
fn edits_config(args: &[String]) -> bool {
    args.windows(2)
        .any(|w| w[0] == "config" && matches!(w[1].as_str(), "set" | "unset"))
        || args
            .iter()
            .position(|a| a == "setup")
            .is_some_and(|at| runs_setup(&args[at + 1..]))
}

fn runs_setup(args: &[String]) -> bool {
    !args.iter().any(|a| a == "--help" || a == "-h")
}

fn sets_lead_defaults(args: &[String]) -> Option<Switch> {
    args.iter()
        .any(|a| is_flag(a, "--set"))
        .then_some(Switch::LeadDefaults)
}

/// Whether `arg` is `flag`, alone or as `flag=value`.
fn is_flag(arg: &str, flag: &str) -> bool {
    arg.strip_prefix(flag)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
}

fn forces_restart(args: &[String]) -> Option<Switch> {
    args.iter()
        .any(|a| a == "--force")
        .then_some(Switch::ForceRestart)
}

fn git_needs(args: &[String]) -> Option<Switch> {
    const GLOBAL_WITH_VALUE: [&str; 7] = [
        "-C",
        "-c",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--config-env",
        "--super-prefix",
    ];
    let start = skip_options(args, 0, &GLOBAL_WITH_VALUE);
    let (subcommand, rest) = args.get(start..)?.split_first()?;
    let rebases = |a: &String| {
        a == "-r" || a == "--rebase" || a.strip_prefix("--rebase=").is_some_and(|v| v != "false")
    };
    let forces = |a: &String| {
        a == "--force"
            || a.starts_with("--force-with-lease")
            || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
            || a.starts_with('+')
            || a == "--mirror"
    };
    let needed = match subcommand.as_str() {
        "rebase" => true,
        "pull" => rest.iter().any(rebases),
        "push" => rest.iter().any(forces),
        _ => false,
    };
    needed.then_some(Switch::Rebase)
}

/// `gh api` sends a GET only with no method and no fields, in any form
/// (`-X PUT`, `-XPUT`, `--method=PUT`, `-fkey=v`, `--field=…`).
fn gh_api_writes(args: &[String]) -> bool {
    args.iter().any(|a| {
        ["-X", "-f", "-F"].iter().any(|f| a.starts_with(f))
            || ["--method", "--field", "--raw-field", "--input"]
                .iter()
                .any(|f| a == f || a.starts_with(&format!("{f}=")))
    })
}

/// Whether `word` is a redirection, and if so whether it's bare (`>`, `2>`),
/// so its target is the next word.
fn redirection(word: &str) -> Option<bool> {
    let op = word.trim_start_matches(|c: char| c.is_ascii_digit());
    let op = op
        .strip_prefix('&')
        .filter(|o| o.starts_with('>'))
        .unwrap_or(op);
    if !op.starts_with(['>', '<']) {
        return None;
    }
    let target = op.trim_start_matches(['>', '<', '&', '|']);
    Some(target.is_empty() && !op.ends_with('&'))
}

/// A GitHub API path that merges a pull request: `…/pulls/<n>/merge`.
fn merges(arg: &str) -> bool {
    arg.match_indices("pulls/").any(|(at, found)| {
        let rest = &arg[at + found.len()..];
        let Some((number, tail)) = rest.split_once('/') else {
            return false;
        };
        !number.is_empty()
            && tail
                .strip_prefix("merge")
                .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    })
}

/// The index of the first word from `i` on that isn't an option or an
/// option's value; a `--` is skipped and ends the options.
fn skip_options(words: &[String], mut i: usize, with_value: &[&str]) -> usize {
    while let Some(word) = words.get(i) {
        if word == "--" {
            return i + 1;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        i += if with_value.contains(&word.as_str()) {
            2
        } else {
            1
        };
    }
    i.min(words.len())
}

fn positionals<'a>(args: &'a [String], with_value: &[&str]) -> Vec<&'a str> {
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        let next = skip_options(args, i, with_value);
        match args.get(next) {
            Some(arg) => out.push(arg.as_str()),
            None => break,
        }
        i = next + 1;
    }
    out
}

fn file_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let name = name.strip_suffix('+').unwrap_or(name);
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::anyhow;

    use super::*;
    use crate::ledger::{AgentRecord, FileLedger, MemLedger, Role};
    use crate::lock::Lock;

    const SOCKET: &str = "/run/herdr/herdr.sock";
    const PANE: &str = "w1:p1";

    const MERGES: &[&str] = &[
        "gh pr merge 12",
        "gh pr merge 12 --squash --delete-branch",
        "gh -R junderdo/loopherd pr merge 12",
        "gh pr --repo junderdo/loopherd merge 12",
        "/usr/bin/gh pr merge 12",
        "GH_TOKEN=$(gh auth token --user junderdo) gh pr merge 12",
        "cd /src/repo && gh pr merge 12",
        "bash -c 'gh pr merge 12'",
        "sh -lc \"cd /x; gh pr merge 12\"",
        "timeout 30 gh pr merge 12",
        "echo 12 | xargs gh pr merge",
        "gh api -X PUT repos/junderdo/loopherd/pulls/12/merge",
        "gh api repos/{owner}/{repo}/pulls/$N/merge --method PUT",
        "curl -X PUT -H 'Authorization: token x' https://api.github.com/repos/o/r/pulls/12/merge",
        "wget --method=PUT https://api.github.com/repos/o/r/pulls/12/merge",
        "eval 'gh pr merge 12'",
        "sudo gh pr merge 12",
        "exec gh pr merge 12",
        "command gh pr merge 12",
        "builtin gh pr merge 12",
        "nohup gh pr merge 12",
        "time gh pr merge 12",
        "nice -n 5 gh pr merge 12",
        "stdbuf -o L gh pr merge 12",
        "noglob gh pr merge 12",
        "env -u GH_TOKEN gh pr merge 12",
        "echo 12 | xargs -n 1 gh pr merge",
        "if gh pr merge 12; then echo ok; fi",
        "while true; do gh pr merge 12; done",
        "until false; do :; done; gh pr merge 12",
        "if false; then :; elif gh pr merge 12; then :; else :; fi",
        "! gh pr merge 12",
        "{ gh pr merge 12; }",
        "zsh -c 'gh pr merge 12'",
        "dash -c 'gh pr merge 12'",
        "ksh -c 'gh pr merge 12'",
        "bash -o pipefail -c 'gh pr merge 12'",
        "echo start\ngh pr merge 12",
        "echo `gh pr merge 12`",
        "(gh pr merge 12)",
        "out=\"$(gh pr merge 12 --squash 2>&1)\"; echo \"$out\"",
        "echo \"merged: $(echo $(date) && gh pr merge 12)\"",
        "2>/dev/null gh pr merge 3",
        "2>&1 gh pr merge 3",
        "&>/dev/null gh pr merge 3",
        "> out.txt gh pr merge 3",
        "bash -c -- \"gh pr merge 3\"",
        "sh -c -- \"gh pr merge 3\"",
        "bash -c -e \"gh pr merge 3\"",
        "curl -d {} https://api.github.com/repos/o/r/pulls/12/merge",
        "curl --data-raw x https://api.github.com/repos/o/r/pulls/12/merge",
        "curl -XPUT https://api.github.com/repos/o/r/pulls/12/merge",
        "curl -T body.json https://api.github.com/repos/o/r/pulls/12/merge",
        "wget --post-data x https://api.github.com/repos/o/r/pulls/12/merge",
        "gh api -f merge_method=squash repos/o/r/pulls/12/merge",
        "gh api --input body.json repos/o/r/pulls/12/merge",
        "gh api --method=PUT repos/o/r/pulls/12/merge",
        "gh pr merge --help",
        "gh pr merge 12 -h",
        "gh pr merge 3 --body --help",
        "gh api -X GET repos/o/r/pulls/12/merge",
        "gh api -X GET -X PUT repos/o/r/pulls/12/merge",
        "gh api -XGET repos/o/r/pulls/12/merge",
        "gh api --method=GET repos/o/r/pulls/12/merge",
        "gh api -fmerge_method=squash repos/o/r/pulls/12/merge",
        "gh api --raw-field=a=b repos/o/r/pulls/12/merge",
        "curl https://api.github.com/repos/o/r/pulls/12/merge",
        "curl -s -o /dev/null -w %{http_code} https://api.github.com/repos/o/r/pulls/12/merge",
        "curl -X GET https://api.github.com/repos/o/r/pulls/12/merge",
        "curl -sX PUT https://api.github.com/repos/o/r/pulls/12/merge",
        "curl -X GET -X PUT https://api.github.com/repos/o/r/pulls/12/merge",
        "wget -qO- https://api.github.com/repos/o/r/pulls/12/merge",
        "setsid gh pr merge 12",
        "setsid -f -w gh pr merge 12",
        "rustup run stable gh pr merge 12",
        "rustup --quiet run --install nightly-2026-01-01 gh pr merge 12",
        "rustup +stable run stable gh pr merge 12",
        "rustup run stable -- gh pr merge 12",
        "rustup run stable --install gh pr merge 12",
        "rustup run stable --install -- gh pr merge 12",
        "GH_TOKEN=$(echo t)x gh pr merge 12",
        "GH_TOKEN=`echo t`x gh pr merge 12",
        "$(true) gh pr merge 12",
        "`true` gh pr merge 12",
        "echo $(gh pr merge 1)",
        "echo x$(gh pr merge 1)y",
        "X+=1 gh pr merge 12",
        "echo $(echo ')'; gh pr merge 12)",
        "echo $(echo \")\"; gh pr merge 12)",
        "echo $(echo \\); gh pr merge 12)",
        "x=$(echo ')'; gh pr merge 12)",
        "echo \"$(echo ')'; gh pr merge 12)\"",
        "echo $(echo \"$(echo ')')\"; gh pr merge 12)",
        "echo $(echo \"\\\")\"; gh pr merge 12)",
    ];
    const INSTALLS: &[&str] = &[
        "scripts/install",
        "scripts/herdr-dev cargo run -- install-hook",
        "herdr-dev loopherd install-hook --settings /tmp/s.json",
        "./scripts/install",
        "/home/me/src/loopherd/scripts/install",
        "sh scripts/install",
        "bash -x ./scripts/install",
        "bash -c 'cd /src/loopherd && scripts/install'",
        "FOO=1 scripts/install",
        "loopherd install-hook",
        "/home/me/.local/bin/loopherd install-hook --remove",
        "env XDG_STATE_HOME=/tmp/s loopherd install-hook",
        "source scripts/install",
        ". scripts/install",
        "bash -e -o pipefail scripts/install",
        "x=\"$(scripts/install)\"",
        "cargo run -- install-hook",
        "cargo run --release -- install-hook",
        "cargo run -q --bin loopherd -- install-hook --remove",
        "cargo r -- install-hook",
        "cargo +nightly run -- install-hook",
        "cargo test install-hook",
        "echo $(echo ')'; scripts/install)",
    ];
    const REBASES: &[&str] = &[
        "git rebase origin/main",
        "git -C /src/repo rebase -i HEAD~3",
        "git -c core.editor=true rebase -i HEAD~2",
        "/usr/bin/git rebase main",
        "git pull --rebase",
        "git pull --rebase=merges origin main",
        "git push --force",
        "git push --force-with-lease origin feat/x",
        "git push --force-with-lease=feat/x:abc origin feat/x",
        "git push -f origin feat/x",
        "git push -uf origin feat/x",
        "git push origin +feat/x",
        "GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push --force origin feat/x",
        "bash -c 'git push -f'",
        "cd /src/repo && git rebase main",
        "git pull -r",
        "git --git-dir /src/repo/.git rebase main",
        "git --work-tree /src/repo rebase main",
        "git --namespace ns rebase main",
        "git --config-env core.editor=ED rebase main",
        "git --super-prefix x/ rebase main",
        "echo \"`git rebase main`\"",
        "if ! out=\"$(git push --force-with-lease 2>&1)\"; then echo \"$out\"; fi",
        "</dev/null git push -f",
        "git push --mirror backup",
        "git rebase --abort",
        "git rebase --continue",
        "git rebase --quit",
        "git rebase --skip",
        "git rebase --help",
        "git rebase -x --abort main",
        "echo $(echo ')'; git rebase main)",
        "echo $(echo ')'; git push --force)",
    ];
    const ALLOWED: &[&str] = &[
        "gh pr view 12",
        "gh pr view 12 --json mergeable,mergedAt",
        "gh pr list --search merge",
        "gh pr create --title 'merge things' --body x",
        "gh api repos/o/r/pulls/12",
        "gh api repos/o/r/pulls/12/merged",
        "curl https://api.github.com/repos/o/r/pulls/12",
        "echo gh pr merge 12",
        "cat scripts/install",
        "git diff scripts/install",
        "scripts/install-check",
        "loopherd status",
        "loopherd report --kind question 'may I run install-hook?'",
        "git log --grep rebase",
        "git pull",
        "git pull --no-rebase",
        "git pull --rebase=false",
        "git push",
        "git push -u origin feat/x",
        "GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin feat/x",
        "git merge origin/main",
        "echo done # ; gh pr merge 12",
        "# then: git rebase main",
        "out=\"$(gh pr view 12)\"",
        "cargo test",
        "",
        "gh api repos/o/r/pulls/12/merge",
        "gh api --jq .merged repos/o/r/pulls/12/merge",
        "cargo run -- status",
        "cargo build",
        "rustup update",
        "rustup run stable cargo test",
        "setsid cargo build",
        "out=$(gh pr view 12)x; echo \"$out\"",
        "grep -rn CARGO_ALIAS_TEST= src; cargo test",
        "rg 'CARGO_ALIAS_B=' . && cargo b",
        "echo CARGO_ALIAS_$N=x; cargo x",
        "CARGO_ALIAS_BUILD=$X cargo build",
        "CARGO_ALIAS_BUILD='run -- herd set h lead.merge true' cargo build",
        "CARGO_ALIAS_TEST=\"$(cat alias)\" cargo test",
        "export CARGO_ALIAS_$N=x; cargo build",
        "CARGO_ALIAS_X='build --release' cargo x",
        "CARGO_ALIAS_X='run -- herd set' cargo build",
        "CARGO_ALIAS_X=$CMD cargo test",
        "CARGO_ALIAS_X=$CMD cargo y",
        "export PATH=\"$HOME/bin:$PATH\"; cargo nextest run",
        "N=$(git rev-parse HEAD) cargo nextest run",
        "echo $(echo ')') done",
        "echo \"$(echo ')')\" done",
        "'' gh pr merge 12",
        "gh pr create --title t --body \"$(cat <<'EOF'\n- Handles the edge\ncase where it is empty.\nEOF\n)\"",
        "gh pr create --title t --body \"$(cat <<'EOF'\n| case | result |\n|---|---|\nEOF\n)\"",
        "git commit -m \"$(cat <<'EOF'\nFix the empty herd (case 3)\nEOF\n)\"",
        "gh pr create --title t --body \"$(cat <<'EOF'\n- [ ] cargo test (case 1); case 2 next\nEOF\n)\"",
        "gh pr create --title t --body \"$(cat <<-EOF\n	it is a case in point\n	EOF\n)\"",
        "export -n RUST_LOG; cargo nextest run",
        "x=$(cat <<'EOF'\nBody\nEOF\n); echo \"$x\"",
        "gh pr create --body \"$(cat <<'EOF'\nBody\nEOF\n)\"",
        "x=$(cat <<'EOF'\nBody\n EOF\nEOF\n); echo \"$x\"",
        "echo \"$((1 << 2))\"",
    ];

    #[test]
    fn each_pattern_names_its_switch() {
        let tables = [
            (MERGES, Some(Switch::Merge)),
            (INSTALLS, Some(Switch::Install)),
            (REBASES, Some(Switch::Rebase)),
            (ALLOWED, None),
        ];
        for (commands, want) in tables {
            for command in commands {
                assert_eq!(needs(command), Vec::from_iter(want), "{command}");
            }
        }
    }

    #[test]
    fn a_trailing_git_option_without_its_value_still_reads_the_rest() {
        for option in ["-C", "-c", "--git-dir", "--work-tree"] {
            let command = format!("gh pr merge 3; git {option}");
            assert_eq!(needs(&command), [Switch::Merge], "{command}");
        }
    }

    #[test]
    fn a_panicking_matcher_denies_a_lead_and_frees_everyone_else() {
        let panics = |_: &str| -> Vec<Switch> { panic!("matcher bug") };
        let all_off = |_: &str| Ok(Switches::default());
        let pane = Pane {
            id: PANE,
            socket: Some(Path::new(SOCKET)),
        };
        let lead_ledger = ledger(vec![lead()]);
        let denied = decide(
            &lead_ledger,
            &pane,
            input("ls").as_bytes(),
            &all_off,
            &panics,
        )
        .unwrap()
        .expect("a lead was allowed after a panic");
        assert!(denied.contains("switch is off"), "{denied}");
        let on = |_: &str| Ok(all_on());
        let denied = decide(&lead_ledger, &pane, input("ls").as_bytes(), &on, &panics)
            .unwrap()
            .expect("no switch allows a forced restart");
        assert!(denied.contains("restart --force"), "{denied}");
        let worker = ledger(vec![entry("lh-1", None)]);
        assert_eq!(
            decide(&worker, &pane, input("ls").as_bytes(), &all_off, &panics).unwrap(),
            None
        );
    }

    #[test]
    fn a_lead_without_a_recorded_herd_is_denied_all_three() {
        let mut herdless = lead();
        herdless.herd = None;
        let l = ledger(vec![herdless]);
        let on = |_: &str| -> Result<Switches> { panic!("no herd to read") };
        for (commands, _) in tables() {
            for command in commands {
                let message = run_with(&l, Some(SOCKET), &input(command), &on)
                    .unwrap_or_else(|| panic!("{command} was allowed"));
                assert!(message.contains("no herd recorded"), "{message}");
            }
        }
    }

    #[test]
    fn deny_is_the_pre_tool_use_decision() {
        assert_eq!(
            deny("no"),
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "no",
            }})
        );
    }

    #[test]
    fn a_command_needing_two_switches_is_blocked_by_either_one_off() {
        let leads = ledger(vec![lead()]);
        for command in [
            "gh pr merge 12 && git rebase origin/main",
            "git push -f; gh pr merge 12",
        ] {
            assert_eq!(needs(command).len(), 2, "{command}");
            for off in [Switch::Merge, Switch::Rebase] {
                let message = run(&leads, only_off(off), command)
                    .unwrap_or_else(|| panic!("{command} with {off:?} off was allowed"));
                assert!(message.contains(&format!("`{}`", off.name())), "{message}");
            }
            assert_eq!(run(&leads, all_on(), command), None, "{command}");
        }
    }

    fn entry(name: &str, role: Option<Role>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: PANE.into(),
            session_id: Some("s1".into()),
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-02T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            herd: Some("h".into()),
            template: None,
            params: None,
            socket: Some(PathBuf::from(SOCKET)),
            process: None,
            role,
            after_merge: None,
        }
    }

    fn lead() -> AgentRecord {
        entry("lead-h", Some(Role::Lead))
    }

    fn ledger(entries: Vec<AgentRecord>) -> MemLedger {
        MemLedger(
            entries.into_iter().map(|e| (e.name.clone(), e)).collect(),
            vec![],
        )
    }

    fn input(command: &str) -> String {
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
        })
        .to_string()
    }

    fn run_with(
        ledger: &dyn Ledger,
        socket: Option<&str>,
        input: &str,
        switches: &dyn Fn(&str) -> Result<Switches>,
    ) -> Option<String> {
        let pane = Pane {
            id: PANE,
            socket: socket.map(Path::new),
        };
        tool_hook(ledger, &pane, input.as_bytes(), switches).unwrap()
    }

    fn run(ledger: &dyn Ledger, switches: Switches, command: &str) -> Option<String> {
        run_with(ledger, Some(SOCKET), &input(command), &|herd| {
            assert_eq!(herd, "h");
            Ok(switches)
        })
    }

    fn all_on() -> Switches {
        Switches {
            merge: true,
            install: true,
            rebase: true,
            ..Switches::default()
        }
    }

    fn only_off(switch: Switch) -> Switches {
        let mut s = all_on();
        match switch {
            Switch::Merge => s.merge = false,
            Switch::Install => s.install = false,
            Switch::Rebase => s.rebase = false,
            Switch::ForceRestart
            | Switch::Pause
            | Switch::HerdEdit
            | Switch::LeadDefaults
            | Switch::Config
            | Switch::OpaqueCargoAlias
            | Switch::Unreadable => {}
        }
        s
    }

    fn tables() -> [(&'static [&'static str], Switch); 3] {
        [
            (MERGES, Switch::Merge),
            (INSTALLS, Switch::Install),
            (REBASES, Switch::Rebase),
        ]
    }

    #[test]
    fn a_lead_with_the_switch_off_is_blocked_and_told_why() {
        let leads = ledger(vec![lead()]);
        for (commands, switch) in tables() {
            for command in commands {
                let message = run(&leads, only_off(switch), command)
                    .unwrap_or_else(|| panic!("{command} was allowed"));
                assert!(
                    message.contains(&format!("`{}` switch is off", switch.name()))
                        && message.contains(switch.humans_part())
                        && message.contains("lead-h"),
                    "{message}"
                );
                assert!(!message.contains('\n'), "{message}");
            }
        }
    }

    #[test]
    fn a_lead_with_the_switch_on_is_allowed() {
        let leads = ledger(vec![lead()]);
        for (commands, switch) in tables() {
            let mut others_off = Switches::default();
            match switch {
                Switch::Merge => others_off.merge = true,
                Switch::Install => others_off.install = true,
                Switch::Rebase => others_off.rebase = true,
                Switch::ForceRestart
                | Switch::Pause
                | Switch::HerdEdit
                | Switch::LeadDefaults
                | Switch::Config
                | Switch::OpaqueCargoAlias
                | Switch::Unreadable => {}
            }
            for command in commands {
                assert_eq!(run(&leads, others_off, command), None, "{command}");
            }
        }
    }

    #[test]
    fn a_worker_or_a_retired_lead_is_allowed_everything() {
        let mut retired = lead();
        retired.retired = true;
        for entries in [vec![entry("lh-1", None)], vec![retired]] {
            let l = ledger(entries);
            for (commands, _) in tables() {
                for command in commands {
                    assert_eq!(run(&l, Switches::default(), command), None, "{command}");
                }
            }
        }
    }

    #[test]
    fn a_lead_is_matched_on_socket_and_pane_only() {
        let leads = ledger(vec![lead()]);
        let off = |_: &str| Ok(Switches::default());
        let merge = input("gh pr merge 1");
        assert!(run_with(&leads, Some(SOCKET), &merge, &off).is_some());
        assert_eq!(
            run_with(&leads, Some("/run/other.sock"), &merge, &off),
            None
        );
        assert_eq!(run_with(&leads, None, &merge, &off), None);

        let mut socketless = lead();
        socketless.socket = None;
        assert_eq!(
            run_with(&ledger(vec![socketless]), Some(SOCKET), &merge, &off),
            None
        );
        let mut elsewhere = lead();
        elsewhere.pane_id = "w1:p2".into();
        assert_eq!(
            run_with(&ledger(vec![elsewhere]), Some(SOCKET), &merge, &off),
            None
        );
    }

    #[test]
    fn an_unreadable_herd_file_blocks_all_three() {
        let leads = ledger(vec![lead()]);
        let unreadable = |_: &str| Err(anyhow!("read /cfg/herds/h.toml: gone"));
        for (commands, _) in tables() {
            for command in commands {
                let message = run_with(&leads, Some(SOCKET), &input(command), &unreadable)
                    .unwrap_or_else(|| panic!("{command} was allowed"));
                assert!(message.contains("h.toml: gone"), "{message}");
            }
        }
        assert_eq!(
            run_with(&leads, Some(SOCKET), &input("cargo test"), &unreadable),
            None
        );
    }

    #[test]
    fn other_tools_and_unusable_input_are_allowed() {
        let leads = ledger(vec![lead()]);
        let off = |_: &str| Ok(Switches::default());
        for input in [
            "not json",
            "{}",
            r#"{"tool_name":"Bash"}"#,
            r#"{"tool_name":"Bash","tool_input":{"command":7}}"#,
            r#"{"tool_name":"Write","tool_input":{"command":"gh pr merge 1"}}"#,
        ] {
            assert_eq!(run_with(&leads, Some(SOCKET), input, &off), None, "{input}");
        }
    }

    #[test]
    fn it_runs_while_another_process_holds_the_ledger_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut file = FileLedger::new(path.clone());
        crate::ledger::Ledger::record(&mut file, lead()).unwrap();
        let lock_file = std::fs::File::create(path.with_extension("lock")).unwrap();
        let _held = Lock::try_take(lock_file).unwrap().unwrap();

        let message = run(&file, Switches::default(), "gh pr merge 1");

        assert!(message.is_some());
    }

    #[test]
    fn a_leads_forced_restart_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in [
            "loopherd restart lh-1 --force",
            "loopherd restart --force lh-1",
            "cd /x && loopherd restart lh-1 --force",
            "cargo run -- restart lh-1 --force",
        ] {
            assert_eq!(needs(command), [Switch::ForceRestart], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains("lead-h may not run `loopherd restart --force`")
                    && message.contains("the human forces a restart"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_plain_restart_or_a_workers_forced_one_is_allowed() {
        let leads = ledger(vec![lead()]);
        assert_eq!(
            run(&leads, Switches::default(), "loopherd restart lh-1"),
            None
        );
        let workers = ledger(vec![entry("lh-1", None)]);
        assert_eq!(
            run(
                &workers,
                Switches::default(),
                "loopherd restart lh-1 --force"
            ),
            None
        );
    }

    const PAUSES: &[&str] = &[
        "loopherd pause h",
        "loopherd pause --all --reason 'lunch'",
        "loopherd resume h",
        "loopherd resume --all",
        "loopherd pause --help",
        "loopherd resume",
        "/usr/local/bin/loopherd resume h",
        "cd /x && loopherd pause h",
        "bash -c 'loopherd resume h'",
        "env -u X loopherd pause h",
        "echo h | xargs loopherd resume",
        "out=$(loopherd pause h)",
        "cargo run -- pause h",
        "cargo run --release -- resume --all",
        "cargo run -- pause h --reason restart",
        "cargo run -- resume restart",
        "cargo run -- pause h --reason install-hook",
        "cargo +nightly run -- pause h",
        "cargo r -- pause h",
        "cargo --color always run -- pause h",
        "cargo run -q --bin loopherd -- resume --all",
        "cargo run pause h",
        "cargo run --release resume --all",
        "cargo r -q resume h",
        "cargo +nightly --color always r pause h",
        "cargo run --profile bench -- pause h",
        "cargo run --profile test -- resume h",
        "scripts/herdr-dev loopherd pause h",
        "scripts/herdr-dev cargo run -- resume h",
    ];

    #[test]
    fn a_leads_pause_or_resume_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in PAUSES {
            assert_eq!(needs(command), [Switch::Pause], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains("lead-h may not run `loopherd pause` or `loopherd resume`")
                    && message.contains("the human pauses and resumes herds"),
                "{message}"
            );
        }
    }

    #[test]
    fn pause_and_resume_pass_outside_a_leads_pane() {
        let workers = ledger(vec![entry("lh-1", None)]);
        for command in PAUSES {
            assert_eq!(
                run(&workers, Switches::default(), command),
                None,
                "{command}"
            );
        }
        let leads = ledger(vec![lead()]);
        let merge = input("loopherd pause h");
        let off = |_: &str| Ok(Switches::default());
        assert_eq!(run_with(&leads, None, &merge, &off), None);
        assert_eq!(
            run_with(&leads, Some("/run/other.sock"), &merge, &off),
            None
        );
    }

    #[test]
    fn words_that_only_mention_pause_need_nothing() {
        for command in [
            "loopherd status",
            "echo loopherd pause h",
            "cargo test pause",
            "cargo test resume -- --nocapture",
            "cargo test -- pause",
            "cargo nextest run -- pause",
            "cargo --color always test resume",
            "cargo bench -- resume",
            "grep -n 'loopherd pause' src/skill.md",
        ] {
            assert_eq!(needs(command), [], "{command}");
        }
    }

    const HERD_EDITS: &[&str] = &[
        "loopherd herd set h lead.merge true",
        "loopherd herd unset h lead.merge",
        "/home/me/.local/bin/loopherd herd set h lead.install true",
        "bash -c 'loopherd herd set h lead.rebase true'",
        "cd /x && loopherd herd unset h queue.max",
        "scripts/herdr-dev loopherd herd set h lead.merge true",
        "cargo run -- herd set h lead.merge true",
        "cargo r -q -- herd unset h lead.merge",
        "cargo +nightly run --release herd set h lead.merge true",
        "cargo run -- herd set h template.t.prompt '\"install-hook\"'",
        "cargo run -- herd set h template.t.prompt '\"pause\"'",
        "setsid loopherd herd set h lead.merge true",
        "setsid -f cargo run -- herd set h lead.merge true",
        "rustup run stable loopherd herd set h lead.merge true",
        "rustup run --install stable cargo run -- herd unset h lead.merge",
        "rustup run stable -- loopherd herd set h lead.merge true",
        "rustup run stable --install loopherd herd set h lead.merge true",
    ];

    #[test]
    fn a_leads_herd_set_or_unset_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in HERD_EDITS {
            assert_eq!(needs(command), [Switch::HerdEdit], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains("lead-h may not run `loopherd herd set` or `loopherd herd unset`")
                    && message.contains("the human edits herd files"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_leads_lead_defaults_write_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in [
            "loopherd lead-defaults --set merge",
            "loopherd lead-defaults --set=none",
            "bash -c 'loopherd lead-defaults --set merge,install'",
            "cargo run -- lead-defaults --set merge",
            "cargo r -q lead-defaults --set=rebase",
            "scripts/install --lead-defaults merge",
            "./scripts/install --no-splash --lead-defaults=none",
            "bash scripts/install --lead-defaults",
        ] {
            assert_eq!(needs(command), [Switch::LeadDefaults], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains("lead-h may not set lead defaults")
                    && message.contains("the human sets lead defaults"),
                "{message}"
            );
        }
        for command in [
            "loopherd lead-defaults",
            "cargo run -- lead-defaults",
            "cat ~/.config/loopherd/config.toml",
        ] {
            assert_eq!(needs(command), [], "{command}");
        }
        let workers = ledger(vec![entry("lh-1", None)]);
        assert_eq!(
            run(
                &workers,
                Switches::default(),
                "loopherd lead-defaults --set merge"
            ),
            None
        );
    }

    #[test]
    fn a_leads_config_write_or_setup_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in [
            "loopherd config set context_window 1000000",
            "loopherd config unset shepherd.context_limit",
            "loopherd setup",
            "loopherd setup --again",
            "loopherd setup --context limit=500000",
            "bash -c 'loopherd config set default_context_limit 500000'",
            "cargo run -- config set context_window 1000000",
            "cargo r -q config unset context_window",
            "cargo run -- setup --lead-defaults merge",
            "scripts/install --context limit=500000",
            "./scripts/install --no-splash --context=window=1000000",
        ] {
            assert_eq!(needs(command), [Switch::Config], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains("lead-h may not change loopherd's config")
                    && message.contains("the human sets loopherd's config"),
                "{message}"
            );
        }
        for command in [
            "loopherd config get context_window",
            "cargo run -- config get default_context_limit",
            "loopherd setup --help",
        ] {
            assert_eq!(needs(command), [], "{command}");
        }
        let workers = ledger(vec![entry("lh-1", None)]);
        assert_eq!(run(&workers, Switches::default(), "loopherd setup"), None);
    }

    #[test]
    fn a_cargo_run_herd_get_needs_nothing_whatever_its_key_or_value_words() {
        for command in [
            "cargo run -- herd get h install-hook",
            "cargo run -- herd get h restart --force",
            "cargo run -q --bin loopherd -- herd get h pause",
            "cargo r herd get h resume",
        ] {
            assert_eq!(needs(command), [], "{command}");
        }
        for command in [
            "cargo run -- herd get h x; cargo run -- install-hook",
            "cargo run -- install-hook herd get",
        ] {
            assert_eq!(needs(command), [Switch::Install], "{command}");
        }
    }

    #[test]
    fn cargo_options_cant_hide_a_command_behind_herd_get() {
        for (command, want) in [
            (
                "cargo run --config herd --config get pause h",
                Switch::Pause,
            ),
            ("cargo run -Z herd -Z get resume h", Switch::Pause),
            ("cargo run -C herd -C get install-hook", Switch::Install),
            (
                "cargo run --config herd --config get restart x --force",
                Switch::ForceRestart,
            ),
            ("cargo run --unknown herd get pause h", Switch::Pause),
        ] {
            assert_eq!(needs(command), [want], "{command}");
        }
    }

    #[test]
    fn runs_herd_get_never_covers_a_herd_set_or_unset() {
        for words in [
            "-- herd get h herd set h lead.merge true",
            "herd get h herd unset h lead.merge",
        ] {
            let args: Vec<String> = words.split(' ').map(String::from).collect();
            assert!(!runs_herd_get(&args), "{words}");
        }
    }

    #[test]
    fn set_or_unset_without_herd_before_it_is_no_herd_edit() {
        for command in [
            "cargo run -- report set",
            "cargo run -- report unset x",
            "loopherd report set",
        ] {
            assert_eq!(needs(command), [], "{command}");
        }
    }

    #[test]
    fn herd_set_passes_outside_a_leads_pane_and_herd_get_passes_everywhere() {
        let workers = ledger(vec![entry("lh-1", None)]);
        for command in HERD_EDITS {
            assert_eq!(
                run(&workers, Switches::default(), command),
                None,
                "{command}"
            );
        }
        for command in [
            "loopherd herd get h lead.merge",
            "cargo run -- herd get h lead.merge",
            "loopherd herds",
            "echo loopherd herd set h lead.merge true",
            "cargo test herd set",
        ] {
            assert_eq!(needs(command), [], "{command}");
        }
    }

    #[test]
    fn allowed_commands_pass_a_lead_with_every_switch_off() {
        let leads = ledger(vec![lead()]);
        for command in ALLOWED {
            assert_eq!(run(&leads, Switches::default(), command), None, "{command}");
        }
    }

    #[test]
    fn a_cargo_alias_assigned_in_the_script_is_judged_by_its_expansion() {
        let leads = ledger(vec![lead()]);
        for (command, want) in [
            (
                "CARGO_ALIAS_X='run -- herd set h lead.merge true' cargo x",
                Switch::HerdEdit,
            ),
            (
                "env CARGO_ALIAS_X='run -- herd set h lead.merge true' cargo x",
                Switch::HerdEdit,
            ),
            (
                "env -u FOO CARGO_ALIAS_HS='run -- herd set' cargo -q hs h lead.merge true",
                Switch::HerdEdit,
            ),
            (
                "export CARGO_ALIAS_X='run -- pause h'; cargo x",
                Switch::Pause,
            ),
            (
                "declare -x CARGO_ALIAS_X='run -- pause h'; cargo x",
                Switch::Pause,
            ),
            (
                "typeset -x CARGO_ALIAS_X='run -- pause h'; cargo x",
                Switch::Pause,
            ),
            (
                "local CARGO_ALIAS_X='run -- pause h'; cargo x",
                Switch::Pause,
            ),
            (
                "readonly CARGO_ALIAS_X='run -- pause h'; export CARGO_ALIAS_X; cargo x",
                Switch::Pause,
            ),
            (
                "CARGO_ALIAS_A=b CARGO_ALIAS_B='run -- install-hook' cargo a",
                Switch::Install,
            ),
            (
                "CARGO_ALIAS_MY_HOOK='run -- install-hook' cargo my-hook",
                Switch::Install,
            ),
            (
                "CARGO_ALIAS_MY_HOOK='run -- install-hook' cargo my.hook",
                Switch::Install,
            ),
            (
                "CARGO_ALIAS_X='run -- herd set' bash -c 'cargo x h lead.merge true'",
                Switch::HerdEdit,
            ),
            (
                "CARGO_ALIAS_X='run -- herd set h lead.merge true' eval cargo x",
                Switch::HerdEdit,
            ),
            (
                "CARGO_ALIAS_X='r -- restart --force' rustup run stable cargo +nightly x lh-1",
                Switch::ForceRestart,
            ),
            ("CARGO_ALIAS_R='run -- pause h' cargo r", Switch::Pause),
            ("CARGO_ALIAS_RUN=build cargo run -- pause h", Switch::Pause),
        ] {
            assert_eq!(needs(command), [want], "{command}");
            assert!(
                run(&leads, only_off(want), command).is_some(),
                "{command} was allowed"
            );
        }
    }

    #[test]
    fn a_cargo_alias_that_cant_be_read_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in [
            "CARGO_ALIAS_X=$CMD cargo x",
            "CARGO_ALIAS_X=\"$(cat alias)\" cargo x h lead.merge true",
            "CARGO_ALIAS_X=$(cat alias) cargo x",
            "CARGO_ALIAS_X=`cat alias` cargo x",
            "CARGO_ALIAS_X=\"run -- herd $(echo set) h lead.merge true\" cargo x",
            "CARGO_ALIAS_X=run$(echo ' -- herd set h lead.merge true') cargo x",
            "CARGO_ALIAS_X=run`echo ' -- herd set h lead.merge true'` cargo x",
            "CARGO_ALIAS_X=x cargo x",
            "env CARGO_ALIAS_X= cargo x",
            "N=X; export CARGO_ALIAS_$N='run -- herd set h lead.merge true'; cargo x",
            "export CARGO_ALIAS_X\"$EMPTY\"='run -- herd set h lead.merge true'; cargo x",
            "export CARGO_ALIAS_${N}='run -- pause h'; cargo clippy",
            "CARGO_ALIAS_\"$(echo X)\"='run -- pause h' cargo x",
            "CARGO_ALIAS_X=$(echo run)' -- herd set h lead.merge true' cargo x",
            "CARGO_ALIAS_X=`echo run`' -- herd set h lead.merge true' cargo x",
            "CARGO_ALIAS_X=$(echo run)x cargo x",
            "export CARGO_ALIAS_$(echo X)='run -- herd set h lead.merge true'; cargo x",
            "export CARGO_ALIAS_`echo X`='run -- herd set h lead.merge true'; cargo x",
            "CARGO_ALIAS_X='run -- pause h'; export CARGO_ALIAS_$(echo Y); cargo y",
            "CARGO_ALIAS_X=\"run -- `echo pause` h\" cargo x",
            "CARGO_ALIAS_X=run CARGO_ALIAS_X+=' -- herd set h lead.merge true' cargo x",
            "CARGO_ALIAS_X=$(echo ')' >/dev/null)'run -- pause h' cargo x",
            "export CARGO_ALIA$(echo S_X)='run -- pause h'; cargo x",
            "export $(echo CARGO_ALIAS_X)='run -- pause h'; cargo x",
            "N=CARGO_ALIAS_X; export $N='run -- pause h'; cargo x",
            "env CARGO_ALIA$(echo S_X)='run -- pause h' cargo x",
            "env $(printf 'CARGO_ALIAS_%s=run' X) cargo x -- pause h",
            "declare -n r=CARGO_ALIAS_X; r='run -- pause h'; export CARGO_ALIAS_X; cargo x",
            "local -n r=CARGO_ALIAS_X; r='run -- pause h'; cargo x",
            "typeset -xn r=CARGO_ALIAS_X; r='run -- pause h'; cargo x",
        ] {
            assert_eq!(needs(command), [Switch::OpaqueCargoAlias], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains("lead-h may not run cargo through a `CARGO_ALIAS_*`"),
                "{message}"
            );
        }
        let workers = ledger(vec![entry("lh-1", None)]);
        assert_eq!(
            run(&workers, Switches::default(), "CARGO_ALIAS_X=$CMD cargo x"),
            None
        );
    }

    fn assert_unreadable(command: &str) {
        let leads = ledger(vec![lead()]);
        let message =
            run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
        assert!(
            message.contains("lead-h may not run a command the hook can't read"),
            "{message}"
        );
    }

    #[test]
    fn a_backslash_newline_anywhere_is_denied() {
        for command in [
            "gh pr view 115 \\\n  --json state",
            "echo \"a\\\nb\"",
            "x=$(echo a \\\n b)",
            "x=\"$(cat <\\\n<EOF\n)\nEOF\ngh pr merge 1\n)\"",
            "echo \"$\\\n(gh pr merge 1)\"",
            "x=\"$(echo $\\\n'it\\'s' )\"; gh pr merge 1",
            "x=$(cat <<EOF\nit's\nEOF\\\n)\ngh pr merge 1\nEOF\n)",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn a_dollar_before_a_quote_is_denied() {
        for command in [
            "echo $$'\\'; gh pr merge 1 #'",
            "x=\"$(echo $$'\\' )\"; gh pr merge 1 #'",
            "x=$(echo $'it\\'s'); gh pr merge 1",
            "echo $'it\\'s'; gh pr merge 1",
            "echo $\"hi\"",
            "$'\\x67h' pr merge 1",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn an_expansion_holding_parentheses_or_a_heredoc_is_denied() {
        for command in [
            "x=\"$(echo ${y:-$(echo })<<E}\n)\"; gh pr merge 1\nE}\n)\"",
            "x=\"$(echo $[$(echo ])<<2]\n)\"; gh pr merge 1\n2]\n)\"",
            "x=\"$(echo ${y:-)}; gh pr merge 1)\"",
            "x=\"$(echo ${y//<<E/z}\n)\"; gh pr merge 1 #\"\nE/z}\n)\"",
            "echo ${x:- #}; gh pr merge 1",
            "echo ${x:-",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn nested_backticks_are_denied() {
        for command in [
            "echo \"`echo \\`gh pr merge 1\\``\"",
            "echo `echo \\`gh pr merge 1\\``",
            "echo `echo \\`gh pr merge 1\\`` #`",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn a_heredoc_body_that_leaves_a_quote_open_is_denied() {
        for command in [
            "cat > notes.md <<'EOF'\nIt's green, merging now.\nEOF\ngh pr merge 115 --squash",
            "cat > notes.md <<'EOF'\nIt's rebased.\nEOF\ngit rebase origin/main",
            "cat <<'EOF'\nsay \"hi\nEOF",
            "cat <<'EOF'\nuse ` here\nEOF",
            "cat <<'EOF'\nuse $(here\nEOF",
            "x=$(cat <<'EOF'\nit's\nEOF\ngh pr merge 1\n)",
            "gh pr create --body \"$(cat <<'EOF'\nIt's ready.\nEOF\n)\"; gh pr merge 1",
            "cat <<E\r\nx\nE\r\necho '\"' \"\nE\n\"; gh pr merge 1",
            "cat <<\\'\n'\n'; gh pr merge 1 #'",
            "x=$(cat <<'EOF'\n\"'\" '\nEOF\ngh pr merge 1\n)",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn a_shift_the_hook_would_read_as_a_heredoc_is_denied() {
        for gated in [
            "gh pr merge 1",
            "scripts/install",
            "loopherd herd set h lead.merge true",
            "git rebase origin/main",
        ] {
            for command in [
                format!("((y=1<<2))\necho '\"' \"\n2\n\"; {gated}"),
                format!("(( y = 1 << 2 ))\necho '\"' \"\n2\n\"; {gated}"),
                format!("a[1<<2]=x\necho '\"' \"\n2]=x\n\"; {gated}"),
                format!("((y=1<<2))\necho \"'\" '\n2\n'; {gated}"),
                format!("x=$( ((y=1<<2))\necho '\"' \"\n2\n\"; {gated}\n)"),
                format!("((y=1<<2 '\n')); {gated} #'\n2"),
                format!("((y=1<<\\'))\n'\n'; {gated} #'"),
                format!("a[1<<2]=x\n2]=x\n{gated}"),
            ] {
                assert_unreadable(&command);
            }
        }
    }

    #[test]
    fn a_heredoc_fed_to_a_shell_is_judged_line_by_line() {
        let leads = ledger(vec![lead()]);
        for command in [
            "bash <<'EOF'\ngh pr merge 1\nEOF",
            "x=$(bash <<'EOF'\ngh pr merge 1\nEOF\n)",
        ] {
            let message = run(&leads, only_off(Switch::Merge), command)
                .unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(message.contains("the human merges PRs"), "{message}");
        }
    }

    #[test]
    fn a_heredoc_after_a_parenthesis_and_hash_inside_a_substitution_is_denied() {
        for command in [
            "x=\"$(echo $(echo)#<<EOF\n)\nEOF\ngh pr merge 1\n)\"",
            "x=\"$(echo $((1))#<<EOF\n)\nEOF\ngh pr merge 1\n)\"",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn everyday_lead_commands_pass_with_every_switch_off() {
        let leads = ledger(vec![lead()]);
        for command in [
            "gh pr create --title t --body \"$(cat <<'EOF'\nIt's green and it's merged.\nEOF\n)\"",
            "gh pr create --title t --body \"$(cat <<'EOF'\nGreen, ready for review.\nEOF\n)\"",
            "cat > notes.md <<'EOF'\nIt's green and it's ready.\nEOF\ngh pr view 115 --json state",
            "cat <<'EOF' # notes\nGreen.\nEOF",
            "gh pr view 115 --json state",
            "loopherd feed loopherd-cards",
            "echo \"${HOME}/x\" ${arr[@]}",
            "gh pr comment 115 --body \"Fixed in \\`tool_hook.rs\\`; see \\`D17\\`.\"",
            "git commit -m \"Deny escaped backticks in \\`unreadable\\`\"",
            "gh pr create --body \"$(cat <<EOF\nRun \\`cargo test\\`.\nEOF\n)\"",
        ] {
            assert_eq!(run(&leads, Switches::default(), command), None, "{command}");
        }
    }

    #[test]
    fn a_heredoc_outside_a_substitution_still_shows_what_follows() {
        let leads = ledger(vec![lead()]);
        for command in [
            "cat <<'EOF'\nGreen.\nEOF\ngh pr merge 1",
            "cat <<'EOF' # notes\nGreen, it's\nEOF\ngh pr merge 1",
        ] {
            assert!(
                run(&leads, only_off(Switch::Merge), command).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn a_substitution_the_hook_cant_end_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in [
            "echo $(case x in x) gh pr merge 12;; esac)",
            "echo \"$(case x in x) echo hi;; esac)\"; gh pr merge 12",
            "x=$(case x in (x) loopherd pause h;; esac)",
            "echo \"$(true; case x in x) :;; esac)\"",
            "echo \"$(true&&case x in x) :;; esac)\"",
            "echo \"$(echo \"$(case x in x) echo;; esac)\")\"",
            "echo $(time case x in x) gh pr merge 12;; esac)",
            "echo $(time -p case x in x) gh pr merge 12;; esac)",
            "echo $(! time case x in x) gh pr merge 12;; esac)",
            "echo $(coproc case x in x) gh pr merge 12;; esac)",
            "echo $(function f case x in x) gh pr merge 12;; esac; f)",
            "echo $(\\\ncase x in x) gh pr merge 12;; esac)",
            "echo $(ca\\\nse x in x) gh pr merge 12;; esac)",
            "echo $({ case x in x) gh pr merge 12;; esac; })",
            "echo $(! case x in x) gh pr merge 12;; esac)",
            "echo $(if case x in x) gh pr merge 12;; esac; then :; fi)",
            "echo $(if true; then case x in x) gh pr merge 12;; esac; fi)",
            "gh pr create --body \"$(cat <<'EOF'\nBody\nEOF)\" && gh pr merge 1",
            "gh pr create --body \"$(cat <<'EOF'\nBody\nEOF)\"\ngh pr merge 1",
            "gh pr create --body \"$(cat <<EOF\nBody\nEOF)\" && gh pr merge 1",
            "git commit -m \"$(cat <<'EOF'\nFix it\nEOF)\" && git push --force",
            "git commit -m \"$(cat <<'EOF'\nFix it\nEOF)\" && git rebase origin/main",
            "gh pr create --body \"$(cat <<'EOF'\nBody\nEOF)\" && loopherd herd set h lead.merge true",
            "gh pr create --body \"$(cat <<'EOF'\nBody\nEOF)\" && scripts/install",
            "gh pr create --body \"$(cat <<'EOF'\nBody\nEOF )\" && gh pr merge 1",
            "x=$(cat <<'EOF'\nBody\nEOF)x\ngh pr merge 1",
            "x=$(cat <<'EOF'\nBody\nEOF); gh pr merge 1",
            "x=$(cat <<'EOF'\nBody\nEOF)\ngh pr merge 1",
            "x=$(cat <<-'EOF'\n	Body\n	EOF); gh pr merge 1",
            "x=$(cat <<'EOF'\nBody",
            "x=$(cat <<'EOF'\nit's, it's\nEOF)\ngh pr merge 1\nEOF\n)",
            "x=$(echo $((1<<2)); cat <<'EOF'\nit's, it's\nEOF)\ngh pr merge 1\nEOF\n)",
            "x=$(cat <<EOF\nit's\nEOF\\\n)\ngh pr merge 1\nEOF\n)",
            "x=$(cat <<-EOF\nit's\n	EOF\\\n)\ngh pr merge 1\nEOF\n)",
            "gh pr create --body \"$(cat <<EOF\nit's\nEOF\\\n)\" && gh pr merge 1\nEOF\n)\"",
            "x=$(cat <<EOF\nit's\nE\\\nOF\n)\ngh pr merge 1\nEOF\n)",
            "x=$(cat <<EOF\nit's\n\\\nEOF\n)\ngh pr merge 1\nEOF\n)",
            "x=$(cat <<EOF\nit's\nEO\\\nF)\ngh pr merge 1\nEOF\n)",
            "gh pr create --body \"$(echo $[1<<2]\n)\" && gh pr merge 1\n2]\n)\"",
            "x=\"$(echo $[1<<2]\n)\"; gh pr merge 1\n2]\n)\"",
            "x=\"$(echo ${y//<<E/z}\n)\"; gh pr merge 1\nE/z}\n)\"",
            "x=\"$(cat <<$'EOF'\na\nEOF\n)\"; gh pr merge 1\n$EOF\n)\"",
            "x=\"$(cat <<EOF)\"\n\"\nEOF\ngh pr merge 1",
            "x=\"$( ((cat <<EOF\n)\nEOF\n) ) )\"; gh pr merge 1",
        ] {
            assert_unreadable(command);
        }
        for command in [
            "echo $(echo case) done",
            "echo $(echo 'case x in x)')",
            "git commit -m \"$(cat <<'EOF'\nHandle EOF in parser\n\nEOF handling was wrong.\nEOF\n)\"",
            "gh pr create --body \"$(cat <<'EOF'\nEOF is reached early now.\nEOF\n)\"",
            "x=$(cat <<'EOF'\nEOFError fix\nEOF\n)",
            "x=$(cat <<'BODY'\nBODY text\nBODY\n)",
            "x=$(cat <<'MSG'\nMSGs are fine\nMSG\n)",
            "x=$(cat <<'END'\nENDPOINT changes\nEND\n)",
            "x=$(cat <<-EOF\n\tEOF is reached\n\tEOF\n)",
            "x=$(echo $((1<<2))\necho hi\n)",
            "x=$(echo \"$((1<<2))\"\necho hi\n)",
            "x=$(echo $((1 << 2)) $(( (1+2) * 3 ))\necho hi\n)",
            "x=$(echo ${y:-z} ${arr[@]}\necho hi\n)",
        ] {
            assert_eq!(run(&leads, Switches::default(), command), None, "{command}");
        }
        let workers = ledger(vec![entry("lh-1", None)]);
        assert_eq!(
            run(
                &workers,
                Switches::default(),
                "echo $(case x in x) :;; esac)"
            ),
            None
        );
    }

    #[test]
    fn a_command_after_an_env_dollar_word_heredoc_or_comment_is_judged() {
        let leads = ledger(vec![lead()]);
        for (command, want) in [
            ("env $HOME/bin/gh pr merge 12", Switch::Merge),
            ("env \"$HOME/bin/gh\" pr merge 12", Switch::Merge),
            ("env GH_TOKEN=x $HOME/bin/gh pr merge 12", Switch::Merge),
            (
                "env -u GITHUB_TOKEN ${HOME}/bin/gh pr merge 12",
                Switch::Merge,
            ),
            ("env nohup $HOME/bin/gh pr merge 12", Switch::Merge),
            ("env \"$PWD/scripts/install\"", Switch::Install),
            (
                "env $(git rev-parse --show-toplevel)/scripts/install --lead-defaults",
                Switch::LeadDefaults,
            ),
            (
                "env $HOME/.cargo/bin/loopherd herd set h lead.merge true",
                Switch::HerdEdit,
            ),
            ("env $HOME/.cargo/bin/loopherd pause h", Switch::Pause),
            ("env $HOME/bin/git rebase main", Switch::Rebase),
            (
                "env CARGO_ALIAS_X='run -- pause h' $HOME/.cargo/bin/cargo x",
                Switch::Pause,
            ),
            (
                "gh pr create --body \"$(cat <<'EOF'\nThe lead's hook's\nEOF\n)\" && gh pr merge 1",
                Switch::Merge,
            ),
            (
                "gh pr create --body \"$(cat <<'EOF'\nThe lead's hook's\nEOF\n)\"\ngh pr merge 1",
                Switch::Merge,
            ),
            (
                "gh pr create --body \"$(cat <<'EOF'\nThe lead's hook's\nEOF\n)\" && loopherd herd set h lead.merge true",
                Switch::HerdEdit,
            ),
            (
                "gh pr create --body \"$(cat <<'EOF'\nThe lead's hook's\nEOF\n)\" && scripts/install",
                Switch::Install,
            ),
            (
                "gh pr create --body \"$(cat <<EOF\nThe lead's hook's\nEOF\n)\" && gh pr merge 1",
                Switch::Merge,
            ),
            (
                "gh pr create --body \"$(cat << \"EOF\"\nThe lead's hook's\nEOF\n)\" && gh pr merge 1",
                Switch::Merge,
            ),
            (
                "git commit -m \"$(cat <<'EOF'\nDon't deny it, don't\nEOF\n)\" && git push --force",
                Switch::Rebase,
            ),
            (
                "git commit -m \"$(cat <<'EOF'\nDon't deny it, don't\nEOF\n)\" && git rebase origin/main",
                Switch::Rebase,
            ),
            (
                "x=$(cat <<'EOF'\nit's, it's\nEOF\n); gh pr merge 1",
                Switch::Merge,
            ),
            (
                "x=$(cat <<-'EOF'\n	it's, it's\n	EOF\n); gh pr merge 1",
                Switch::Merge,
            ),
            (
                "x=$(cat <<'A' <<'B'\nit's, it's\nA\nit's, it's\nB\n); gh pr merge 1",
                Switch::Merge,
            ),
            ("x=$(true # 1) don't\n); gh pr merge 1", Switch::Merge),
            ("echo $(true # )'\ngh pr merge 12\n#'\n)", Switch::Merge),
            (
                "echo $(echo \"`echo \")\"`\"; gh pr merge 12)",
                Switch::Merge,
            ),
            (
                "echo $(echo ')'; loopherd herd set h lead.merge true)",
                Switch::HerdEdit,
            ),
            (
                "eval \"export CARGO_ALIAS_X='run -- pause h'\"; cargo x",
                Switch::Pause,
            ),
        ] {
            assert!(
                needs(command).contains(&want),
                "{command}: {:?}",
                needs(command)
            );
            assert!(
                run(&leads, only_off(want), command).is_some(),
                "{command} was allowed"
            );
        }
    }
}
