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
    outcome: Option<&'a str>,
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
            outcome: None,
        }
    }

    /// Retired by feed with its merged check passing.
    fn retired(self, at: &'a str) -> Self {
        self.ended(at, Some("merged"))
    }

    /// Retired with `outcome`; `None` as in ledgers written before retire
    /// recorded it.
    fn ended(mut self, at: &'a str, outcome: Option<&'a str>) -> Self {
        self.retired = true;
        self.retired_at = Some(at);
        self.outcome = outcome;
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
        let mut json = serde_json::json!({
            "name": self.name, "pane_id": "w1:p1", "session_id": null, "repo": "/",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": self.created, "retired": self.retired,
            "retired_at": self.retired_at, "herd": self.herd,
            "template": self.herd.map(|_| "card"),
            "role": if self.lead { Some("lead") } else { None },
        });
        if let Some(outcome) = self.outcome {
            json["outcome"] = outcome.into();
        }
        json
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
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("PATH", "")
        // Printed times are UTC whatever the local zone.
        .env("TZ", "America/New_York")
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
            "5h00m",
            "-",
            "-",
            "merged"
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
            "summary: 2 members: 2 merged; first done median 1h30m, max 2h00m; rounds median 1.5; \
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
    assert!(out.contains("summary: 1 members: 1 merged;"), "{out}");
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
fn both_tables_label_started_as_utc() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lh-1", "2026-11-10T10:00:00Z").retired("2026-11-10T12:00:00Z"),
            Member::new("lh-2", "2026-11-10T11:00:00Z"),
        ],
    );

    let out = stdout(&run(state.path(), &[]));

    let header = |first: &str| {
        out.lines()
            .find(|l| l.trim_start().starts_with(first))
            .unwrap_or_else(|| panic!("no {first} header in:\n{out}"))
            .to_string()
    };
    assert!(header("member").contains("  started (UTC)  "), "{out}");
    assert!(header("open").contains("  started (UTC)  "), "{out}");
    assert!(out.contains("  2026-11-10 10:00  "), "{out}");
    assert!(out.contains("  2026-11-10 11:00  "), "{out}");
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
            "total_secs": 18000, "tokens": null, "outcome": "merged",
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
fn a_released_and_refed_name_gets_a_row_per_run_and_since_filters_each() {
    let state = tempfile::tempdir().unwrap();
    let mut current = Member::new("lh-1", "2026-11-05T00:00:00Z")
        .retired("2026-11-05T05:00:00Z")
        .json();
    let mut earlier = Member::new("lh-1", "2026-11-01T00:00:00Z")
        .retired("2026-11-01T02:00:00Z")
        .json();
    earlier["released_at"] = "2026-11-04T00:00:00Z".into();
    current["earlier"] = serde_json::json!([earlier]);
    fs::create_dir_all(state.path().join("loopherd")).unwrap();
    fs::write(
        state.path().join("loopherd/ledger.json"),
        serde_json::json!({ "lh-1": current }).to_string(),
    )
    .unwrap();
    write_mailbox(
        state.path(),
        "lh-1",
        &[
            ("2026-11-01T01:00:00Z", "done"),
            ("2026-11-05T03:00:00Z", "done"),
        ],
    );

    let all = json(state.path(), &[]);
    let since = json(state.path(), &["--since", "2026-11-03"]);

    let rows: Vec<_> = all["herds"][0]["retired"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["started"].as_str().unwrap(),
                r["rounds"].as_u64().unwrap(),
                r["first_done_secs"].as_u64().unwrap(),
                r["total_secs"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("2026-11-01T00:00:00Z", 1, 3600, 2 * 3600),
            ("2026-11-05T00:00:00Z", 1, 3 * 3600, 5 * 3600),
        ]
    );
    let since_rows: Vec<_> = since["herds"][0]["retired"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["started"].as_str().unwrap())
        .collect();
    assert_eq!(since_rows, ["2026-11-05T00:00:00Z"]);
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
            "total_secs": 3600, "tokens": null, "outcome": "merged",
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

#[test]
fn an_unreadable_mailbox_warns_and_its_row_keeps_the_ledger_columns() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z"),
            Member::new("lh-2", "2026-11-02T00:00:00Z").retired("2026-11-02T03:00:00Z"),
        ],
    );
    write_mailbox(state.path(), "lh-1", &[("2026-11-01T02:00:00Z", "done")]);
    let unreadable = state.path().join("loopherd/reports/lh-2.jsonl");
    fs::create_dir(&unreadable).unwrap();

    let out = run(state.path(), &[]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr,
        format!(
            "loopherd stats: warning: could not read {}: Is a directory (os error 21)\n",
            unreadable.display()
        )
    );
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let row = |name: &str| -> Vec<String> {
        stdout
            .lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("no {name} row in:\n{stdout}"))
            .split_whitespace()
            .map(String::from)
            .collect()
    };
    assert_eq!(
        row("lh-1"),
        [
            "lh-1",
            "card",
            "2026-11-01",
            "00:00",
            "2h00m",
            "1",
            "3h00m",
            "5h00m",
            "-",
            "-",
            "merged"
        ]
    );
    assert_eq!(
        row("lh-2"),
        [
            "lh-2",
            "card",
            "2026-11-02",
            "00:00",
            "-",
            "-",
            "-",
            "3h00m",
            "-",
            "-",
            "merged"
        ]
    );
}

#[test]
fn an_unknown_herd_is_refused_by_name() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());

    for args in [&["--herd", "nope"][..], &["--herd", "nope", "--json"]] {
        let out = run(state.path(), args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "loopherd stats: no herd nope\n"
        );
    }
}

#[test]
fn a_herd_with_a_file_but_no_members_is_empty() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());
    fs::create_dir_all(state.path().join("config/herds")).unwrap();
    fs::write(state.path().join("config/herds/quiet.toml"), "").unwrap();

    assert_eq!(stdout(&run(state.path(), &["--herd", "quiet"])), "");
    assert_eq!(
        json(state.path(), &["--herd", "quiet"]),
        serde_json::json!({ "herds": [] })
    );
}

#[test]
fn a_herd_known_only_from_retired_records_is_fine() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());

    assert_eq!(
        names(&json(state.path(), &["--herd", "h"]), "retired"),
        ["lh-1"]
    );
    let out = stdout(&run(
        state.path(),
        &["--herd", "h", "--since", "2030-01-01"],
    ));
    assert_eq!(out, "");
}

#[test]
fn json_gives_null_rounds_for_a_member_whose_mailbox_is_unreadable() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z")],
    );
    fs::create_dir_all(state.path().join("loopherd/reports/lh-1.jsonl")).unwrap();

    let out = run(state.path(), &["--json"]);

    assert!(
        String::from_utf8_lossy(&out.stderr).contains("warning: could not read "),
        "{out:?}"
    );
    assert_eq!(out.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let retired = &json["herds"][0]["retired"][0];
    assert_eq!(retired["name"], "lh-1");
    assert_eq!(
        retired.get("rounds"),
        Some(&serde_json::Value::Null),
        "{retired}"
    );
}

/// Appends raw `entries` to `name`'s mailbox.
fn append_entries(state: &Path, name: &str, entries: &[serde_json::Value]) {
    let dir = state.join("loopherd/reports");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.jsonl"));
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    for entry in entries {
        text.push_str(&format!("{entry}\n"));
    }
    fs::write(path, text).unwrap();
}

fn stop(at: &str, session: &str) -> serde_json::Value {
    serde_json::json!({
        "at": at, "kind": "stop", "session": session, "crons": 0,
        "background": {"shells": 0, "subagents": 0, "other": 0},
    })
}

/// An assistant line of a transcript; `[input, cache write, cache read, output]`.
fn assistant(id: &str, model: &str, at: &str, tokens: [u64; 4]) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant", "timestamp": at,
        "message": {
            "id": id, "model": model, "role": "assistant",
            "usage": {
                "input_tokens": tokens[0],
                "cache_creation_input_tokens": tokens[1],
                "cache_read_input_tokens": tokens[2],
                "output_tokens": tokens[3],
            },
        },
    })
}

/// `relative` is under a Claude project dir: `<session>.jsonl`, or
/// `<session>/subagents/<agent>.jsonl`.
fn write_transcript(state: &Path, relative: &str, lines: &[serde_json::Value]) {
    let path = state.join("claude/projects/-repo").join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text =
        String::from("{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n");
    for line in lines {
        text.push_str(&format!("{line}\n"));
    }
    fs::write(path, text).unwrap();
}

#[test]
fn a_workers_tokens_sum_its_sessions_across_a_restart_counting_each_message_once() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());
    append_entries(
        state.path(),
        "lh-1",
        &[
            stop("2026-11-01T01:00:00Z", "s1"),
            serde_json::json!({"at": "2026-11-01T02:30:00Z", "kind": "start"}),
            stop("2026-11-01T03:00:00Z", "s2"),
            stop("2026-11-01T03:30:00Z", "s2"),
        ],
    );
    let first = assistant("m1", "opus", "2026-11-01T00:10:00Z", [1, 2, 3, 4]);
    write_transcript(state.path(), "s1.jsonl", &[first.clone(), first]);
    write_transcript(
        state.path(),
        "s2.jsonl",
        &[assistant(
            "m2",
            "opus",
            "2026-11-01T02:10:00Z",
            [10, 20, 30, 40],
        )],
    );

    let json = json(state.path(), &[]);

    assert_eq!(
        json["herds"][0]["retired"][0]["tokens"],
        serde_json::json!({
            "opus": {"input": 11, "cache_write": 22, "cache_read": 33, "output": 44},
        })
    );
}

#[test]
fn a_workers_tokens_add_its_subagents_and_skip_a_missing_transcript() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());
    append_entries(
        state.path(),
        "lh-1",
        &[
            stop("2026-11-01T01:00:00Z", "gone"),
            stop("2026-11-01T03:00:00Z", "s1"),
        ],
    );
    write_transcript(
        state.path(),
        "s1.jsonl",
        &[assistant(
            "m1",
            "opus",
            "2026-11-01T00:10:00Z",
            [1, 2, 3, 4],
        )],
    );
    write_transcript(
        state.path(),
        "s1/subagents/agent-a.jsonl",
        &[
            assistant("m2", "haiku", "2026-11-01T00:20:00Z", [5, 0, 0, 1]),
            assistant("m2", "haiku", "2026-11-01T00:20:00Z", [5, 0, 0, 1]),
        ],
    );

    let json = json(state.path(), &[]);

    assert_eq!(
        json["herds"][0]["retired"][0]["tokens"],
        serde_json::json!({
            "haiku": {"input": 5, "cache_write": 0, "cache_read": 0, "output": 1},
            "opus": {"input": 1, "cache_write": 2, "cache_read": 3, "output": 4},
        })
    );
}

#[test]
fn a_worker_with_no_transcript_left_has_no_tokens() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());
    append_entries(
        state.path(),
        "lh-1",
        &[stop("2026-11-01T01:00:00Z", "gone")],
    );

    let json = json(state.path(), &[]);

    assert_eq!(
        json["herds"][0]["retired"][0]["tokens"],
        serde_json::Value::Null
    );
}

#[test]
fn the_table_shows_in_and_out_and_the_summary_their_medians() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lh-1", "2026-11-01T00:00:00Z").retired("2026-11-01T05:00:00Z"),
            Member::new("lh-2", "2026-11-02T00:00:00Z").retired("2026-11-02T05:00:00Z"),
            Member::new("lh-3", "2026-11-03T00:00:00Z").retired("2026-11-03T05:00:00Z"),
            Member::new("lh-4", "2026-11-04T00:00:00Z"),
        ],
    );
    append_entries(state.path(), "lh-1", &[stop("2026-11-01T01:00:00Z", "s1")]);
    append_entries(state.path(), "lh-2", &[stop("2026-11-02T01:00:00Z", "s2")]);
    append_entries(state.path(), "lh-4", &[stop("2026-11-04T01:00:00Z", "s4")]);
    write_transcript(
        state.path(),
        "s1.jsonl",
        &[assistant(
            "m1",
            "opus",
            "2026-11-01T00:10:00Z",
            [100, 200, 700, 50],
        )],
    );
    write_transcript(
        state.path(),
        "s2.jsonl",
        &[assistant(
            "m2",
            "opus",
            "2026-11-02T00:10:00Z",
            [1_000, 2_000, 2_500_000, 3_000],
        )],
    );
    write_transcript(
        state.path(),
        "s4.jsonl",
        &[assistant(
            "m4",
            "opus",
            "2026-11-04T00:10:00Z",
            [1, 1, 1, 7],
        )],
    );

    let out = stdout(&run(state.path(), &[]));

    let row = |name: &str| -> Vec<String> {
        out.lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("no {name} row in:\n{out}"))
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };
    let header = row("member");
    assert_eq!(
        header[header.len() - 3..],
        ["in", "out", "outcome"],
        "{out}"
    );
    assert_eq!(row("lh-1")[8..10], ["1.0k", "50"], "{out}");
    assert_eq!(row("lh-2")[8..10], ["2.5M", "3.0k"], "{out}");
    assert_eq!(row("lh-3")[8..10], ["-", "-"], "{out}");
    assert_eq!(
        row("lh-4")[5..],
        ["3", "7"],
        "open members show theirs: {out}"
    );
    assert!(
        out.contains("; in median 1.3M; out median 1.5k\n"),
        "lh-3's unknown tokens are left out: {out}"
    );
}

#[test]
fn each_herd_gets_its_leads_tokens_since_the_window_as_overhead() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lead-h", "2026-10-01T00:00:00Z").lead(),
            Member::new("lh-1", "2026-11-02T00:00:00Z").retired("2026-11-02T05:00:00Z"),
        ],
    );
    append_entries(
        state.path(),
        "lead-h",
        &[
            stop("2026-10-31T00:00:00Z", "l1"),
            stop("2026-11-02T00:00:00Z", "l2"),
        ],
    );
    write_transcript(
        state.path(),
        "l1.jsonl",
        &[
            assistant("a", "opus", "2026-10-31T00:00:00Z", [1_000, 0, 0, 1_000]),
            assistant("b", "opus", "2026-11-01T12:00:00Z", [1, 2, 3, 4]),
        ],
    );
    write_transcript(
        state.path(),
        "l2.jsonl",
        &[assistant(
            "c",
            "opus",
            "2026-11-02T00:00:00Z",
            [10, 20, 30, 40],
        )],
    );
    write_transcript(
        state.path(),
        "l2/subagents/agent-r.jsonl",
        &[assistant(
            "d",
            "sonnet",
            "2026-11-02T01:00:00Z",
            [100, 0, 0, 5],
        )],
    );

    let out = stdout(&run(state.path(), &["--since", "2026-11-01"]));

    assert!(out.contains("  lead overhead: in 166, out 49\n"), "{out}");
    assert!(
        !out.contains("  lead-h "),
        "the lead still gets no row: {out}"
    );
    let json = json(state.path(), &["--since", "2026-11-01"]);
    assert_eq!(
        json["herds"][0]["lead_overhead"],
        serde_json::json!({
            "opus": {"input": 11, "cache_write": 22, "cache_read": 33, "output": 44},
            "sonnet": {"input": 100, "cache_write": 0, "cache_read": 0, "output": 5},
        })
    );
}

#[test]
fn a_retired_workers_recorded_usage_outlasts_its_transcripts() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());
    append_entries(
        state.path(),
        "lh-1",
        &[
            stop("2026-11-01T01:00:00Z", "s1"),
            serde_json::json!({
                "at": "2026-11-01T05:00:00Z", "kind": "usage",
                "models": {"opus": {"input": 7, "cache_write": 8, "cache_read": 9, "output": 10}},
            }),
        ],
    );
    write_transcript(
        state.path(),
        "s1.jsonl",
        &[assistant(
            "m1",
            "opus",
            "2026-11-01T00:10:00Z",
            [1, 1, 1, 1],
        )],
    );

    let json = json(state.path(), &[]);

    assert_eq!(
        json["herds"][0]["retired"][0]["tokens"],
        serde_json::json!({
            "opus": {"input": 7, "cache_write": 8, "cache_read": 9, "output": 10},
        })
    );
}

#[test]
fn a_herd_with_only_its_lead_in_the_window_still_gets_its_overhead() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lead-h", "2026-10-01T00:00:00Z").lead(),
            Member::new("lh-1", "2026-10-02T00:00:00Z").retired("2026-10-02T05:00:00Z"),
        ],
    );
    append_entries(
        state.path(),
        "lead-h",
        &[stop("2026-11-02T00:00:00Z", "l1")],
    );
    write_transcript(
        state.path(),
        "l1.jsonl",
        &[assistant("a", "opus", "2026-11-02T00:00:00Z", [1, 2, 3, 4])],
    );

    let out = stdout(&run(state.path(), &["--since", "2026-11-01"]));

    assert!(out.contains("herd h\n"), "{out}");
    assert!(out.contains("  lead overhead: in 6, out 4\n"), "{out}");
}

#[test]
fn synthetic_messages_and_the_ledgers_session_are_handled() {
    let state = tempfile::tempdir().unwrap();
    let mut ledger = Member::new("lh-1", "2026-11-01T00:00:00Z").json();
    ledger["session_id"] = "s1".into();
    fs::create_dir_all(state.path().join("loopherd")).unwrap();
    fs::write(
        state.path().join("loopherd/ledger.json"),
        serde_json::json!({"lh-1": ledger}).to_string(),
    )
    .unwrap();
    write_transcript(
        state.path(),
        "s1.jsonl",
        &[
            assistant("m1", "opus", "2026-11-01T00:10:00Z", [1, 2, 3, 4]),
            assistant("m2", "<synthetic>", "2026-11-01T00:11:00Z", [0, 0, 0, 0]),
        ],
    );

    let json = json(state.path(), &[]);

    assert_eq!(
        json["herds"][0]["open"][0]["tokens"],
        serde_json::json!({
            "opus": {"input": 1, "cache_write": 2, "cache_read": 3, "output": 4},
        }),
        "a first turn has no stop yet; the ledger names its session"
    );
}

#[test]
fn a_reused_name_counts_only_its_own_runs_sessions() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[Member::new("lh-1", "2026-11-05T00:00:00Z").retired("2026-11-05T05:00:00Z")],
    );
    append_entries(
        state.path(),
        "lh-1",
        &[
            stop("2026-11-01T01:00:00Z", "old"),
            serde_json::json!({
                "at": "2026-11-01T05:00:00Z", "kind": "usage",
                "models": {"opus": {"input": 9, "cache_write": 9, "cache_read": 9, "output": 9}},
            }),
            stop("2026-11-05T01:00:00Z", "new"),
        ],
    );
    write_transcript(
        state.path(),
        "old.jsonl",
        &[assistant(
            "m1",
            "opus",
            "2026-11-01T00:10:00Z",
            [100, 0, 0, 100],
        )],
    );
    write_transcript(
        state.path(),
        "new.jsonl",
        &[assistant(
            "m2",
            "opus",
            "2026-11-05T00:10:00Z",
            [1, 0, 0, 1],
        )],
    );

    let json = json(state.path(), &[]);

    assert_eq!(
        json["herds"][0]["retired"][0]["tokens"],
        serde_json::json!({
            "opus": {"input": 1, "cache_write": 0, "cache_read": 0, "output": 1},
        })
    );
}

#[test]
fn a_herd_with_only_an_idle_lead_in_the_window_is_left_out() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lead-h", "2026-10-01T00:00:00Z").lead(),
            Member::new("lh-1", "2026-10-02T00:00:00Z").retired("2026-10-02T05:00:00Z"),
        ],
    );
    append_entries(
        state.path(),
        "lead-h",
        &[stop("2026-10-02T00:00:00Z", "l1")],
    );
    write_transcript(
        state.path(),
        "l1.jsonl",
        &[assistant("a", "opus", "2026-10-02T00:00:00Z", [1, 2, 3, 4])],
    );

    let out = stdout(&run(state.path(), &["--since", "2026-11-01"]));

    assert_eq!(out, "", "nothing happened in h since then");
}

#[test]
fn a_leads_mailbox_is_read_once_however_many_runs_it_had() {
    let state = tempfile::tempdir().unwrap();
    let mut lead = Member::new("lead-h", "2026-11-03T00:00:00Z").lead().json();
    lead["earlier"] = serde_json::json!([
        Member::new("lead-h", "2026-11-01T00:00:00Z")
            .lead()
            .retired("2026-11-02T00:00:00Z")
            .json(),
        Member::new("lead-h", "2026-11-02T00:00:00Z")
            .lead()
            .retired("2026-11-03T00:00:00Z")
            .json(),
    ]);
    fs::create_dir_all(state.path().join("loopherd/reports")).unwrap();
    fs::write(
        state.path().join("loopherd/ledger.json"),
        serde_json::json!({
            "lead-h": lead,
            "lh-1": Member::new("lh-1", "2026-11-01T00:00:00Z").json(),
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        state.path().join("loopherd/reports/lead-h.jsonl"),
        "not json\n",
    )
    .unwrap();

    let out = run(state.path(), &[]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("skipped a bad line").count(), 1, "{stderr}");
}

#[test]
fn a_torn_transcript_line_and_an_unstamped_message_are_skipped_under_since() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[
            Member::new("lead-h", "2026-10-01T00:00:00Z").lead(),
            Member::new("lh-1", "2026-11-02T00:00:00Z").retired("2026-11-02T05:00:00Z"),
        ],
    );
    append_entries(
        state.path(),
        "lead-h",
        &[stop("2026-11-02T00:00:00Z", "l1")],
    );
    append_entries(state.path(), "lh-1", &[stop("2026-11-02T01:00:00Z", "s1")]);
    let mut unstamped = assistant("b", "opus", "2026-11-02T00:00:00Z", [100, 0, 0, 100]);
    unstamped.as_object_mut().unwrap().remove("timestamp");
    write_transcript(
        state.path(),
        "l1.jsonl",
        &[
            assistant("a", "opus", "2026-11-02T00:00:00Z", [1, 2, 3, 4]),
            unstamped,
        ],
    );
    write_transcript(
        state.path(),
        "s1.jsonl",
        &[assistant("c", "opus", "2026-11-02T00:10:00Z", [1, 0, 0, 1])],
    );
    let s1 = state.path().join("claude/projects/-repo/s1.jsonl");
    let mut text = fs::read_to_string(&s1).unwrap();
    text.push_str(r#"{"type":"assistant","message":{"id":"torn","model":"opus","usage":{"inp"#);
    fs::write(&s1, text).unwrap();

    let json = json(state.path(), &["--since", "2026-11-01"]);

    let herd = &json["herds"][0];
    assert_eq!(
        herd["lead_overhead"]["opus"],
        serde_json::json!({"input": 1, "cache_write": 2, "cache_read": 3, "output": 4}),
        "the unstamped message can't be placed in the window"
    );
    assert_eq!(
        herd["retired"][0]["tokens"]["opus"],
        serde_json::json!({"input": 1, "cache_write": 0, "cache_read": 0, "output": 1}),
    );
}

#[test]
fn a_corrupt_huge_count_saturates_instead_of_overflowing() {
    let state = tempfile::tempdir().unwrap();
    one_retired_member(state.path());
    append_entries(state.path(), "lh-1", &[stop("2026-11-01T01:00:00Z", "s1")]);
    write_transcript(
        state.path(),
        "s1.jsonl",
        &[
            assistant(
                "m1",
                "opus",
                "2026-11-01T00:10:00Z",
                [u64::MAX, 1, 0, u64::MAX],
            ),
            assistant("m2", "opus", "2026-11-01T00:11:00Z", [1, 0, 0, 1]),
        ],
    );

    let out = stdout(&run(state.path(), &[]));

    assert!(
        out.contains("in median 18446744073709.6M; out median 18446744073709.6M"),
        "{out}"
    );
}

/// One run per outcome, plus one from a ledger written before outcomes. The
/// merged lh-1 and lh-2 took 2h and 4h with one `done` an hour in, and read
/// 10 and 30 tokens and wrote 100 and 300. Every other run has two `done`s,
/// the first five hours in, and 9000 tokens each way.
fn a_run_of_each_outcome(state: &Path) {
    let runs = [
        ("lh-1", "2026-11-01", "02:00:00Z", Some("merged")),
        ("lh-2", "2026-11-02", "04:00:00Z", Some("merged")),
        ("lh-3", "2026-11-03", "20:00:00Z", Some("closed")),
        ("lh-4", "2026-11-04", "21:00:00Z", Some("released")),
        ("lh-5", "2026-11-05", "22:00:00Z", Some("retired")),
        ("lh-6", "2026-11-06", "23:00:00Z", None),
    ];
    let times: Vec<(String, String)> = runs
        .iter()
        .map(|(_, day, end, _)| (format!("{day}T00:00:00Z"), format!("{day}T{end}")))
        .collect();
    let members: Vec<Member> = runs
        .iter()
        .zip(&times)
        .map(|((name, _, _, outcome), (start, end))| Member::new(name, start).ended(end, *outcome))
        .collect();
    write_ledger(state, &members);
    for ((name, day, _, outcome), (_, end)) in runs.iter().zip(&times) {
        let merged = *outcome == Some("merged");
        let dones: &[&str] = if merged { &["01"] } else { &["05", "06"] };
        let (read, written) = match *name {
            "lh-1" => (10, 100),
            "lh-2" => (30, 300),
            _ => (9000, 9000),
        };
        let mut entries: Vec<serde_json::Value> = dones
            .iter()
            .map(|hour| {
                serde_json::json!({
                    "at": format!("{day}T{hour}:00:00Z"), "kind": "done", "text": "PR #1",
                })
            })
            .collect();
        entries.push(serde_json::json!({
            "at": end, "kind": "usage",
            "models": {"opus": {
                "input": read, "cache_write": 0, "cache_read": 0, "output": written,
            }},
        }));
        append_entries(state, name, &entries);
    }
}

#[test]
fn each_row_shows_how_its_run_ended_and_a_dash_for_runs_retired_before_outcomes() {
    let state = tempfile::tempdir().unwrap();
    a_run_of_each_outcome(state.path());

    let out = stdout(&run(state.path(), &[]));
    let herd = &json(state.path(), &[])["herds"][0];

    let last_cells: Vec<&str> = out
        .lines()
        .filter(|l| l.trim_start().starts_with("member") || l.trim_start().starts_with("lh-"))
        .map(|l| l.split_whitespace().last().unwrap())
        .collect();
    assert_eq!(
        last_cells,
        [
            "outcome", "merged", "merged", "closed", "released", "retired", "-"
        ],
        "{out}"
    );
    let outcomes: Vec<&serde_json::Value> = herd["retired"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["outcome"])
        .collect();
    assert_eq!(
        outcomes,
        ["merged", "merged", "closed", "released", "retired"]
            .map(serde_json::Value::from)
            .iter()
            .chain([&serde_json::Value::Null])
            .collect::<Vec<_>>()
    );
}

#[test]
fn medians_and_maxima_cover_only_merged_runs_and_the_summary_counts_the_rest_apart() {
    let state = tempfile::tempdir().unwrap();
    a_run_of_each_outcome(state.path());

    let out = stdout(&run(state.path(), &[]));
    let summary = &json(state.path(), &[])["herds"][0]["summary"];

    assert!(
        out.contains(
            "summary: 6 members: 2 merged, 1 closed, 1 released, 1 retired, 1 unrecorded; \
             first done median 1h00m, max 1h00m; rounds median 1; \
             merge wait median 2h00m, max 3h00m; total median 3h00m, max 4h00m; in median 20; out median 200"
        ),
        "{out}"
    );
    assert_eq!(
        *summary,
        serde_json::json!({
            "members": 6,
            "outcomes": {
                "merged": 2, "closed": 1, "released": 1, "retired": 1, "unrecorded": 1,
            },
            "first_done": {"median_secs": 3600, "max_secs": 3600},
            "rounds_median": 1.0,
            "merge_wait": {"median_secs": 7200, "max_secs": 10800},
            "total": {"median_secs": 10800, "max_secs": 14400},
            "in_median": 20, "out_median": 200,
        })
    );
}

#[test]
fn a_herd_with_no_merged_run_has_no_medians() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(
        state.path(),
        &[Member::new("lh-1", "2026-11-01T00:00:00Z").ended("2026-11-01T05:00:00Z", None)],
    );
    write_mailbox(state.path(), "lh-1", &[("2026-11-01T02:00:00Z", "done")]);

    let out = stdout(&run(state.path(), &[]));

    assert!(
        out.contains(
            "summary: 1 members: 0 merged, 1 unrecorded; first done median -, max -; \
             rounds median -; merge wait median -, max -; total median -, max -; in median -; out median -"
        ),
        "{out}"
    );
}
