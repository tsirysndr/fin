use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use fin_jellyfin::BaseItem;

use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Music,
    Videos,
    Playlists,
    Favorites,
    Queue,
    Search,
    Devices,
    Settings,
}

impl Screen {
    pub const ALL: &'static [Screen] = &[
        Screen::Music,
        Screen::Videos,
        Screen::Playlists,
        Screen::Favorites,
        Screen::Queue,
        Screen::Search,
        Screen::Devices,
        Screen::Settings,
    ];

    pub fn icon(&self) -> &'static str {
        match self {
            Screen::Music => "♪",
            Screen::Videos => "▶",
            Screen::Playlists => "▤",
            Screen::Favorites => "♥",
            Screen::Queue => "≡",
            Screen::Search => "⌕",
            Screen::Devices => "◈",
            Screen::Settings => "⚙",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Screen::Music => "Music",
            Screen::Videos => "Videos",
            Screen::Playlists => "Playlists",
            Screen::Favorites => "Favorites",
            Screen::Queue => "Queue",
            Screen::Search => "Search",
            Screen::Devices => "Devices",
            Screen::Settings => "Settings",
        }
    }

    pub fn next(&self) -> Self {
        let i = Self::ALL.iter().position(|s| s == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(&self) -> Self {
        let i = Self::ALL.iter().position(|s| s == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Format a duration as `M:SS` (or `H:MM:SS` when >= 1h).
fn fmt_dur(secs: u64) -> String {
    let (h, rem) = (secs / 3600, secs % 3600);
    let (m, s) = (rem / 60, rem % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

/// Column layout used by every list row. Widths are computed once per draw
/// from the available inner width; every row uses the same widths so titles,
/// subtitles, and durations line up as columns.
#[derive(Debug, Clone, Copy)]
pub struct RowLayout {
    pub icon_col: usize,  // fixed
    pub title_col: usize, // fills ~55% of the middle
    pub gap1: usize,      // between title and subtitle
    pub sub_col: usize,   // fills the rest of the middle
    pub gap2: usize,      // between subtitle and time
    pub time_col: usize,  // right-aligned, fixed max width
}

impl RowLayout {
    pub const ICON: usize = 3;
    pub const GAP: usize = 2;
    pub const TIME_MAX: usize = 8; // "HH:MM:SS"

    /// Compute per-column widths for a list rendered in `total` columns.
    pub fn compute(total: u16) -> Self {
        let total = total as usize;
        let icon = Self::ICON;
        let gap1 = Self::GAP;
        let gap2 = Self::GAP;
        let time = Self::TIME_MAX;
        let fixed = icon + gap1 + gap2 + time;
        let mid = total.saturating_sub(fixed);
        // Title gets ~55%, subtitle ~45%, but the subtitle column has a
        // reasonable floor so it doesn't disappear on narrow terminals.
        let mut title = (mid * 55) / 100;
        let mut sub = mid.saturating_sub(title);
        if mid < 30 {
            // On very narrow terminals, hide the subtitle column entirely.
            title = mid;
            sub = 0;
        }
        Self {
            icon_col: icon,
            title_col: title,
            gap1,
            sub_col: sub,
            gap2,
            time_col: time,
        }
    }
}

/// Build a row for a list-of-items view using the shared `RowLayout` so
/// every row's columns land at the same character positions.
///
/// `now_playing` swaps the icon for a ▶ marker and paints the row in the
/// highlight color — distinct from `selected`, which reflects the cursor.
/// The Queue screen uses both: cursor for the row the user is inspecting,
/// now-playing for the track actually coming out of the speakers.
pub fn item_row_line<'a>(
    item: &'a BaseItem,
    selected: bool,
    now_playing: bool,
    layout: RowLayout,
) -> Line<'a> {
    let icon = if now_playing {
        "▶"
    } else {
        item.kind().icon()
    };
    let (icon_fg, main_style) = if now_playing {
        (
            Palette::HIGHLIGHT,
            Style::default()
                .fg(Palette::HIGHLIGHT)
                .add_modifier(Modifier::BOLD),
        )
    } else if selected {
        (
            Palette::PRIMARY,
            Style::default()
                .fg(Palette::FG)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (Palette::ACCENT, Style::default().fg(Palette::FG))
    };
    let sub = item.subtitle();
    let time = item.duration_secs().map(fmt_dur).unwrap_or_default();

    let icon_text = pad_to(&format!(" {} ", icon), layout.icon_col);
    let title_text = pad_to(&truncate(&item.name, layout.title_col), layout.title_col);
    let sub_text = if layout.sub_col > 0 {
        pad_to(&truncate(&sub, layout.sub_col), layout.sub_col)
    } else {
        String::new()
    };
    // Right-align time within its column.
    let time_pad = layout.time_col.saturating_sub(time.width());
    let time_text = format!("{}{}", " ".repeat(time_pad), time);

    let gap1 = " ".repeat(layout.gap1);
    let gap2 = " ".repeat(layout.gap2);

    Line::from(vec![
        Span::styled(
            icon_text,
            Style::default().fg(icon_fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(title_text, main_style),
        Span::raw(gap1),
        Span::styled(sub_text, Style::default().fg(Palette::MUTED)),
        Span::raw(gap2),
        Span::styled(time_text, Style::default().fg(Palette::SKY)),
    ])
}

fn truncate(s: &str, max_cols: usize) -> String {
    if s.width() <= max_cols {
        return s.to_string();
    }
    if max_cols <= 1 {
        return "…".into();
    }
    let target = max_cols - 1;
    let mut acc = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.to_string().width();
        if w + cw > target {
            break;
        }
        acc.push(ch);
        w += cw;
    }
    acc.push('…');
    acc
}

/// Right-pad `s` with spaces so it takes exactly `cols` columns.
fn pad_to(s: &str, cols: usize) -> String {
    let w = s.width();
    if w >= cols {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(cols - w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Screen tab navigation
    // ------------------------------------------------------------------

    #[test]
    fn screen_all_matches_declared_tab_order() {
        // The header row in the TUI walks Screen::ALL in this order; if it
        // drifts, the `1`…`8` shortcuts silently jump to the wrong tab.
        assert_eq!(
            Screen::ALL,
            &[
                Screen::Music,
                Screen::Videos,
                Screen::Playlists,
                Screen::Favorites,
                Screen::Queue,
                Screen::Search,
                Screen::Devices,
                Screen::Settings,
            ]
        );
    }

    #[test]
    fn screen_next_wraps_around_at_end() {
        assert_eq!(Screen::Music.next(), Screen::Videos);
        assert_eq!(Screen::Videos.next(), Screen::Playlists);
        assert_eq!(Screen::Playlists.next(), Screen::Favorites);
        assert_eq!(Screen::Favorites.next(), Screen::Queue);
        // Last tab wraps back to the first.
        assert_eq!(Screen::Settings.next(), Screen::Music);
    }

    #[test]
    fn screen_prev_wraps_around_at_start() {
        assert_eq!(Screen::Music.prev(), Screen::Settings);
        assert_eq!(Screen::Videos.prev(), Screen::Music);
        assert_eq!(Screen::Settings.prev(), Screen::Devices);
    }

    #[test]
    fn screen_next_prev_are_inverses() {
        for &s in Screen::ALL {
            assert_eq!(s.next().prev(), s);
            assert_eq!(s.prev().next(), s);
        }
    }

    #[test]
    fn screen_icon_and_label_lookup_by_variant() {
        // Guards against a rename that would blank out the header UI.
        assert_eq!(Screen::Music.icon(), "♪");
        assert_eq!(Screen::Music.label(), "Music");
        assert_eq!(Screen::Queue.icon(), "≡");
        assert_eq!(Screen::Settings.label(), "Settings");
    }

    // ------------------------------------------------------------------
    // Duration formatting
    // ------------------------------------------------------------------

    #[test]
    fn fmt_dur_zero() {
        assert_eq!(fmt_dur(0), "0:00");
    }

    #[test]
    fn fmt_dur_under_an_hour() {
        assert_eq!(fmt_dur(59), "0:59");
        assert_eq!(fmt_dur(60), "1:00");
        assert_eq!(fmt_dur(3599), "59:59");
    }

    #[test]
    fn fmt_dur_hour_and_over() {
        assert_eq!(fmt_dur(3600), "1:00:00");
        assert_eq!(fmt_dur(3661), "1:01:01");
        assert_eq!(fmt_dur(36_000), "10:00:00");
    }

    // ------------------------------------------------------------------
    // RowLayout column widths
    // ------------------------------------------------------------------

    #[test]
    fn row_layout_sums_to_total_width() {
        for total in [40u16, 80, 120, 200] {
            let l = RowLayout::compute(total);
            let sum = l.icon_col + l.title_col + l.gap1 + l.sub_col + l.gap2 + l.time_col;
            assert_eq!(sum, total as usize);
        }
    }

    #[test]
    fn row_layout_hides_subtitle_column_on_narrow_terminals() {
        // 30 chars is our documented cutoff: sub_col goes to 0 so the title
        // column takes the full middle.
        let l = RowLayout::compute(40);
        assert_eq!(l.sub_col, 0);
        // But nothing else disappears — icon, gap1/2, time still present.
        assert!(l.title_col > 0);
        assert_eq!(l.icon_col, RowLayout::ICON);
        assert_eq!(l.time_col, RowLayout::TIME_MAX);
    }

    #[test]
    fn row_layout_gives_title_roughly_55_percent_of_middle() {
        // Loose bound — implementation uses integer math, so ±1 is fine.
        let l = RowLayout::compute(120);
        let mid = l.title_col + l.sub_col;
        assert!(mid > 0);
        let ratio = l.title_col as f32 / mid as f32;
        assert!(
            (0.5..=0.6).contains(&ratio),
            "title ratio {} not in [0.50, 0.60]",
            ratio
        );
    }

    // ------------------------------------------------------------------
    // Truncation helper
    // ------------------------------------------------------------------

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn truncate_adds_ellipsis_and_stays_within_budget() {
        // "abcdefghij" → 10 cols. Truncated to 5 must land at 5 cols total,
        // ending with the ellipsis character.
        let out = truncate("abcdefghij", 5);
        assert!(out.ends_with('…'));
        assert!(out.width() <= 5);
    }

    #[test]
    fn truncate_single_column_gives_bare_ellipsis() {
        assert_eq!(truncate("longstring", 1), "…");
    }
}
