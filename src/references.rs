//! loopherd's user docs, embedded from the repo's own files: written beside
//! the skill as `references/` by `install-hook` and printed by `loopherd docs`.

use std::path::{Component, Path, PathBuf};

pub struct Reference {
    pub topic: &'static str,
    /// When to read it, for the topic list.
    pub summary: &'static str,
    /// The repo file it is embedded from.
    source: &'static str,
    raw: &'static str,
}

pub const ALL: [Reference; 6] = [
    Reference {
        topic: "tour",
        summary: "every command and its flags",
        source: "docs/tour.md",
        raw: include_str!("../docs/tour.md"),
    },
    Reference {
        topic: "setup",
        summary: "setting up a project and a first herd",
        source: "docs/setup.md",
        raw: include_str!("../docs/setup.md"),
    },
    Reference {
        topic: "install",
        summary: "installing, updating, the herdr plugin and key bindings",
        source: "docs/install.md",
        raw: include_str!("../docs/install.md"),
    },
    Reference {
        topic: "glossary",
        summary: "the terms: herd, shepherd, lead, member, mailbox and the rest",
        source: "CONTEXT.md",
        raw: include_str!("../CONTEXT.md"),
    },
    Reference {
        topic: "herd-file",
        summary: "an annotated herd file and lead prompt",
        source: "docs/herd-file.md",
        raw: include_str!("../docs/herd-file.md"),
    },
    Reference {
        topic: "runbook",
        summary: "what a project's shepherd runbook holds",
        source: "docs/runbook.md",
        raw: include_str!("../docs/runbook.md"),
    },
];

/// Reference file names loopherd shipped once and no longer does, which
/// `install-hook` deletes. Any other file in `references/` is the human's.
pub const RETIRED: [&str; 0] = [];

/// Repo files whose links point at the annotated herd file instead.
const HERD_FILE_EXAMPLES: &str = "docs/examples";

impl Reference {
    pub fn file_name(&self) -> String {
        format!("{}.md", self.topic)
    }

    /// The doc with each relative link pointing within the references dir, or
    /// left as plain text when it names a file that isn't shipped.
    pub fn text(&self) -> String {
        relink(self.raw, self.source)
    }
}

pub fn find(topic: &str) -> Option<&'static Reference> {
    ALL.iter().find(|r| r.topic == topic)
}

fn relink(text: &str, source: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        match fenced {
            true => out.push_str(line),
            false => relink_line(line, source, &mut out),
        }
    }
    out
}

fn relink_line(line: &str, source: &str, out: &mut String) {
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some((label, target, after)) = link_at(&rest[open..]) else {
            out.push_str(&rest[..=open]);
            rest = &rest[open + 1..];
            continue;
        };
        let image = rest[..open].ends_with('!');
        let before = match image {
            true => &rest[..open - 1],
            false => &rest[..open],
        };
        out.push_str(before);
        match retarget(target, source) {
            Some(target) if !image => out.push_str(&format!("[{label}]({target})")),
            _ if image => {}
            _ => out.push_str(label),
        }
        rest = after;
    }
    out.push_str(rest);
}

/// `[label](target)` at the start of `s`, and what follows it.
fn link_at(s: &str) -> Option<(&str, &str, &str)> {
    let mut depth = 0;
    let close = s.char_indices().find_map(|(i, c)| {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
        (depth == 0).then_some(i)
    })?;
    let after_label = s[close + 1..].strip_prefix('(')?;
    let end = after_label.find(')')?;
    Some((&s[1..close], &after_label[..end], &after_label[end + 1..]))
}

/// Where a link should point from the references dir; `None` for a repo file
/// that isn't shipped.
fn retarget(target: &str, source: &str) -> Option<String> {
    if target.starts_with('#') || target.contains("://") || target.starts_with("mailto:") {
        return Some(target.to_string());
    }
    let (path, anchor) = match target.find('#') {
        Some(i) => target.split_at(i),
        None => (target, ""),
    };
    let base = Path::new(source).parent().unwrap_or(Path::new(""));
    let resolved = normalize(&base.join(path));
    if let Some(reference) = ALL.iter().find(|r| Path::new(r.source) == resolved) {
        return Some(format!("{}{anchor}", reference.file_name()));
    }
    resolved
        .starts_with(HERD_FILE_EXAMPLES)
        .then(|| find("herd-file").expect("herd-file ships").file_name())
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut fenced = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                continue;
            }
            let mut rest = line;
            while let Some(open) = rest.find('[') {
                match link_at(&rest[open..]) {
                    Some((_, target, after)) if !fenced => {
                        found.push(target.to_string());
                        rest = after;
                    }
                    _ => rest = &rest[open + 1..],
                }
            }
        }
        found
    }

    #[test]
    fn every_relative_link_resolves_within_the_references_dir() {
        let shipped: Vec<String> = ALL.iter().map(Reference::file_name).collect();
        for reference in &ALL {
            for target in links(&reference.text()) {
                let file = target.split('#').next().unwrap();
                let ok = target.starts_with('#')
                    || target.starts_with("https://")
                    || shipped.iter().any(|name| name == file);
                assert!(ok, "{} links to {target}", reference.topic);
            }
        }
    }

    #[test]
    fn a_link_to_a_shipped_doc_points_at_its_reference_keeping_the_anchor() {
        assert_eq!(
            relink(
                "see [the glossary](../CONTEXT.md#herds) and [tour](tour.md)\n",
                "docs/setup.md"
            ),
            "see [the glossary](glossary.md#herds) and [tour](tour.md)\n"
        );
    }

    #[test]
    fn a_link_to_a_repo_only_file_becomes_its_label() {
        assert_eq!(
            relink(
                "per ([D100](decisions/D100.md)) and [README](../README.md#install).\n",
                "docs/setup.md"
            ),
            "per (D100) and README.\n"
        );
    }

    #[test]
    fn a_link_to_the_examples_points_at_the_annotated_herd_file() {
        assert_eq!(
            relink("[examples/](examples/)\n", "docs/setup.md"),
            "[examples/](herd-file.md)\n"
        );
    }

    #[test]
    fn web_links_anchors_and_fenced_code_stay_as_they_are() {
        let text = "[herdr](https://herdr.dev/) [up](#top)\n```\n[x](decisions/D1.md)\n```\n";
        assert_eq!(relink(text, "docs/install.md"), text);
    }

    #[test]
    fn brackets_that_arent_links_stay_as_they_are() {
        let text = "the `[lead]` table, `[template.issue]` and [a] (b)\n";
        assert_eq!(relink(text, "docs/setup.md"), text);
    }

    #[test]
    fn the_general_references_name_nothing_from_this_repo() {
        for topic in ["herd-file", "runbook"] {
            let text = find(topic).unwrap().text();
            for word in [
                "loopherd-cards",
                "Trello",
                "trello",
                "junderdo",
                "LH-",
                "cargo",
            ] {
                assert!(!text.contains(word), "{topic} names {word:?}");
            }
        }
    }

    #[test]
    fn the_herd_file_example_parses_as_a_herd() {
        let text = find("herd-file").unwrap().text();
        let toml = text
            .split_once("```toml\n")
            .and_then(|(_, rest)| rest.split_once("\n```"))
            .map(|(toml, _)| toml)
            .expect("a toml block");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("my-app.toml"), toml).unwrap();
        let paths = crate::herd::Paths {
            env: |_| None,
            herds: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            feed_lock: dir.path().join("feed.lock"),
            paused: dir.path().join("paused"),
        };
        let outline = crate::herd::outline(&paths, "my-app").unwrap();
        assert_eq!(outline.max, Some(3));
        assert!(outline.switches.is_some());
    }
}
