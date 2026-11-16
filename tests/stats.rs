//! `loopherd stats` through the binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Member<'a> {
    name: &'a str,
    herd: Option<&'a str>,
    lead: bool,
    created: &'a str,
    retired: bool,
    retired_at: Option<&'a str>,
}

impl<'a> Member<'a> {
    fn new(name: &'a str, created: &'a str) -> Self {
        Member {
            name,
            herd: Some("h"),
            lead: false,
            created,
            retired: false,
            retired_at: None,
        }
    }

    fn retired(mut self, at: &'a str) -> Self {
        self.retired = true;
        self.retired_at = Some(at);
        self
    }

    /// As in ledgers written before retire recorded the time.
    fn retired_untimed(mut self) -> Self {
        self.retired = true;
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
            "created": self.created, "retired": self.retired,
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

fn json(state: &Path, args: &[&str]) -> serde_json::Value {
    let args: Vec<&str> = args.iter().copied().chain(["--json"]).collect();
    serde_json::from_str(&stdout(&run(state, &args))).unwrap()
}

fn names(json: &serde_json::Value, list: &str) -> Vec<String> {
    json["herds"][0][list]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect()
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
    for since in [
        "last week",
        "2026-11-01T12:00:00+0500",
        "2026-11-01T12:00:00+05x00",
        "2026-11-01T12:00:00+25:00",
        "2026-11-01T12:00:00+24:00",
        "2026-11-01T12:00:00+05:60",
        "2026-11-01T12:00:00+-5:00",
        "2026-11-01T12:00:00++5:00",
        "2026-11-01T12:00:00+05:-1",
        "2026-11-01T12:00:00Z+05:00",
        "2026-11-01T12:00:00+00:00+05:00",
        "2026-11-01T12:00:00ZZ",
        "2026-11-01T12:00:00.+05:00",
        "2026-11-01T12:00:00",
    ] {
        let out = run(state.path(), &["--since", since]);
        assert_eq!(out.status.code(), Some(2), "--since {since}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("--since"));
    }
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

#[test]
fn a_done_in_the_second_of_retirement_counts() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z")],
    );
    write_mailbox(
        state.path(),
        "lh-1",
        &[("2026-11-01T05:00:00.400000000Z", "done")],
    );

    let row = &json(state.path(), &[])["herds"][0]["retired"][0];

    assert_eq!(row["rounds"], 1);
    assert_eq!(row["merge_wait_secs"], 0);
}

#[test]
fn a_done_after_retirement_is_not_counted() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z")],
    );
    write_mailbox(
        state.path(),
        "lh-1",
        &[
            ("2026-11-01T02:00:00Z", "done"),
            ("2026-11-01T05:00:01Z", "done"),
        ],
    );

    let row = &json(state.path(), &[])["herds"][0]["retired"][0];

    assert_eq!(row["rounds"], 1);
    assert_eq!(row["merge_wait_secs"], 3 * 3600);
}

#[test]
fn a_member_without_a_done_keeps_its_row_but_not_the_rounds_median() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z"),
            Member::new("lh-2", "2026-11-02T00:00:00Z").retired("2026-11-02T01:00:00Z"),
        ],
    );
    write_mailbox(state.path(), "lh-1", &[("2026-11-01T02:00:00Z", "done")]);

    let herd = &json(state.path(), &[])["herds"][0];

    assert_eq!(
        herd["retired"][1],
        serde_json::json!({
            "name": "lh-2", "template": "card", "started": "2026-11-02T00:00:00Z",
            "first_done_secs": null, "rounds": 0, "merge_wait_secs": null,
            "total_secs": 3600,
        })
    );
    assert_eq!(herd["summary"]["rounds_median"], 1.0);
    assert_eq!(herd["summary"]["total"]["median_secs"], 3 * 3600);
}

#[test]
fn an_untimed_retirement_shows_no_merge_wait_or_total() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[Member::new("lh-1", "2026-11-01T00:00:00Z").retired_untimed()],
    );
    write_mailbox(state.path(), "lh-1", &[("2026-11-01T02:00:00Z", "done")]);

    let row = &json(state.path(), &[])["herds"][0]["retired"][0];

    assert_eq!(row["first_done_secs"], 7200);
    assert_eq!(row["merge_wait_secs"], serde_json::Value::Null);
    assert_eq!(row["total_secs"], serde_json::Value::Null);
}

#[test]
fn rows_are_ordered_by_start_time() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lh-a", "2026-11-02T00:00:00Z").retired("2026-11-02T05:00:00Z"),
            Member::new("lh-b", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z"),
        ],
    );

    assert_eq!(names(&json(state.path(), &[]), "retired"), ["lh-b", "lh-a"]);
}

fn since_fixture(state: &Path) {
    write_ledger(
        state,
        &[
            Member::new("before", "2026-11-01T11:59:59Z"),
            Member::new("at", "2026-11-01T12:00:00Z"),
        ],
    );
}

#[test]
fn since_is_on_or_after_and_honors_an_offset() {
    let state = tempfile::tempdir().unwrap();
    since_fixture(state.path());

    for since in [
        "2026-11-01T12:00:00Z",
        "2026-11-01T12:00:00+00:00",
        "2026-11-01T05:00:00-07:00",
        "2026-11-01T17:00:00.0+05:00",
        "2026-11-01T17:30:00+05:30",
        "2026-11-01T02:30:00-09:30",
        "2026-11-01T12:00:00z",
    ] {
        let json = json(state.path(), &["--since", since]);
        assert_eq!(names(&json, "open"), ["at"], "--since {since}");
    }
}

#[test]
fn a_date_since_means_midnight_utc() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("before", "2026-10-31T23:59:59Z"),
            Member::new("at", "2026-11-01T00:00:00Z"),
        ],
    );

    let json = json(state.path(), &["--since", "2026-11-01"]);

    assert_eq!(names(&json, "open"), ["at"]);
}

#[test]
fn an_unparsable_mailbox_line_is_warned_about() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());
    let mailbox = state.path().join("loopherd/reports/lh-1.jsonl");
    let mut text = fs::read_to_string(&mailbox).unwrap();
    text.push_str("not json\n");
    fs::write(&mailbox, text).unwrap();

    let out = run(state.path(), &[]);

    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("loopherd stats: warning: skipped a bad line in"),
        "{stderr}"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("lh-1"));
}

#[test]
fn a_since_without_a_zone_says_so() {
    let state = tempfile::tempdir().unwrap();
    let out = run(state.path(), &["--since", "2026-11-01T12:00:00"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no zone"),
        "{out:?}"
    );
}
