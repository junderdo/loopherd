//! `{key}` placeholders in prompts, names and branches (D9).

use std::collections::HashMap;

use anyhow::{Result, bail};

pub struct Template {
    parts: Vec<Part>,
}

enum Part {
    Text(String),
    Key(String),
}

impl Template {
    /// Fails on an unclosed `{` or a stray `}`, naming it.
    pub fn parse(text: &str) -> Result<Template> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = text.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            match c {
                '{' if chars.next_if(|&(_, c)| c == '{').is_some() => literal.push('{'),
                '}' if chars.next_if(|&(_, c)| c == '}').is_some() => literal.push('}'),
                '{' => {
                    let key = placeholder(&text[at + 1..])?;
                    chars.nth(key.chars().count());
                    parts.push(Part::Text(std::mem::take(&mut literal)));
                    parts.push(Part::Key(key.to_string()));
                }
                '}' => bail!("stray }} after {:?}", context_before(&text[..at])),
                c => literal.push(c),
            }
        }
        parts.push(Part::Text(literal));
        Ok(Template { parts })
    }

    /// Fails on the first placeholder whose key is not in `keys`.
    pub fn check<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let keys: Vec<&str> = keys.into_iter().collect();
        for key in self.keys() {
            if !keys.contains(&key) {
                bail!("unknown placeholder {{{key}}}");
            }
        }
        Ok(())
    }

    fn keys(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|part| match part {
            Part::Key(key) => Some(key.as_str()),
            Part::Text(_) => None,
        })
    }

    /// Fails like `check` if a placeholder has no value.
    pub fn render(&self, values: &HashMap<&str, &str>) -> Result<String> {
        self.check(values.keys().copied())?;
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Key(key) => out.push_str(values[key.as_str()]),
            }
        }
        Ok(out)
    }
}

/// The key at the start of `rest`, which follows a `{`.
fn placeholder(rest: &str) -> Result<&str> {
    match rest.find(['{', '}']) {
        Some(end) if rest[end..].starts_with('}') => Ok(&rest[..end]),
        Some(end) => bail!("unclosed placeholder {{{}", &rest[..end]),
        None => bail!("unclosed placeholder {{{}", context_after(rest)),
    }
}

const CONTEXT_CHARS: usize = 20;

fn context_before(text: &str) -> String {
    let skip = text.chars().count().saturating_sub(CONTEXT_CHARS);
    text.chars().skip(skip).collect()
}

fn context_after(text: &str) -> String {
    text.chars().take(CONTEXT_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str, values: &[(&str, &str)]) -> Result<String> {
        Template::parse(text)?.render(&values.iter().copied().collect())
    }

    #[test]
    fn placeholders_become_their_values() {
        assert_eq!(
            render(
                "You're {name} on {branch}; {name} again.",
                &[("name", "lh-14"), ("branch", "feat/x")]
            )
            .unwrap(),
            "You're lh-14 on feat/x; lh-14 again."
        );
    }

    #[test]
    fn an_unknown_key_is_an_error_naming_it() {
        let err = render("hi {nmae}", &[("name", "a")]).unwrap_err();
        assert_eq!(format!("{err:#}"), "unknown placeholder {nmae}");
    }

    #[test]
    fn doubled_braces_are_literal_braces() {
        assert_eq!(
            render("fn {name}() {{ x }} {{{name}}}", &[("name", "f")]).unwrap(),
            "fn f() { x } {f}"
        );
    }

    #[test]
    fn an_unclosed_or_stray_brace_is_an_error_naming_it() {
        for (text, message) in [
            ("hi {name", "unclosed placeholder {name"),
            ("hi {na{me}", "unclosed placeholder {na"),
            ("hi {", "unclosed placeholder {"),
            ("hi name}", "stray } after \"hi name\""),
            ("}", "stray } after \"\""),
            (
                "a long line of prompt text}",
                "stray } after \" line of prompt text\"",
            ),
            (
                "{a long line of prompt text",
                "unclosed placeholder {a long line of promp",
            ),
        ] {
            let err = Template::parse(text).err().unwrap();
            assert_eq!(format!("{err:#}"), message, "{text:?}");
        }
    }
}
