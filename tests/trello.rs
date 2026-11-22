//! `scripts/trello`, run against a stub `curl` that serves a fixture
//! board and logs every write, so no test reaches the real Trello API. The
//! board carries a stale duplicate `blocked` label ahead of the one cards use.

mod fakes;

use fakes::executable;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const REPO: &str = env!("CARGO_MANIFEST_DIR");

const CURL_STUB: &str = r#"#!/usr/bin/env bash
method=GET url= data=()
while (($#)); do
  case $1 in
    -X) method=$2; shift ;;
    --data-urlencode) data+=("$2"); shift ;;
    https://*) url=$1 ;;
  esac
  shift
done
path=${url#https://api.trello.com/1}
if [[ -f $STUB_DIR/fail && "$method $path" == $(<"$STUB_DIR/fail") ]]; then
  echo '{"error":"stub failure"}'; echo 400; exit 0
fi
board="$STUB_DIR/board.json"
case "$method $path" in
  "GET /boards/"*/lists) jq .lists "$board" ;;
  "GET /boards/"*/labels) jq .labels "$board" ;;
  "GET /boards/"*/cards/all) jq .cards "$board" ;;
  "GET /boards/"*/cards) jq '.cards | map(select(.closed | not))' "$board" ;;
  GET*) echo "unexpected GET $path" >&2; echo '{}' ;;
  *) echo "$method $path ${data[*]}" >> "$STUB_DIR/writes"; echo '{}' ;;
esac
echo 200
"#;

struct Board {
    _dir: tempfile::TempDir,
    stub: PathBuf,
    bin: PathBuf,
}

/// A card on the fixture board: number, list name, label names, description.
struct Card<'a>(u32, &'a str, &'a [&'a str], &'a str);

impl Board {
    fn new(cards: &[Card]) -> Board {
        Board::with_closed(cards, &[])
    }

    /// `closed` cards are archived: absent from open-card listings.
    fn with_closed(cards: &[Card], closed: &[Card]) -> Board {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("stub");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&stub).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let curl = bin.join("curl");
        executable(&curl, CURL_STUB);

        let lists = ["Backlog", "In Progress", "Review", "Done"];
        let labels = ["blocked", "ready-for-agent", "impl:feat"];
        let card_json = |c: &Card, is_closed: bool| {
            serde_json::json!({
                "id": format!("card{}", c.0),
                "idShort": c.0,
                "name": format!("Card {}", c.0),
                "desc": c.3,
                "idList": format!("list-{}", c.1),
                "closed": is_closed,
                "labels": c.2.iter().map(|l| serde_json::json!({"id": format!("label-{l}"), "name": l})).collect::<Vec<_>>(),
            })
        };
        let board = serde_json::json!({
            "lists": lists.iter().map(|l| serde_json::json!({"id": format!("list-{l}"), "name": l})).collect::<Vec<_>>(),
            "labels": std::iter::once(serde_json::json!({"id": "label-blocked-stale", "name": "blocked"}))
                .chain(labels.iter().map(|l| serde_json::json!({"id": format!("label-{l}"), "name": l})))
                .collect::<Vec<_>>(),
            "cards": cards.iter().map(|c| card_json(c, false))
                .chain(closed.iter().map(|c| card_json(c, true)))
                .collect::<Vec<_>>(),
        });
        fs::write(stub.join("board.json"), board.to_string()).unwrap();
        Board {
            _dir: dir,
            stub,
            bin,
        }
    }

    fn trello(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "trello {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Makes the stub answer HTTP 400 to `call`, e.g. `POST /cards/card3/actions/comments`.
    fn fail(&self, call: &str) {
        fs::write(self.stub.join("fail"), call).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(format!("{REPO}/scripts/trello"))
            .args(args)
            .env(
                "PATH",
                format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("STUB_DIR", &self.stub)
            .env("TRELLO_API_KEY", "key")
            .env("TRELLO_TOKEN", "token")
            .env("XDG_CONFIG_HOME", self.stub.join("no-config"))
            .output()
            .unwrap()
    }

    fn writes(&self) -> Vec<String> {
        fs::read_to_string(self.stub.join("writes"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn unblocking_a_card_without_ready_for_agent_adds_no_label() {
    let board = Board::new(&[
        Card(1, "Done", &[], ""),
        Card(2, "Done", &[], ""),
        Card(
            3,
            "Backlog",
            &["blocked", "impl:feat"],
            "## Goal\nx\n\nblocked-by: LH-1, LH-2\n",
        ),
    ]);

    let out = board.trello(&["unblock"]);

    assert_eq!(stdout(&out), "unblocked LH-3 (LH-1, LH-2 Done)\n");
    assert_eq!(
        board.writes(),
        [
            "POST /cards/card3/actions/comments text=Unblocked: LH-1, LH-2 Done",
            "DELETE /cards/card3/idLabels/label-blocked ",
        ]
    );
}

#[test]
fn an_archived_blocker_in_done_counts_as_done() {
    let board = Board::with_closed(
        &[Card(3, "Backlog", &["blocked"], "blocked-by: LH-1")],
        &[Card(1, "Done", &[], "")],
    );

    let out = board.trello(&["unblock"]);

    assert_eq!(stdout(&out), "unblocked LH-3 (LH-1 Done)\n");
}

#[test]
fn leaves_alone_cards_it_cannot_unblock() {
    let board = Board::new(&[
        Card(1, "Done", &[], ""),
        Card(2, "Review", &[], ""),
        Card(3, "Backlog", &["blocked"], "waits on LH-1"),
        Card(4, "Backlog", &["blocked"], "blocked-by: LH-1, LH-2"),
        Card(5, "Backlog", &["blocked"], "blocked-by: LH-1, LH-99"),
        Card(6, "Backlog", &["impl:feat"], "blocked-by: LH-1"),
    ]);

    let out = board.trello(&["unblock"]);

    assert_eq!(stdout(&out), "");
    assert_eq!(
        stderr(&out),
        "trello: warning: LH-5 is blocked by LH-99, which doesn't exist\n"
    );
    assert!(board.writes().is_empty(), "wrote: {:?}", board.writes());
}

#[test]
fn dry_run_prints_what_it_would_change_and_writes_nothing() {
    let board = Board::new(&[
        Card(1, "Done", &[], ""),
        Card(3, "Backlog", &["blocked"], "blocked-by: LH-1"),
    ]);

    let out = board.trello(&["unblock", "--dry-run"]);

    assert_eq!(stdout(&out), "would unblock LH-3 (LH-1 Done)\n");
    assert!(board.writes().is_empty(), "wrote: {:?}", board.writes());
}

#[test]
fn a_card_with_ready_for_agent_keeps_it_and_loses_blocked() {
    let board = Board::new(&[
        Card(1, "Done", &[], ""),
        Card(
            3,
            "Backlog",
            &["blocked", "ready-for-agent"],
            "blocked-by: LH-1",
        ),
    ]);

    board.trello(&["unblock"]);

    assert_eq!(
        board.writes(),
        [
            "POST /cards/card3/actions/comments text=Unblocked: LH-1 Done",
            "DELETE /cards/card3/idLabels/label-blocked ",
        ]
    );
}

#[test]
fn a_failed_write_leaves_the_card_blocked() {
    let board = Board::new(&[
        Card(1, "Done", &[], ""),
        Card(3, "Backlog", &["blocked"], "blocked-by: LH-1"),
    ]);
    board.fail("POST /cards/card3/actions/comments");

    let out = board.run(&["unblock"]);

    assert!(!out.status.success());
    assert!(board.writes().is_empty(), "wrote: {:?}", board.writes());
}

#[test]
fn an_archived_blocked_card_is_left_alone() {
    let board = Board::with_closed(
        &[Card(1, "Done", &[], "")],
        &[Card(3, "Backlog", &["blocked"], "blocked-by: LH-1")],
    );

    let out = board.trello(&["unblock"]);

    assert_eq!(stdout(&out), "");
    assert!(board.writes().is_empty(), "wrote: {:?}", board.writes());
}

#[test]
fn a_malformed_blocked_by_line_is_warned_about_and_left_alone() {
    for line in [
        "blocked-by: LH-1, lh-2",
        "blocked-by: LH-1, 2",
        "blocked-by: LH-1 and LH 2",
        "blocked-by: XLH-1",
        "blocked-by:",
        "Blocked-by: LH-1",
        "BLOCKED-BY: LH-1",
        "  blocked-by: LH-1",
    ] {
        let board = Board::new(&[
            Card(1, "Done", &[], ""),
            Card(3, "Backlog", &["blocked"], &format!("## Goal\nx\n{line}\n")),
        ]);

        let out = board.trello(&["unblock"]);

        assert_eq!(stdout(&out), "", "{line}");
        assert_eq!(
            stderr(&out),
            format!("trello: warning: LH-3 has a malformed blocked-by line: {line}\n"),
            "{line}"
        );
        assert!(
            board.writes().is_empty(),
            "{line} wrote: {:?}",
            board.writes()
        );
    }
}

fn run_line(board: &Board, line: &str) -> Output {
    Command::new("bash")
        .args(["-c", line])
        .current_dir(REPO)
        .env(
            "PATH",
            format!("{}:{}", board.bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("STUB_DIR", &board.stub)
        .env("TRELLO_API_KEY", "key")
        .env("TRELLO_TOKEN", "token")
        .env("XDG_CONFIG_HOME", board.stub.join("no-config"))
        .output()
        .unwrap()
}

#[test]
fn archive_closes_the_card() {
    let board = Board::new(&[Card(3, "Backlog", &[], "")]);

    let output = board.trello(&["archive", "LH-3"]);

    assert_eq!(stdout(&output), "archived LH-3\n");
    assert_eq!(board.writes(), ["PUT /cards/card3 closed=true"]);
}

#[test]
fn a_groom_batchs_archive_line_runs_as_written_with_its_reason_as_a_shell_comment() {
    let board = Board::new(&[Card(3, "Backlog", &[], "")]);

    let output = run_line(&board, "scripts/trello archive LH-3  # superseded by LH-12");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(board.writes(), ["PUT /cards/card3 closed=true"]);
}

#[test]
fn a_groom_batchs_double_quoted_comment_line_keeps_apostrophes_quotes_and_dollars() {
    let board = Board::new(&[Card(3, "Backlog", &[], "")]);

    let output = run_line(
        &board,
        r#"scripts/trello comment LH-3 "Doesn't say which \"file\" or \$HOME path to change""#,
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        board.writes(),
        [
            r#"POST /cards/card3/actions/comments text=Doesn't say which "file" or $HOME path to change"#
        ]
    );
}
