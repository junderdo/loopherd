//! `loopherd tool-hook`: Claude's PreToolUse hook on Bash. Blocks a command
//! from a lead's pane that needs a switch its herd file leaves off (D17).
//! A guardrail on the command text, not a sandbox.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::herd::Switches;
use crate::ledger::Ledger;

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
}

impl Switch {
    fn name(self) -> &'static str {
        match self {
            Switch::Merge => "merge",
            Switch::Install => "install",
            Switch::Rebase => "rebase",
        }
    }

    fn humans_part(self) -> &'static str {
        match self {
            Switch::Merge => "the human merges PRs",
            Switch::Install => "the human runs scripts/install and install-hook",
            Switch::Rebase => "the human rebases and force-pushes",
        }
    }

    fn on(self, switches: &Switches) -> bool {
        match self {
            Switch::Merge => switches.merge,
            Switch::Install => switches.install,
            Switch::Rebase => switches.rebase,
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
    let Some(socket) = pane.socket else {
        return Ok(None);
    };
    let Some(command) = bash_command(input) else {
        return Ok(None);
    };
    let needed = needs(&command);
    if needed.is_empty() {
        return Ok(None);
    }
    let entries = ledger.entries()?;
    let Some(lead) = entries.values().find(|e| {
        !e.retired && e.is_lead() && e.pane_id == pane.id && e.socket.as_deref() == Some(socket)
    }) else {
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
    let mut message = format!(
        "loopherd: blocked: {}'s `{}` switch is off, so {}; ask with `loopherd report --kind question` instead",
        lead.name,
        needed.name(),
        needed.humans_part()
    );
    if let Some(why) = unread {
        message.push_str(&format!(
            " (its herd file couldn't be read, so every switch counts as off: {why})"
        ));
    }
    Ok(Some(message))
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
    commands(script)
        .iter()
        .flat_map(|words| needs_one(words))
        .collect()
}

/// The simple commands in `script`, each as its words with quotes removed.
/// `;`, `&`, `|`, newlines, parentheses, backticks and `$(` end a command;
/// a `$(…)` or backtick inside double quotes is lexed as commands too.
fn commands(script: &str) -> Vec<Vec<String>> {
    let mut commands = vec![];
    let mut words = vec![];
    let mut word: Option<String> = None;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let w = word.get_or_insert_default();
                w.extend(chars.by_ref().take_while(|&c| c != '\''));
            }
            '"' => {
                let w = word.get_or_insert_default();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => w.extend(chars.next()),
                        '$' if chars.peek() == Some(&'(') => {
                            chars.next();
                            commands.extend(self::commands(&substitution(&mut chars)));
                        }
                        '`' => {
                            let inner: String = chars.by_ref().take_while(|&c| c != '`').collect();
                            commands.extend(self::commands(&inner));
                        }
                        _ => w.push(c),
                    }
                }
            }
            '#' if word.is_none() => {
                chars.by_ref().find(|&c| c == '\n');
                words.extend(word.take());
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(c) => word.get_or_insert_default().push(c),
            },
            ';' | '&' | '|' | '\n' | '(' | ')' | '`' | '$'
                if c != '$' || chars.peek() == Some(&'(') =>
            {
                words.extend(word.take());
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => words.extend(word.take()),
            _ => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word.take());
    if !words.is_empty() {
        commands.push(words);
    }
    commands
}

/// The text of a `$(…)` whose `$(` was just read, through its matching `)`.
fn substitution(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut depth = 1;
    let mut inner = String::new();
    for c in chars.by_ref() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        inner.push(c);
    }
    inner
}

/// Words that run the command after them, with their options that take a value.
const WRAPPERS: [(&str, &[&str]); 12] = [
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
];
const KEYWORDS: [&str; 10] = [
    "if", "then", "else", "elif", "do", "while", "until", "!", "{", "source",
];
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];

fn needs_one(words: &[String]) -> Vec<Switch> {
    let mut i = 0;
    loop {
        let Some(word) = words.get(i) else {
            return vec![];
        };
        let program = file_name(word);
        if is_assignment(word) || KEYWORDS.contains(&word.as_str()) || word == "." {
            i += 1;
        } else if let Some((_, with_value)) = WRAPPERS.iter().find(|(w, _)| *w == program) {
            i = skip_options(words, i + 1, with_value);
            if program == "timeout" {
                i += 1;
            }
        } else if SHELLS.contains(&program) {
            i += 1;
            while let Some(option) = words.get(i).filter(|w| w.starts_with(['-', '+'])) {
                if !option.starts_with("--") && option.contains('c') {
                    return words.get(i + 1).map_or(vec![], |script| needs(script));
                }
                // `-o pipefail`, `-O extglob`: the next word is the option's value.
                let has_value = ["-o", "+o", "-O", "+O"].contains(&option.as_str());
                i += if has_value { 2 } else { 1 };
            }
        } else if program == "eval" {
            return needs(&words[i + 1..].join(" "));
        } else {
            return needs_program(word, &words[i + 1..]).into_iter().collect();
        }
    }
}

fn needs_program(path: &str, args: &[String]) -> Option<Switch> {
    if Path::new(path).ends_with("scripts/install") {
        return Some(Switch::Install);
    }
    let positional = positionals(args, &[]);
    match file_name(path) {
        "gh" => {
            let positional = positionals(args, &["-R", "--repo"]);
            let pr_merge = positional.starts_with(&["pr", "merge"]);
            let api_merge = positional.first() == Some(&"api") && args.iter().any(|a| merges(a));
            (pr_merge || api_merge).then_some(Switch::Merge)
        }
        "curl" | "wget" => args.iter().any(|a| merges(a)).then_some(Switch::Merge),
        "git" => git_needs(args),
        "loopherd" => (positional.first() == Some(&"install-hook")).then_some(Switch::Install),
        _ => None,
    }
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
    let (subcommand, rest) = args[start..].split_first()?;
    let rebases = |a: &String| {
        a == "-r" || a == "--rebase" || a.strip_prefix("--rebase=").is_some_and(|v| v != "false")
    };
    let forces = |a: &String| {
        a == "--force"
            || a.starts_with("--force-with-lease")
            || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
            || a.starts_with('+')
    };
    let needed = match subcommand.as_str() {
        "rebase" => true,
        "pull" => rest.iter().any(rebases),
        "push" => rest.iter().any(forces),
        _ => false,
    };
    needed.then_some(Switch::Rebase)
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
    i
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
    ];
    const INSTALLS: &[&str] = &[
        "scripts/install",
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
    ];
    const REBASES: &[&str] = &[
        "git rebase origin/main",
        "git -C /src/repo rebase -i HEAD~3",
        "git -c core.editor=true rebase --continue",
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
}
