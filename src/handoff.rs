//! Where an agent's handoff lives in loopherd's state dir: its own directory
//! `handoffs/<name>/`, holding `handoff.md` and, once read, `prev.md`.
//! Readers fall back to the flat `handoffs/<name>.md` older releases wrote.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `name`'s own directory under `handoffs/`.
pub fn dir(state: &Path, name: &str) -> PathBuf {
    state.join("handoffs").join(name)
}

/// Where `name` writes its handoff.
pub fn path(state: &Path, name: &str) -> PathBuf {
    dir(state, name).join("handoff.md")
}

/// Where `name`'s handoff goes once read, so no later restart reuses it.
pub fn prev(state: &Path, name: &str) -> PathBuf {
    dir(state, name).join("prev.md")
}

/// `name`'s handoff waiting to be read: [`path`], else the flat file an
/// older release wrote.
pub fn find(state: &Path, name: &str) -> Option<PathBuf> {
    [path(state, name), flat(state, name)]
        .into_iter()
        .find(|p| p.is_file())
}

/// Moves `handoff`, as [`find`] gave it, to [`prev`]; returns where it went.
/// A flat file left beside a handoff in [`path`] is older, so it goes too,
/// rather than come back as the next handoff.
pub fn set_aside(state: &Path, name: &str, handoff: &Path) -> Result<PathBuf> {
    let prev = prev(state, name);
    let dir = dir(state, name);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::rename(handoff, &prev)
        .with_context(|| format!("move {} to {}", handoff.display(), prev.display()))?;
    let flat = flat(state, name);
    if handoff != flat {
        match fs::remove_file(&flat) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("remove {}", flat.display()));
            }
            _ => {}
        }
    }
    Ok(prev)
}

/// Writes `text` as `name`'s handoff, replacing any there; returns its path.
pub fn write(state: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let dir = dir(state, name);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = path(state, name);
    // Renamed into place, so a reader never sees half a handoff.
    let part = dir.join(".handoff.md.part");
    fs::write(&part, text).with_context(|| format!("write {}", part.display()))?;
    fs::rename(&part, &path)
        .with_context(|| format!("move {} to {}", part.display(), path.display()))?;
    Ok(path)
}

/// Every file `name`'s handoffs may sit in, the older flat ones included.
pub fn files(state: &Path, name: &str) -> [PathBuf; 4] {
    let handoffs = state.join("handoffs");
    [
        path(state, name),
        prev(state, name),
        flat(state, name),
        handoffs.join(format!("{name}.prev.md")),
    ]
}

/// Where an older release wrote `name`'s handoff, which [`find`] still reads.
pub fn flat(state: &Path, name: &str) -> PathBuf {
    state.join("handoffs").join(format!("{name}.md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handoff_is_found_in_its_own_dir_first_then_in_the_flat_file() {
        let state = tempfile::tempdir().unwrap();
        let state = state.path();
        assert_eq!(find(state, "lh-1"), None);

        fs::create_dir_all(state.join("handoffs")).unwrap();
        fs::write(state.join("handoffs/lh-1.md"), "old").unwrap();
        assert_eq!(find(state, "lh-1"), Some(state.join("handoffs/lh-1.md")));

        fs::create_dir_all(state.join("handoffs/lh-1")).unwrap();
        fs::write(state.join("handoffs/lh-1/handoff.md"), "new").unwrap();
        assert_eq!(
            find(state, "lh-1"),
            Some(state.join("handoffs/lh-1/handoff.md"))
        );
    }

    #[test]
    fn a_flat_handoff_set_aside_lands_in_the_agents_own_dir() {
        let state = tempfile::tempdir().unwrap();
        let state = state.path();
        fs::create_dir_all(state.join("handoffs")).unwrap();
        fs::write(state.join("handoffs/lh-1.md"), "old").unwrap();

        let moved = set_aside(state, "lh-1", &find(state, "lh-1").unwrap()).unwrap();

        assert_eq!(moved, state.join("handoffs/lh-1/prev.md"));
        assert_eq!(fs::read_to_string(moved).unwrap(), "old");
        assert_eq!(find(state, "lh-1"), None);
    }

    #[test]
    fn a_stale_flat_handoff_goes_with_the_new_one_it_sat_beside() {
        let state = tempfile::tempdir().unwrap();
        let state = state.path();
        fs::create_dir_all(state.join("handoffs/lh-1")).unwrap();
        fs::write(state.join("handoffs/lh-1.md"), "old").unwrap();
        fs::write(state.join("handoffs/lh-1/handoff.md"), "new").unwrap();

        let moved = set_aside(state, "lh-1", &find(state, "lh-1").unwrap()).unwrap();

        assert_eq!(fs::read_to_string(moved).unwrap(), "new");
        assert_eq!(find(state, "lh-1"), None);
    }
}
