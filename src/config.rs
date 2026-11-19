//! `$LOOPHERD_CONFIG_DIR/config.toml`: user-wide settings, every one optional.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, RawString, Table, TableLike, Value};

use crate::context::check_tokens;
use crate::herd::SwitchValues;
use crate::line_ending::LineEnding;
use crate::tui::theme::Palette;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub default_context_limit: Option<u64>,
    pub context_window: Option<u64>,
    pub shepherd: Option<ShepherdConfig>,
    /// Each lead's switches where its herd file's `[lead]` leaves one unset.
    pub lead_defaults: Option<SwitchValues>,
    pub herdr_plugin: Option<HerdrPluginConfig>,
    pub view: Option<ViewConfig>,
    /// Read and checked by `notify`, so a broken table can't cost the rest.
    #[serde(rename = "notify")]
    _notify: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShepherdConfig {
    pub context_limit: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HerdrPluginConfig {
    pub revive_on_startup: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewConfig {
    /// Checked by `view_theme`, so a theme it doesn't know can't cost the rest.
    pub theme: Option<String>,
}

/// `config` parsed and checked; a missing file is an empty one.
pub fn read(config: &Path) -> Result<ConfigFile> {
    check(&read_text(config)?).with_context(|| config.display().to_string())
}

/// The `[lead_defaults]` table, `None` when `config` has none.
pub fn lead_defaults(config: &Path) -> Result<Option<SwitchValues>> {
    Ok(read(config)?.lead_defaults)
}

/// Whether the herdr plugin's startup hook runs revive; off unless set.
pub fn revive_on_startup(config: &Path) -> Result<bool> {
    Ok(read(config)?
        .herdr_plugin
        .and_then(|p| p.revive_on_startup)
        .unwrap_or(false))
}

/// `loopherd view`'s palette, with a warning when `config` can't be read
/// or names a theme this build doesn't know; both fall back to the default.
pub fn view_theme(config: &Path) -> (Palette, Option<String>) {
    let theme = match read(config) {
        Ok(file) => file.view.and_then(|v| v.theme),
        Err(e) => return (Palette::default(), Some(format!("{e:#}"))),
    };
    match theme.map(|name| name.parse::<Palette>()) {
        Some(Ok(palette)) => (palette, None),
        Some(Err(e)) => (Palette::default(), Some(format!("view.theme: {e}"))),
        None => (Palette::default(), None),
    }
}

/// Writes `[lead_defaults]` with the switches in `on` (comma-separated, or
/// `none`) on and the rest off, keeping the file's other keys and comments.
/// Refuses, changing nothing, when the result would be an invalid config.
pub fn set_lead_defaults(config: &Path, on: &str) -> Result<()> {
    let on = switch_list(on)?;
    edit(config, |doc| {
        let table = doc
            .entry("lead_defaults")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_like_mut()
            .context("lead_defaults is not a table")?;
        for name in SwitchValues::NAMES {
            replace_value(table, name, Value::from(on.contains(&name)));
        }
        Ok(())
    })
}

/// The keys `config get`, `set` and `unset` take.
pub const KEYS: [&str; 5] = [
    "context_window",
    "default_context_limit",
    "shepherd.context_limit",
    "lead_defaults.review_rounds",
    "view.theme",
];

/// `key`'s value in `config`, `None` when unset.
pub fn get(config: &Path, key: &str) -> Result<Option<String>> {
    let file = read(config)?;
    let number = match known(key)? {
        "view.theme" => return Ok(file.view.and_then(|v| v.theme)),
        "context_window" => file.context_window,
        "default_context_limit" => file.default_context_limit,
        "lead_defaults.review_rounds" => file
            .lead_defaults
            .and_then(|d| d.review_rounds)
            .map(|rounds| rounds.get().into()),
        _ => file.shepherd.and_then(|s| s.context_limit),
    };
    Ok(number.map(|n| n.to_string()))
}

/// Writes `key = value`, keeping the file's other keys and comments.
/// Refuses, changing nothing, an unknown key, an unknown theme, or a value
/// under the minimum.
pub fn set(config: &Path, key: &str, value: &str) -> Result<()> {
    let key = known(key)?;
    let value = match key {
        "view.theme" => Value::from(
            value
                .trim()
                .parse::<Palette>()
                .map_err(anyhow::Error::msg)?
                .name(),
        ),
        _ => Value::from(whole_number(key, value)?),
    };
    edit(config, |doc| {
        let (table, name) = match key.split_once('.') {
            Some((table, name)) => (
                doc.entry(table)
                    .or_insert_with(|| Item::Table(Table::new()))
                    .as_table_like_mut()
                    .with_context(|| format!("{table} is not a table"))?,
                name,
            ),
            None => {
                if doc.get(key).is_none() {
                    lift_header(doc);
                }
                (doc.as_table_mut() as &mut dyn TableLike, key)
            }
        };
        replace_value(table, name, value);
        Ok(())
    })
}

fn whole_number(key: &str, value: &str) -> Result<i64> {
    let number: u64 = value.trim().replace('_', "").parse().with_context(|| {
        format!(
            "{key} must be a whole number of {}, got {value:?}",
            unit(key)
        )
    })?;
    i64::try_from(number).with_context(|| format!("{key}: {number} is too big"))
}

/// Removes `key`, and its table once it's empty; false when `key` was unset.
pub fn unset(config: &Path, key: &str) -> Result<bool> {
    let key = known(key)?;
    if get(config, key)?.is_none() {
        return Ok(false);
    }
    edit(config, |doc| {
        match key.split_once('.') {
            Some((table, name)) => {
                let root = doc.as_table_mut();
                let only_key = root
                    .get(table)
                    .and_then(Item::as_table_like)
                    .is_some_and(|t| t.len() == 1 && t.contains_key(name));
                match root.get_mut(table) {
                    Some(Item::Table(t)) if !t.is_dotted() => {
                        let mut comment = remove_key(t, name);
                        let position = t.position();
                        if t.is_empty() {
                            comment = comment_in(t.decor().prefix()) + &comment;
                            doc.remove(table);
                        }
                        place_comment(doc, position, &comment);
                    }
                    // An inline or dotted table is one root line, kept or removed whole.
                    _ if only_key => {
                        let comment = remove_key(root, table);
                        place_comment(doc, None, &comment);
                    }
                    Some(item) => {
                        if let Some(t) = item.as_table_like_mut() {
                            t.remove(name);
                        }
                    }
                    None => {}
                }
            }
            None => {
                let comment = remove_key(doc.as_table_mut(), key);
                place_comment(doc, None, &comment);
            }
        }
        Ok(())
    })?;
    Ok(true)
}

/// Removes `name` from `table`, moving the comment lines above it onto the
/// next key's line; returns them when no line follows.
fn remove_key(table: &mut Table, name: &str) -> String {
    let comment = line_comment(table, name);
    let next = table
        .iter()
        .filter(|(_, item)| is_line(item))
        .map(|(k, _)| k)
        .skip_while(|k| *k != name)
        .nth(1)
        .map(str::to_string);
    table.remove(name);
    match next {
        Some(next) => {
            prepend_comment(table, &next, &comment);
            String::new()
        }
        None => comment,
    }
}

/// Whether `item` prints as a `key = value` line among its table's keys.
fn is_line(item: &Item) -> bool {
    item.is_value() || item.as_table().is_some_and(Table::is_dotted)
}

/// The comment above `name`'s line, which a dotted table keeps on its first key.
fn line_comment(table: &Table, name: &str) -> String {
    match table
        .get(name)
        .and_then(Item::as_table)
        .filter(|t| t.is_dotted())
    {
        Some(dotted) => dotted
            .iter()
            .next()
            .map(|(first, _)| line_comment(dotted, first))
            .unwrap_or_default(),
        None => table
            .key(name)
            .map(|k| comment_in(k.leaf_decor().prefix()))
            .unwrap_or_default(),
    }
}

fn prepend_comment(table: &mut Table, name: &str, comment: &str) {
    if let Some(dotted) = table
        .get_mut(name)
        .and_then(Item::as_table_mut)
        .filter(|t| t.is_dotted())
    {
        let first = dotted.iter().next().map(|(k, _)| k.to_string());
        if let Some(first) = first {
            prepend_comment(dotted, &first, comment);
        }
        return;
    }
    if let Some(mut key) = table.key_mut(name) {
        let decor = key.leaf_decor_mut();
        let own = text_of(decor.prefix()).to_string();
        decor.set_prefix(format!("{comment}{own}"));
    }
}

/// Puts `comment` above the first table after `position` (any table when
/// `None`), or at the end of the file when there's none.
fn place_comment(doc: &mut DocumentMut, position: Option<isize>, comment: &str) {
    if comment.is_empty() {
        return;
    }
    let after = position.unwrap_or(isize::MIN);
    let next = doc
        .as_table_mut()
        .iter_mut()
        .filter_map(|(_, item)| item.as_table_mut())
        .filter(|table| table.position().is_some_and(|p| p > after))
        .min_by_key(|table| table.position());
    match next {
        Some(table) => {
            let own = text_of(table.decor().prefix()).to_string();
            table.decor_mut().set_prefix(format!("{comment}{own}"));
        }
        None => {
            let own = text_of(Some(doc.trailing())).to_string();
            doc.set_trailing(format!("{comment}{own}"));
        }
    }
}

/// `prefix` when it holds a comment; blank lines alone aren't kept.
fn comment_in(prefix: Option<&RawString>) -> String {
    let text = text_of(prefix);
    match text.contains('#') {
        true => text.to_string(),
        false => String::new(),
    }
}

fn text_of(raw: Option<&RawString>) -> &str {
    raw.and_then(RawString::as_str).unwrap_or_default()
}

/// A file with tables and no root keys keeps its header comment in the first
/// table's decor, so a new root key would land above it: move the comment
/// lines before that table's last blank line, or all of them when there's
/// none, to the doc's own prefix.
fn lift_header(doc: &mut DocumentMut) {
    if doc.iter().any(|(_, item)| item.is_value()) {
        return;
    }
    let first = doc
        .as_table_mut()
        .iter_mut()
        .filter_map(|(_, item)| item.as_table_mut())
        .min_by_key(|table| table.position());
    let Some(table) = first else {
        return;
    };
    let prefix = text_of(table.decor().prefix()).to_string();
    let split = match prefix.rfind("\n\n") {
        Some(blank) => blank + 1,
        None if prefix.contains('#') => prefix.len(),
        None => return,
    };
    table.decor_mut().set_prefix(&prefix[split..]);
    doc.decor_mut().set_prefix(&prefix[..split]);
}

fn unit(key: &str) -> &'static str {
    match key {
        "lead_defaults.review_rounds" => "rounds",
        _ => "tokens",
    }
}

fn known(key: &str) -> Result<&'static str> {
    match KEYS.iter().find(|k| **k == key) {
        Some(key) => Ok(key),
        None => bail!("unknown key {key:?}: name one of {}", KEYS.join(", ")),
    }
}

/// Sets `name` to `value`, keeping the comments around an old value.
fn replace_value(table: &mut dyn TableLike, name: &str, mut value: Value) {
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

/// Applies `change` to `config` through its symlinks, refusing, changing
/// nothing, a result that isn't a valid config.
fn edit(config: &Path, change: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let config = &link_target(config)?;
    let original = read_text(config)?;
    let mut doc: DocumentMut = original
        .parse()
        .with_context(|| config.display().to_string())?;
    if doc.is_empty() {
        // A comment-only file parses as trailing text, which would print after anything added.
        let mut comments = doc.trailing().as_str().unwrap_or_default().to_string();
        if !comments.trim().is_empty() && !comments.ends_with("\n\n") {
            comments.push('\n');
        }
        doc.set_trailing("");
        doc.decor_mut().set_prefix(comments);
    }
    change(&mut doc)?;
    let text = LineEnding::of(&original).apply(&doc.to_string());
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
            review_rounds: None,
        }
    }

    #[test]
    fn revive_on_startup_is_off_unless_the_herdr_plugin_table_turns_it_on() {
        for (text, on) in [
            (None, false),
            (Some("context_window = 1000000\n"), false),
            (Some("[herdr_plugin]\n"), false),
            (Some("[herdr_plugin]\nrevive_on_startup = false\n"), false),
            (Some("[herdr_plugin]\nrevive_on_startup = true\n"), true),
        ] {
            let (_dir, path) = config_with(text);
            assert_eq!(revive_on_startup(&path).unwrap(), on, "{text:?}");
        }
    }

    #[test]
    fn a_misspelled_or_mistyped_herdr_plugin_key_is_refused() {
        for (text, wrong) in [
            (
                "[herdr_plugin]\nrevive_on_start = true\n",
                "revive_on_start",
            ),
            (
                "[herdr_plugin]\nrevive_on_startup = \"yes\"\n",
                "revive_on_startup",
            ),
        ] {
            let (_dir, path) = config_with(Some(text));
            let error = format!("{:#}", revive_on_startup(&path).unwrap_err());
            assert!(error.contains(wrong), "{error}");
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
    fn setting_lead_defaults_in_a_crlf_file_writes_crlf() {
        let (_dir, path) = config_with(Some("# mine\r\ncontext_window = 1000000\r\n"));

        set_lead_defaults(&path, "merge").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# mine\r\ncontext_window = 1000000\r\n\r\n[lead_defaults]\r\n\
             merge = true\r\ninstall = false\r\nrebase = false\r\nnew_cards = false\r\n\
             design_escalation = false\r\nblocked_dialogs = false\r\n"
        );
    }

    #[test]
    fn a_refused_lead_defaults_edit_leaves_a_crlf_files_bytes() {
        let (_dir, path) = config_with(Some("# mine\r\ncontext_window = 1\r\n"));
        let err = format!("{:#}", set_lead_defaults(&path, "merge").unwrap_err());
        assert!(err.contains("context_window"), "{err}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# mine\r\ncontext_window = 1\r\n"
        );
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

#[cfg(test)]
mod key_tests {
    use std::num::NonZeroU32;

    use super::*;

    fn config_with(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn each_key_round_trips_through_set_get_and_unset() {
        let (_dir, path) = config_with("");
        for key in [
            "default_context_limit",
            "context_window",
            "shepherd.context_limit",
        ] {
            assert_eq!(get(&path, key).unwrap(), None, "{key}");
            set(&path, key, "300000").unwrap();
            assert_eq!(get(&path, key).unwrap().as_deref(), Some("300000"), "{key}");
            assert!(unset(&path, key).unwrap(), "{key}");
            assert_eq!(get(&path, key).unwrap(), None, "{key}");
            assert!(!unset(&path, key).unwrap(), "{key}");
        }
    }

    #[test]
    fn lead_review_rounds_round_trip_through_set_get_and_unset() {
        let (_dir, path) = config_with("[lead_defaults]\nmerge = true\n");
        let key = "lead_defaults.review_rounds";
        set(&path, key, "5").unwrap();
        assert_eq!(get(&path, key).unwrap().as_deref(), Some("5"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[lead_defaults]\nmerge = true\nreview_rounds = 5\n"
        );
        assert!(unset(&path, key).unwrap());
        assert_eq!(get(&path, key).unwrap(), None);
        assert_eq!(lead_defaults(&path).unwrap().unwrap().merge, Some(true));
    }

    #[test]
    fn setting_lead_defaults_keeps_their_review_rounds() {
        let (_dir, path) = config_with("[lead_defaults]\nreview_rounds = 4\nmerge = true\n");

        set_lead_defaults(&path, "install").unwrap();

        let defaults = lead_defaults(&path).unwrap().unwrap();
        assert_eq!(defaults.review_rounds.map(NonZeroU32::get), Some(4));
        assert_eq!(
            (defaults.merge, defaults.install),
            (Some(false), Some(true))
        );
    }

    #[test]
    fn setting_a_key_keeps_the_files_comments_and_layout() {
        let (_dir, path) = config_with(
            "# my limits\ncontext_window = 1000000 # big\n\n[shepherd] # me\ncontext_limit = 400000 # mine\n",
        );

        set(&path, "context_window", "500000").unwrap();
        set(&path, "shepherd.context_limit", "300000").unwrap();
        set(&path, "default_context_limit", "200000").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# my limits\ncontext_window = 500000 # big\ndefault_context_limit = 200000\n\n\
             [shepherd] # me\ncontext_limit = 300000 # mine\n"
        );
    }

    #[test]
    fn setting_and_unsetting_keys_in_a_crlf_file_keeps_crlf() {
        let (_dir, path) = config_with("# my limits\r\ncontext_window = 1000000 # big\r\n");

        set(&path, "context_window", "500000").unwrap();
        set(&path, "shepherd.context_limit", "300000").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# my limits\r\ncontext_window = 500000 # big\r\n\r\n[shepherd]\r\ncontext_limit = 300000\r\n"
        );

        assert!(unset(&path, "context_window").unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# my limits\r\n\r\n[shepherd]\r\ncontext_limit = 300000\r\n"
        );
    }

    #[test]
    fn a_root_key_set_after_a_table_keeps_a_leading_comment_first() {
        let (_dir, path) = config_with("# mine\n\n[lead_defaults]\nmerge = true\n");

        set(&path, "context_window", "1000000").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# mine\ncontext_window = 1000000\n\n[lead_defaults]\nmerge = true\n"
        );
    }

    #[test]
    fn unset_keeps_the_comment_above_an_inline_or_dotted_shepherd_line() {
        for (text, left) in [
            (
                "# a\nshepherd = { context_limit = 100000 }\ncontext_window = 1000000\n",
                "# a\ncontext_window = 1000000\n",
            ),
            (
                "# a\nshepherd.context_limit = 100000\n# b\ncontext_window = 1000000\n\n[notify]\nenabled = false\n",
                "# a\n# b\ncontext_window = 1000000\n\n[notify]\nenabled = false\n",
            ),
            (
                "context_window = 1000000\n# a\nshepherd.context_limit = 100000\n\n[notify]\nenabled = false\n",
                "context_window = 1000000\n# a\n\n[notify]\nenabled = false\n",
            ),
        ] {
            let (_dir, path) = config_with(text);

            unset(&path, "shepherd.context_limit").unwrap();

            assert_eq!(fs::read_to_string(&path).unwrap(), left, "{text:?}");
        }
    }

    #[test]
    fn unsetting_a_root_key_moves_its_comment_onto_a_dotted_line_after_it() {
        let (_dir, path) =
            config_with("# a\ncontext_window = 1000000\n# b\nshepherd.context_limit = 100000\n");

        unset(&path, "context_window").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# a\n# b\nshepherd.context_limit = 100000\n"
        );
    }

    #[test]
    fn a_header_comment_with_no_blank_line_after_it_stays_first() {
        let text = "# loopherd config\n[lead_defaults]\nmerge = true\n";
        let (_dir, path) = config_with(text);

        set(&path, "context_window", "1000000").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# loopherd config\ncontext_window = 1000000\n[lead_defaults]\nmerge = true\n"
        );
        unset(&path, "context_window").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn set_then_unset_leaves_the_file_as_it_was() {
        for text in [
            "",
            "# top\n\n[notify] # n\nenabled = false\n",
            "# mine\n[notify]\nenabled = false\n",
            "# mine\ndefault_context_limit = 150000\n",
            "default_context_limit = 150000 # mine\n\n[notify]\nenabled = false\n",
        ] {
            let (_dir, path) = config_with(text);

            set(&path, "context_window", "1000000").unwrap();
            unset(&path, "context_window").unwrap();
            set(&path, "shepherd.context_limit", "100000").unwrap();
            unset(&path, "shepherd.context_limit").unwrap();

            assert_eq!(fs::read_to_string(&path).unwrap(), text, "{text:?}");
        }
    }

    #[test]
    fn unset_keeps_the_comment_above_the_key_it_removes() {
        for (text, left) in [
            (
                "# my limits: tuned for opus 1M\ncontext_window = 1000000\n",
                "# my limits: tuned for opus 1M\n",
            ),
            (
                "# window\ncontext_window = 1000000\n# limit\ndefault_context_limit = 150000\n",
                "# window\n# limit\ndefault_context_limit = 150000\n",
            ),
            (
                "# window\ncontext_window = 1000000\n\n[notify]\nenabled = false\n",
                "# window\n\n[notify]\nenabled = false\n",
            ),
            (
                "[shepherd]\n# tuned\ncontext_limit = 100000\n\n# n\n[notify]\nenabled = false\n",
                "# tuned\n\n# n\n[notify]\nenabled = false\n",
            ),
            (
                "[notify]\nenabled = false\n\n# the shepherd\n[shepherd]\n# tuned\ncontext_limit = 100000\n",
                "[notify]\nenabled = false\n\n# the shepherd\n# tuned\n",
            ),
        ] {
            let (_dir, path) = config_with(text);
            let key = match text.contains("[shepherd]") {
                true => "shepherd.context_limit",
                false => "context_window",
            };

            unset(&path, key).unwrap();

            assert_eq!(fs::read_to_string(&path).unwrap(), left, "{text:?}");
        }
    }

    #[test]
    fn a_comment_only_file_keeps_its_comment_first() {
        let (_dir, path) = config_with("# mine\n");

        set_lead_defaults(&path, "merge").unwrap();
        set(&path, "context_window", "1000000").unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# mine\ncontext_window = 1000000\n\n[lead_defaults]\nmerge = true\n"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn setting_a_key_through_a_symlink_writes_its_target() {
        let (dir, target) = config_with("context_window = 1000000\n");
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        set(&link, "context_window", "500000").unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            get(&target, "context_window").unwrap().as_deref(),
            Some("500000")
        );
    }

    #[test]
    fn an_unknown_key_or_a_bad_value_is_refused_and_changes_nothing() {
        let text = "context_window = 1000000\n";
        let (_dir, path) = config_with(text);
        for (key, value, says) in [
            ("context_windw", "300000", "unknown key"),
            ("lead_defaults.merge", "true", "unknown key"),
            ("context_window", "9999", "at least 10000"),
            ("default_context_limit", "lots", "a whole number"),
            ("shepherd.context_limit", "-1", "a whole number"),
            ("lead_defaults.review_rounds", "0", "nonzero"),
            (
                "lead_defaults.review_rounds",
                "2.5",
                "a whole number of rounds",
            ),
            (
                "view.theme",
                "solarized",
                "purple-orange, phosphor, catppuccin",
            ),
        ] {
            let err = format!("{:#}", set(&path, key, value).unwrap_err());
            assert!(err.contains(says), "{key} {value}: {err}");
        }
        assert!(format!("{:#}", unset(&path, "window").unwrap_err()).contains("unknown key"));
        assert!(format!("{:#}", get(&path, "window").unwrap_err()).contains("unknown key"));
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn view_theme_round_trips_through_set_get_and_unset() {
        let (_dir, path) = config_with("context_window = 1000000\n");
        set(&path, "view.theme", "phosphor").unwrap();
        assert_eq!(
            get(&path, "view.theme").unwrap().as_deref(),
            Some("phosphor")
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "context_window = 1000000\n\n[view]\ntheme = \"phosphor\"\n"
        );
        assert_eq!(view_theme(&path), (Palette::Phosphor, None));
        assert!(unset(&path, "view.theme").unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "context_window = 1000000\n"
        );
    }

    #[test]
    fn view_theme_is_the_default_when_unset() {
        let (_dir, path) = config_with("");
        assert_eq!(view_theme(&path), (Palette::PurpleOrange, None));
    }

    #[test]
    fn an_unknown_view_theme_warns_and_falls_back_to_the_default() {
        let (_dir, path) = config_with("[view]\ntheme = \"solarized\"\n");
        let (palette, warning) = view_theme(&path);
        assert_eq!(palette, Palette::PurpleOrange);
        let warning = warning.unwrap();
        assert!(warning.contains("unknown theme \"solarized\""), "{warning}");
        assert!(warning.contains("purple-orange"), "{warning}");
    }

    #[test]
    fn an_unreadable_config_warns_and_the_view_theme_falls_back() {
        let (_dir, path) = config_with("view = 3\n");
        let (palette, warning) = view_theme(&path);
        assert_eq!(palette, Palette::PurpleOrange);
        assert!(warning.unwrap().contains(&path.display().to_string()));
    }
}
