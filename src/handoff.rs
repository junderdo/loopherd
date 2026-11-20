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

/// `name`'s handoff waiting to be read: [`path`] or the flat file an older
/// release wrote, the newer when both exist (a tie goes to [`path`]).
pub fn find(state: &Path, name: &str) -> Option<PathBuf> {
    let written = |p: &Path| {
        fs::metadata(p)
            .ok()
            .filter(|m| m.is_file())?
            .modified()
            .ok()
    };
    let (new, flat) = (path(state, name), flat(state, name));
    match (written(&new), written(&flat)) {
        (Some(n), Some(f)) if f > n => Some(flat),
        (Some(_), _) => Some(new),
        (None, Some(_)) => Some(flat),
        (None, None) => None,
    }
}

/// Moves `handoff`, as [`find`] gave it, to [`prev`]; returns where it went.
/// The other layout's file, if any, is older: it moves to the flat
/// `<name>.prev.md`, so it can't come back as the next handoff.
pub fn set_aside(state: &Path, name: &str, handoff: &Path) -> Result<PathBuf> {
    let prev = prev(state, name);
    let dir = dir(state, name);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::rename(handoff, &prev)
        .with_context(|| format!("move {} to {}", handoff.display(), prev.display()))?;
    let other = match handoff == path(state, name) {
        true => flat(state, name),
        false => path(state, name),
    };
    let aside = flat_prev(state, name);
    match fs::rename(&other, &aside) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("move {} to {}", other.display(), aside.display()))
        }
        _ => Ok(prev),
    }
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
    [
        path(state, name),
        prev(state, name),
        flat(state, name),
        flat_prev(state, name),
    ]
}

/// Where an older release wrote `name`'s handoff, which [`find`] still reads.
pub fn flat(state: &Path, name: &str) -> PathBuf {
    state.join("handoffs").join(format!("{name}.md"))
}

fn flat_prev(state: &Path, name: &str) -> PathBuf {
    state.join("handoffs").join(format!("{name}.prev.md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written_at(path: &Path, text: &str, at: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(humantime::parse_rfc3339(at).unwrap())
            .unwrap();
    }

    #[test]
    fn a_handoff_is_found_in_either_layout_and_the_newer_wins_when_both_exist() {
        let state = tempfile::tempdir().unwrap();
        let state = state.path();
        let (new, flat) = (path(state, "lh-1"), state.join("handoffs/lh-1.md"));
        assert_eq!(find(state, "lh-1"), None);

        written_at(&flat, "flat", "2026-11-20T10:00:00Z");
        assert_eq!(find(state, "lh-1"), Some(flat.clone()));

        written_at(&new, "new", "2026-11-20T11:00:00Z");
        assert_eq!(find(state, "lh-1"), Some(new.clone()));

        written_at(&flat, "flat again", "2026-11-20T12:00:00Z");
        assert_eq!(find(state, "lh-1"), Some(flat));
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
    fn a_flat_handoff_newer_than_the_new_layouts_is_used_and_neither_is_deleted() {
        let state = tempfile::tempdir().unwrap();
        let state = state.path();
        written_at(&path(state, "lh-1"), "stale", "2026-11-20T10:00:00Z");
        written_at(
            &state.join("handoffs/lh-1.md"),
            "fresh",
            "2026-11-20T11:00:00Z",
        );

        let moved = set_aside(state, "lh-1", &find(state, "lh-1").unwrap()).unwrap();

        assert_eq!(fs::read_to_string(moved).unwrap(), "fresh");
        let aside = state.join("handoffs/lh-1.prev.md");
        assert_eq!(fs::read_to_string(aside).unwrap(), "stale");
        assert_eq!(find(state, "lh-1"), None);
    }

    #[test]
    fn an_older_flat_handoff_beside_a_new_one_is_moved_aside_not_deleted() {
        let state = tempfile::tempdir().unwrap();
        let state = state.path();
        written_at(
            &state.join("handoffs/lh-1.md"),
            "stale",
            "2026-11-20T10:00:00Z",
        );
        written_at(&path(state, "lh-1"), "fresh", "2026-11-20T11:00:00Z");

        let moved = set_aside(state, "lh-1", &find(state, "lh-1").unwrap()).unwrap();

        assert_eq!(fs::read_to_string(moved).unwrap(), "fresh");
        let aside = state.join("handoffs/lh-1.prev.md");
        assert_eq!(fs::read_to_string(aside).unwrap(), "stale");
        assert_eq!(find(state, "lh-1"), None);
    }
}
