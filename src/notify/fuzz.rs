//! Compares [`summary`] against another revision's on seeded random questions, to show
//! a redaction change hides at least what the base hid. `scripts/redaction-fuzz` drives
//! both tests; each is ignored in a plain `cargo test`.

use super::summary;
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
const PLAIN: [&str; 21] = [
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
const QUOTE_PAIRS: [(&str, &str); 13] = [
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
    let mut word = match rng.below(10) {
        0..=2 => rng.pick(&KEYS).to_string(),
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
        let words = 3 + rng.below(9);
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

/// Each word `summary` shows, without the punctuation a hidden value can leave glued on,
/// counted.
fn shown_words(summary: &str) -> HashMap<&str, usize> {
    let mut shown = HashMap::new();
    for word in summary
        .split_whitespace()
        .filter(|w| !w.contains("[redacted]"))
    {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric());
        if !word.is_empty() {
            *shown.entry(word).or_default() += 1;
        }
    }
    shown
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
    let (mut leaks, mut changed) = (Vec::new(), 0);
    for (question, base) in questions(seed, count).zip(base.lines()) {
        let head = summary(&question);
        if head == base {
            continue;
        }
        changed += 1;
        if changed <= 20 {
            println!("{question}\n  base: {base}\n  head: {head}");
        }
        let base_shown = shown_words(base);
        let new: Vec<_> = shown_words(&head)
            .into_iter()
            .filter(|(word, n)| base_shown.get(word).unwrap_or(&0) < n)
            .map(|(word, _)| word.to_string())
            .collect();
        if !new.is_empty() {
            leaks.push(format!(
                "{question}\n  base: {base}\n  head: {head}\n  new: {new:?}"
            ));
        }
    }
    println!(
        "{count} questions, {changed} summaries changed, {} show a word the base hid",
        leaks.len()
    );
    assert!(leaks.is_empty(), "{}", leaks.join("\n"));
}
