//! `$LOOPHERD_CONFIG_DIR/config.toml`: user-wide settings, every one optional.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

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
    /// Read and checked by `notify`, so a broken table can't cost the rest.
    #[serde(rename = "notify")]
    _notify: Option<toml::Value>,
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
    let config = &link_target(config)?;
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
        match table.get_mut(name).and_then(Item::as_value_mut) {
            Some(old) => {
                *value.decor_mut() = old.decor().clone();
                *old = value;
            }
            None => {
                table.insert(name, Item::Value(value));
            }
        }
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

/// The file at the end of `path`'s symlinks, existing or not: renaming over
/// a symlink would replace it, so writes go to its target.
fn link_target(path: &Path) -> Result<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target =
                    fs::read_link(&path).with_context(|| format!("read {}", path.display()))?;
                path = path.parent().unwrap_or(Path::new("")).join(target);
            }
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(path),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
    bail!("{}: too many levels of symlinks", path.display())
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
    fn lead_defaults_set_and_read_beside_a_notify_table() {
        let (_dir, path) = config_with(Some("[notify]\nenabled = true\ncommand = 'n {text}'\n"));

        set_lead_defaults(&path, "merge").unwrap();

        assert_eq!(lead_defaults(&path).unwrap(), Some(only_on(true, false)));
        assert!(fs::read_to_string(&path).unwrap().starts_with("[notify]\n"));
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
    fn setting_lead_defaults_keeps_comment_lines_above_existing_switches() {
        let (_dir, path) = config_with(Some("[lead_defaults]\n# lets leads merge\nmerge = true\n"));

        set_lead_defaults(&path, "none").unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("[lead_defaults]\n# lets leads merge\nmerge = false\n"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn setting_lead_defaults_through_a_symlink_writes_its_target() {
        let (dir, target) = config_with(Some("context_window = 1000000\n"));
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        set_lead_defaults(&link, "merge").unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(lead_defaults(&target).unwrap().unwrap().merge, Some(true));
    }

    #[cfg(unix)]
    #[test]
    fn setting_lead_defaults_through_a_dangling_symlink_creates_its_target() {
        let (dir, _) = config_with(None);
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink("dotfiles/loopherd.toml", &link).unwrap();

        set_lead_defaults(&link, "merge").unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let target = dir.path().join("dotfiles/loopherd.toml");
        assert_eq!(lead_defaults(&target).unwrap().unwrap().merge, Some(true));
    }

    #[cfg(unix)]
    #[test]
    fn setting_lead_defaults_keeps_the_files_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = config_with(Some("context_window = 1000000\n"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        set_lead_defaults(&path, "merge").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
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
