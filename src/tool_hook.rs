//! `loopherd tool-hook`: Claude's PreToolUse hook on Bash. Blocks a command
//! written out in a lead's pane that needs a switch its herd file leaves off,
//! or that is always the human's: a forced restart or retire, a pause or
//! resume, a retire of a lead, or a write of a herd file, the lead defaults or
//! loopherd's config.
//! It catches an honest lead's mistake and reads only plain forms; the lead's
//! sandbox and credentials are what contain it.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::herd::Switches;
use crate::ledger::{AgentRecord, Ledger, Process};

/// Where the hook runs: `$HERDR_PANE_ID` and `$HERDR_SOCKET_PATH`.
pub struct Pane<'a> {
    pub id: &'a str,
    pub socket: Option<&'a Path>,
    /// The caller and its ancestors, which carry a moved member's old pane id.
    pub lineage: &'a [Process],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    Merge,
    Install,
    Rebase,
    /// Not a switch: always the human's.
    ForceRestart,
    /// `loopherd retire --force`, which skips the checks that the member has
    /// finished. Not a switch: always the human's.
    ForceRetire,
    /// `loopherd pause` or `resume`. Not a switch: always the human's.
    Pause,
    /// `loopherd herd set`, `unset` or `remove`, which could turn a lead's own
    /// switches on, raise its review-round limit or delete its herd. Not a
    /// switch: always the human's.
    HerdEdit,
    /// `loopherd retire` of a lead, since a lead that retires itself would
    /// drop its own gates. Not a switch:
    /// the shepherd's or the human's.
    RetireLead,
    /// `loopherd lead-defaults --set` or `scripts/install --lead-defaults`,
    /// which could turn a lead's own switches on. Not a switch: always the human's.
    LeadDefaults,
    /// `loopherd config set` or `unset`, `loopherd setup`, or `scripts/install
    /// --context`, which could raise a lead's own handoff or review-round limit
    /// or switches.
    /// Not a switch: always the human's.
    Config,
}

impl Switch {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Switch::Merge => "merge",
            Switch::Install => "install",
            Switch::Rebase => "rebase",
            Switch::ForceRestart => "restart --force",
            Switch::ForceRetire => "retire --force",
            Switch::Pause => "pause",
            Switch::HerdEdit => "herd set",
            Switch::RetireLead => "retire lead",
            Switch::LeadDefaults => "lead-defaults",
            Switch::Config => "config",
        }
    }

    fn humans_part(self) -> &'static str {
        match self {
            Switch::Merge => "the human merges PRs",
            Switch::Install => "the human runs scripts/install and install-hook",
            Switch::Rebase => "the human rebases and force-pushes",
            Switch::ForceRestart => "the human forces a restart",
            Switch::ForceRetire => "the human forces a retire",
            Switch::Pause => "the human pauses and resumes herds",
            Switch::HerdEdit => "the human edits herd files",
            Switch::RetireLead => "the shepherd or the human retires leads",
            Switch::LeadDefaults => "the human sets lead defaults",
            Switch::Config => "the human sets loopherd's config",
        }
    }

    pub(crate) fn on(self, switches: &Switches) -> bool {
        match self {
            Switch::Merge => switches.merge,
            Switch::Install => switches.install,
            Switch::Rebase => switches.rebase,
            Switch::ForceRestart
            | Switch::ForceRetire
            | Switch::Pause
            | Switch::HerdEdit
            | Switch::RetireLead
            | Switch::LeadDefaults
            | Switch::Config => false,
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
                Switch::ForceRetire,
                Switch::Pause,
                Switch::HerdEdit,
                Switch::RetireLead,
                Switch::LeadDefaults,
                Switch::Config,
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
            "loopherd: blocked: {} may not run `loopherd herd set`, `herd unset` or `herd remove`, which could turn its own switches on, raise its review-round limit or delete its herd: {}; ask with `loopherd report --kind question`",
            lead.name,
            needed.humans_part()
        )));
    }
    if needed == Switch::RetireLead {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not retire a lead with `loopherd retire`: {}; ask with `loopherd report --kind question`",
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
            "loopherd: blocked: {} may not change loopherd's config with `loopherd config set`, `config unset`, `loopherd setup` or `scripts/install --context`, which could raise its own handoff or review-round limit or switches: {}; ask with `loopherd report --kind question`",
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
    if needed == Switch::ForceRetire {
        return Ok(Some(format!(
            "loopherd: blocked: {} may not run `loopherd retire --force`, which skips the checks that the member has finished, so {}; retire it once it is idle and has reported, or once herdr no longer lists it, or ask with `loopherd report --kind question`",
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

/// The lead running in `pane`. A retired lead counts while no unretired
/// entry holds the pane, since its agent may still run there.
pub fn lead_in(ledger: &dyn Ledger, pane: &Pane) -> Result<Option<AgentRecord>> {
    let Some(socket) = pane.socket else {
        return Ok(None);
    };
    let (live, retired): (Vec<_>, Vec<_>) = ledger
        .entries()?
        .into_values()
        .filter(|e| e.is_own_pane(pane.id, pane.lineage) && e.socket.as_deref() == Some(socket))
        .partition(|e| !e.retired);
    Ok(match live.into_iter().next() {
        Some(entry) => entry.is_lead().then_some(entry),
        None => retired.into_iter().find(AgentRecord::is_lead),
    })
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

/// Every switch the commands written out in `script` need, in order. Only
/// plain forms count: a heredoc's body, a quoted script (`bash -c`, `eval`),
/// a variable and another program's arguments are not looked into.
pub fn needs(script: &str) -> Vec<Switch> {
    let mut lexer = Lexer {
        chars: script.chars().peekable(),
        commands: vec![],
        heredocs: vec![],
    };
    lexer.script(false);
    lexer
        .commands
        .iter()
        .filter_map(|command| needs_one(command))
        .collect()
}

/// Splits a script into simple commands, quotes removed and heredoc bodies
/// skipped, following `$(…)` and `(…)` but no other expansion.
struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    commands: Vec<Vec<String>>,
    /// Delimiters of the heredocs opened on this line, and whether `<<-`
    /// strips their leading tabs.
    heredocs: Vec<(String, bool)>,
}

impl Lexer<'_> {
    /// Reads commands to the end of the script, or to the `)` closing the
    /// substitution or subshell this call is `nested` in.
    fn script(&mut self, nested: bool) {
        let mut command: Vec<String> = vec![];
        let mut word: Option<String> = None;
        while let Some(c) = self.chars.next() {
            match c {
                '\'' => word
                    .get_or_insert_default()
                    .extend(self.chars.by_ref().take_while(|&q| q != '\'')),
                '"' => self.double_quoted(word.get_or_insert_default()),
                '\\' => match self.chars.next() {
                    Some('\n') | None => {}
                    Some(escaped) => word.get_or_insert_default().push(escaped),
                },
                '#' if word.is_none() => while self.chars.next_if(|&c| c != '\n').is_some() {},
                '<' if self.chars.next_if_eq(&'<').is_some() => {
                    if self.chars.next_if_eq(&'<').is_none() {
                        self.heredoc_operator();
                    }
                }
                '(' if word.as_deref().is_some_and(|w| w.ends_with('$')) => {
                    if self.chars.next_if_eq(&'(').is_some() {
                        self.arithmetic();
                    } else {
                        self.script(true);
                    }
                }
                '(' if word.is_none() && self.chars.next_if_eq(&'(').is_some() => {
                    self.arithmetic();
                }
                ' ' | '\t' => command.extend(word.take()),
                ';' | '&' | '|' | '(' | ')' | '`' | '\n' => {
                    command.extend(word.take());
                    self.commands.push(std::mem::take(&mut command));
                    match c {
                        '\n' => self.skip_heredoc_bodies(),
                        '(' => self.script(true),
                        ')' if nested => return,
                        _ => {}
                    }
                }
                _ => word.get_or_insert_default().push(c),
            }
        }
        command.extend(word);
        self.commands.push(command);
    }

    /// Skips a `((…))` or `$((…))` whose `((` was just read; a `<<` in it is
    /// a shift, not a heredoc.
    fn arithmetic(&mut self) {
        let mut depth = 2;
        for c in self.chars.by_ref() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                return;
            }
        }
    }

    fn double_quoted(&mut self, word: &mut String) {
        while let Some(c) = self.chars.next() {
            match c {
                '"' => return,
                '\\' => word.extend(self.chars.next()),
                '$' if self.chars.next_if_eq(&'(').is_some() => {
                    if self.chars.next_if_eq(&'(').is_some() {
                        self.arithmetic();
                    } else {
                        self.script(true);
                    }
                }
                _ => word.push(c),
            }
        }
    }

    fn heredoc_operator(&mut self) {
        let strips_tabs = self.chars.next_if_eq(&'-').is_some();
        while self.chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
        let mut delimiter = String::new();
        while let Some(c) = self
            .chars
            .next_if(|c| !c.is_whitespace() && !";&|()<>".contains(*c))
        {
            match c {
                '\'' | '"' => delimiter.extend(self.chars.by_ref().take_while(|&q| q != c)),
                '\\' => delimiter.extend(self.chars.next()),
                _ => delimiter.push(c),
            }
        }
        self.heredocs.push((delimiter, strips_tabs));
    }

    fn skip_heredoc_bodies(&mut self) {
        for (delimiter, strips_tabs) in std::mem::take(&mut self.heredocs) {
            while self.chars.peek().is_some() {
                let line: String = self.chars.by_ref().take_while(|&c| c != '\n').collect();
                let line = if strips_tabs {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if line == delimiter {
                    break;
                }
            }
        }
    }
}

/// Words that run the command written after them, taken without options.
const PREFIXES: [&str; 19] = [
    "if", "then", "else", "elif", "do", "while", "until", "!", "{", "time", "command", "exec",
    "nohup", "env", "sudo", "sh", "bash", "source", ".",
];

fn needs_one(command: &[String]) -> Option<Switch> {
    let words: Vec<&str> = command
        .iter()
        .map(String::as_str)
        .skip_while(|word| is_assignment(word) || PREFIXES.contains(word))
        .collect();
    let (program, args) = words.split_first()?;
    let flag = |name: &str| args.iter().any(|a| is_flag(a, name));
    if Path::new(program).ends_with("scripts/install") {
        return Some(if flag("--lead-defaults") {
            Switch::LeadDefaults
        } else if flag("--context") {
            Switch::Config
        } else {
            Switch::Install
        });
    }
    let forces = |switch| flag("--force").then_some(switch);
    match program.rsplit('/').next()? {
        "gh" => {
            let positional = positionals(args, &["-R", "--repo"]);
            let api_merge = positional.first() == Some(&"api")
                && args.iter().any(|a| merges(a))
                && gh_api_writes(args);
            (positional.starts_with(&["pr", "merge"]) || api_merge).then_some(Switch::Merge)
        }
        "curl" | "wget" => args.iter().any(|a| merges(a)).then_some(Switch::Merge),
        "git" => git_needs(args),
        "loopherd" => {
            let positional = positionals(args, &[]);
            match *positional.first()? {
                "install-hook" => Some(Switch::Install),
                "lead-defaults" => flag("--set").then_some(Switch::LeadDefaults),
                "config" => {
                    matches!(positional.get(1), Some(&"set" | &"unset")).then_some(Switch::Config)
                }
                "setup" => (!flag("--help") && !flag("-h")).then_some(Switch::Config),
                "restart" => forces(Switch::ForceRestart),
                "pause" | "resume" => Some(Switch::Pause),
                "retire" if positional[1..].iter().any(|name| name.starts_with("lead-")) => {
                    Some(Switch::RetireLead)
                }
                "retire" => forces(Switch::ForceRetire),
                "herd" => matches!(positional.get(1), Some(&"set" | &"unset" | &"remove"))
                    .then_some(Switch::HerdEdit),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether `arg` is `flag`, alone or as `flag=value`.
fn is_flag(arg: &str, flag: &str) -> bool {
    arg.strip_prefix(flag)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
}

/// git's global options that take the next word as their value.
const GIT_GLOBAL_WITH_VALUE: [&str; 7] = [
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
    "--super-prefix",
];

fn git_needs(args: &[&str]) -> Option<Switch> {
    let start = skip_options(args, 0, &GIT_GLOBAL_WITH_VALUE);
    let (subcommand, rest) = args.get(start..)?.split_first()?;
    let rebases = |a: &&str| {
        *a == "-r" || *a == "--rebase" || a.strip_prefix("--rebase=").is_some_and(|v| v != "false")
    };
    let forces = |a: &&str| {
        *a == "--force"
            || a.starts_with("--force-with-lease")
            || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
            || a.starts_with('+')
            || *a == "--mirror"
    };
    let needed = match *subcommand {
        "rebase" => true,
        "pull" => rest.iter().any(rebases),
        "push" => rest.iter().any(forces),
        _ => false,
    };
    needed.then_some(Switch::Rebase)
}

/// `gh api` sends a GET only with no method and no fields, in any form
/// (`-X PUT`, `-XPUT`, `--method=PUT`, `-fkey=v`, `--field=…`).
fn gh_api_writes(args: &[&str]) -> bool {
    args.iter().any(|a| {
        ["-X", "-f", "-F"].iter().any(|f| a.starts_with(f))
            || ["--method", "--field", "--raw-field", "--input"]
                .iter()
                .any(|f| is_flag(a, f))
    })
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

/// The index of the first word from `i` on that isn't an option or the
/// value of one in `with_value`; a `--` is skipped and ends the options.
fn skip_options(words: &[&str], mut i: usize, with_value: &[&str]) -> usize {
    while let Some(word) = words.get(i) {
        if *word == "--" {
            return i + 1;
        }
        if !word.starts_with('-') || *word == "-" {
            break;
        }
        i += if with_value.contains(word) { 2 } else { 1 };
    }
    i.min(words.len())
}

fn positionals<'a>(args: &[&'a str], with_value: &[&str]) -> Vec<&'a str> {
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        let next = skip_options(args, i, with_value);
        match args.get(next) {
            Some(arg) => out.push(*arg),
            None => break,
        }
        i = next + 1;
    }
    out
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
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
        "gh pr merge --help",
        "gh -R junderdo/loopherd pr merge 12",
        "gh pr --repo junderdo/loopherd merge 12",
        "/usr/bin/gh pr merge 12",
        "GH_TOKEN=$(gh auth token --user junderdo) gh pr merge 12",
        "cd /src/repo && gh pr merge 12",
        "gh pr view 12\ngh pr merge 12",
        "gh pr merge 12 2>&1 | tee merge.log",
        "gh api -X PUT repos/junderdo/loopherd/pulls/12/merge",
        "gh api repos/{owner}/{repo}/pulls/$N/merge --method PUT",
        "curl -X PUT -H 'Authorization: token x' https://api.github.com/repos/o/r/pulls/12/merge",
        "wget --method=PUT https://api.github.com/repos/o/r/pulls/12/merge",
        "sudo gh pr merge 12",
        "exec gh pr merge 12",
        "command gh pr merge 12",
        "nohup gh pr merge 12",
        "time gh pr merge 12",
        "env GH_TOKEN=x gh pr merge 12",
        "if gh pr merge 12; then echo ok; fi",
        "while true; do gh pr merge 12; done",
        "! gh pr merge 12",
        "{ gh pr merge 12; }",
        "(gh pr merge 12)",
        "out=$(gh pr merge 12)",
        "out=\"$(gh pr merge 12 2>&1)\"",
    ];
    const INSTALLS: &[&str] = &[
        "scripts/install",
        "./scripts/install",
        "/home/me/src/loopherd/scripts/install",
        "sh scripts/install",
        "bash scripts/install",
        "FOO=1 scripts/install",
        "source scripts/install",
        ". scripts/install",
        "x=\"$(scripts/install)\"",
        "loopherd install-hook",
        "/home/me/.local/bin/loopherd install-hook --remove",
        "env XDG_STATE_HOME=/tmp/s loopherd install-hook",
    ];
    const REBASES: &[&str] = &[
        "git rebase origin/main",
        "git -C /src/repo rebase -i HEAD~3",
        "git -c core.editor=true rebase -i HEAD~2",
        "/usr/bin/git rebase main",
        "git pull --rebase",
        "git pull --rebase=merges origin main",
        "git pull -r",
        "git push --force",
        "git push --force-with-lease origin feat/x",
        "git push --force-with-lease=feat/x:abc origin feat/x",
        "git push -f origin feat/x",
        "git push -uf origin feat/x",
        "git push origin +feat/x",
        "git push --mirror backup",
        "GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push --force origin feat/x",
        "cd /src/repo && git rebase main",
        "git --git-dir /src/repo/.git rebase main",
        "git --work-tree /src/repo rebase main",
        "if ! out=\"$(git push --force-with-lease 2>&1)\"; then echo \"$out\"; fi",
        "git rebase --abort",
        "git rebase --continue",
        "git rebase --help",
        "echo $(echo ')'; git rebase main)",
    ];
    const ALLOWED: &[&str] = &[
        "gh pr view 12",
        "gh pr view 12 --json mergeable,mergedAt",
        "gh pr list --search merge",
        "gh pr create --title 'merge things' --body x",
        "gh api repos/o/r/pulls/12",
        "gh api repos/o/r/pulls/12/merged",
        "gh api repos/o/r/pulls/12/merge",
        "gh api --jq .merged repos/o/r/pulls/12/merge",
        "curl https://api.github.com/repos/o/r/pulls/12",
        "echo gh pr merge 12",
        "cat scripts/install",
        "git diff scripts/install",
        "scripts/install-check",
        "loopherd status",
        "loopherd report --kind question 'may I run install-hook?'",
        "loopherd report --kind question 'merge PR #12? run: gh pr merge 12 --squash'",
        "git log --grep rebase",
        "git pull",
        "git pull --no-rebase",
        "git pull --rebase=false",
        "git push",
        "git push -u origin feat/x",
        "GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin feat/x",
        "git merge origin/main",
        "git commit -m 'gh pr merge 12; git rebase main'",
        "echo done # ; gh pr merge 12",
        "# then: git rebase main",
        "out=\"$(gh pr view 12)\"",
        "cargo test",
        "cargo test pause",
        "",
        "'' gh pr merge 12",
        "echo \"$(echo ')')\" done",
        "gh pr create --title t --body \"$(cat <<'EOF'\n- Handles the edge\ncase where it is empty.\nEOF\n)\"",
        "git commit -m \"$(cat <<'EOF'\nFix the empty herd (case 3)\nEOF\n)\"",
        "gh pr create --title t --body \"$(cat <<-EOF\n\tit is a case in point\n\tEOF\n)\"",
        "x=$(cat <<'EOF'\nBody\n EOF\nEOF\n); echo \"$x\"",
        "echo \"$((1 << 2))\"",
        "find . -name '*.rs' -exec grep -l foo {} +",
    ];
    /// Forms the hook no longer looks into: containing a lead that reaches
    /// for these is the sandbox's and the credentials' job.
    const INDIRECT: &[&str] = &[
        "bash -c 'gh pr merge 12'",
        "sh -lc \"cd /x; gh pr merge 12\"",
        "eval 'gh pr merge 12'",
        "M='gh pr merge 12'; $M",
        "G=gh; $G pr merge 12",
        "echo 12 | xargs gh pr merge",
        "timeout 30 gh pr merge 12",
        "watch -n1 git push --force",
        "find . -exec git push --force ';'",
        "bash <<'EOF'\ngh pr merge 12\nEOF",
        "cat <<EOF\n$(gh pr merge 12)\nEOF",
        "trap 'git rebase main' EXIT",
        "echo \"`git rebase main`\"",
        "cargo run -- pause h",
        "cargo run -- install-hook",
        "CARGO_ALIAS_X='run -- herd set h lead.merge true' cargo x",
        "git -c alias.x='!gh pr merge 12' x",
        "gh alias set m 'pr merge'",
        "GIT_CONFIG_COUNT=1 git status",
        "loopherd retire \"$ME\"",
        "gh pr create \\\n  --title t",
        "echo ${x:-a default}",
        "git add src/{a,b}.rs",
    ];

    #[test]
    fn indirection_is_not_looked_into() {
        let leads = ledger(vec![lead()]);
        for command in INDIRECT {
            assert_eq!(run(&leads, Switches::default(), command), None, "{command}");
        }
    }

    #[test]
    fn a_heredoc_body_is_skipped_whatever_the_script_runs_next() {
        let leads = ledger(vec![lead()]);
        for command in [
            "cat > notes.md <<'EOF'\nIt's green, so the human can run\ngh pr merge 12\ngit push --force\nEOF\ncargo test",
            "cat > notes.md <<EOF\ndon't; (unbalanced \"prose\nscripts/install\nEOF\npython3 check.py notes.md",
            "cat <<-EOF > a; cat <<'B' > b\n\tloopherd pause h\n\tEOF\ngit rebase main\nB\nls",
        ] {
            assert_eq!(run(&leads, Switches::default(), command), None, "{command}");
        }
    }

    #[test]
    fn a_shift_in_arithmetic_is_no_heredoc_and_hides_no_line() {
        for (command, want) in [
            ("x=$((1 << 2))\ngh pr merge 12", Switch::Merge),
            ("(( n <<= 1 ))\ngit push --force", Switch::Rebase),
            ("echo \"$((1 << 2))\"\nscripts/install", Switch::Install),
            ("echo $(( (1 << 2) + 1 ))\ngit rebase main", Switch::Rebase),
        ] {
            assert_eq!(needs(command), [want], "{command}");
        }
    }

    #[test]
    fn a_plain_form_after_a_heredoc_body_is_still_read() {
        for (command, want) in [
            (
                "cat > n.md <<'EOF'\nIt's prose.\nEOF\ngh pr merge 12",
                Switch::Merge,
            ),
            (
                "cat <<EOF | tee n.md; git push -f\nprose\nEOF",
                Switch::Rebase,
            ),
            (
                "cat <<-EOF\n\tprose\n\tEOF\nscripts/install",
                Switch::Install,
            ),
            ("cat <<< 'one line'\ngit rebase main", Switch::Rebase),
        ] {
            assert_eq!(needs(command), [want], "{command}");
        }
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
        "env X=1 loopherd pause h",
        "out=$(loopherd pause h)",
    ];

    const LEAD_RETIRES: &[&str] = &[
        "loopherd retire lead-h",
        "loopherd retire --close lead-h",
        "loopherd retire lead-h --close",
        "loopherd retire -- lead-h",
        "loopherd retire lead-other",
        "loopherd retire lh-1 lead-h",
        "~/.cargo/bin/loopherd retire lead-h",
        "env loopherd retire lead-h",
        "nohup loopherd retire lead-h",
        "gh pr view 1; loopherd retire lead-h",
    ];

    const HERD_EDITS: &[&str] = &[
        "loopherd herd set h lead.merge true",
        "loopherd herd unset h lead.merge",
        "loopherd herd set h lead.review_rounds 9",
        "/home/me/.local/bin/loopherd herd set h lead.install true",
        "cd /x && loopherd herd unset h queue.max",
        "loopherd herd remove h",
        "loopherd herd remove --dry-run h",
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
            lineage: &[],
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
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
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
            outcome: None,
            released_at: None,
            herd: Some("h".into()),
            template: None,
            params: None,
            socket: Some(PathBuf::from(SOCKET)),
            process: None,
            role,
            after_merge: None,
            unsandboxed: None,
            earlier: vec![],
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
            lineage: &[],
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
            | Switch::ForceRetire
            | Switch::Pause
            | Switch::HerdEdit
            | Switch::RetireLead
            | Switch::LeadDefaults
            | Switch::Config => {}
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
                | Switch::ForceRetire
                | Switch::Pause
                | Switch::HerdEdit
                | Switch::RetireLead
                | Switch::LeadDefaults
                | Switch::Config => {}
            }
            for command in commands {
                assert_eq!(run(&leads, others_off, command), None, "{command}");
            }
        }
    }

    #[test]
    fn a_worker_is_allowed_everything() {
        let workers = ledger(vec![entry("lh-1", None)]);
        for (commands, _) in tables() {
            for command in commands {
                assert_eq!(
                    run(&workers, Switches::default(), command),
                    None,
                    "{command}"
                );
            }
        }
    }

    #[test]
    fn a_live_entry_on_a_retired_leads_pane_id_wins_over_the_retired_lead() {
        let mut retired = entry("lead-a", Some(Role::Lead));
        retired.retired = true;
        retired.herd = Some("a".into());
        let live = entry("lead-b", Some(Role::Lead));
        let leads = ledger(vec![retired.clone(), live]);
        let switches = |herd: &str| {
            Ok(match herd {
                "a" => all_on(),
                _ => Switches::default(),
            })
        };
        for command in ["gh pr merge 3", "git push --force"] {
            let message = run_with(&leads, Some(SOCKET), &input(command), &switches)
                .unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(message.contains("lead-b"), "{message}");
        }

        let with_worker = ledger(vec![retired, entry("lh-9", None)]);
        for command in [
            "gh pr merge 3",
            "git push --force",
            "loopherd retire lead-a",
        ] {
            assert_eq!(
                run_with(&with_worker, Some(SOCKET), &input(command), &switches),
                None,
                "{command}"
            );
        }
    }

    #[test]
    fn a_retired_moved_leads_old_pane_id_is_its_only_under_its_old_shell() {
        let mut retired = lead();
        retired.retired = true;
        retired.pane_id = "w5:p3".into();
        retired.env_pane_id = Some(PANE.into());
        retired.env_pane_shell = Some(crate::process::PANE_SHELL);
        let leads = ledger(vec![retired]);
        let in_pane = |lineage: &[Process]| {
            let pane = Pane {
                id: PANE,
                socket: Some(Path::new(SOCKET)),
                lineage,
            };
            lead_in(&leads, &pane).unwrap().map(|l| l.name)
        };

        assert_eq!(
            in_pane(&[crate::process::PANE_SHELL]).as_deref(),
            Some("lead-h")
        );
        assert_eq!(
            in_pane(&[Process {
                pid: 20,
                start: 910
            }]),
            None
        );
    }

    #[test]
    fn a_retired_leads_pane_keeps_the_leads_gates() {
        let mut retired = lead();
        retired.retired = true;
        let retired = ledger(vec![retired]);
        for (commands, switch) in tables() {
            for command in commands {
                assert!(
                    run(&retired, only_off(switch), command).is_some(),
                    "{command} was allowed"
                );
            }
        }
        for command in [
            "loopherd herd set h lead.merge true",
            "loopherd herd unset h lead.merge",
            "loopherd herd remove h",
            "loopherd pause h",
            "loopherd resume h",
            "loopherd setup",
            "loopherd config set handoff.limit 1",
            "loopherd retire lead-h",
        ] {
            let message =
                run(&retired, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(message.contains("lead-h"), "{message}");
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
    fn a_leads_forced_retire_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in [
            "loopherd retire lh-1 --force",
            "loopherd retire --force --close lh-1",
            "cd /x && loopherd retire lh-1 --force",
        ] {
            assert_eq!(needs(command), [Switch::ForceRetire], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains("lead-h may not run `loopherd retire --force`")
                    && message.contains("the human forces a retire"),
                "{message}"
            );
        }
        for command in ["loopherd retire lh-1", "cargo run -- retire --close lh-1"] {
            assert_eq!(needs(command), [], "{command}");
        }
    }

    #[test]
    fn a_leads_forced_restart_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in [
            "loopherd restart lh-1 --force",
            "loopherd restart --force lh-1",
            "cd /x && loopherd restart lh-1 --force",
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
        assert_eq!(
            run(&leads, Switches::default(), "loopherd restart --self"),
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

    #[test]
    fn a_leads_retire_of_a_lead_is_denied_whatever_its_switches() {
        let leads = ledger(vec![lead()]);
        for command in LEAD_RETIRES {
            assert_eq!(needs(command), [Switch::RetireLead], "{command}");
            let message =
                run(&leads, all_on(), command).unwrap_or_else(|| panic!("{command} was allowed"));
            assert!(
                message.contains("lead-h may not retire a lead with `loopherd retire`")
                    && message.contains("the shepherd or the human retires leads"),
                "{message}"
            );
        }
    }

    #[test]
    fn retiring_a_member_or_retiring_outside_a_leads_pane_passes() {
        let leads = ledger(vec![lead()]);
        for command in [
            "loopherd retire lh-1",
            "loopherd retire --close lh-1",
            "cargo run -- retire lh-1",
            "cargo run -- tell lh-1 retire",
            "cargo run -- report --kind note lead-h will retire",
            "ls | xargs -I X echo X",
            "echo lh-1 | xargs loopherd retire --close lh-1",
            "echo lh-1 | xargs -n1 --max-procs=2 loopherd retire lh-1",
            "loopherd status lead-h",
            "git commit -m 'retire lead-h'",
        ] {
            assert_eq!(run(&leads, Switches::default(), command), None, "{command}");
        }
        let workers = ledger(vec![entry("lh-1", None)]);
        for command in LEAD_RETIRES {
            assert_eq!(
                run(&workers, Switches::default(), command),
                None,
                "{command}"
            );
        }
    }

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
            "loopherd config set lead_defaults.review_rounds 9",
            "loopherd setup",
            "loopherd setup --again",
            "loopherd setup --context limit=500000",
            "scripts/install --context limit=500000",
            "./scripts/install --no-splash --context=window=1000000",
            "bash scripts/install --context limit=500000",
            "env FOO=1 loopherd setup",
            "command loopherd config unset default_context_limit",
            "/home/u/.local/bin/loopherd setup --again",
            "./target/debug/loopherd config set context_window 1000000",
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
            "loopherd config get lead_defaults.review_rounds",
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
    fn a_lead_may_release_a_retired_member_with_every_switch_off() {
        let leads = ledger(vec![lead()]);
        for command in [
            "loopherd release lh-139",
            "loopherd release lh-139 --keep-branch",
            "loopherd release lh-139 --drop-branch",
            "cargo run -- release lh-139 --drop-branch",
        ] {
            assert_eq!(needs(command), [], "{command}");
            assert_eq!(run(&leads, Switches::default(), command), None, "{command}");
        }
    }
}
