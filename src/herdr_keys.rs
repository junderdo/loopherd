//! herdr key bindings for the loopherd plugin's actions, added to and taken
//! out of the human's herdr `config.toml` as text, so every other byte stays.

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use toml_edit::{Document, Item, Table, Value};

use crate::install_hook::{back_up, through_symlinks, write_atomically};

pub struct Binding {
    pub key: &'static str,
    pub action: &'static str,
    pub description: &'static str,
}

pub const BINDINGS: [Binding; 2] = [
    Binding {
        key: "prefix+m",
        action: "loopherd.view",
        description: "loopherd: herd view",
    },
    Binding {
        key: "prefix+shift+m",
        action: "loopherd.view-beside-shepherd",
        description: "loopherd: view beside shepherd",
    },
];

pub enum Step {
    Bound(&'static Binding),
    AlreadyBound {
        action: &'static str,
        key: String,
    },
    KeyTaken {
        binding: &'static Binding,
        holder: String,
    },
}

pub struct Removed {
    pub action: String,
    pub key: String,
}

pub struct Outcome<S> {
    pub steps: Vec<S>,
    /// Set when the file was written; the backup exists only when it was there before.
    pub written: Option<Option<PathBuf>>,
}

/// The file herdr reads: `$HERDR_CONFIG_PATH`, else `herdr/config.toml` in
/// `$XDG_CONFIG_HOME` or `~/.config`.
pub fn config_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HERDR_CONFIG_PATH") {
        return Ok(PathBuf::from(path));
    }
    let dir = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(dir.join("herdr/config.toml"))
}

/// Binds each of [`BINDINGS`] whose action has no key yet and whose key
/// neither the config nor herdr's defaults (`herdr --default-config`) hold.
pub fn install(path: &Path, defaults: &str, now: SystemTime) -> Result<Outcome<Step>> {
    let path = through_symlinks(path)?;
    let old = read(&path)?;
    let text = old.as_deref().unwrap_or("");
    let doc = parse(&path, text)?;
    let keys = doc.as_table().get("keys");
    if keys.is_some_and(|k| k.is_inline_table())
        || keys
            .and_then(|k| k.get("command"))
            .is_some_and(Item::is_value)
    {
        bail!(
            "keys.command is an inline array in {}; bind the keys by hand (see loopherd's README)",
            path.display()
        );
    }
    let held = held_keys(doc.as_table(), defaults);
    let commands = commands(doc.as_table());
    let mut steps = Vec::new();
    let mut added = String::new();
    for binding in &BINDINGS {
        if let Some(entry) = commands.iter().find(|c| c.is_action(binding.action)) {
            steps.push(Step::AlreadyBound {
                action: binding.action,
                key: entry.keys.join(", "),
            });
        } else if let Some((_, holder)) = held.iter().find(|(key, _)| *key == normal(binding.key)) {
            steps.push(Step::KeyTaken {
                binding,
                holder: holder.clone(),
            });
        } else {
            if !(text.is_empty() && added.is_empty()) {
                added.push('\n');
            }
            added.push_str(&format!(
                "[[keys.command]]\nkey = {:?}\ntype = \"plugin_action\"\ncommand = {:?}\ndescription = {:?}\n",
                binding.key, binding.action, binding.description
            ));
            steps.push(Step::Bound(binding));
        }
    }
    if added.is_empty() {
        return Ok(Outcome {
            steps,
            written: None,
        });
    }
    let mut new = text.to_string();
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&added);
    if let Err(e) = Document::parse(new.as_str()) {
        bail!(
            "adding the bindings would break {}: {e}; left it unchanged",
            path.display()
        );
    }
    Ok(Outcome {
        steps,
        written: Some(write(&path, old.as_deref(), &new, now)?),
    })
}

/// Takes out the `[[keys.command]]` entries [`install`] writes: a `loopherd.*`
/// command with the description it gives that action.
pub fn remove(path: &Path, now: SystemTime) -> Result<Outcome<Removed>> {
    let path = through_symlinks(path)?;
    let Some(old) = read(&path)? else {
        return Ok(Outcome {
            steps: Vec::new(),
            written: None,
        });
    };
    let doc = parse(&path, &old)?;
    let ours: Vec<Command> = commands(doc.as_table())
        .into_iter()
        .filter(|c| {
            BINDINGS.iter().any(|b| {
                c.action.as_deref() == Some(b.action)
                    && c.description.as_deref() == Some(b.description)
            })
        })
        .collect();
    let mut new = old.clone();
    let mut steps = Vec::new();
    for command in ours.iter().rev() {
        let Some(span) = &command.span else { continue };
        let mut start = line_start(&old, span.start);
        // The blank line install puts before each entry goes with it.
        if old[..start].ends_with("\n\n") {
            start -= 1;
        }
        new.replace_range(start..line_end(&old, span.end), "");
        steps.push(Removed {
            action: command.action.clone().unwrap_or_default(),
            key: command.keys.join(", "),
        });
    }
    steps.reverse();
    if steps.is_empty() {
        return Ok(Outcome {
            steps,
            written: None,
        });
    }
    Ok(Outcome {
        steps,
        written: Some(write(&path, Some(&old), &new, now)?),
    })
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn parse<'a>(path: &Path, text: &'a str) -> Result<Document<&'a str>> {
    Document::parse(text).with_context(|| format!("parse {}; left it unchanged", path.display()))
}

fn write(path: &Path, old: Option<&str>, new: &str, now: SystemTime) -> Result<Option<PathBuf>> {
    let backup = old
        .map(|old| back_up(path, old.as_bytes(), now))
        .transpose()?;
    write_atomically(path, new.as_bytes())?;
    Ok(backup)
}

struct Command {
    keys: Vec<String>,
    kind: Option<String>,
    action: Option<String>,
    description: Option<String>,
    /// From the `[[keys.command]]` header to the end of its last value.
    span: Option<Range<usize>>,
}

impl Command {
    fn is_action(&self, action: &str) -> bool {
        self.kind.as_deref() == Some("plugin_action") && self.action.as_deref() == Some(action)
    }
}

fn commands(root: &Table) -> Vec<Command> {
    let Some(item) = root.get("keys").and_then(|k| k.get("command")) else {
        return Vec::new();
    };
    let string = |table: &dyn toml_edit::TableLike, field: &str| {
        table.get(field).and_then(Item::as_str).map(str::to_string)
    };
    let entry = |table: &dyn toml_edit::TableLike, span: Option<Range<usize>>| Command {
        keys: table.get("key").map(key_values).unwrap_or_default(),
        kind: string(table, "type"),
        action: string(table, "command"),
        description: string(table, "description"),
        span,
    };
    match item {
        Item::ArrayOfTables(tables) => tables
            .iter()
            .map(|t| {
                let end = t.iter().filter_map(|(_, v)| v.span()).map(|s| s.end).max();
                let span = t.span().map(|s| s.start..end.unwrap_or(s.end));
                entry(t, span)
            })
            .collect(),
        Item::Value(Value::Array(array)) => array
            .iter()
            .filter_map(Value::as_inline_table)
            .map(|t| entry(t, None))
            .collect(),
        _ => Vec::new(),
    }
}

/// Every key the config binds, then every default it doesn't override, with what holds it.
fn held_keys(root: &Table, defaults: &str) -> Vec<(String, String)> {
    let mut held = Vec::new();
    let mut overridden = Vec::new();
    if let Some(keys) = root.get("keys").and_then(Item::as_table_like) {
        for (field, item) in keys.iter() {
            if matches!(field, "prefix" | "command" | "indexed") {
                continue;
            }
            overridden.push(field.to_string());
            for key in key_values(item) {
                held.push((normal(&key), format!("keys.{field} in your config")));
            }
        }
    }
    for command in commands(root) {
        let what = command.action.as_deref().unwrap_or("a command");
        for key in &command.keys {
            held.push((normal(key), format!("a [[keys.command]] running {what}")));
        }
    }
    for (field, key) in default_keys(defaults) {
        if !overridden.contains(&field) {
            held.push((normal(&key), format!("herdr's default {field}")));
        }
    }
    held
}

/// The `# field = "key"` lines in the `[keys]` section of herdr's default config.
fn default_keys(defaults: &str) -> Vec<(String, String)> {
    let mut in_keys = false;
    let mut found = Vec::new();
    for line in defaults.lines().map(str::trim) {
        if line.starts_with('[') {
            in_keys = line == "[keys]";
            continue;
        }
        // The commented [[keys.command]] example's fields aren't defaults.
        if line.trim_start_matches('#').trim_start().starts_with('[') {
            in_keys = false;
            continue;
        }
        let Some(setting) = line.strip_prefix('#').filter(|_| in_keys) else {
            continue;
        };
        let Some((field, rest)) = setting.split_once('=') else {
            continue;
        };
        let field = field.trim();
        let Some(key) = rest
            .trim()
            .strip_prefix('"')
            .and_then(|r| r.split('"').next())
        else {
            continue;
        };
        let simple =
            !field.is_empty() && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if simple && field != "prefix" && !key.is_empty() {
            found.push((field.to_string(), key.to_string()));
        }
    }
    found
}

fn key_values(item: &Item) -> Vec<String> {
    match item.as_value() {
        Some(Value::String(s)) => vec![s.value().clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// A key as herdr compares it: modifier aliases and order folded, and an
/// uppercase letter read as shift plus that letter.
fn normal(key: &str) -> String {
    let mut prefix = false;
    let mut modifiers = Vec::new();
    let mut code = String::new();
    for part in key.split('+').map(str::trim) {
        let lower = part.to_lowercase();
        let modifier = match lower.as_str() {
            "prefix" => {
                prefix = true;
                continue;
            }
            "ctrl" | "control" => "ctrl",
            "alt" | "option" | "meta" => "alt",
            "shift" => "shift",
            "super" | "cmd" | "command" => "super",
            _ => {
                let mut chars = part.chars();
                if let (Some(c), None) = (chars.next(), chars.next())
                    && c.is_ascii_uppercase()
                {
                    modifiers.push("shift");
                }
                code = lower;
                continue;
            }
        };
        modifiers.push(modifier);
    }
    modifiers.sort_unstable();
    modifiers.dedup();
    let mut parts: Vec<&str> = Vec::new();
    if prefix {
        parts.push("prefix");
    }
    parts.extend(modifiers);
    parts.push(&code);
    parts.join("+")
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i + 1)
}
