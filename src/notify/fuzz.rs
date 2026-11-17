//! Compares [`summary`] against another revision's on seeded random questions, to show
//! a redaction change hides at least what the base hid. `scripts/redaction-fuzz` drives
//! both tests; each is ignored in a plain `cargo test`.

use super::{MAX_TEXT, summary};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Shapes that leaked in earlier attempts at reading non-ASCII quotes.
const KNOWN_SHAPES: [&str; 19] = [
    "TOKEN passphrase=Bearer” abc123def456 then merge?",
    "TOKEN passphrase=\"Bearer\" abc123def456 then merge?",
    "password: ‘a’ꭓb hunter22 xyz’ ok?",
    "password: ‘a’ɐb hunter22 xyz’ ok?",
    "password: ‘a’ﬁb hunter22 xyz’ ok?",
    "password: ‘a’ｂb hunter22 xyz’ ok?",
    "password: ‘a’ᴀb hunter22 xyz’ ok?",
    "password: ‘a’ấb hunter22 xyz’ ok?",
    "password: ‘a’ḍb hunter22 xyz’ ok?",
    "password passphrase=hunter22 auth=password” abc123def456 ok",
    "«password»: ‚password‘: auth abc123; sid=abc123;",
    "--password ’password’: password=\"abc abc” abc123;",
    "”password”= «password»: password=\"abc x] s3cr3tval",
    "‚password‘: «password»: «abc «TOKEN» s3cr3tval",
    "token : \"Bearer\" : \"token\"= abc hunter22;",
    "«token» : «Bearer» : «token»: abc hunter22;",
    "passphrase=Bearer’ : «password»: l’été abc123;",
    "abc’s passphrase=«Bearer» : “password”: it’s hunter22 session=abc123 Cookie:",
    "password: ‘п’ять hunter22’ ok?",
];

const KEYS: [&str; 18] = [
    "password",
    "passphrase",
    "token",
    "TOKEN",
    "PASSWORD",
    "SECRET",
    "auth",
    "session",
    "key",
    "Bearer",
    "Cookie",
    "Authorization",
    "GITHUB_TOKEN",
    "--password",
    "pw",
    "pass",
    "pwd",
    "sid",
];
const SEPARATORS: [&str; 7] = [":", "=", "is", "to", "was", "is:", "="];
const SECRETS: [&str; 7] = [
    "hunter22",
    "abc123def456",
    "s3cr3tval",
    "abc123",
    "xkcdqwrtz",
    "ghp_abcdefghijkl1234",
    "correct",
];
const PLAIN: [&str; 23] = [
    "https://u:pw@h/x",
    "https://x.io/?token=abc123",
    "then",
    "merge",
    "#107",
    "ok",
    "horse",
    "battery",
    "abc",
    "x]",
    "it’s",
    "l’été",
    "abc’s",
    "don't",
    "п’ять",
    "是吗",
    "Then",
    "the",
    "Set",
    "Use",
    "a",
    "v2",
    "flow",
];
const QUOTE_PAIRS: [(&str, &str); 18] = [
    ("‟", "”"),
    ("‛", "’"),
    ("＇", "＇"),
    ("‹", "›"),
    ("＂", "＂"),
    ("\"", "\""),
    ("'", "'"),
    ("“", "”"),
    ("”", "”"),
    ("„", "“"),
    ("„", "”"),
    ("‚", "‘"),
    ("‚", "’"),
    ("‘", "’"),
    ("’", "’"),
    ("«", "»"),
    ("»", "«"),
    ("(", ")"),
];
const ENDINGS: [&str; 8] = [",", ";", "?", "!", ".", ":", "=", "”"];

/// xorshift64*, so the base and the head draw the same questions from a seed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % n
    }

    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.below(from.len())]
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

fn word(rng: &mut Rng) -> String {
    let mut word = match rng.below(11) {
        0..=2 => rng.pick(&KEYS).to_string(),
        10 => rng.pick(&["a", "b", "x7"]).repeat(5 + rng.below(40)),
        3 => rng.pick(&SEPARATORS).to_string(),
        4 | 5 => rng.pick(&SECRETS).to_string(),
        6 => {
            let (open, close) = QUOTE_PAIRS[rng.below(QUOTE_PAIRS.len())];
            let value = match rng.chance(50) {
                true => format!("{open}{}{close}", rng.pick(&SECRETS)),
                false => rng.pick(&SECRETS).to_string(),
            };
            format!("{}{}{value}", rng.pick(&KEYS), rng.pick(&[":", "="]))
        }
        _ => rng.pick(&PLAIN).to_string(),
    };
    if rng.chance(30) {
        let (open, close) = QUOTE_PAIRS[rng.below(QUOTE_PAIRS.len())];
        word = match rng.below(3) {
            0 => format!("{open}{word}"),
            1 => format!("{word}{close}"),
            _ => format!("{open}{word}{close}"),
        };
    }
    if rng.chance(20) {
        word.push_str(rng.pick(&ENDINGS));
    }
    word
}

fn questions(seed: u64, count: usize) -> impl Iterator<Item = String> {
    let mut rng = Rng(seed.max(1));
    let random = std::iter::repeat_with(move || {
        let words = match rng.chance(25) {
            true => 15 + rng.below(30),
            false => 3 + rng.below(9),
        };
        let words: Vec<String> = (0..words).map(|_| word(&mut rng)).collect();
        words.join(" ")
    });
    KNOWN_SHAPES
        .into_iter()
        .map(String::from)
        .chain(random)
        .take(count)
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}; see scripts/redaction-fuzz"))
}

fn seed_and_count() -> (u64, usize) {
    (
        env("LOOPHERD_FUZZ_SEED").parse().unwrap(),
        env("LOOPHERD_FUZZ_COUNT").parse().unwrap(),
    )
}

/// Each word `summary` shows whole, without the punctuation a hidden value can leave
/// glued on, counted; and the words in the token a cut line ends on.
fn shown_words(summary: &str) -> (HashMap<&str, usize>, Vec<&str>) {
    let (whole, cut) = match summary.strip_suffix('…') {
        Some(line) => line.rsplit_once(' ').unwrap_or(("", line)),
        None => (summary, ""),
    };
    let mut shown = HashMap::new();
    for word in words(whole) {
        *shown.entry(word).or_default() += 1;
    }
    let cut = match cut.rfind('[') {
        Some(at) if "[redacted]".starts_with(&cut[at..]) => &cut[..at],
        _ => cut,
    };
    (shown, words(cut).collect())
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace()
        .flat_map(|w| w.split("[redacted]"))
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
}

/// What `head` shows that `base` doesn't: a whole word shown more often, or a cut
/// word's start that no word `base` shows begins with. `base` is the base's summary
/// with its cut lifted, so a word the base only cut off doesn't count as hidden; a line
/// that still ends in `…` ends in a token that may be a whole word, so it counts as shown.
fn shown_only_in(head: &str, base: &str) -> Vec<String> {
    let (mut base_whole, base_cut) = shown_words(base);
    for &word in &base_cut {
        *base_whole.entry(word).or_default() += 1;
    }
    let (head_whole, head_cut) = shown_words(head);
    let whole = head_whole
        .into_iter()
        .filter(|(word, n)| base_whole.get(word).unwrap_or(&0) < n)
        .map(|(word, _)| word);
    let cut = head_cut
        .into_iter()
        .filter(|start| !base_whole.keys().any(|word| word.starts_with(start)));
    whole.chain(cut).map(String::from).collect()
}

#[test]
#[ignore = "run by scripts/redaction-fuzz"]
fn write_summaries() {
    let (seed, count) = seed_and_count();
    let mut out = String::new();
    for question in questions(seed, count) {
        writeln!(out, "{}", summary(&question)).unwrap();
    }
    std::fs::write(env("LOOPHERD_FUZZ_OUT"), out).unwrap();
}

#[test]
#[ignore = "run by scripts/redaction-fuzz"]
fn shows_no_word_the_base_hid() {
    let (seed, count) = seed_and_count();
    let base = std::fs::read_to_string(env("LOOPHERD_FUZZ_BASE")).unwrap();
    let uncut = std::fs::read_to_string(env("LOOPHERD_FUZZ_BASE_UNCUT")).unwrap();
    let (mut leaks, mut changed, mut cut) = (Vec::new(), 0, 0);
    for ((question, base), uncut) in questions(seed, count).zip(base.lines()).zip(uncut.lines()) {
        cut += usize::from(base.ends_with('…'));
        let head = summary(&question);
        if head == base {
            continue;
        }
        changed += 1;
        if changed <= 20 {
            println!("{question}\n  base: {base}\n  head: {head}");
        }
        let new = shown_only_in(&head, uncut);
        if !new.is_empty() {
            leaks.push(format!(
                "{question}\n  base: {base}\n  uncut: {uncut}\n  head: {head}\n  new: {new:?}"
            ));
        }
    }
    println!(
        "{count} questions, {cut} cut at {MAX_TEXT} chars by the base, {changed} summaries changed, {} show a word the base hid",
        leaks.len()
    );
    assert!(leaks.is_empty(), "{}", leaks.join("\n"));
}

#[test]
fn a_word_past_the_bases_cut_is_not_a_leak_but_one_its_rules_hid_is() {
    let uncut = "password: [redacted] then merge #107?";
    assert!(shown_only_in("password: [redacted] then merge #107?", uncut).is_empty());
    assert!(shown_only_in("password: [redacted] then me…", uncut).is_empty());
    assert_eq!(
        shown_only_in("password: hunter22 then merge", uncut),
        ["hunter22"]
    );
    assert_eq!(
        shown_only_in("password: [redacted] then hun…", uncut),
        ["hun"]
    );
}
