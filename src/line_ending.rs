//! A file's line ending, restored after a serialiser that writes only `\n`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    /// The ending `text`'s first line uses; LF when it has no line break.
    pub fn of(text: &str) -> Self {
        match text.split_once('\n') {
            Some((first, _)) if first.ends_with('\r') => Self::Crlf,
            _ => Self::Lf,
        }
    }

    /// `text` with every line break made this ending.
    pub fn apply(self, text: &str) -> String {
        let lf = text.replace("\r\n", "\n");
        match self {
            Self::Lf => lf,
            Self::Crlf => lf.replace('\n', "\r\n"),
        }
    }
}
