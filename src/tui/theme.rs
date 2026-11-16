//! The install splash's purple-and-orange palette, as ratatui styles.

use ratatui::style::{Color, Modifier, Style};

use crate::herdr::AgentStatus;

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

    pub fn key(&self) -> Style {
        self.fg(LIGHT_ORANGE)
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
