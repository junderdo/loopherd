//! `scripts/trello unblock`, run against a stub `curl` that serves a fixture
//! board and logs every write, so no test reaches the real Trello API.

use std::fs;
use std::os::unix::fs::PermissionsExt;
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
        fs::write(&curl, CURL_STUB).unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();

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
            "labels": labels.iter().map(|l| serde_json::json!({"id": format!("label-{l}"), "name": l})).collect::<Vec<_>>(),
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
        let output = Command::new(format!("{REPO}/scripts/trello"))
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
            .unwrap();
        assert!(
            output.status.success(),
            "trello {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
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
fn unblocks_a_card_whose_blockers_are_all_done() {
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
            "DELETE /cards/card3/idLabels/label-blocked ",
            "POST /cards/card3/idLabels value=label-ready-for-agent",
            "POST /cards/card3/actions/comments text=Unblocked: LH-1, LH-2 Done",
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
