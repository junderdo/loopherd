//! `loopherd tool-hook`: Claude's PreToolUse hook on Bash. Blocks a command
//! from a lead's pane that needs a switch its herd file leaves off (D17), or
//! a `loopherd restart --force` (D20), a `loopherd pause` or `resume`
//! (D23), a `loopherd herd set`, `unset` (D29) or `remove` (D43), a write of the lead
//! defaults (D31), a `loopherd config set`, `unset` or `setup` (D39), cargo
//! run through an alias it can't read, or a `$(…)` whose end it can't find,
//! which no switch allows.
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
    /// `loopherd herd set`, `unset` or `remove`, which could turn a lead's own
    /// switches on or delete its herd. Not a switch: always the human's.
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
    /// A `gh`, `git` or shell alias, which could name a gated command the
    /// hook can't see. Not a switch: always the human's.
    Alias,
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
            Switch::Alias => "alias",
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
            Switch::Alias => "the human sets gh, git and shell aliases",
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
            | Switch::Alias
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
            "loopherd: blocked: {} may not run `loopherd herd set`, `herd unset` or `herd remove`, which could turn its own switches on or delete its herd: {}; ask with `loopherd report --kind question`",
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
    if needed == Switch::Alias {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not set a `gh alias`, a `git -c alias.*` or a shell `alias`, which could hide a command its switches gate: {}; spell the command out, or ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    if needed == Switch::Unreadable {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not run a command the hook can't read, as with a backslash-newline, a `$'\\c…'` or `$\"…\"`, a `${{…}}` holding more than a name, a `\\`` in a backtick substitution, a heredoc without its terminator, or with a quote left open in a script that runs more than cat, tee, gh, git and the like, a `<<` in `((…))`, a space or `(` in an assignment's `a[…]`, a shell or `source` reading its script from `<(…)` or `<`, a `case` in `$(…)`, a glob or brace in the program's name, or a brace expansion in gh's or git's arguments: {}; rewrite it, or ask with `loopherd report --kind question`",
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
    needs_with(script, &mut Aliases::default(), false)
}

/// The `CARGO_ALIAS_*` variables assigned so far in a script.
#[derive(Clone, Default, PartialEq)]
struct Aliases {
    /// Values by the alias's upper-case name; `None` when it can't be read.
    values: std::collections::HashMap<String, Option<String>>,
    /// Whether one was assigned under a name that can't be read, or in a
    /// heredoc body that may run in this shell or a child, so any alias may
    /// be set.
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
/// counts for the rest of the script, exported or not. `in_shell` judges
/// heredoc bodies as commands whatever the script runs, as for a `bash -c`
/// or `eval` script whose output a shell may read.
fn needs_with(script: &str, aliases: &mut Aliases, in_shell: bool) -> Vec<Switch> {
    let Some(lexed) = commands(script).filter(|_| !unreadable(script)) else {
        return vec![Switch::Unreadable];
    };
    let read_bodies = in_shell || runs_code(&lexed.commands);
    judge(&lexed, aliases, read_bodies)
}

/// The switches `lexed` needs, judging each heredoc body that `read_bodies`
/// says may run where bash runs it, so the aliases it sees are the ones set
/// before it.
fn judge(lexed: &Lexed, aliases: &mut Aliases, read_bodies: bool) -> Vec<Switch> {
    let mut needs = vec![];
    let mut bodies = lexed.bodies.iter().peekable();
    for at in 0..=lexed.commands.len() {
        while let Some((_, body)) = bodies.next_if(|(from, _)| *from <= at) {
            match body {
                _ if !read_bodies => {}
                Some(body) => {
                    let mut seen = aliases.clone();
                    needs.extend(judge(body, &mut seen, true));
                    if seen != *aliases {
                        aliases.unreadable_name = true;
                    }
                }
                None => needs.push(Switch::Unreadable),
            }
        }
        if let Some(command) = lexed.commands.get(at) {
            needs.extend(needs_one(command, aliases));
        }
    }
    needs
}

/// Whether the script may run a heredoc's body, as its input or a file it
/// wrote: unless every command is a [`SINKS`] program, it may.
fn runs_code(commands: &[Vec<Word>]) -> bool {
    !commands.iter().all(|command| runs_nothing(command))
}

/// Whether a command runs only a [`SINKS`] program, or none, and sets only
/// lower-case variables, which no program reads from the environment, bar
/// zsh's [`ZSH_PATHS`].
fn runs_nothing(command: &[Word]) -> bool {
    let mut words = command.iter();
    while let Some(word) = words.next() {
        let text = word.text.as_str();
        if let Some(bare) = word.redirection() {
            if bare {
                words.next();
            }
        } else if is_assignment(text) {
            let name = text.split_once('=').map_or(text, |(n, _)| n);
            if word.name_expands()
                || name.contains(|c: char| c.is_ascii_uppercase())
                || ZSH_PATHS.contains(&name.split(['+', '[']).next().unwrap_or(name))
            {
                return false;
            }
        } else if !COMPOUND.contains(&text) {
            return word.substituted_at.is_none() && SINKS.contains(&text);
        }
    }
    true
}

/// A script's simple commands, and its heredoc bodies lexed as commands of
/// their own, which count only when the script may run them.
#[derive(Default)]
struct Lexed {
    commands: Vec<Vec<Word>>,
    /// Each body with the index of the command it runs before; `None` for
    /// one that doesn't lex.
    bodies: Vec<(usize, Option<Lexed>)>,
}

impl Lexed {
    fn extend(&mut self, other: Lexed) {
        let offset = self.commands.len();
        self.commands.extend(other.commands);
        self.bodies.extend(
            other
                .bodies
                .into_iter()
                .map(|(at, body)| (at + offset, body)),
        );
    }
}

/// A word with quotes removed, and where in `text` its first `$(…)` or
/// backtick substitution stood.
#[derive(Default)]
struct Word {
    text: String,
    substituted_at: Option<usize>,
    /// Any part of it quoted, so it can't be a keyword.
    quoted: bool,
    /// Where in `text` its first quote or backslash stood.
    quoted_at: Option<usize>,
    /// Its unquoted `*?[]{},` characters, a `.` for each `..`, and each `$`
    /// that opens a `${` or `$[`, in order.
    globs: String,
}

impl Word {
    /// Whether a `<` or `>` right after this word starts a redirection of
    /// its own, as bash reads `gh>/dev/null`; not after a descriptor
    /// (`2>`, `{fd}>`) or an operator already begun.
    fn ends_before_redirection(&self) -> bool {
        let text = self.text.as_str();
        !text.is_empty()
            && !text.ends_with(['<', '>', '&'])
            && !text.chars().all(|c| c.is_ascii_digit())
            && !(text.starts_with('{') && text.ends_with('}'))
            && !self.in_subscript()
    }

    /// Whether the word ends inside a `name[…]` subscript.
    fn in_subscript(&self) -> bool {
        self.text.matches('[').count() > self.text.matches(']').count()
    }

    /// Whether the word ends inside a subscript after a variable's name, as
    /// in an assignment `a[i]=x`.
    fn in_name_subscript(&self) -> bool {
        self.in_subscript()
            && self
                .text
                .split_once('[')
                .is_some_and(|(name, _)| is_name(name))
    }

    /// Whether the word is a redirection, and if so whether it's bare, so
    /// its target is the next word. A quoted operator (`"<<EOF"`) is an
    /// argument, not a redirection.
    fn redirection(&self) -> Option<bool> {
        let bare = redirection(&self.text)?;
        let target = self
            .text
            .trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '<' | '>' | '&' | '|'));
        let operator = self.text.len() - target.len();
        self.quoted_at
            .is_none_or(|at| at >= operator)
            .then_some(bare)
    }

    fn mark_substituted(&mut self) {
        self.substituted_at.get_or_insert(self.text.len());
    }

    /// Whether a `$` or a substitution may build the part before the first
    /// `=`, or the whole word when it has none.
    fn name_expands(&self) -> bool {
        let name = self.text.split_once('=').map_or(&*self.text, |(n, _)| n);
        name.contains('$') || self.substituted_at.is_some_and(|at| at <= name.len())
    }

    /// Whether bash may read the word as a glob or a brace expansion.
    fn pattern(&self) -> bool {
        let mut globs = self.globs.chars().peekable();
        let mut braces = vec![];
        let mut bracket = false;
        while let Some(c) = globs.next() {
            match c {
                '*' | '?' => return true,
                '$' => {
                    if globs.next_if_eq(&'{').is_some() {
                        braces.push(false);
                    } else {
                        globs.next_if_eq(&'[');
                    }
                }
                '{' => braces.push(true),
                '}' if braces.pop() == Some(true) => return true,
                '[' => bracket = true,
                ']' if bracket => return true,
                _ => {}
            }
        }
        false
    }

    /// Whether bash expands the word's braces: a `{…}` holding a `,` or `..`.
    fn braced(&self) -> bool {
        // For each open brace, whether it expands; `None` for a `${`.
        let mut braces: Vec<Option<bool>> = vec![];
        let mut globs = self.globs.chars().peekable();
        while let Some(c) = globs.next() {
            match c {
                '$' if globs.next_if_eq(&'{').is_some() => braces.push(None),
                '{' => braces.push(Some(false)),
                '}' if braces.pop() == Some(Some(true)) => return true,
                ',' | '.' => {
                    if let Some(Some(expands)) = braces.last_mut() {
                        *expands = true;
                    }
                }
                _ => {}
            }
        }
        false
    }
}

/// Whether `script` holds a form the lexer may read differently from bash,
/// wherever it sits, even in quotes: a backslash-newline, or a `${…}` or `$[…]` holding more than a plain name and operator.
fn unreadable(script: &str) -> bool {
    script.contains("\\\n")
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
/// `$(…)` or backtick substitution, including those in an unquoted heredoc
/// body, are lexed as commands of their own. `None` when the script ends
/// inside a quote, backtick or `$(…)`, a `$(…)` holds a `case`, a heredoc's
/// end can't be told, a `$'…'` can't be decoded, or bash may read a `<<` as a
/// shift or a backtick as escaped where the hook doesn't.
fn commands(script: &str) -> Option<Lexed> {
    let mut lexed = Lexed::default();
    let mut words = vec![];
    let mut word: Option<Word> = None;
    // Heredocs whose bodies start after this line, with the parentheses open
    // when each was read and the index of the command it ends.
    let mut heredocs: Vec<(Heredoc, usize, Option<usize>)> = vec![];
    // While in `((…))`, how many parentheses were open before it.
    let mut arithmetic_from: Option<usize> = None;
    let mut parens: Vec<Paren> = vec![];
    // After `case … in` or `;;`, a `)` ends a pattern.
    let mut in_patterns = false;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Bash keeps an assignment's `name[` or an array's `[` open to its `]`.
            ';' | '&' | '|' | '\n' | '(' | ')' | ' ' | '\t'
                if arithmetic_from.is_none()
                    && word.as_ref().is_some_and(|w| {
                        (w.in_name_subscript()
                            && words.iter().all(|w: &Word| is_assignment(&w.text)))
                            || (w.in_subscript()
                                && w.text.starts_with('[')
                                && w.globs.starts_with('[')
                                && parens.last() == Some(&Paren::Data))
                    }) =>
            {
                return None;
            }
            '\'' => {
                let w = word.get_or_insert_default();
                w.quoted = true;
                w.quoted_at.get_or_insert(w.text.len());
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.text.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_default();
                w.quoted = true;
                w.quoted_at.get_or_insert(w.text.len());
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => w.text.extend(chars.next()),
                        '$' if chars.peek() == Some(&'(') => {
                            chars.next();
                            w.mark_substituted();
                            lexed.extend(substituted(&substitution(&mut chars)?)?);
                        }
                        '`' => {
                            w.mark_substituted();
                            lexed.extend(self::commands(&backticked(&mut chars, true)?)?);
                        }
                        c => w.text.push(c),
                    }
                }
            }
            '#' if word.is_none() => while chars.next_if(|&c| c != '\n').is_some() {},
            '\\' => {
                let w = word.get_or_insert_default();
                w.quoted_at.get_or_insert(w.text.len());
                w.text.extend(chars.next());
            }
            // `2>&1`, `<&3` and `&>file` are redirections, not `&`.
            '&' if word.as_ref().is_some_and(|w| w.text.ends_with(['>', '<'])) => {
                word.get_or_insert_default().text.push(c);
            }
            '&' if chars.peek() == Some(&'>') => {
                if word.as_ref().is_some_and(Word::ends_before_redirection) {
                    words.extend(word.take());
                }
                word.get_or_insert_default().text.push(c);
            }
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                // Bash reads `<<` in `((…))` or a `name[…]` subscript as a shift.
                if arithmetic_from.is_some() || word.as_ref().is_some_and(Word::in_subscript) {
                    return None;
                }
                if parens.contains(&Paren::Process) {
                    return None;
                }
                if word.as_ref().is_some_and(Word::ends_before_redirection) {
                    words.extend(word.take());
                }
                let w = word.get_or_insert_default();
                w.text.push_str("<<");
                if chars.next_if_eq(&'<').is_some() {
                    w.text.push('<');
                } else {
                    let heredoc = heredoc_operator(&mut chars, &mut w.text)?;
                    heredocs.push((heredoc, parens.len(), None));
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                word.get_or_insert_default().mark_substituted();
                lexed.extend(substituted(&substitution(&mut chars)?)?);
            }
            '$' if chars.next_if_eq(&'$').is_some() => {
                word.get_or_insert_default().text.push_str("$$");
            }
            '$' if chars.next_if_eq(&'\'').is_some() => {
                let decoded = ansi_c(&mut chars)?;
                word.get_or_insert_default().text.push_str(&decoded);
            }
            // Bash translates `$"…"` through a message catalog the hook can't read.
            '$' if chars.peek() == Some(&'"') => return None,
            '`' => {
                word.get_or_insert_default().mark_substituted();
                lexed.extend(self::commands(&backticked(&mut chars, false)?)?);
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                let doubled = chars.peek() == Some(&c) && matches!(c, '(' | ')');
                let in_arithmetic = arithmetic_from.is_some();
                if c == '(' {
                    let text = word.as_ref().map_or("", |w| w.text.as_str());
                    // `@(…)`, `+(…)` and `!(…)` are extglob patterns.
                    if !in_arithmetic && text.ends_with(['@', '+', '!']) {
                        return None;
                    }
                    if doubled && !in_arithmetic {
                        arithmetic_from = Some(parens.len());
                    }
                    parens.push(if text.ends_with('=') || doubled || in_arithmetic {
                        Paren::Data
                    } else if text.ends_with(['<', '>']) {
                        Paren::Process
                    } else {
                        Paren::Subshell
                    });
                }
                words.extend(word.take());
                let first = |n: usize| {
                    words
                        .get(n)
                        .filter(|w: &&Word| !w.quoted)
                        .map(|w| w.text.as_str())
                };
                let closes_case = words.last().is_some_and(|w| w.text == "esac");
                let opens_case = first(0) == Some("case") && first(2) == Some("in") && !closes_case;
                if c == '|' && in_patterns {
                    words.iter_mut().for_each(|w| w.globs.clear());
                }
                if c == ')' {
                    let closes_data = parens.pop() == Some(Paren::Data);
                    if in_patterns || opens_case || in_arithmetic || closes_data {
                        words.iter_mut().for_each(|w| w.globs.clear());
                    }
                    in_patterns = false;
                    if let Some(from) = arithmetic_from {
                        // `((…)` then anything but `)` may be nested subshells, not arithmetic.
                        if parens.len() == from + 1 && chars.peek() != Some(&')') {
                            return None;
                        }
                        if parens.len() <= from {
                            arithmetic_from = None;
                        }
                    }
                } else if opens_case {
                    in_patterns = true;
                } else if closes_case {
                    in_patterns = false;
                }
                if c == ';' && chars.next_if(|&c| c == ';' || c == '&').is_some() {
                    chars.next_if_eq(&'&');
                    in_patterns = true;
                }
                if !words.is_empty() {
                    lexed.commands.push(std::mem::take(&mut words));
                    for (_, _, ends) in &mut heredocs {
                        ends.get_or_insert(lexed.commands.len());
                    }
                }
                if c == '\n' {
                    for (heredoc, depth, ends) in std::mem::take(&mut heredocs) {
                        // Bash starts the body after the line a `<(…)` or `((…))` ends.
                        if depth < parens.len() {
                            return None;
                        }
                        let mut body = String::new();
                        heredoc_body(&mut chars, &heredoc, &mut body)?;
                        let at = ends.unwrap_or(lexed.commands.len());
                        if !heredoc.quoted {
                            lexed.extend(expansions(&body)?);
                        }
                        lexed.bodies.push((at, commands(&body)));
                    }
                }
            }
            ' ' | '\t' => words.extend(word.take()),
            _ => {
                let glued = matches!(c, '<' | '>') && arithmetic_from.is_none();
                if glued && word.as_ref().is_some_and(Word::ends_before_redirection) {
                    words.extend(word.take());
                }
                let w = word.get_or_insert_default();
                // A lone `.` can't make a brace sequence; `..` records one `.`.
                let records = match c {
                    '.' => w.text.ends_with('.'),
                    '$' => matches!(chars.peek(), Some('{' | '[')),
                    _ => "*?[]{},".contains(c),
                };
                w.text.push(c);
                if records {
                    w.globs.push(c);
                }
            }
        }
    }
    words.extend(word.take());
    if !words.is_empty() {
        lexed.commands.push(words);
    }
    Some(lexed)
}

/// The commands a `$(…)` runs, given the text [`substitution`] read. A
/// `$((…))`, which it returns starting with `(`, runs only its substitutions.
fn substituted(text: &str) -> Option<Lexed> {
    if text.starts_with('(') {
        expansions(text)
    } else {
        commands(text)
    }
}

/// The `$(…)` and backtick substitutions in an unquoted heredoc body, which
/// bash runs wherever the body goes.
fn expansions(body: &str) -> Option<Lexed> {
    let mut lexed = Lexed::default();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '$' if chars.next_if_eq(&'(').is_some() => {
                lexed.extend(substituted(&substitution(&mut chars)?)?);
            }
            '`' => lexed.extend(commands(&backticked(&mut chars, false)?)?),
            _ => {}
        }
    }
    Some(lexed)
}

/// The text of a `$'…'` whose `$'` was just read, decoded as bash does: a
/// NUL ends the text, and a byte past ASCII reads as U+FFFD, which no program
/// name holds. `None` when it isn't closed or holds a `\c` escape.
fn ansi_c(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    let byte = |v: u32| match v & 0xff {
        v @ 0..0x80 => char::from(v as u8),
        _ => char::REPLACEMENT_CHARACTER,
    };
    let mut text = String::new();
    let mut ended = false;
    loop {
        let c = match chars.next()? {
            '\'' => return Some(text),
            '\\' => match chars.next()? {
                'a' => '\x07',
                'b' => '\x08',
                'e' | 'E' => '\x1b',
                'f' => '\x0c',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\x0b',
                c @ ('\\' | '\'' | '"' | '?') => c,
                d @ '0'..='7' => byte(digits(chars, 8, 2, d.to_digit(8))?),
                'x' if chars.next_if_eq(&'{').is_some() => {
                    let mut v = 0;
                    loop {
                        match chars.next()? {
                            '}' => break byte(v),
                            c => v = (v << 4 | c.to_digit(16)?) & 0xff,
                        }
                    }
                }
                'x' => match digits(chars, 16, 2, None) {
                    Some(v) => byte(v),
                    None => {
                        text.push('\\');
                        'x'
                    }
                },
                e @ ('u' | 'U') => match digits(chars, 16, if e == 'u' { 4 } else { 8 }, None) {
                    Some(v) => char::from_u32(v)?,
                    None => {
                        text.push('\\');
                        e
                    }
                },
                'c' => return None,
                c => {
                    text.push('\\');
                    c
                }
            },
            c => c,
        };
        ended |= c == '\0';
        if !ended {
            text.push(c);
        }
    }
}

/// Reads up to `max` more digits in `radix` onto `value`; `None` when there
/// are none at all.
fn digits(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    radix: u32,
    max: usize,
    mut value: Option<u32>,
) -> Option<u32> {
    for _ in 0..max {
        let Some(d) = chars.peek().and_then(|c| c.to_digit(radix)) else {
            break;
        };
        chars.next();
        value = Some(value.unwrap_or(0) * radix + d);
    }
    value
}

/// The text of a backtick substitution whose opening backtick was just read,
/// with `\\` and `\$` undone as bash does, and `\"` too in double quotes. `None` when it isn't closed, or
/// holds a `` \` ``, as in ``echo `a \`b\`` ``, where bash nests a
/// substitution the hook doesn't.
fn backticked(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    in_double_quotes: bool,
) -> Option<String> {
    let mut inner = String::new();
    loop {
        match chars.next()? {
            '\\' => match chars.next()? {
                '`' => return None,
                c @ ('\\' | '$') => inner.push(c),
                '"' if in_double_quotes => inner.push('"'),
                c => {
                    inner.push('\\');
                    inner.push(c);
                }
            },
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
    // Heredocs whose bodies start after this line, with the depth they were read at.
    let mut heredocs: Vec<(Heredoc, usize)> = vec![];
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
                for (heredoc, at) in std::mem::take(&mut heredocs) {
                    // Bash starts the body after the line a nested `$(…)` ends.
                    if at != depth {
                        return None;
                    }
                    heredoc_body(chars, &heredoc, &mut inner)?;
                }
                continue;
            }
            '<' if arithmetic_from.is_none() && chars.peek() == Some(&'<') => {
                chars.next();
                inner.push_str("<<");
                if chars.next_if_eq(&'<').is_some() {
                    inner.push('<');
                } else {
                    heredocs.push((heredoc_operator(chars, &mut inner)?, depth));
                }
                continue;
            }
            '$' if chars.next_if_eq(&'$').is_some() => {
                inner.push_str("$$");
                word.push(c);
                continue;
            }
            '$' if chars.next_if_eq(&'\'').is_some() => {
                inner.push_str("$'");
                word.push(c);
                loop {
                    let c = chars.next()?;
                    inner.push(c);
                    match c {
                        '\'' => break,
                        '\\' => inner.extend(chars.next()),
                        _ => {}
                    }
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

/// Bash's word breaks; other whitespace, such as a `\r`, is part of a word.
fn separates(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>'
    )
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

#[derive(PartialEq)]
enum Paren {
    Subshell,
    /// `<(…)` or `>(…)`, whose heredocs bash reads differently.
    Process,
    /// An array, or arithmetic.
    Data,
}

struct Heredoc {
    delimiter: String,
    /// `<<-`, which strips leading tabs.
    strips_tabs: bool,
    /// Any part of the delimiter quoted, so bash doesn't expand the body.
    quoted: bool,
}

/// Copies the rest of a heredoc operator whose `<<` was just read onto
/// `text`. `None` when the delimiter holds a `$` or a backtick, or a
/// backslash in double quotes, which bash may remove.
fn heredoc_operator(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    text: &mut String,
) -> Option<Heredoc> {
    let tabs = chars.next_if_eq(&'-').is_some();
    if tabs {
        text.push('-');
    }
    while let Some(c) = chars.next_if(|&c| c == ' ' || c == '\t') {
        text.push(c);
    }
    let mut delimiter = String::new();
    let mut quoted = false;
    while let Some(c) = chars.next_if(|&c| !separates(c)) {
        text.push(c);
        quoted |= matches!(c, '\'' | '"' | '\\');
        match c {
            '$' | '`' => return None,
            '\'' | '"' => {
                for q in chars.by_ref() {
                    text.push(q);
                    if q == c {
                        break;
                    }
                    if c == '"' && q == '\\' {
                        return None;
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
    Some(Heredoc {
        delimiter,
        strips_tabs: tabs,
        quoted,
    })
}

/// Copies a heredoc's body onto `text` through its `delimiter` line. `None`
/// when the script ends first, or a line starts with the delimiter and holds
/// a `)`, as `EOF)` does, which bash inside `$(…)` takes as the end.
fn heredoc_body(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    heredoc: &Heredoc,
    text: &mut String,
) -> Option<()> {
    let delimiter = heredoc.delimiter.as_str();
    while chars.peek().is_some() {
        let mut line = String::new();
        while let Some(c) = chars.next_if(|&c| c != '\n') {
            line.push(c);
        }
        chars.next();
        text.push_str(&line);
        text.push('\n');
        let stripped = if heredoc.strips_tabs {
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
const WRAPPERS: [(&str, &[&str]); 17] = [
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
    (
        "ionice",
        &[
            "-c",
            "-n",
            "-p",
            "-P",
            "-u",
            "--class",
            "--classdata",
            "--pid",
            "--pgid",
            "--uid",
        ],
    ),
    ("taskset", &[]),
    (
        "flock",
        &["-w", "--wait", "--timeout", "-E", "--conflict-exit-code"],
    ),
    // This repo's `scripts/herdr-dev`.
    ("herdr-dev", &[]),
];
const KEYWORDS: [&str; 10] = [
    "if", "then", "else", "elif", "do", "while", "until", "!", "{", "source",
];
/// Programs a brace expansion in the arguments could turn into a gated command.
const GATED: [&str; 7] = ["gh", "git", "loopherd", "cargo", "curl", "wget", "install"];
/// Programs that don't run a heredoc body fed to them as code. A body one of
/// them writes is data to this hook, though `git` and `gh` can later run a
/// written hook, alias or extension (D29 Limits).
const SINKS: [&str; 22] = [
    "cat", "tee", "echo", "gh", "git", "loopherd", "mkdir", "ls", "rm", "mv", "cp", "touch", "wc",
    "head", "tail", "grep", "diff", "jq", "true", "false", "test", "cd",
];
/// zsh ties these lower-case arrays to `PATH`, `CDPATH`, `FPATH` and
/// `MODULE_PATH`.
const ZSH_PATHS: [&str; 4] = ["path", "cdpath", "fpath", "module_path"];
/// Words that open or close a compound command and run nothing themselves.
const COMPOUND: [&str; 12] = [
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "!", "{", "}",
];
/// Reserved words after which `coproc`'s first word is its name.
const COPROC_COMPOUND: [&str; 9] = [
    "{", "while", "until", "if", "for", "select", "case", "[[", "((",
];
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];
/// Builtins whose arguments may assign variables.
const DECLARERS: [&str; 5] = ["export", "declare", "typeset", "local", "readonly"];

fn needs_one(command: &[Word], aliases: &mut Aliases) -> Vec<Switch> {
    // Bash takes a redirection out of the words wherever it stands.
    let mut kept: Vec<&Word> = vec![];
    let mut redirects: Vec<&Word> = vec![];
    let mut all = command.iter();
    while let Some(word) = all.next() {
        match word.redirection() {
            Some(bare) => {
                redirects.push(word);
                if bare {
                    all.next();
                }
            }
            None => kept.push(word),
        }
    }
    let texts: Vec<&str> = kept.iter().map(|w| w.text.as_str()).collect();
    if sets_alias_variable(&texts) {
        return vec![Switch::Alias];
    }
    // A shell's script from its input is out of the hook's sight, unless a
    // `<<` heredoc, whose body is judged on its own, feeds it.
    let reads_script_from_input = |script: Option<&String>| match script {
        None => !heredoc_feeds(&redirects, 0),
        Some(script) => match input_descriptor(script) {
            Some(Some(fd)) => !heredoc_feeds(&redirects, fd),
            Some(None) => true,
            None => false,
        },
    };
    let words: Vec<String> = kept.iter().map(|w| w.text.clone()).collect();
    let words = words.as_slice();
    let mut i = 0;
    let mut in_env = false;
    loop {
        let Some(word) = words.get(i) else {
            return vec![];
        };
        let program = file_name(word);
        if in_env && (word.contains('=') || word.is_empty()) && kept[i].name_expands() {
            // `env` may get an assignment whose name the hook can't read.
            aliases.assign(kept[i]);
            i += 1;
        } else if word.is_empty() && kept[i].substituted_at.is_some() {
            // An unquoted substitution that prints nothing leaves no word.
            i += 1;
        } else if is_assignment(word) {
            aliases.assign(kept[i]);
            i += 1;
        } else if kept[i].pattern()
            || ((word == "source" || word == ".") && reads_script_from_input(words.get(i + 1)))
        {
            return vec![Switch::Unreadable];
        } else if KEYWORDS.contains(&word.as_str()) || word == "." {
            i += 1;
        } else if word == "function" {
            i += 2;
        } else if word == "coproc" {
            // `coproc NAME { … }` names the coprocess; `coproc cmd args` doesn't.
            let opens_compound = kept
                .get(i + 2)
                .is_some_and(|w| !w.quoted && COPROC_COMPOUND.contains(&w.text.as_str()));
            i += if opens_compound { 2 } else { 1 };
        } else if word == "trap" {
            let action = skip_options(words, i + 1, &[]);
            return words.get(action).map_or(vec![], |script| {
                needs_with(script, &mut aliases.clone(), true)
            });
        } else if DECLARERS.contains(&program) {
            let args = &kept[i + 1..];
            let nameref = |w: &&Word| w.text.starts_with('-') && w.text.contains('n');
            if !["export", "readonly"].contains(&program) && args.iter().any(nameref) {
                // A nameref (`declare -n r=CARGO_ALIAS_X`) assigns through another name.
                aliases.unreadable_name = true;
            }
            args.iter().for_each(|w| aliases.assign(w));
            return vec![];
        } else if let Some((_, with_value)) = WRAPPERS.iter().find(|(w, _)| *w == program) {
            in_env |= program == "env";
            i = skip_options(words, i + 1, with_value);
            // The duration, CPU mask or lock file before the command.
            if ["timeout", "taskset", "flock"].contains(&program) {
                i += 1;
            }
            if program == "flock" && words.get(i).is_some_and(|w| w == "-c" || w == "--command") {
                return words.get(i + 1).map_or(vec![], |script| {
                    needs_with(script, &mut aliases.clone(), true)
                });
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
            let mut stdin_script = false;
            while let Some(option) = words.get(i).filter(|w| w.starts_with(['-', '+'])) {
                if option == "--" {
                    i += 1;
                    break;
                }
                if !option.starts_with("--") && option.contains('c') {
                    let script = skip_options(words, i + 1, &WITH_VALUE);
                    return words.get(script).map_or(vec![], |script| {
                        needs_with(script, &mut aliases.clone(), true)
                    });
                }
                stdin_script |= !option.starts_with("--") && option.contains('s');
                i += if WITH_VALUE.contains(&option.as_str()) {
                    2
                } else {
                    1
                };
            }
            if (stdin_script && !heredoc_feeds(&redirects, 0))
                || reads_script_from_input(words.get(i))
            {
                return vec![Switch::Unreadable];
            }
        } else if program == "eval" {
            return needs_with(&words[i + 1..].join(" "), aliases, true);
        } else if GATED.contains(&program) && kept[i + 1..].iter().any(|w| w.braced()) {
            return vec![Switch::Unreadable];
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
            if positional.starts_with(&["alias", "set"])
                || positional.starts_with(&["alias", "import"])
            {
                return Some(Switch::Alias);
            }
            (pr_merge || api_merge).then_some(Switch::Merge)
        }
        "alias" => args
            .iter()
            .any(|a| a.contains(['=', '$']) || a.is_empty())
            .then_some(Switch::Alias),
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
            Some(&"herd") => matches!(positional.get(1), Some(&"set" | &"unset" | &"remove"))
                .then_some(Switch::HerdEdit),
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

/// Whether `run_args` run loopherd's `herd get`, with no `herd set` or
/// `unset` hidden among the words.
fn runs_herd_get(run_args: &[String]) -> bool {
    loopherd_args(run_args).is_some_and(|rest| {
        rest.starts_with(&["herd".to_string(), "get".to_string()]) && !edits_herd(rest)
    })
}

/// The args `cargo run` hands loopherd, past cargo's options and `--`;
/// `None` when an option cargo might take a value for comes first.
fn loopherd_args(run_args: &[String]) -> Option<&[String]> {
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
            a if a.starts_with('-') => return None,
            _ => break,
        }
    }
    Some(run_args.get(i..).unwrap_or_default())
}

/// Whether `herd` then `set`, `unset` or `remove` appear in `args`, side by side.
fn edits_herd<S: AsRef<str>>(args: &[S]) -> bool {
    args.windows(2)
        .any(|w| w[0].as_ref() == "herd" && matches!(w[1].as_ref(), "set" | "unset" | "remove"))
}

/// Whether `cargo run` with `run_args` runs `config set` or `unset`, or a
/// `setup` that isn't just its help. Past an option it can't read, any such
/// words count.
fn edits_config(run_args: &[String]) -> bool {
    let (args, anywhere) = match loopherd_args(run_args) {
        Some(args) => (args, false),
        None => (run_args, true),
    };
    let starts = |at: usize| anywhere || at == 0;
    args.windows(2)
        .enumerate()
        .any(|(at, w)| starts(at) && w[0] == "config" && matches!(w[1].as_str(), "set" | "unset"))
        || args
            .iter()
            .enumerate()
            .any(|(at, a)| starts(at) && a == "setup" && runs_setup(&args[at + 1..]))
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
    let globals = &args[..start.min(args.len())];
    let sets_alias = globals.iter().enumerate().any(|(at, option)| {
        let value = match option.as_str() {
            "-c" | "--config-env" => globals.get(at + 1).map(String::as_str),
            _ => option
                .strip_prefix("--config-env=")
                .or_else(|| option.strip_prefix("-c")),
        };
        value.is_some_and(defines_git_alias)
    });
    if sets_alias {
        return Some(Switch::Alias);
    }
    let (subcommand, rest) = args.get(start..)?.split_first()?;
    if subcommand == "config" && rest.iter().any(|a| defines_git_alias(a)) {
        return Some(Switch::Alias);
    }
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

/// A git config key that defines an alias or pulls in a file that may.
fn defines_git_alias(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["alias.", "include.", "includeif."]
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

/// Whether a command sets a variable that sets git config or a shell alias:
/// by assignment, as a name a builtin sets (`read`, `export`, `printf -v`),
/// or as a nameref's target (`declare -n A=BASH_ALIASES`).
fn sets_alias_variable(words: &[&str]) -> bool {
    let names = |w: &str| w.starts_with("GIT_CONFIG") || w.starts_with("BASH_ALIASES");
    if words.iter().any(|w| is_assignment(w) && names(w)) {
        return true;
    }
    let mut rest = words
        .iter()
        .copied()
        .skip_while(|w| is_assignment(w) || matches!(*w, "builtin" | "command"));
    match rest.next() {
        Some(
            "read" | "export" | "declare" | "local" | "typeset" | "readonly" | "unset" | "mapfile"
            | "readarray" | "let" | "getopts" | "for" | "select",
        ) => rest.any(|w| names(w) || w.split_once('=').is_some_and(|(_, value)| names(value))),
        Some("printf") => {
            let mut after_v = false;
            rest.any(|w| {
                let named = (after_v && names(w)) || w.strip_prefix("-v").is_some_and(names);
                after_v = w == "-v";
                named
            })
        }
        _ => false,
    }
}

/// Where a shell script operand reads its script from: `Some(Some(fd))` for
/// one of the shell's own descriptors, `Some(None)` for another process's.
fn input_descriptor(script: &str) -> Option<Option<u32>> {
    if script == "-" {
        return Some(Some(0));
    }
    let mut parts: Vec<&str> = vec![];
    for part in script.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    if !script.starts_with('/') || !matches!(parts.first(), Some(&("dev" | "proc"))) {
        return None;
    }
    let fd = |n: &str| n.parse::<u32>().ok();
    match parts.as_slice() {
        [.., "dev", "stdin"] => Some(Some(0)),
        [.., "dev", "fd", n] => fd(n).map(Some),
        [.., "proc", "self" | "thread-self", "fd", n] => fd(n).map(Some),
        [.., "fd", n] if fd(n).is_some() => Some(None),
        [.., "stdin"] => Some(None),
        _ => None,
    }
}

/// Whether the last redirection of descriptor `fd` on `command` is a `<<`
/// heredoc, whose body the hook judges on its own.
fn heredoc_feeds(redirects: &[&Word], fd: u32) -> bool {
    let mut fed = false;
    for word in redirects {
        let digits: String = word.text.chars().take_while(char::is_ascii_digit).collect();
        let op = &word.text[digits.len()..];
        let target = match digits.parse::<u32>() {
            Ok(n) => n,
            Err(_) if op.starts_with('<') => 0,
            Err(_) => 1,
        };
        if target == fd {
            fed = op.starts_with("<<") && !op.starts_with("<<<");
        }
    }
    fed
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

/// Whether `word` is a redirection, and if so whether it's bare (`>`, `2>`,
/// `>&`), so its target is the next word.
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
    Some(target.is_empty())
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
        i += if takes_next_word(word, with_value) {
            2
        } else {
            1
        };
    }
    i.min(words.len())
}

/// Whether option word `word` takes the next word as its value. In a cluster
/// of short options (`-nw 5`), the first that takes a value ends it, and
/// takes the next word only when nothing follows it (`env -uC` unsets `C`).
fn takes_next_word(word: &str, with_value: &[&str]) -> bool {
    if word.starts_with("--") || with_value.contains(&word) {
        return with_value.contains(&word);
    }
    let shorts: Vec<char> = word.chars().skip(1).collect();
    shorts
        .iter()
        .position(|c| with_value.contains(&format!("-{c}").as_str()))
        .is_some_and(|at| at == shorts.len() - 1)
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
        let name = match name.split_once('[') {
            Some((array, subscript)) if subscript.ends_with(']') => array,
            _ => name,
        };
        is_name(name)
    })
}

fn is_name(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with(|c: char| c.is_ascii_digit())
        && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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
            released_at: None,
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
            | Switch::Alias
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
                | Switch::Alias
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
        "loopherd herd remove h",
        "loopherd herd remove --dry-run h",
        "cargo run -- herd remove --keep-history h",
        "cd /x && scripts/herdr-dev loopherd herd remove h",
    ];

    #[test]
    fn a_leads_herd_set_unset_or_remove_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in HERD_EDITS {
            assert_eq!(needs(command), [Switch::HerdEdit], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains(
                    "lead-h may not run `loopherd herd set`, `herd unset` or `herd remove`"
                ) && message.contains("the human edits herd files"),
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
            "cargo run --some-new-option x -- setup",
            "scripts/install --context limit=500000",
            "./scripts/install --no-splash --context=window=1000000",
            "bash scripts/install --context limit=500000",
            "env FOO=1 loopherd setup",
            "command loopherd config unset default_context_limit",
            "/home/u/.local/bin/loopherd setup --again",
            "./target/debug/loopherd config set context_window 1000000",
            "timeout 5 target/release/loopherd setup",
            "sh -c 'loopherd config unset context_window'",
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
            "env loopherd config get context_window",
            "./target/debug/loopherd config get context_window",
            "cargo run -- feed h setup",
            "cargo run -- report --body setup",
            "cargo run -- feed h config set context_window 1",
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
            "herd get h herd remove h",
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
    fn a_heredoc_fed_to_a_shell_that_leaves_a_quote_open_is_denied() {
        for command in [
            "cat <<'EOF' | bash\nsay \"hi\nEOF",
            "bash <<'EOF'\nuse ` here\nEOF",
            "x=$(bash <<'EOF'\nuse $(here\nEOF\n)",
        ] {
            assert_unreadable(command);
        }
        assert_judged(
            "cat <<E\r\nx\nE\r\necho '\"' \"\nE\n\"; gh pr merge 1",
            Switch::Merge,
        );
        for command in [
            "cat <<'EOF'\nsay \"hi\nEOF",
            "cat <<'EOF'\nuse ` here\nEOF",
            "cat <<'EOF'\nuse $(here\nEOF",
            "cat <<\\'\n'\necho '; gh pr merge 1 #'",
        ] {
            assert_passes(command);
        }
        assert_judged(
            "x=$(cat <<'EOF'\n\"'\" '\nEOF\ngh pr merge 1\n)",
            Switch::Merge,
        );
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
            "loopherd release lh-139 --drop-branch",
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

    fn assert_judged(command: &str, want: Switch) {
        let leads = ledger(vec![lead()]);
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

    fn assert_passes(command: &str) {
        let leads = ledger(vec![lead()]);
        assert_eq!(run(&leads, Switches::default(), command), None, "{command}");
    }

    #[test]
    fn a_heredoc_body_is_data_so_what_follows_its_terminator_is_judged() {
        for (gated, want) in [
            ("gh pr merge 12", Switch::Merge),
            ("git rebase origin/main", Switch::Rebase),
            ("git push --force", Switch::Rebase),
            ("scripts/install", Switch::Install),
            ("loopherd herd set h lead.merge true", Switch::HerdEdit),
        ] {
            for command in [
                format!("cat <<EOF\nI don't think so\nEOF\n{gated}"),
                format!("cat <<'EOF'\nI don't think so\nEOF\n{gated}"),
                format!("cat <<\"EOF\"\nI don't think so\nEOF\n{gated}"),
                format!("cat <<-EOF\n\tI don't think so\n\tEOF\n{gated}"),
                format!("cat > notes.md <<'EOF'\nsay \"hi\nuse ` and $(here\nEOF\n{gated}"),
                format!("x=$(cat <<'EOF'\nit's\nEOF\n{gated}\n)"),
                format!("x=$(cat <<EOF\nit's $(echo hi) and `echo hi`\nEOF\n{gated}\n)"),
                format!("gh pr create --body \"$(cat <<'EOF'\nIt's ready.\nEOF\n)\"; {gated}"),
            ] {
                assert_judged(&command, want);
            }
        }
    }

    #[test]
    fn a_substitution_in_an_unquoted_heredoc_body_is_judged() {
        for command in [
            "cat <<EOF\nit's $(gh pr merge 1)\nEOF",
            "cat <<EOF\nit's `gh pr merge 1`\nEOF",
            "x=$(cat <<EOF\nit's $(gh pr merge 1)\nEOF\n)",
        ] {
            assert_judged(command, Switch::Merge);
        }
        for command in [
            "cat <<EOF\nuse $(case x in y) here\nEOF",
            "cat <<EOF\nuse ` here\nEOF",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn a_heredoc_that_only_writes_a_file_passes_whatever_its_body_says() {
        for command in [
            "cat > review.md <<'EOF'\nIt's not merged; don't run gh pr merge 12 yet.\nEOF",
            "cat > review.md <<'EOF'\nThe lead's fix needs scripts/install rerun.\nEOF",
            "cat >> review.md <<EOF\nIt's fine; git rebase origin/main isn't needed.\nEOF",
            "tee review.md <<'EOF' >/dev/null\nWe can't loopherd herd set h lead.merge true.\nEOF",
            "gh pr create --title t --body \"$(cat <<'EOF'\nIt's (mostly) done; the case of `gh pr merge 1` and $(case x in y) is text.\nEOF\n)\"",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn a_heredoc_fed_to_a_shell_or_eval_is_read_as_commands() {
        for command in [
            "bash <<'EOF'\ngh pr merge 1\nEOF",
            "cat <<'EOF' | sh\ngh pr merge 1\nEOF",
            "eval \"$(cat <<'EOF'\ngh pr merge 1\nEOF\n)\"",
            "source /dev/stdin <<'EOF'\ngh pr merge 1\nEOF",
            ". /dev/stdin <<'EOF'\ngh pr merge 1\nEOF",
            "cat > s.sh <<'EOF'\ngh pr merge 1\nEOF\nchmod +x s.sh; ./s.sh",
            "cat > s.sh <<'EOF'\ngh pr merge 1\nEOF\nnohup /tmp/s.sh",
        ] {
            assert_judged(command, Switch::Merge);
        }
        assert_unreadable("bash <<'EOF'\necho it's\nEOF");
        assert_passes("bash <<'EOF'\necho hi\nEOF");
    }

    #[test]
    fn a_heredoc_without_its_terminator_or_whose_start_line_runs_on_is_denied() {
        for command in [
            "cat <<'EOF'\nit's\ngh pr merge 1",
            "cat <<EOF\nno end",
            "x=$(cat <<'EOF' $(echo\n)\nit's\nEOF\ngh pr merge 1\n)",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn ansi_c_and_locale_quoting_are_decoded() {
        for command in [
            "$'gh' pr merge 12",
            "echo $'it\\'s'; gh pr merge 12",
            "$'\\x67h' pr merge 12",
            "$'\\147h' pr merge 12",
            "$'\\u0067h' pr merge 12",
            "echo $$'\\'; gh pr merge 12 #'",
            "x=\"$(echo $$'\\' )\"; gh pr merge 12 #'",
            "x=$(echo $'it\\'s'); gh pr merge 12",
            "x=$(echo $'\\\\'); gh pr merge 12",
        ] {
            assert_judged(command, Switch::Merge);
        }
        for command in ["$'g\\ch' pr merge 12", "echo $'it", "$\"gh\" pr merge 12"] {
            assert_unreadable(command);
        }
        for command in [
            "printf '%s' $'\\n'",
            "echo \"it costs $'5'\"",
            "git log --format=$'%h\\t%s'",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn a_glob_or_brace_in_the_program_word_is_denied() {
        for command in [
            "{gh,} pr merge 12",
            "/usr/bin/g? pr merge 12",
            "/usr/bin/g* pr merge 12",
            "/usr/bin/g[h] pr merge 12",
            "sudo {gh,} pr merge 12",
            "x=1 g? pr merge 12",
            "{echo,hi}",
        ] {
            assert_unreadable(command);
        }
        for command in [
            "ls *.rs src/[a-z]*.rs {a,b}.md",
            "\"{gh,}\" pr view 12",
            "'g?' --version",
            "${HOME}/bin/gh pr view 12",
            "[ -f Cargo.toml ]",
            "[[ -f Cargo.toml ]]",
            "{ echo hi; }",
            "rg 'a*b' src",
            "a[1]=x",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn a_heredoc_inside_a_shell_or_eval_script_is_read_as_commands() {
        for command in [
            "bash -c 'cat <<EOF\ngh pr merge 1\nEOF' | bash",
            "eval 'cat <<EOF\ngh pr merge 1\nEOF' | sh",
            "bash -c 'cat > s.sh <<EOF\ngh pr merge 1\nEOF'; bash s.sh",
        ] {
            assert_judged(command, Switch::Merge);
        }
    }

    #[test]
    fn a_file_a_heredoc_wrote_and_the_script_may_run_is_read_as_commands() {
        for run in [
            "timeout 30 ./s.sh",
            "2> err ./s.sh",
            "nice -n 5 ./s.sh",
            "sudo -u x ./s.sh",
            "PATH=$PWD:$PATH s.sh",
            "rbash s.sh",
            "\"$BASH\" s.sh",
            "$SHELL s.sh",
            "sudo -s < s.sh",
            "ssh host < s.sh",
        ] {
            assert_judged(
                &format!("cat > s.sh <<'EOF'\ngh pr merge 1\nEOF\nchmod +x s.sh; {run}"),
                Switch::Merge,
            );
        }
        for shell in ["rbash", "\"$BASH\"", "$SHELL", "sudo -s", "ssh host"] {
            assert_judged(
                &format!("{shell} <<'EOF'\ngh pr merge 1\nEOF"),
                Switch::Merge,
            );
        }
    }

    #[test]
    fn a_backtick_substitution_is_read_with_its_escapes_undone() {
        assert_judged("echo `echo \\\\'; gh pr merge 1 #'`", Switch::Merge);
        assert_judged("echo `echo \\$(gh pr merge 1)`", Switch::Merge);
        assert_passes("echo `echo \\\\`");
    }

    #[test]
    fn ansi_c_braced_hex_nul_and_high_bytes_decode_as_bash_does() {
        for command in [
            "$'\\x{67}h' pr merge 12",
            "$'\\x{167}h' pr merge 12",
            "$'gh\\0x' pr merge 12",
        ] {
            assert_judged(command, Switch::Merge);
        }
        assert_unreadable("$'\\x{67' pr merge 12");
        for command in [
            "find . -print0 | while IFS= read -r -d $'\\0' f; do echo \"$f\"; done",
            "printf '%s\\n' $'\\xe2\\x9c\\x93'",
            "echo $'\\777'",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn globs_in_case_patterns_arrays_and_arithmetic_pass() {
        for command in [
            "case \"$1\" in start) echo s;; *) echo usage;; esac",
            "case x in\n  *) echo hi ;;\nesac",
            "files=(src/*.rs)",
            "(( x*2 ))",
        ] {
            assert_passes(command);
        }
        for command in [
            "(g? pr merge 1)",
            "( {gh,} pr merge 1 )",
            "(echo; g? pr merge 1)",
            "(case x in a) echo;; esac; g? pr merge 1)",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn a_heredoc_body_is_read_as_commands_unless_every_command_is_a_known_sink() {
        let body = "<<'EOF'\ngh pr merge 1\nEOF";
        let written = "cat > s.sh <<'EOF'\ngh pr merge 1\nEOF\n";
        for command in [
            format!("$(cat {body}\n)"),
            format!("`cat {body}\n`"),
            format!("{written}$(<s.sh)"),
            format!("timeout 30 $SHELL {body}"),
            format!("time -p $SHELL {body}"),
            format!("stdbuf -oL $SHELL {body}"),
            format!("nice -n 5 $x {body}"),
            format!("unshare {body}"),
            format!("{written}timeout 30 d/s"),
            format!("{written}hash -p s.sh xq; xq"),
            format!("{written}find s.sh -exec {{}} \\;"),
            format!("{written}alias xq=s.sh"),
            format!("{written}PAGER=s.sh; git log"),
        ] {
            assert_judged(&command, Switch::Merge);
        }
        for command in [
            "cat > notes.md <<'EOF'\nIt's green.\nEOF\ngit add notes.md && git commit -F notes.md",
            "body=$(cat <<'EOF'\nIt's done; gh pr merge 1 is the human's.\nEOF\n)\ngh pr comment 1 --body \"$body\"",
            "mkdir -p d && cat > d/notes.md <<'EOF'\nIt's green.\nEOF",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn a_backslash_in_a_double_quoted_heredoc_delimiter_is_denied() {
        for command in [
            "cat <<\"X\\$\"\nX$\ngh pr merge 1\nX\\$",
            "cat <<\"X\\`\"\nX`\ngh pr merge 1\nX\\`",
        ] {
            assert_unreadable(command);
        }
        assert_passes("cat <<'X\\'\nit's\nX\\");
    }

    #[test]
    fn locale_quoting_is_denied() {
        for command in ["$\"gh\" pr merge 12", "echo $\"hi\"", "x=$(echo $\"hi\")"] {
            assert_unreadable(command);
        }
        assert_passes("echo \"it costs $5\"");
    }

    #[test]
    fn nested_subshells_or_an_empty_case_dont_hide_a_glob() {
        for command in [
            "((echo a) ); ( {gh,} pr merge 1 )",
            "case x in esac; ( {gh,} pr merge 1 )",
            "shopt -s extglob; bin/@(g)h pr merge 1",
            "bin/+(g)h pr merge 1",
            "bin/!(x)h pr merge 1",
        ] {
            assert_unreadable(command);
        }
        for command in ["((x = a + (1) ))", "(( (1+2) * 3 ))", "a+=(x y)"] {
            assert_passes(command);
        }
    }

    #[test]
    fn an_escaped_double_quote_in_a_double_quoted_backtick_is_undone() {
        assert_judged(
            "echo \"`echo \\\"'\\\"; gh pr merge 1; echo \\\"'\\\"`\"",
            Switch::Merge,
        );
    }

    #[test]
    fn a_brace_expansion_in_a_gated_programs_arguments_is_denied() {
        for command in [
            "gh pr {merge,} 1",
            "git {rebase,} origin/main",
            "loopherd herd {set,} h lead.merge true",
            "gh pr merg{e..e} 1",
        ] {
            assert_unreadable(command);
        }
        for command in [
            "ls {a,b}.md",
            "git add src/*.rs",
            "echo {a,b}",
            "gh api repos/{owner}/{repo}/pulls/1",
            "gh pr view 1 --json title --jq '{a,b}'",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn only_space_and_tab_separate_words_as_in_bash() {
        for blank in ["\r", "\u{b}", "\u{c}", "\u{a0}", "\u{2028}"] {
            assert_judged(
                &format!("cat <<E{blank}\nx\nE{blank}\ngh pr merge 1\nE"),
                Switch::Merge,
            );
            assert_judged(&format!("true{blank}#;gh pr merge 1"), Switch::Merge);
        }
        assert_judged(
            "cat > notes.md <<'EOF'\r\nIt's ready\nEOF\r\ngh pr merge 1\nEOF",
            Switch::Merge,
        );
        assert_judged("echo hi\r# ; gh pr merge 1", Switch::Merge);
    }

    #[test]
    fn a_heredoc_left_open_across_a_newline_in_parentheses_is_denied() {
        for command in [
            "cat <<'true' <(true\ngh pr merge 1\ntrue\n)\ntrue\n",
            "cat <<'true' && ((\n$(gh pr merge 1)\ntrue\n))\ntrue\n",
        ] {
            assert_unreadable(command);
        }
        assert_passes("(cat <<'EOF'\nIt's green.\nEOF\n)");
    }

    #[test]
    fn sinks_exclude_programs_that_can_run_another() {
        let written = "cat > s.sh <<'EOF'\ngh pr merge 1\nEOF\n";
        for command in [
            format!("{written}rg --pre=sh . s.sh"),
            format!("sort --compress-program=sh <<'EOF' >/dev/null\ngh pr merge 1\nEOF"),
            format!("printf -v PATH '%s' \"$PWD:$PATH\"; git <<'EOF'\ngh pr merge 1\nEOF"),
        ] {
            assert_judged(&command, Switch::Merge);
        }
    }

    #[test]
    fn a_heredoc_body_is_judged_where_it_runs_among_the_commands() {
        assert_judged(
            "export CARGO_ALIAS_X='run -- herd set h lead.merge true'\nbash <<'EOF'\ncargo x\nEOF\nCARGO_ALIAS_X=build",
            Switch::HerdEdit,
        );
    }

    #[test]
    fn a_variable_or_quoted_case_doesnt_hide_a_glob_or_brace() {
        for command in [
            "g$x[h] pr merge 1",
            "$@{gh,} pr merge 1",
            "gh pr $@{merge,} 1",
            "'case' x in; (g? pr merge 1)",
        ] {
            assert_unreadable(command);
        }
    }

    #[test]
    fn a_hash_after_dollar_dollar_or_ansi_c_in_a_substitution_is_no_comment() {
        for word in ["$$#", "$'x'#"] {
            assert_judged(
                &format!("cat <<cat $(echo {word}) <<B\nB\ncat\n)\nB\ngh pr merge 1\ncat"),
                Switch::Merge,
            );
        }
    }

    #[test]
    fn a_body_is_judged_right_after_its_own_command() {
        let gated = "export CARGO_ALIAS_X='run -- herd set h lead.merge true'\n";
        for command in [
            format!("{gated}bash <<'EOF'; CARGO_ALIAS_X=build\ncargo x\nEOF"),
            format!("{gated}bash <<'EOF'\nCARGO_ALIAS_X=build\nEOF\ncargo x"),
            format!("{gated}sh <<'EOF'\nexport CARGO_ALIAS_X=build\nEOF\ncargo x"),
        ] {
            assert!(
                needs(&command)
                    .iter()
                    .any(|n| matches!(n, Switch::HerdEdit | Switch::OpaqueCargoAlias)),
                "{command}: {:?}",
                needs(&command)
            );
        }
    }

    #[test]
    fn a_redirection_glued_to_the_program_ends_its_name() {
        for command in [
            "gh<<EOF pr merge 1\nhi\nEOF",
            "gh>/dev/null pr merge 1",
            "gh&>/dev/null pr merge 1",
            "gh</dev/null pr merge 1",
            "gh 2>/dev/null pr merge 1",
            "gh > out pr merge 1",
        ] {
            assert_judged(command, Switch::Merge);
        }
        assert_passes("gh pr view 1 2>&1 >/dev/null");
    }

    #[test]
    fn the_word_after_a_bare_duplicating_redirection_is_its_target() {
        let leads = ledger(vec![lead()]);
        for command in [
            "gh >& x pr merge 1",
            ">& x gh pr merge 1",
            "gh <& 0 pr merge 1",
            "gh >& 2 pr merge 1",
            "gh 2>& x pr merge 1",
            "loopherd >& x herd set h lead.merge true",
            "env >& x loopherd pause",
            "git >& x rebase main",
            "git >& x push --force origin main",
            "cargo >& x run -- herd set h lead.merge true",
        ] {
            assert!(
                run(&leads, Switches::default(), command).is_some(),
                "{command} was allowed"
            );
        }
        for command in [
            "gh pr view 1 >&2",
            "gh pr view 1 2>&1",
            "gh pr view 1 >& out",
            "git status <&0",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn setting_a_zsh_path_array_lets_a_heredoc_body_run() {
        for assignment in [
            "path=d",
            "path+=d",
            "path[1]=d",
            "cdpath=d",
            "fpath=d",
            "module_path=d",
        ] {
            assert_judged(
                &format!("{assignment} cat <<'EOF'\ngh pr merge 1\nEOF"),
                Switch::Merge,
            );
        }
        assert_passes("paths=d cat <<'EOF'\ngh pr merge 1\nEOF");
    }

    #[test]
    fn git_reflog_dates_subshell_heredocs_and_case_alternatives_pass() {
        for command in [
            "git log -g HEAD@{1.week.ago}",
            "git show HEAD@{2.days.ago}",
            "(cd . && cat <<EOF)\nhi\nEOF",
            "grep -c '^$' notes.md",
            "rg 'EOF$' src",
            "echo \"Total: $\"",
            "x=b; case $x in a) echo a;; b*|c) echo b;; esac",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn a_heredoc_delimiter_with_a_cr_or_unicode_space_is_one_word() {
        assert_judged(
            "cat <<E\u{a0}x\nE\n'\nE\u{a0}x\ngh pr merge 1 #'",
            Switch::Merge,
        );
        assert_judged("cat <<E\r\nE\n'\nE\r\ngh pr merge 1 #'", Switch::Merge);
    }

    #[test]
    fn function_coproc_and_trap_bodies_are_read_as_commands() {
        for command in [
            "function f { gh pr merge 1; }; f",
            "function f() { gh pr merge 1; }; f",
            "coproc gh pr merge 1; wait",
            "coproc { gh pr merge 1; }; wait",
            "coproc N { gh pr merge 1; }; wait",
            "coproc N while gh pr merge 1; do break; done; wait",
            "coproc N if gh pr merge 1; then :; fi; wait",
            "coproc eval \"{\" 'gh pr merge 1;' \"}\"; wait",
            "trap 'gh pr merge 1' EXIT",
            "trap -- 'gh pr merge 1' EXIT",
        ] {
            assert_judged(command, Switch::Merge);
        }
        assert_passes("trap 'rm -f /tmp/x' EXIT");
        assert_passes("trap -p");
    }

    #[test]
    fn ionice_taskset_and_flock_wrap_a_command() {
        for command in [
            "ionice gh pr merge 1",
            "ionice -c 3 gh pr merge 1",
            "taskset 1 gh pr merge 1",
            "taskset -c 0 gh pr merge 1",
            "flock /dev/null gh pr merge 1",
            "flock -w 5 /tmp/l gh pr merge 1",
            "flock /tmp/l -c 'gh pr merge 1'",
            "flock /tmp/l --command 'gh pr merge 1'",
            "flock --wait 5 /tmp/l gh pr merge 1",
            "flock -nw 5 /tmp/l gh pr merge 1",
            "ionice --class 3 gh pr merge 1",
            "ionice -tc 3 gh pr merge 1",
        ] {
            assert_judged(command, Switch::Merge);
        }
        assert_judged("flock -x /tmp/l git rebase origin/main", Switch::Rebase);
        assert_judged("flock /tmp/l git -c user.name=x rebase", Switch::Rebase);
        assert_passes("flock /tmp/l gh pr view 1");
        assert_passes("flock /tmp/l grep -c 'gh pr merge' notes");
    }

    #[test]
    fn a_leads_gh_git_or_shell_alias_is_denied_whatever_its_switches() {
        for command in [
            "gh alias set mm 'pr merge' && gh mm 1",
            "gh alias import aliases.yml",
            "git -c alias.r=rebase r origin/main",
            "git -c Alias.r=rebase r origin/main",
            "git -calias.r=rebase r origin/main",
            "git --config-env=alias.r=R r origin/main",
            "git --config-env alias.r=R r origin/main",
            "shopt -s expand_aliases\nalias g=gh\ng pr merge 1",
            "x=g=gh; shopt -s expand_aliases\nalias $x\ng pr merge 1",
            "shopt -s expand_aliases\nBASH_ALIASES[g]=gh\ng pr merge 1",
            "shopt -s expand_aliases\nBASH_ALIASES=([g]=gh)\ng pr merge 1",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.r GIT_CONFIG_VALUE_0=rebase git r origin/main",
            "GIT_CONFIG_PARAMETERS=\"'alias.r=rebase'\" git r origin/main",
            "export GIT_CONFIG_GLOBAL=x.cfg; git r origin/main",
            "git -c include.path=x.cfg r origin/main",
            "git config alias.r rebase && git r origin/main",
        ] {
            assert!(needs(command).contains(&Switch::Alias), "{command}");
            let leads = ledger(vec![lead()]);
            let message = run(&leads, all_on(), command).expect(command);
            assert!(message.contains("may not set a `gh alias`"), "{message}");
        }
        for command in [
            "gh alias list",
            "git -c user.name=x commit -m x",
            "git config user.name x",
            "alias",
            "alias -p",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn a_shell_or_source_reading_its_script_from_input_is_denied() {
        for command in [
            "bash <(echo gh pr merge 1)",
            "source <(echo gh pr merge 1)",
            ". <(echo gh pr merge 1)",
            "bash <<< 'gh pr merge 1'",
            "sh < script.sh",
            "{ bash; } <<< 'gh pr merge 1'",
            "( source /dev/stdin ) <<< 'gh pr merge 1'",
            "if true; then bash; fi <<< 'gh pr merge 1'",
            "exec 0<<<'gh pr merge 1'; bash",
            "exec 3<<<'gh pr merge 1'; bash /dev/fd/3",
            "exec < <(echo 'gh pr merge 1'); bash",
            "echo 'gh pr merge 1' | bash",
            "echo 'gh pr merge 1' | sh -s x",
        ] {
            assert_unreadable(command);
        }
        assert_judged("bash <<EOF\ngh pr merge 1\nEOF", Switch::Merge);
        assert_judged("bash -s <<EOF\ngh pr merge 1\nEOF", Switch::Merge);
        assert_passes("bash -c 'cat' < notes.md");
        assert_passes("bash scripts/check.sh < /dev/null");
        assert_passes("source ./env.sh < /dev/null");
        assert_passes("cat <(echo hi)");
    }

    #[test]
    fn a_brace_expansion_building_the_program_is_denied() {
        assert_unreadable("{gh,} pr merge 1");
    }

    #[test]
    fn a_shift_nested_in_arithmetic_or_a_spaced_subscript_is_denied() {
        let hidden = |first: &str, gated: &str| format!("{first}; cat <<E\n'\nE\n{gated} #'\n2\nE");
        for gated in [
            "gh pr merge 1",
            "git rebase origin/main",
            "scripts/install",
            "loopherd herd set h lead.merge true",
        ] {
            for first in [
                "((x=((1))<<2))",
                "(( ((1)) << 2 ))",
                "a[(1<<2)]=x",
                "a[ 1<<2 ]=x",
                "x=1 a[ 1<<2 ]=x",
                "a[1|1<<2]=x",
                "a[1&1<<2]=x",
                "a[1\n<<2]=x",
                "arr=( [ 1<<2 ]=x )",
                "arr=([1 <<2]=x)",
            ] {
                assert_unreadable(&hidden(first, gated));
            }
            assert_unreadable(&format!("x=$( {} )", hidden("((x=((1))<<2))", gated)));
        }
        assert_passes("a[i+1]=x; echo ${a[1]}");
        assert_passes("declare a[1]=x");
        assert_passes("[ -f x ] && echo y");
        assert_passes("(( a[ 1 ] ))");
        assert_passes("x=( [k]=v ); a[$i]=x b=1");
    }

    #[test]
    fn an_option_value_attached_in_a_cluster_does_not_take_the_next_word() {
        for command in [
            "env -uC gh pr merge 1",
            "env -uS gh pr merge 1",
            "env -uu gh pr merge 1",
            "env -iuC gh pr merge 1",
            "sudo -uhug gh pr merge 1",
        ] {
            assert_judged(command, Switch::Merge);
        }
        assert_judged("env -uC git rebase origin/main", Switch::Rebase);
        assert_judged("env -uC scripts/install", Switch::Install);
        assert_judged("env -iu HOME gh pr merge 1", Switch::Merge);
    }

    #[test]
    fn a_shell_is_heredoc_fed_only_when_its_last_input_redirect_is_the_heredoc() {
        for command in [
            "bash 3<<EOF <<< 'gh pr merge 1'\n:\nEOF",
            "bash <<EOF <<< 'gh pr merge 1'\n:\nEOF",
            "bash <<EOF 0<&3\n:\nEOF",
            "bash <<EOF < s.sh\n:\nEOF",
            "bash /dev/stdin 3<<EOF\n:\nEOF",
            "bash /proc/1/fd/0 <<EOF\n:\nEOF",
            "bash -s <<< 'gh pr merge 1' '<<EOF'",
            "bash -s <<< 'gh pr merge 1' \"<<EOF\"",
            "bash -s <<< 'gh pr merge 1' -- \"<<EOF\"",
            "bash -s <<< 'gh pr merge 1' \\<<EOF",
            "bash -s <<< 'gh pr merge 1' 2> \"<<EOF\"",
            "bash -s <<< 'gh pr merge 1' > '<<EOF'",
        ] {
            assert_unreadable(command);
        }
        assert_judged("bash /dev/fd/3 3<<'EOF'\ngh pr merge 1\nEOF", Switch::Merge);
        assert_judged("bash <<< x 0<<'EOF'\ngh pr merge 1\nEOF", Switch::Merge);
    }

    #[test]
    fn every_name_for_a_shells_input_is_read_as_its_input() {
        for script in [
            "/proc/self/fd/0",
            "/proc/thread-self/fd/0",
            "//dev/stdin",
            "/dev/./stdin",
            "/dev/../dev/stdin",
            "/dev//fd/0",
        ] {
            assert_unreadable(&format!("bash {script} <<< 'gh pr merge 1'"));
            assert_unreadable(&format!("source {script} <<< 'gh pr merge 1'"));
            assert_judged(
                &format!("bash {script} <<'EOF'\ngh pr merge 1\nEOF"),
                Switch::Merge,
            );
        }
    }

    #[test]
    fn a_script_that_only_ends_in_stdin_or_fd_n_is_a_file() {
        assert_passes("bash ./stdin");
        assert_passes("bash scripts/fd/3");
    }

    #[test]
    fn an_alias_or_git_config_variable_set_by_a_builtin_is_denied() {
        for command in [
            "shopt -s expand_aliases\nread 'BASH_ALIASES[g]' <<< gh\ng pr merge 1",
            "shopt -s expand_aliases\nprintf -v 'BASH_ALIASES[g]' gh\ng pr merge 1",
            "shopt -s expand_aliases\ndeclare -n A=BASH_ALIASES; A[g]=gh\ng pr merge 1",
            "read GIT_CONFIG_GLOBAL <<< x.cfg; export GIT_CONFIG_GLOBAL; git r",
            "printf -vGIT_CONFIG_GLOBAL x.cfg; export GIT_CONFIG_GLOBAL; git r",
        ] {
            assert_judged(command, Switch::Alias);
        }
        for command in [
            "rg -n GIT_CONFIG src/tool_hook.rs",
            "grep -rn BASH_ALIASES docs",
            "git log --oneline -S GIT_CONFIG",
            "loopherd tell lh-1 \"GIT_CONFIG_GLOBAL is denied now\"",
            "loopherd report --kind done \"GIT_CONFIG variables are denied\"",
            "echo GIT_CONFIG_COUNT",
            "MSG=BASH_ALIASES true",
            "printf '%s' GIT_CONFIG",
        ] {
            assert_passes(command);
        }
    }

    #[test]
    fn a_quoted_bracket_in_an_array_literal_passes() {
        assert_passes("args=( -e '[' x )");
        assert_passes("args=( \"[\" )");
        assert_unreadable("arr=( [ 1<<2 ]=x ); gh pr merge 1");
    }
}
