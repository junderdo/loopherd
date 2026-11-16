//! The version `loopherd --version` prints, worked out by build.rs from git.

/// What build.rs learns from git about the tree it builds.
pub struct Git {
    pub commit: String,
    pub dirty: bool,
    /// HEAD carries the `v<version>` tag.
    pub tagged: bool,
}

pub fn describe(version: &str, git: Option<Git>) -> String {
    match git {
        Some(git) if git.dirty => format!("{version} ({}-dirty)", git.commit),
        Some(git) if !git.tagged => format!("{version} ({})", git.commit),
        _ => version.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dirty: bool, tagged: bool) -> Option<Git> {
        Some(Git {
            commit: "ea71305".into(),
            dirty,
            tagged,
        })
    }

    #[test]
    fn a_clone_build_names_its_commit() {
        assert_eq!(describe("0.1.0", git(false, false)), "0.1.0 (ea71305)");
    }

    #[test]
    fn a_dirty_tree_says_so() {
        assert_eq!(describe("0.1.0", git(true, false)), "0.1.0 (ea71305-dirty)");
        assert_eq!(describe("0.1.0", git(true, true)), "0.1.0 (ea71305-dirty)");
    }

    #[test]
    fn a_clean_build_of_the_release_tag_is_the_bare_version() {
        assert_eq!(describe("0.1.0", git(false, true)), "0.1.0");
    }

    #[test]
    fn a_build_without_git_is_the_bare_version() {
        assert_eq!(describe("0.1.0", None), "0.1.0");
    }
}
