//! `loopherd view`'s palettes, as ratatui styles.

use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::herd_view::Member;
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

/// A palette's colors by role.
struct Colors {
    /// Borders and handoff reports.
    primary: Paint,
    /// Titles, the selection, the working spinner and questions.
    accent: Paint,
    /// Keys, paused and stale times.
    accent_light: Paint,
    /// Headings, labels and notes.
    muted: Paint,
    /// Idle and done glyphs.
    soft: Paint,
    /// The selected row's text, dark to read on `accent`.
    on_accent: Paint,
    bright: Paint,
    /// The wordmark runs from `primary` to this in truecolor.
    wordmark_end: (u8, u8, u8),
    /// The wordmark's 256-color steps.
    wordmark_steps: [u8; 5],
}

/// The install splash's purple and orange.
const PURPLE_ORANGE: Colors = Colors {
    primary: ((150, 50, 230), 93),
    accent: ((255, 154, 31), 208),
    accent_light: ((255, 192, 77), 214),
    muted: ((120, 110, 140), 60),
    soft: ((160, 165, 190), 146),
    on_accent: ((20, 16, 28), 232),
    bright: ((255, 255, 255), 231),
    wordmark_end: (255, 80, 0),
    wordmark_steps: [93, 134, 169, 203, 202],
};

/// junderdo.dev's green and amber on stone.
const PHOSPHOR: Colors = Colors {
    primary: ((0x05, 0xdf, 0x72), 41),
    accent: ((0xff, 0xd2, 0x30), 221),
    accent_light: ((0x7b, 0xf1, 0xa8), 121),
    muted: ((0xa6, 0xa0, 0x9b), 247),
    soft: ((0xd6, 0xd3, 0xd1), 252),
    on_accent: ((0x0c, 0x0f, 0x0b), 233),
    bright: ((255, 255, 255), 231),
    wordmark_end: (0xff, 0xd2, 0x30),
    wordmark_steps: [41, 77, 113, 149, 221],
};

/// Catppuccin Mocha, the flavor of herdr's `catppuccin` theme.
const CATPPUCCIN: Colors = Colors {
    primary: ((0xcb, 0xa6, 0xf7), 183),
    accent: ((0xfa, 0xb3, 0x87), 216),
    accent_light: ((0xf9, 0xe2, 0xaf), 223),
    muted: ((0x6c, 0x70, 0x86), 243),
    soft: ((0xa6, 0xad, 0xc8), 146),
    on_accent: ((0x1e, 0x1e, 0x2e), 234),
    bright: ((0xcd, 0xd6, 0xf4), 189),
    wordmark_end: (0xfa, 0xb3, 0x87),
    wordmark_steps: [183, 182, 181, 217, 216],
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Palette {
    #[default]
    PurpleOrange,
    Phosphor,
    Catppuccin,
}

impl Palette {
    pub const ALL: [Palette; 3] = [
        Palette::PurpleOrange,
        Palette::Phosphor,
        Palette::Catppuccin,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Palette::PurpleOrange => "purple-orange",
            Palette::Phosphor => "phosphor",
            Palette::Catppuccin => "catppuccin",
        }
    }

    /// The palette after this one, wrapping to the first.
    pub fn next(self) -> Palette {
        let at = Palette::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Palette::ALL[(at + 1) % Palette::ALL.len()]
    }

    fn colors(self) -> &'static Colors {
        match self {
            Palette::PurpleOrange => &PURPLE_ORANGE,
            Palette::Phosphor => &PHOSPHOR,
            Palette::Catppuccin => &CATPPUCCIN,
        }
    }
}

impl FromStr for Palette {
    type Err = String;

    fn from_str(name: &str) -> Result<Palette, String> {
        Palette::ALL
            .into_iter()
            .find(|p| p.name() == name)
            .ok_or_else(|| {
                let names: Vec<_> = Palette::ALL.map(Palette::name).into();
                format!("unknown theme {name:?}: name one of {}", names.join(", "))
            })
    }
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    depth: Depth,
    palette: Palette,
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
        Theme {
            depth,
            palette: Palette::default(),
        }
    }

    pub fn with_palette(self, palette: Palette) -> Theme {
        Theme { palette, ..self }
    }

    pub fn palette(&self) -> Palette {
        self.palette
    }

    fn colors(&self) -> &'static Colors {
        self.palette.colors()
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
        self.fg(self.colors().primary)
    }

    pub fn title(&self) -> Style {
        self.fg(self.colors().accent).add_modifier(Modifier::BOLD)
    }

    /// `text` in bold, its letters running from the palette's primary color
    /// like the splash's wordmark.
    pub fn wordmark(&self, text: &str) -> Line<'static> {
        let colors = self.colors();
        let (from, to, steps) = (colors.primary.0, colors.wordmark_end, colors.wordmark_steps);
        let blend = |a: u8, b: u8, letter: u32, last: u32| {
            (i64::from(a) + (i64::from(b) - i64::from(a)) * i64::from(letter) / i64::from(last))
                as u8
        };
        let last = text.trim().chars().count().saturating_sub(1).max(1) as u32;
        let mut letter = 0;
        let spans = text.chars().map(|c| {
            if c == ' ' {
                return Span::raw(" ");
            }
            let fg = match self.depth {
                Depth::Truecolor => Some(Color::Rgb(
                    blend(from.0, to.0, letter, last),
                    blend(from.1, to.1, letter, last),
                    blend(from.2, to.2, letter, last),
                )),
                Depth::Indexed => Some(Color::Indexed(steps[(4 * letter / last) as usize])),
                Depth::None => None,
            };
            letter += 1;
            let style = Style::new().add_modifier(Modifier::BOLD);
            Span::styled(c.to_string(), fg.map_or(style, |fg| style.fg(fg)))
        });
        Line::from(spans.collect::<Vec<_>>())
    }

    pub fn heading(&self) -> Style {
        self.fg(self.colors().muted).add_modifier(Modifier::BOLD)
    }

    /// Without color the selection is shown reversed.
    pub fn selected(&self) -> Style {
        match (
            self.color(self.colors().accent),
            self.color(self.colors().on_accent),
        ) {
            (Some(bg), Some(fg)) => Style::new().bg(bg).fg(fg),
            _ => Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// The splash's nearest sheep: bold and bright.
    pub fn sheep(&self) -> Style {
        self.fg(self.colors().bright).add_modifier(Modifier::BOLD)
    }

    pub fn key(&self) -> Style {
        self.fg(self.colors().accent_light)
    }

    pub fn paused(&self) -> Style {
        self.fg(self.colors().accent_light)
    }

    /// A time in status past its first threshold.
    /// Without color it is underlined, to stay apart from a fresh time.
    pub fn stale(&self) -> Style {
        match self.depth {
            Depth::None => Style::new().add_modifier(Modifier::UNDERLINED),
            _ => self.fg(self.colors().accent_light),
        }
    }

    /// A time in status past its last threshold.
    pub fn overdue(&self) -> Style {
        self.plain_fg(Color::Red).add_modifier(Modifier::BOLD)
    }

    pub fn label(&self) -> Style {
        self.fg(self.colors().muted)
    }

    pub fn toggle(&self, on: bool) -> Style {
        match on {
            true => self.fg(self.colors().accent),
            false => Style::new().add_modifier(Modifier::DIM),
        }
    }

    /// A report's kind: questions in the accent, done green, handoffs in the
    /// primary, notes muted.
    pub fn report(&self, kind: Kind) -> Style {
        let style = match kind {
            Kind::Question => self.fg(self.colors().accent),
            Kind::Done => self.plain_fg(Color::Green),
            Kind::Handoff => self.fg(self.colors().primary),
            Kind::Note => self.fg(self.colors().muted),
        };
        style.add_modifier(Modifier::BOLD)
    }

    /// `context` of `limit` as six cells and a percent: muted below
    /// 60%, styled as stale from 60% and as overdue from 85%.
    pub fn gauge(&self, context: u64, limit: u64) -> Span<'static> {
        let (context, limit) = (u128::from(context), u128::from(limit.max(1)));
        let filled = ((context * 12 + limit) / (limit * 2)).min(6) as usize;
        let percent = context * 100 / limit;
        let style = match percent {
            85.. => self.overdue(),
            60.. => self.stale(),
            _ => self.fg(self.colors().muted),
        };
        let cells = "▰".repeat(filled) + &"▱".repeat(6 - filled);
        Span::styled(format!("{cells} {percent}%"), style)
    }

    /// The glyph leading a member's row; `frame` picks the spinner's frame.
    pub fn glyph(&self, member: &Member, frame: usize) -> (&'static str, Style) {
        if member.retired.is_some() {
            return ("·", Style::new().add_modifier(Modifier::DIM));
        }
        if member.gone {
            return ("✕", self.plain_fg(Color::Red));
        }
        match member.status {
            AgentStatus::Working => (
                SPINNER[frame % SPINNER.len()],
                self.fg(self.colors().accent),
            ),
            AgentStatus::Idle => ("○", self.fg(self.colors().soft)),
            AgentStatus::Done => ("✓", self.fg(self.colors().soft)),
            AgentStatus::Blocked => ("⚠", self.plain_fg(Color::Yellow)),
            AgentStatus::Unknown => ("?", Style::new().add_modifier(Modifier::DIM)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fg(style: Style) -> Option<Color> {
        style.fg
    }

    /// border, title, key, heading, idle glyph, selected row, sheep, then the
    /// wordmark's first and last letters.
    fn roles(theme: Theme) -> [Option<Color>; 10] {
        let idle = Member {
            name: "a".into(),
            status: AgentStatus::Idle,
            gone: false,
            branch: None,
            worktree: None,
            worktree_path: None,
            pane: "w1:p1".into(),
            tab: None,
            lead: false,
            template: None,
            released: false,
            retired: None,
        };
        let selected = theme.selected();
        let wordmark = theme.wordmark("ab");
        [
            fg(theme.border()),
            fg(theme.title()),
            fg(theme.key()),
            fg(theme.heading()),
            fg(theme.glyph(&idle, 0).1),
            selected.bg,
            selected.fg,
            fg(theme.sheep()),
            wordmark.spans[0].style.fg,
            wordmark.spans[1].style.fg,
        ]
    }

    fn rgb(colors: [(u8, u8, u8); 10]) -> [Option<Color>; 10] {
        colors.map(|(r, g, b)| Some(Color::Rgb(r, g, b)))
    }

    fn indexed(colors: [u8; 10]) -> [Option<Color>; 10] {
        colors.map(|i| Some(Color::Indexed(i)))
    }

    #[test]
    fn each_palette_paints_its_roles_in_truecolor() {
        let truecolor = Theme::from_env(Some("truecolor"), false);
        let cases = [
            (
                Palette::PurpleOrange,
                rgb([
                    (150, 50, 230),
                    (255, 154, 31),
                    (255, 192, 77),
                    (120, 110, 140),
                    (160, 165, 190),
                    (255, 154, 31),
                    (20, 16, 28),
                    (255, 255, 255),
                    (150, 50, 230),
                    (255, 80, 0),
                ]),
            ),
            (
                Palette::Phosphor,
                rgb([
                    (0x05, 0xdf, 0x72),
                    (0xff, 0xd2, 0x30),
                    (0x7b, 0xf1, 0xa8),
                    (0xa6, 0xa0, 0x9b),
                    (0xd6, 0xd3, 0xd1),
                    (0xff, 0xd2, 0x30),
                    (0x0c, 0x0f, 0x0b),
                    (255, 255, 255),
                    (0x05, 0xdf, 0x72),
                    (0xff, 0xd2, 0x30),
                ]),
            ),
            (
                Palette::Catppuccin,
                rgb([
                    (0xcb, 0xa6, 0xf7),
                    (0xfa, 0xb3, 0x87),
                    (0xf9, 0xe2, 0xaf),
                    (0x6c, 0x70, 0x86),
                    (0xa6, 0xad, 0xc8),
                    (0xfa, 0xb3, 0x87),
                    (0x1e, 0x1e, 0x2e),
                    (0xcd, 0xd6, 0xf4),
                    (0xcb, 0xa6, 0xf7),
                    (0xfa, 0xb3, 0x87),
                ]),
            ),
        ];
        for (palette, expected) in cases {
            assert_eq!(
                roles(truecolor.with_palette(palette)),
                expected,
                "{palette:?}"
            );
        }
    }

    #[test]
    fn each_palette_falls_back_to_256_colors() {
        let indexed_theme = Theme::from_env(None, false);
        for (palette, expected) in [
            (
                Palette::PurpleOrange,
                indexed([93, 208, 214, 60, 146, 208, 232, 231, 93, 202]),
            ),
            (
                Palette::Phosphor,
                indexed([41, 221, 121, 247, 252, 221, 233, 231, 41, 221]),
            ),
            (
                Palette::Catppuccin,
                indexed([183, 216, 223, 243, 146, 216, 234, 189, 183, 216]),
            ),
        ] {
            assert_eq!(
                roles(indexed_theme.with_palette(palette)),
                expected,
                "{palette:?}"
            );
        }
    }

    /// The xterm RGB of a 256-color index from 16 up.
    fn xterm_rgb(index: u8) -> (u8, u8, u8) {
        match index {
            232.. => {
                let v = 8 + 10 * (index - 232);
                (v, v, v)
            }
            16.. => {
                let level = |n: u8| [0, 95, 135, 175, 215, 255][usize::from(n)];
                let i = index - 16;
                (level(i / 36), level(i / 6 % 6), level(i % 6))
            }
            _ => panic!("ANSI color {index} has no fixed RGB"),
        }
    }

    /// WCAG 2 contrast ratio.
    fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        let luminance = |(r, g, b): (u8, u8, u8)| {
            let channel = |c: u8| {
                let c = f64::from(c) / 255.0;
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        };
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    fn rgb_of(color: Option<Color>) -> (u8, u8, u8) {
        match color {
            Some(Color::Rgb(r, g, b)) => (r, g, b),
            Some(Color::Indexed(i)) => xterm_rgb(i),
            other => panic!("not a fixed color: {other:?}"),
        }
    }

    #[test]
    fn the_selected_row_reaches_4_5_to_1_in_every_palette_and_depth() {
        for colorterm in [Some("truecolor"), None] {
            for palette in Palette::ALL {
                let selected = Theme::from_env(colorterm, false)
                    .with_palette(palette)
                    .selected();
                let ratio = contrast(rgb_of(selected.fg), rgb_of(selected.bg));
                assert!(ratio >= 4.5, "{palette:?} {colorterm:?}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn without_color_every_palette_drops_color_and_reverses_the_selection() {
        let plain = Theme::from_env(Some("truecolor"), true);
        for palette in Palette::ALL {
            let theme = plain.with_palette(palette);
            let roles = roles(theme);
            assert_eq!(roles[..5], [None; 5], "{palette:?}");
            assert_eq!(roles[7..], [None; 3], "{palette:?}");
            assert!(theme.selected().add_modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn the_default_palette_is_purple_orange_and_next_cycles_through_all_three() {
        let theme = Theme::from_env(Some("truecolor"), false);
        assert_eq!(theme.palette(), Palette::PurpleOrange);
        let names: Vec<&str> = std::iter::successors(Some(theme.palette()), |p| Some(p.next()))
            .take(4)
            .map(Palette::name)
            .collect();
        assert_eq!(
            names,
            ["purple-orange", "phosphor", "catppuccin", "purple-orange"]
        );
    }

    #[test]
    fn a_palette_parses_from_its_name_and_refuses_anything_else() {
        for palette in Palette::ALL {
            assert_eq!(palette.name().parse::<Palette>().unwrap(), palette);
        }
        let err = "solarized".parse::<Palette>().unwrap_err();
        assert!(err.contains("purple-orange, phosphor, catppuccin"), "{err}");
    }

    #[test]
    fn without_color_a_gauge_from_60_is_underlined_and_from_85_bold() {
        let theme = Theme::from_env(None, true);
        assert_eq!(theme.gauge(89_999, 150_000).style, Style::new());
        let middle = theme.gauge(90_000, 150_000).style;
        assert!(middle.add_modifier.contains(Modifier::UNDERLINED));
        let high = theme.gauge(127_500, 150_000).style;
        assert!(high.add_modifier.contains(Modifier::BOLD));
    }

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
