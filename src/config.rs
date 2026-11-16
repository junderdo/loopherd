//! `$LOOPHERD_CONFIG_DIR/config.toml`: user-wide settings, every one optional.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::context::check_tokens;
use crate::herd::SwitchValues;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub default_context_limit: Option<u64>,
    pub context_window: Option<u64>,
    pub shepherd: Option<ShepherdConfig>,
    /// Each lead's switches where its herd file's `[lead]` leaves one unset.
    pub lead_defaults: Option<SwitchValues>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShepherdConfig {
    pub context_limit: Option<u64>,
}

/// `config` parsed and checked; a missing file is an empty one.
pub fn read(config: &Path) -> Result<ConfigFile> {
    check(&read_text(config)?).with_context(|| config.display().to_string())
}

/// The `[lead_defaults]` table, `None` when `config` has none.
pub fn lead_defaults(config: &Path) -> Result<Option<SwitchValues>> {
    Ok(read(config)?.lead_defaults)
}

/// Writes `[lead_defaults]` with the switches in `on` (comma-separated, or
/// `none`) on and the rest off, keeping the file's other keys and comments.
/// Refuses, changing nothing, when the result would be an invalid config.
pub fn set_lead_defaults(config: &Path, on: &str) -> Result<()> {
    let on = switch_list(on)?;
    let mut doc: DocumentMut = read_text(config)?
        .parse()
        .with_context(|| config.display().to_string())?;
    let table = doc
        .entry("lead_defaults")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .context("lead_defaults is not a table")?;
    for name in SwitchValues::NAMES {
        let mut value = Value::from(on.contains(&name));
        if let Some(old) = table.get(name).and_then(Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        table.insert(name, Item::Value(value));
    }
    let text = doc.to_string();
    check(&text).with_context(|| format!("refused, {} is unchanged", config.display()))?;
    let written = || format!("write {}", config.display());
    let dir = config.parent().context("config.toml has no directory")?;
    fs::create_dir_all(dir).with_context(written)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).with_context(written)?;
    temp.write_all(text.as_bytes()).with_context(written)?;
    if let Ok(meta) = fs::metadata(config) {
        temp.as_file()
            .set_permissions(meta.permissions())
            .with_context(written)?;
    }
    temp.persist(config).with_context(written)?;
    Ok(())
}

fn switch_list(list: &str) -> Result<Vec<&str>> {
    if list.trim() == "none" {
        return Ok(vec![]);
    }
    list.split(',')
        .map(str::trim)
        .map(|name| match SwitchValues::NAMES.contains(&name) {
            true => Ok(name),
            false => bail!(
                "unknown switch {name:?}: name some of {}, or none",
                SwitchValues::NAMES.join(", ")
            ),
        })
        .collect()
}

fn read_text(config: &Path) -> Result<String> {
    match fs::read_to_string(config) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", config.display())),
    }
}

fn check(text: &str) -> Result<ConfigFile> {
    let file: ConfigFile = toml::from_str(text)?;
    let shepherd = file.shepherd.as_ref().and_then(|s| s.context_limit);
    for (key, value) in [
        ("default_context_limit", file.default_context_limit),
        ("context_window", file.context_window),
        ("shepherd.context_limit", shepherd),
    ] {
        check_tokens(key, value)?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(text: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loopherd/config.toml");
        if let Some(text) = text {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
        }
        (dir, path)
    }

    fn only_on(merge: bool, install: bool) -> SwitchValues {
        SwitchValues {
            merge: Some(merge),
            install: Some(install),
            rebase: Some(false),
            new_cards: Some(false),
            design_escalation: Some(false),
            blocked_dialogs: Some(false),
        }
    }

    #[test]
    fn a_config_without_lead_defaults_has_none() {
        let (_dir, path) = config_with(Some("context_window = 1000000\n"));
        assert_eq!(lead_defaults(&path).unwrap(), None);
        let (_dir, missing) = config_with(None);
        assert_eq!(lead_defaults(&missing).unwrap(), None);
    }

    #[test]
    fn setting_lead_defaults_writes_every_switch_and_keeps_the_rest_of_the_file() {
        let (_dir, path) = config_with(Some(
            "# my limits\ncontext_window = 1000000 # big\n\n[shepherd]\ncontext_limit = 400000\n",
        ));

        set_lead_defaults(&path, "merge, install").unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(
                "# my limits\ncontext_window = 1000000 # big\n\n[shepherd]\ncontext_limit = 400000\n"
            ),
            "{text}"
        );
        assert_eq!(lead_defaults(&path).unwrap(), Some(only_on(true, true)));
    }

    #[test]
    fn setting_lead_defaults_again_replaces_them_keeping_their_comments() {
        let (_dir, path) = config_with(Some(
            "[lead_defaults] # how free leads are\nmerge = true # yes\nrebase = true\n",
        ));

        set_lead_defaults(&path, "none").unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("[lead_defaults] # how free leads are\nmerge = false # yes\n"),
            "{text}"
        );
        assert_eq!(lead_defaults(&path).unwrap(), Some(only_on(false, false)));
    }

    #[test]
    fn setting_lead_defaults_creates_the_file_and_its_directory() {
        let (_dir, path) = config_with(None);

        set_lead_defaults(&path, "rebase").unwrap();

        assert_eq!(lead_defaults(&path).unwrap().unwrap().rebase, Some(true));
    }

    #[test]
    fn an_unknown_switch_or_an_invalid_file_is_refused_and_changes_nothing() {
        let (_dir, path) = config_with(Some("context_window = 1\n"));
        for list in ["merj", "", "none,merge"] {
            let err = format!("{:#}", set_lead_defaults(&path, list).unwrap_err());
            assert!(err.contains("switch"), "{list}: {err}");
        }
        let err = format!("{:#}", set_lead_defaults(&path, "merge").unwrap_err());
        assert!(err.contains("context_window"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "context_window = 1\n");
    }
}
