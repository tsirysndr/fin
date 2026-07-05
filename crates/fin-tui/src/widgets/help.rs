//! Keyboard-shortcuts help modal. `?` toggles it; Esc closes when open.
//!
//! Rendered as a centered `Clear`-backed popup over the current screen.
//! Content is grouped into sections (Navigation / Playback / …) and each
//! row is a two-column `key -> description` layout so the reader can scan
//! either side at a glance.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget, Wrap};

use crate::theme::{border_style, muted_style, title_style, Palette};
use crate::widgets::neon_block;

/// One row in the help list — a key (or key combo) and what it does.
pub struct HelpEntry {
    pub key: &'static str,
    pub description: &'static str,
}

/// A section header + its rows.
pub struct HelpSection {
    pub title: &'static str,
    pub entries: &'static [HelpEntry],
}

/// Every shortcut fin knows about, grouped for readability. Static data so
/// tests can lock the layout and the compiler catches typos.
pub const HELP_SECTIONS: &[HelpSection] = &[
    HelpSection {
        title: "Navigation",
        entries: &[
            HelpEntry { key: "Tab / Shift+Tab", description: "next / prev screen" },
            HelpEntry { key: "1 … 8", description: "jump to Music / Videos / Playlists / Favorites / Queue / Search / Devices / Settings" },
            HelpEntry { key: "↑ ↓ / k j", description: "move selection" },
            HelpEntry { key: "PgUp / PgDown", description: "jump 10 rows" },
            HelpEntry { key: "Enter", description: "drill in / play leaf / connect (Devices) / switch server (Settings)" },
            HelpEntry { key: "Esc", description: "pop drill-in, close search or this help" },
            HelpEntry { key: "/", description: "focus Search input" },
            HelpEntry { key: "r", description: "refresh current screen" },
            HelpEntry { key: "t", description: "cycle to next saved server (Jellyfin or Subsonic)" },
            HelpEntry { key: "m", description: "switch to local (symphonia + mpv) renderer" },
            HelpEntry { key: "q / Ctrl+C", description: "quit" },
        ],
    },
    HelpSection {
        title: "Playback",
        entries: &[
            HelpEntry { key: "x", description: "play the highlighted container without drilling in" },
            HelpEntry { key: "a", description: "enqueue the highlighted item" },
            HelpEntry { key: "n", description: "play the highlighted item next" },
            HelpEntry { key: "Space / p", description: "pause / resume" },
            HelpEntry { key: "s", description: "stop" },
            HelpEntry { key: "< / > (or h / l)", description: "previous / next track" },
            HelpEntry { key: "+ / −", description: "volume up / down" },
            HelpEntry { key: "Shift+L", description: "like — favorite/star the highlighted item (or the playing track)" },
            HelpEntry { key: "Shift+D", description: "dislike — remove it from favorites" },
        ],
    },
    HelpSection {
        title: "Queue screen",
        entries: &[
            HelpEntry { key: "Enter", description: "jump playhead to the highlighted entry (preserves the queue)" },
            HelpEntry { key: "d", description: "remove the highlighted entry" },
            HelpEntry { key: "Shift+C", description: "clear the whole queue" },
        ],
    },
    HelpSection {
        title: "Modes & effects",
        entries: &[
            HelpEntry { key: "z", description: "toggle shuffle" },
            HelpEntry { key: "Shift+R", description: "cycle repeat mode: off → all → one" },
            HelpEntry { key: "g", description: "cycle ReplayGain: off → track → album" },
            HelpEntry { key: "f", description: "cycle crossfade mode: off → crossfade → mixed" },
            HelpEntry { key: "Shift+F", description: "cycle crossfade duration (3, 5, 8, 12 s)" },
        ],
    },
    HelpSection {
        title: "Equalizer & tone (Settings screen)",
        entries: &[
            HelpEntry { key: "Shift+E", description: "toggle the 10-band Rockbox EQ" },
            HelpEntry { key: "[ / ]", description: "select previous / next EQ band" },
            HelpEntry { key: "Shift+↑ / Shift+↓", description: "nudge selected band's gain by ±1 dB" },
            HelpEntry { key: "b / Shift+B", description: "bass shelf −1 dB / +1 dB" },
            HelpEntry { key: "y / Shift+Y", description: "treble shelf −1 dB / +1 dB" },
        ],
    },
    HelpSection {
        title: "Help",
        entries: &[
            HelpEntry { key: "?", description: "show / hide this help" },
        ],
    },
];

/// The modal itself — clears the underlying area, draws a bordered block,
/// then renders every section stacked inside with a consistent
/// `key  →  description` column layout.
pub struct HelpModal;

impl HelpModal {
    /// Compute the ideal centered rect. The popup consumes as much
    /// vertical space as the terminal offers (up to a comfortable ceiling)
    /// so nothing gets clipped — every current section plus its footer
    /// fits inside ~42 rows.
    pub fn area_for(screen: Rect) -> Rect {
        let w = screen.width.saturating_sub(8).clamp(60, 120);
        // Leave a 2-row margin above + below so the popup doesn't slam
        // into the terminal edges but still shows every section.
        let h = screen.height.saturating_sub(4).clamp(20, 60);
        Rect::new(
            screen.x + (screen.width.saturating_sub(w)) / 2,
            screen.y + (screen.height.saturating_sub(h)) / 2,
            w,
            h,
        )
    }
}

impl Widget for HelpModal {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Blank the underlying content first — otherwise the screen
        // bleeds through the semi-transparent Terminal cells.
        Clear.render(area, buf);

        let block = neon_block(" ? Keyboard shortcuts ", true)
            .border_style(border_style(true))
            .title_style(title_style());
        let inner = block.inner(area);
        block.render(area, buf);

        // Split into as many horizontal rows as the widget occupies, one
        // Line per row. We rely on Paragraph's wrapping for width overflow.
        let lines = build_lines(inner.width as usize);
        let footer = Line::from(vec![Span::styled(
            "  ? or Esc to close",
            muted_style(),
        )]);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(Palette::BG))
            .render(rows[0], buf);
        Paragraph::new(footer)
            .alignment(Alignment::Left)
            .style(Style::default().bg(Palette::BG))
            .render(rows[1], buf);
    }
}

/// Build every visible line in the popup: section headers, entries,
/// blank spacers. `width` is the inner area width; used to right-pad the
/// key column so descriptions line up regardless of key length.
fn build_lines(width: usize) -> Vec<Line<'static>> {
    // Widest key across ALL sections drives the column width so every row
    // stays column-aligned even if one section has "Enter" and the next has
    // "Shift+↑ / Shift+↓".
    let key_col_max = HELP_SECTIONS
        .iter()
        .flat_map(|s| s.entries.iter())
        .map(|e| unicode_width::UnicodeWidthStr::width(e.key))
        .max()
        .unwrap_or(0);
    let key_col_max = key_col_max.min(width.saturating_sub(6)).max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();

    for (idx, section) in HELP_SECTIONS.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}", section.title),
                Style::default()
                    .fg(Palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for entry in section.entries {
            let key_pad = key_col_max
                .saturating_sub(unicode_width::UnicodeWidthStr::width(entry.key));
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    entry.key,
                    Style::default()
                        .fg(Palette::HIGHLIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ".repeat(key_pad)),
                Span::styled("  ", muted_style()),
                Span::styled(entry.description, Style::default().fg(Palette::FG)),
            ]));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_has_at_least_one_entry() {
        for s in HELP_SECTIONS {
            assert!(
                !s.entries.is_empty(),
                "section '{}' has no entries",
                s.title
            );
        }
    }

    #[test]
    fn every_entry_has_non_empty_key_and_description() {
        for s in HELP_SECTIONS {
            for e in s.entries {
                assert!(!e.key.trim().is_empty(), "empty key in section '{}'", s.title);
                assert!(
                    !e.description.trim().is_empty(),
                    "empty description for '{}'",
                    e.key
                );
            }
        }
    }

    #[test]
    fn help_advertises_its_own_toggle_key() {
        // Would be very confusing to open a help panel and NOT tell the
        // user how to close it. Guard against a future refactor dropping
        // the toggle documentation.
        let has_toggle = HELP_SECTIONS
            .iter()
            .flat_map(|s| s.entries.iter())
            .any(|e| e.key == "?");
        assert!(has_toggle);
    }

    #[test]
    fn build_lines_starts_with_a_section_header() {
        // First rendered row is a section title — not a blank spacer, so
        // the popup doesn't waste a row at the top.
        let lines = build_lines(80);
        // A `Line` with a single styled span; peek the raw content.
        let first = lines
            .first()
            .expect("build_lines produced no output")
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(first.trim_start().starts_with("Navigation"));
    }
}
