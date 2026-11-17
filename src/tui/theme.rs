//! The install splash's purple-and-orange palette, as ratatui styles.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::herdr::AgentStatus;
use crate::report::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    Truecolor,
    Indexed,
    None,
}

/// A color in truecolor and its 256-color fallback.
type Paint = ((u8, u8, u8), u8);

const PURPLE: Paint = ((150, 50, 230), 93);
const ORANGE: Paint = ((255, 154, 31), 208);
const LIGHT_ORANGE: Paint = ((255, 192, 77), 214);
const GREY_VIOLET: Paint = ((120, 110, 140), 60);
const SOFT_VIOLET: Paint = ((160, 165, 190), 146);
const LIGHT: Paint = ((235, 235, 242), 254);
const WHITE: Paint = ((255, 255, 255), 231);

/// The splash's 256-color stand-ins for its purple-to-orange gradient.
const PURPLE_TO_ORANGE: [u8; 5] = [93, 134, 169, 203, 202];

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    depth: Depth,
}

impl Theme {
    /// The splash's check: truecolor when `COLORTERM` says so, 256 colors
    /// otherwise, and none when `no_color` (`NO_COLOR` set and non-empty).
    pub fn from_env(colorterm: Option<&str>, no_color: bool) -> Theme {
        let depth = match (colorterm, no_color) {
            (_, true) => Depth::None,
            (Some("truecolor" | "24bit"), _) => Depth::Truecolor,
            _ => Depth::Indexed,
        };
        Theme { depth }
    }

    fn color(&self, ((r, g, b), index): Paint) -> Option<Color> {
        match self.depth {
            Depth::Truecolor => Some(Color::Rgb(r, g, b)),
            Depth::Indexed => Some(Color::Indexed(index)),
            Depth::None => None,
        }
    }

    fn fg(&self, paint: Paint) -> Style {
        self.color(paint)
            .map_or(Style::new(), |c| Style::new().fg(c))
    }

    fn plain_fg(&self, color: Color) -> Style {
        match self.depth {
            Depth::None => Style::new(),
            _ => Style::new().fg(color),
        }
    }

    pub fn border(&self) -> Style {
        self.fg(PURPLE)
    }

    pub fn title(&self) -> Style {
        self.fg(ORANGE).add_modifier(Modifier::BOLD)
    }

    /// `text` in bold, its letters running from purple to orange like the
    /// splash's wordmark.
    pub fn wordmark(&self, text: &str) -> Line<'static> {
        let last = text.trim().chars().count().saturating_sub(1).max(1) as u32;
        let mut letter = 0;
        let spans = text.chars().map(|c| {
            if c == ' ' {
                return Span::raw(" ");
            }
            let fg = match self.depth {
                Depth::Truecolor => Some(Color::Rgb(
                    (150 + 105 * letter / last) as u8,
                    (50 + 30 * letter / last) as u8,
                    (230 - 230 * letter / last) as u8,
                )),
                Depth::Indexed => Some(Color::Indexed(
                    PURPLE_TO_ORANGE[(4 * letter / last) as usize],
                )),
                Depth::None => None,
            };
            letter += 1;
            let style = Style::new().add_modifier(Modifier::BOLD);
            Span::styled(c.to_string(), fg.map_or(style, |fg| style.fg(fg)))
        });
        Line::from(spans.collect::<Vec<_>>())
    }

    pub fn heading(&self) -> Style {
        self.fg(GREY_VIOLET).add_modifier(Modifier::BOLD)
    }

    /// Without color the selection is shown reversed.
    pub fn selected(&self) -> Style {
        match (self.color(ORANGE), self.color(LIGHT)) {
            (Some(bg), Some(fg)) => Style::new().bg(bg).fg(fg),
            _ => Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// The splash's nearest sheep: bold white.
    pub fn sheep(&self) -> Style {
        self.fg(WHITE).add_modifier(Modifier::BOLD)
    }

    pub fn key(&self) -> Style {
        self.fg(LIGHT_ORANGE)
    }

    pub fn paused(&self) -> Style {
        self.fg(LIGHT_ORANGE)
    }

    /// A time in status past its first threshold.
    /// Without color it is underlined, to stay apart from a fresh time.
    pub fn stale(&self) -> Style {
        match self.depth {
            Depth::None => Style::new().add_modifier(Modifier::UNDERLINED),
            _ => self.fg(LIGHT_ORANGE),
        }
    }

    /// A time in status past its last threshold.
    pub fn overdue(&self) -> Style {
        self.plain_fg(Color::Red).add_modifier(Modifier::BOLD)
    }

    pub fn label(&self) -> Style {
        self.fg(GREY_VIOLET)
    }

    pub fn toggle(&self, on: bool) -> Style {
        match on {
            true => self.fg(ORANGE),
            false => Style::new().add_modifier(Modifier::DIM),
        }
    }

    /// A report's kind: questions orange, done green, handoffs purple, notes grey.
    pub fn report(&self, kind: Kind) -> Style {
        let style = match kind {
            Kind::Question => self.fg(ORANGE),
            Kind::Done => self.plain_fg(Color::Green),
            Kind::Handoff => self.fg(PURPLE),
            Kind::Note => self.fg(GREY_VIOLET),
        };
        style.add_modifier(Modifier::BOLD)
    }

    /// `context` of `limit` as six cells and a percent: grey-violet below
    /// 60%, light orange from 60%, bold red from 85%.
    pub fn gauge(&self, context: u64, limit: u64) -> Span<'static> {
        let (context, limit) = (u128::from(context), u128::from(limit.max(1)));
        let filled = ((context * 12 + limit) / (limit * 2)).min(6) as usize;
        let percent = context * 100 / limit;
        let style = match percent {
            85.. => self.plain_fg(Color::Red).add_modifier(Modifier::BOLD),
            60.. => self.fg(LIGHT_ORANGE),
            _ => self.fg(GREY_VIOLET),
        };
        let cells = "▰".repeat(filled) + &"▱".repeat(6 - filled);
        Span::styled(format!("{cells} {percent}%"), style)
    }

    /// The glyph leading a member's row; `frame` picks the spinner's frame.
    pub fn glyph(&self, status: AgentStatus, gone: bool, frame: usize) -> (&'static str, Style) {
        if gone {
            return ("✕", self.plain_fg(Color::Red));
        }
        match status {
            AgentStatus::Working => (SPINNER[frame % SPINNER.len()], self.fg(ORANGE)),
            AgentStatus::Idle => ("○", self.fg(SOFT_VIOLET)),
            AgentStatus::Done => ("✓", self.fg(SOFT_VIOLET)),
            AgentStatus::Blocked => ("⚠", self.plain_fg(Color::Yellow)),
            AgentStatus::Unknown => ("?", Style::new().add_modifier(Modifier::DIM)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gauge_of_huge_values_does_not_overflow() {
        let theme = Theme::from_env(None, true);
        assert_eq!(
            theme.gauge(u64::MAX, 1).content,
            format!("▰▰▰▰▰▰ {}00%", u64::MAX)
        );
        assert_eq!(theme.gauge(1, u64::MAX).content, "▱▱▱▱▱▱ 0%");
    }
}
