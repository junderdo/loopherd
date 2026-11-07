//! `loopherd herd get|set|unset`: read and edit one value in a herd file,
//! keeping its comments and layout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Key, Table, TableLike, Value};

use super::{Paths, check, file_of};

/// The value at dotted `key` in `herd`'s file, as TOML; an error when unset.
pub fn get(paths: &Paths, herd: &str, key: &str) -> Result<String> {
    let (_, doc) = read(paths, herd)?;
    let keys = parse_key(key)?;
    let mut item = doc.as_item();
    for k in &keys {
        item = match item.as_table_like().and_then(|t| t.get(k.get())) {
            Some(item) => item,
            None => bail!("{key} is not set in {herd}"),
        };
    }
    let mut value = item
        .clone()
        .into_value()
        .map_err(|_| anyhow::anyhow!("{key} has no TOML value"))?;
    value.decor_mut().clear();
    Ok(value.to_string())
}

/// Sets dotted `key` in `herd`'s file to `value`, parsed as a TOML value,
/// creating missing tables.
pub fn set(paths: &Paths, herd: &str, key: &str, value: &str) -> Result<()> {
    let (file, mut doc) = read(paths, herd)?;
    let keys = parse_key(key)?;
    let mut value: Value = value
        .parse()
        .with_context(|| format!("{value} is not a TOML value; quote a string"))?;
    let (last, parents) = keys.split_last().context("empty key")?;
    let mut table = doc.as_table_mut() as &mut dyn TableLike;
    for k in parents {
        let item = table.entry_format(k).or_insert_with(|| {
            let mut new = Table::new();
            new.set_implicit(true);
            Item::Table(new)
        });
        table = match item.as_table_like_mut() {
            Some(t) => t,
            None => bail!("{} is not a table", k.get()),
        };
    }
    match table.get_mut(last.get()).and_then(Item::as_value_mut) {
        Some(old) => {
            *value.decor_mut() = old.decor().clone();
            *old = value;
        }
        None => {
            table.insert(last.get(), Item::Value(value));
        }
    }
    write(paths, &file, &doc.to_string())
}

/// Removes dotted `key` from `herd`'s file; false, writing nothing, when it
/// isn't set.
pub fn unset(paths: &Paths, herd: &str, key: &str) -> Result<bool> {
    let (file, mut doc) = read(paths, herd)?;
    let keys = parse_key(key)?;
    let (last, parents) = keys.split_last().context("empty key")?;
    let mut table = Some(doc.as_table_mut() as &mut dyn TableLike);
    for k in parents {
        table = table
            .and_then(|t| t.get_mut(k.get()))
            .and_then(Item::as_table_like_mut);
    }
    if table.and_then(|t| t.remove(last.get())).is_none() {
        return Ok(false);
    }
    write(paths, &file, &doc.to_string())?;
    Ok(true)
}

/// Writes `text` over `file` through a temp file and a rename, once it
/// passes the checks `loopherd herds` makes.
fn write(paths: &Paths, file: &Path, text: &str) -> Result<()> {
    check(text, paths).with_context(|| format!("refused, {} is unchanged", file.display()))?;
    // Renaming over a symlink would replace it, so write to its target.
    let file = &std::fs::canonicalize(file).with_context(|| format!("read {}", file.display()))?;
    let name = file.file_name().context("herd file has no name")?;
    let temp = file.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    std::fs::write(&temp, text).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, file).with_context(|| format!("write {}", file.display()))
}

fn read(paths: &Paths, herd: &str) -> Result<(PathBuf, DocumentMut)> {
    let file = file_of(&paths.herds, herd)?;
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    let doc = text
        .parse::<DocumentMut>()
        .with_context(|| file.display().to_string())?;
    Ok((file, doc))
}

fn parse_key(key: &str) -> Result<Vec<Key>> {
    Key::parse(key).with_context(|| format!("{key:?} is not a dotted key"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::herd::Paths;

    const HERD: &str = r#"# the routed herd
repo = "/src/repo"

[queue]
template = "card"
source = "scripts/trello ready"
max = 2 # slots

[template.card]
params = ["key"]
prompt = "work {key}"
args = ["--model", "opus"]

[lead]
prompt = "lead {herd}"
merge = false
"#;

    fn herds(text: &str) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir(&paths.herds).unwrap();
        fs::write(paths.herds.join("h.toml"), text).unwrap();
        (dir, paths)
    }

    #[test]
    fn get_prints_a_value_as_toml() {
        let (_dir, paths) = herds(HERD);
        assert_eq!(get(&paths, "h", "queue.max").unwrap(), "2");
        assert_eq!(get(&paths, "h", "repo").unwrap(), "\"/src/repo\"");
        assert_eq!(
            get(&paths, "h", "template.card.args").unwrap(),
            "[\"--model\", \"opus\"]"
        );
        assert_eq!(get(&paths, "h", "lead.merge").unwrap(), "false");
    }

    #[test]
    fn get_of_a_missing_key_fails_naming_it() {
        let (_dir, paths) = herds(HERD);
        for key in ["workspace", "lead.install", "template.pr.args", "repo.x"] {
            let err = get(&paths, "h", key).unwrap_err().to_string();
            assert_eq!(err, format!("{key} is not set in h"));
        }
    }

    #[test]
    fn get_of_a_table_prints_it_inline() {
        let (_dir, paths) = herds(HERD);
        assert_eq!(
            get(&paths, "h", "lead").unwrap(),
            "{ prompt = \"lead {herd}\", merge = false }"
        );
    }

    fn text(paths: &Paths) -> String {
        fs::read_to_string(paths.herds.join("h.toml")).unwrap()
    }

    #[test]
    fn set_changes_one_value_and_keeps_the_rest_of_the_file() {
        let (_dir, paths) = herds(HERD);
        set(&paths, "h", "queue.max", "3").unwrap();
        set(&paths, "h", "lead.merge", "true").unwrap();
        set(
            &paths,
            "h",
            "template.card.args",
            r#"["--model", "sonnet"]"#,
        )
        .unwrap();
        assert_eq!(
            text(&paths),
            HERD.replace("max = 2 # slots", "max = 3 # slots")
                .replace("merge = false", "merge = true")
                .replace(r#""opus""#, r#""sonnet""#)
        );
    }

    #[test]
    fn set_creates_missing_tables() {
        let (_dir, paths) = herds("repo = \"/r\"\n");
        set(&paths, "h", "lead.prompt", "\"lead {herd}\"").unwrap();
        assert_eq!(
            text(&paths),
            "repo = \"/r\"\n\n[lead]\nprompt = \"lead {herd}\"\n"
        );
    }

    #[test]
    fn set_refuses_a_result_herds_would_call_invalid_and_leaves_the_file() {
        let (_dir, paths) = herds(HERD);
        for (key, value, expected) in [
            ("queue.max", "0", "queue: max must be at least 1, got 0"),
            ("max", "3", "unknown field `max`"),
            (
                "template.card.prompt",
                "\"hi {nmae}\"",
                "template card: prompt: unknown placeholder {nmae}",
            ),
            (
                "lead.prompt_file",
                "\"missing.md\"",
                "lead: set one of prompt and prompt_file, not both",
            ),
            ("queue.max", "three", "three is not a TOML value"),
            ("repo.x", "1", "repo is not a table"),
        ] {
            let err = format!("{:#}", set(&paths, "h", key, value).unwrap_err());
            assert!(err.contains(expected), "{key} = {value}: {err}");
            assert_eq!(text(&paths), HERD, "{key} = {value}");
        }
    }

    #[test]
    fn unset_removes_a_key_and_keeps_the_rest_of_the_file() {
        let (_dir, paths) = herds(HERD);
        assert!(unset(&paths, "h", "lead.merge").unwrap());
        assert!(unset(&paths, "h", "template.card.args").unwrap());
        assert_eq!(
            text(&paths),
            HERD.replace("merge = false\n", "")
                .replace("args = [\"--model\", \"opus\"]\n", "")
        );
    }

    #[test]
    fn unset_of_a_missing_key_succeeds_and_leaves_the_file() {
        let (_dir, paths) = herds(HERD);
        for key in ["workspace", "lead.install", "template.pr.args", "repo.x"] {
            assert!(!unset(&paths, "h", key).unwrap(), "{key}");
        }
        assert_eq!(text(&paths), HERD);
    }

    #[test]
    fn unset_refuses_a_result_herds_would_call_invalid() {
        let (_dir, paths) = herds(HERD);
        let err = format!("{:#}", unset(&paths, "h", "repo").unwrap_err());
        assert!(err.contains("missing field `repo`"), "{err}");
        assert_eq!(text(&paths), HERD);
    }

    #[test]
    fn set_writes_through_a_symlinked_herd_file() {
        let (dir, paths) = herds("");
        let target = dir.path().join("dotfiles.toml");
        fs::write(&target, "repo = \"/r\"\n").unwrap();
        fs::remove_file(paths.herds.join("h.toml")).unwrap();
        std::os::unix::fs::symlink(&target, paths.herds.join("h.toml")).unwrap();

        set(&paths, "h", "workspace", "\"w\"").unwrap();

        assert!(paths.herds.join("h.toml").is_symlink());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "repo = \"/r\"\nworkspace = \"w\"\n"
        );
    }
}
