//! `loopherd stats` through the binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Member<'a> {
    name: &'a str,
    herd: Option<&'a str>,
    lead: bool,
    created: &'a str,
    retired_at: Option<&'a str>,
}

impl<'a> Member<'a> {
    fn new(name: &'a str, created: &'a str) -> Self {
        Member {
            name,
            herd: Some("h"),
            lead: false,
            created,
            retired_at: None,
        }
    }

    fn retired(mut self, at: &'a str) -> Self {
        self.retired_at = Some(at);
        self
    }

    fn lead(mut self) -> Self {
        self.lead = true;
        self
    }

    fn herd(mut self, herd: Option<&'a str>) -> Self {
        self.herd = herd;
        self
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name, "pane_id": "w1:p1", "session_id": null, "repo": "/",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": self.created, "retired": self.retired_at.is_some(),
            "retired_at": self.retired_at, "herd": self.herd,
            "template": self.herd.map(|_| "card"),
            "role": if self.lead { Some("lead") } else { None },
        })
    }
}

fn write_ledger(state: &Path, members: &[Member]) {
    fs::create_dir_all(state.join("loopherd")).unwrap();
    let ledger: serde_json::Map<String, serde_json::Value> = members
        .iter()
        .map(|m| (m.name.to_string(), m.json()))
        .collect();
    fs::write(
        state.join("loopherd/ledger.json"),
        serde_json::to_string(&ledger).unwrap(),
    )
    .unwrap();
}

/// `entries` are `(at, kind)`; reports get a text.
fn write_mailbox(state: &Path, name: &str, entries: &[(&str, &str)]) {
    let dir = state.join("loopherd/reports");
    fs::create_dir_all(&dir).unwrap();
    let lines: String = entries
        .iter()
        .map(|(at, kind)| {
            let entry = match *kind {
                "start" => serde_json::json!({"at": at, "kind": "start"}),
                kind => serde_json::json!({"at": at, "kind": kind, "text": "PR #1"}),
            };
            format!("{entry}\n")
        })
        .collect();
    fs::write(dir.join(format!("{name}.jsonl")), lines).unwrap();
}

fn run(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("stats")
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("PATH", "")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_SOCKET_PATH")
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// lh-1 took 2h to its first done, two rounds, and waited 1h on merge.
fn one_retired_member(state: &Path) {
    write_ledger(
        state,
        &[Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z")],
    );
    write_mailbox(
        state,
        "lh-1",
        &[
            ("2026-11-01T00:00:05Z", "start"),
            ("2026-11-01T01:00:00Z", "note"),
            ("2026-11-01T02:00:00Z", "done"),
            ("2026-11-01T04:00:00Z", "done"),
        ],
    );
}

#[test]
fn a_retired_member_gets_a_row_of_its_flow() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());

    let out = stdout(&run(state.path(), &[]));

    let row = out
        .lines()
        .find(|l| l.trim_start().starts_with("lh-1"))
        .unwrap_or_else(|| panic!("no lh-1 row in:\n{out}"));
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(
        cells,
        [
            "lh-1",
            "card",
            "2026-11-01",
            "00:00",
            "2h00m",
            "2",
            "1h00m",
            "5h00m"
        ],
        "{out}"
    );
}

#[test]
fn each_herd_gets_a_summary_of_medians_and_maxima() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z"),
            Member::new("lh-2", "2026-11-02T00:00:00Z").retired("2026-11-02T03:00:00Z"),
        ],
    );
    write_mailbox(
        state.path(),
        "lh-1",
        &[
            ("2026-11-01T02:00:00Z", "done"),
            ("2026-11-01T04:00:00Z", "done"),
        ],
    );
    write_mailbox(state.path(), "lh-2", &[("2026-11-02T01:00:00Z", "done")]);

    let out = stdout(&run(state.path(), &[]));

    assert!(
        out.contains(
            "summary: 2 members; first done median 1h30m, max 2h00m; rounds median 1.5; \
             merge wait median 1h30m, max 2h00m; total median 4h00m, max 5h00m"
        ),
        "{out}"
    );
}

#[test]
fn leads_are_left_out_and_unretired_members_are_listed_as_open() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lead-h", "2026-11-01T00:00:00Z")
                .lead()
                .retired("2026-11-01T09:00:00Z"),
            Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z"),
            Member::new("lh-2", "2020-01-01T00:00:00Z"),
        ],
    );

    let out = stdout(&run(state.path(), &[]));

    assert!(!out.contains("lead-h"), "{out}");
    assert!(out.contains("summary: 1 members;"), "{out}");
    let open = out
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("open"))
        .collect::<Vec<_>>();
    let row: Vec<&str> = open
        .iter()
        .find(|l| l.trim_start().starts_with("lh-2"))
        .unwrap_or_else(|| panic!("no open lh-2 in:\n{out}"))
        .split_whitespace()
        .collect();
    assert_eq!(row[..4], ["lh-2", "card", "2020-01-01", "00:00"], "{out}");
    assert!(row[4].contains('d'), "years open, shown in days: {out}");
}

#[test]
fn herd_and_since_narrow_the_members() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z"),
            Member::new("lh-2", "2026-11-03T00:00:00Z").retired("2026-11-03T05:00:00Z"),
            Member::new("x-1", "2026-11-03T00:00:00Z")
                .herd(Some("other"))
                .retired("2026-11-03T05:00:00Z"),
            Member::new("solo", "2026-11-03T00:00:00Z")
                .herd(None)
                .retired("2026-11-03T05:00:00Z"),
        ],
    );

    let out = stdout(&run(state.path(), &[]));
    assert!(
        out.contains("herd h\n") && out.contains("herd other\n"),
        "{out}"
    );
    assert!(
        !out.contains("solo"),
        "members of no herd are left out: {out}"
    );

    let out = stdout(&run(
        state.path(),
        &["--herd", "h", "--since", "2026-11-02"],
    ));
    assert!(!out.contains("herd other"), "{out}");
    assert!(!out.contains("lh-1"), "{out}");
    assert!(out.contains("lh-2"), "{out}");
}

#[test]
fn a_bad_since_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let out = run(state.path(), &["--since", "last week"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--since"));
}

#[test]
fn json_gives_the_same_data() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());

    let out = stdout(&run(state.path(), &["--json"]));

    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    let herd = &json["herds"][0];
    assert_eq!(herd["herd"], "h");
    assert_eq!(
        herd["retired"][0],
        serde_json::json!({
            "name": "lh-1", "template": "card", "started": "2026-11-01T00:00:00Z",
            "first_done_secs": 7200, "rounds": 2, "merge_wait_secs": 3600,
            "total_secs": 18000,
        })
    );
    assert_eq!(herd["summary"]["members"], 1);
    assert_eq!(herd["summary"]["total"]["max_secs"], 18000);
    assert_eq!(herd["open"], serde_json::json!([]));
}

#[test]
fn stats_writes_nothing_and_leaves_reports_unread() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());
    let before = snapshot(state.path());

    stdout(&run(state.path(), &[]));
    stdout(&run(state.path(), &["--json"]));

    assert_eq!(snapshot(state.path()), before);
}

/// Every file under `dir` with its contents.
fn snapshot(dir: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut files = vec![];
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            match path.is_dir() {
                true => dirs.push(path),
                false => files.push((path.clone(), fs::read(&path).unwrap())),
            }
        }
    }
    files.sort();
    files
}

#[test]
fn a_reused_name_counts_only_reports_from_its_own_life() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[Member::new("lh-1", "2026-11-05T00:00:00Z").retired("2026-11-05T05:00:00Z")],
    );
    write_mailbox(
        state.path(),
        "lh-1",
        &[
            ("2026-11-01T02:00:00Z", "done"),
            ("2026-11-05T03:00:00Z", "done"),
        ],
    );

    let out = stdout(&run(state.path(), &["--json"]));

    let json: serde_json::Value = serde_json::from_str(&out).unwrap();
    let row = &json["herds"][0]["retired"][0];
    assert_eq!(row["rounds"], 1);
    assert_eq!(row["first_done_secs"], 3 * 3600);
}
