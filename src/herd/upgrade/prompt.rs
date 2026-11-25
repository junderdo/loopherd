//! Anchored prompt edits: insert before, insert after, or replace one
//! fragment of a lead prompt, only where that fragment appears exactly once
//! (D157).

use toml_edit::Value;

use crate::line_ending::LineEnding;

#[derive(Clone, Copy)]
pub enum Place {
    Before,
    #[allow(dead_code)] // No migration inserts after its anchor yet.
    After,
    Replace,
}

/// One prompt change newer init writes.
pub struct Anchored {
    /// A fragment init always writes once the prompt has the change.
    pub has: &'static str,
    pub anchor: &'static str,
    pub place: Place,
    pub text: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Placed<T = String> {
    Has,
    Edited(T),
    /// The anchor appears `matches` times, not once.
    Unplaced {
        matches: usize,
    },
}

impl Anchored {
    /// `prompt` with the change made, its added line breaks in the prompt's
    /// own line ending.
    pub fn place(&self, prompt: &str) -> Placed {
        if prompt.contains(self.has) {
            return Placed::Has;
        }
        let matches = prompt.matches(self.anchor).count();
        if matches != 1 {
            return Placed::Unplaced { matches };
        }
        let text = LineEnding::of(prompt).apply(self.text);
        Placed::Edited(prompt.replacen(self.anchor, &self.placed(&text), 1))
    }

    /// Where it goes, as a person reads it.
    pub fn place_name(&self) -> String {
        let anchor = self.anchor;
        match self.place {
            Place::Before => format!("before {anchor:?}"),
            Place::After => format!("after {anchor:?}"),
            Place::Replace => format!("in place of {anchor:?}"),
        }
    }

    fn placed(&self, text: &str) -> String {
        match self.place {
            Place::Before => format!("{text}{}", self.anchor),
            Place::After => format!("{}{text}", self.anchor),
            Place::Replace => text.to_string(),
        }
    }

    /// `old`, an inline TOML prompt, with the change made in its own quoting
    /// and layout where the change can be written in them.
    pub fn place_in_toml(&self, old: &Value) -> Option<Placed<Value>> {
        let prompt = old.as_str()?;
        Some(match self.place(prompt) {
            Placed::Edited(edited) => {
                let mut new = self
                    .reencoded(old, &edited)
                    .unwrap_or_else(|| edited.into());
                *new.decor_mut() = old.decor().clone();
                Placed::Edited(new)
            }
            Placed::Has => Placed::Has,
            Placed::Unplaced { matches } => Placed::Unplaced { matches },
        })
    }

    /// The edit made on `old`'s raw text, when that decodes to `edited`.
    fn reencoded(&self, old: &Value, edited: &str) -> Option<Value> {
        let Value::String(formatted) = old else {
            return None;
        };
        let raw = formatted.as_repr()?.as_raw().as_str()?;
        let style = Style::of(raw)?;
        let anchor = style.encode(self.anchor)?;
        if raw.matches(&anchor).count() != 1 {
            return None;
        }
        let placed = style.encode(&self.placed(self.text))?;
        let new: Value = raw.replacen(&anchor, &placed, 1).parse().ok()?;
        (new.as_str() == Some(edited)).then_some(new)
    }
}

#[derive(Clone, Copy)]
enum Style {
    Basic,
    MultilineBasic,
    Literal,
    MultilineLiteral,
}

impl Style {
    fn of(raw: &str) -> Option<Self> {
        [
            ("\"\"\"", Self::MultilineBasic),
            ("'''", Self::MultilineLiteral),
            ("\"", Self::Basic),
            ("'", Self::Literal),
        ]
        .into_iter()
        .find(|(quote, _)| raw.starts_with(quote))
        .map(|(_, style)| style)
    }

    /// `text` as it reads inside a string of this style.
    fn encode(self, text: &str) -> Option<String> {
        match self {
            Self::Literal if text.contains(['\'', '\n', '\r']) => None,
            Self::MultilineLiteral if text.contains("'''") => None,
            Self::Literal | Self::MultilineLiteral => Some(text.to_string()),
            Self::Basic | Self::MultilineBasic => {
                let multiline = matches!(self, Self::MultilineBasic);
                let mut out = String::new();
                for c in text.chars() {
                    match c {
                        '\\' => out.push_str("\\\\"),
                        '"' => out.push_str("\\\""),
                        '\n' if multiline => out.push('\n'),
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        '\r' => out.push_str("\\r"),
                        c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
                        c => out.push(c),
                    }
                }
                Some(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: Anchored = Anchored {
        has: "{limit}",
        anchor: "Grill.",
        place: Place::Before,
        text: "Limit {limit}.\n\n",
    };

    const REPLACE: Anchored = Anchored {
        has: "drained",
        anchor: "re-arm.",
        place: Place::Replace,
        text: "re-arm but on \"drained\".",
    };

    #[test]
    fn it_inserts_before_after_or_in_place_of_the_anchor() {
        let after = Anchored {
            place: Place::After,
            text: "\n\nLimit {limit}.",
            ..BEFORE
        };
        let prompt = "Lead.\n\nGrill.\n";
        assert_eq!(
            BEFORE.place(prompt),
            Placed::Edited("Lead.\n\nLimit {limit}.\n\nGrill.\n".into())
        );
        assert_eq!(
            after.place(prompt),
            Placed::Edited("Lead.\n\nGrill.\n\nLimit {limit}.\n".into())
        );
        assert_eq!(
            REPLACE.place("Watch, re-arm. Feed."),
            Placed::Edited("Watch, re-arm but on \"drained\". Feed.".into())
        );
    }

    #[test]
    fn a_prompt_with_the_fragment_has_it_wherever_the_anchor_is() {
        assert_eq!(BEFORE.place("Limit {limit}. Grill. Grill."), Placed::Has);
    }

    #[test]
    fn an_anchor_missing_or_repeated_is_not_placed() {
        assert_eq!(BEFORE.place("Lead."), Placed::Unplaced { matches: 0 });
        assert_eq!(
            BEFORE.place("Grill. Grill."),
            Placed::Unplaced { matches: 2 }
        );
    }

    #[test]
    fn added_line_breaks_follow_the_prompts_line_ending() {
        assert_eq!(
            BEFORE.place("Lead.\r\n\r\nGrill.\r\n"),
            Placed::Edited("Lead.\r\n\r\nLimit {limit}.\r\n\r\nGrill.\r\n".into())
        );
    }

    fn in_toml(anchored: &Anchored, raw: &str) -> Placed {
        let value: Value = raw.parse().unwrap();
        match anchored.place_in_toml(&value).unwrap() {
            Placed::Edited(value) => Placed::Edited(value.to_string()),
            Placed::Has => Placed::Has,
            Placed::Unplaced { matches } => Placed::Unplaced { matches },
        }
    }

    #[test]
    fn an_inline_prompt_keeps_its_quoting_and_layout() {
        for (raw, edited) in [
            (
                r#""Lead.\n\nGrill.""#,
                r#""Lead.\n\nLimit {limit}.\n\nGrill.""#,
            ),
            (
                "\"\"\"\nLead.\n\nGrill. \\\n  \"Go\".\"\"\"",
                "\"\"\"\nLead.\n\nLimit {limit}.\n\nGrill. \\\n  \"Go\".\"\"\"",
            ),
            (
                "'''\nLead.\n\nGrill.\n'''",
                "'''\nLead.\n\nLimit {limit}.\n\nGrill.\n'''",
            ),
        ] {
            assert_eq!(in_toml(&BEFORE, raw), Placed::Edited(edited.into()));
        }
    }

    #[test]
    fn a_change_a_literal_string_cannot_hold_is_written_in_another_quoting() {
        let edited = in_toml(&BEFORE, "'Lead. Grill.'");
        assert_eq!(
            edited,
            Placed::Edited("\"\"\"\nLead. Limit {limit}.\n\nGrill.\"\"\"".into())
        );
        assert_eq!(
            in_toml(&REPLACE, "'''Watch, re-arm.'''"),
            Placed::Edited("'''Watch, re-arm but on \"drained\".'''".into())
        );
    }

    #[test]
    fn quotes_in_a_basic_string_are_escaped() {
        assert_eq!(
            in_toml(&REPLACE, r#""Watch, re-arm.""#),
            Placed::Edited(r#""Watch, re-arm but on \"drained\".""#.into())
        );
    }

    #[test]
    fn an_inline_anchor_missing_or_repeated_is_not_placed() {
        assert_eq!(
            in_toml(&BEFORE, r#""Grill. Grill.""#),
            Placed::Unplaced { matches: 2 }
        );
        assert_eq!(in_toml(&BEFORE, r#""{limit}""#), Placed::Has);
    }
}
