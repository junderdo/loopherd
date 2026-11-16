//! `$LOOPHERD_CONFIG_DIR/config.toml`: user-wide settings, every one optional.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

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
    let file: ConfigFile = match fs::read_to_string(config) {
        Ok(text) => toml::from_str(&text).with_context(|| config.display().to_string())?,
        Err(e) if e.kind() == ErrorKind::NotFound => ConfigFile::default(),
        Err(e) => return Err(e).with_context(|| format!("read {}", config.display())),
    };
    let shepherd = file.shepherd.as_ref().and_then(|s| s.context_limit);
    for (key, value) in [
        ("default_context_limit", file.default_context_limit),
        ("context_window", file.context_window),
        ("shepherd.context_limit", shepherd),
    ] {
        check_tokens(key, value).with_context(|| config.display().to_string())?;
    }
    Ok(file)
}
