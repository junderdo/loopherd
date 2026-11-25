//! init's questions: asked on a terminal, else answered by a flag or the
//! inferred value.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Result, bail};

pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// `question` on stderr and the line typed in answer.
pub fn line(question: &str) -> Result<String> {
    eprint!("{question}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("no answer on stdin");
    }
    Ok(line)
}

pub struct Asker {
    pub terminal: bool,
}

impl Asker {
    /// `given` when set. Else on a terminal the answer to `question`, shown
    /// below `explanation` when not empty, Enter taking `default` (shown when
    /// not empty), re-asked until `parse` takes it. Else `default`, failing
    /// with `missing` when there is none.
    pub fn value<T>(
        &self,
        question: &str,
        explanation: &str,
        given: Option<&str>,
        default: Option<&str>,
        missing: &str,
        parse: impl Fn(&str) -> Result<T>,
    ) -> Result<T> {
        if let Some(given) = given {
            return parse(given);
        }
        if !self.terminal {
            return match default {
                Some(default) => parse(default),
                None => bail!("{missing}"),
            };
        }
        explain(explanation);
        if default.is_none() && !missing.is_empty() {
            eprintln!("{missing}");
        }
        let shown = match default {
            Some(default) if !default.is_empty() => format!(" ({default})"),
            _ => String::new(),
        };
        loop {
            let answer = line(&format!("{question}{shown}: "))?;
            let answer = match (answer.trim(), default) {
                ("", Some(default)) => default,
                ("", None) => continue,
                (answer, _) => answer,
            };
            match parse(answer) {
                Ok(value) => return Ok(value),
                Err(e) => eprintln!("{e:#}"),
            }
        }
    }

    /// `given` when set, else a yes or no to `question` on a terminal, shown
    /// below `explanation` when not empty, else `default`.
    pub fn yes(
        &self,
        question: &str,
        explanation: &str,
        given: Option<bool>,
        default: bool,
    ) -> Result<bool> {
        if let Some(given) = given {
            return Ok(given);
        }
        if !self.terminal {
            return Ok(default);
        }
        explain(explanation);
        let options = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            match line(&format!("{question} {options}: "))?
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => eprintln!("answer y or n"),
            }
        }
    }
}

/// `explanation` on stderr, above the prompt that follows it.
pub fn explain(explanation: &str) {
    if !explanation.is_empty() {
        eprintln!("{explanation}");
    }
}
