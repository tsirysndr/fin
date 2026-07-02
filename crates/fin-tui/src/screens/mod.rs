use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use fin_jellyfin::BaseItem;

use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Search,
    Music,
    Videos,
    Playlists,
    Queue,
    Devices,
    Settings,
}

impl Screen {
    pub const ALL: &'static [Screen] = &[
        Screen::Home,
        Screen::Search,
        Screen::Music,
        Screen::Videos,
        Screen::Playlists,
        Screen::Queue,
        Screen::Devices,
        Screen::Settings,
    ];

    pub fn icon(&self) -> &'static str {
        match self {
            Screen::Home => "◉",
            Screen::Search => "⌕",
            Screen::Music => "♪",
            Screen::Videos => "▶",
            Screen::Playlists => "▤",
            Screen::Queue => "≡",
            Screen::Devices => "◈",
            Screen::Settings => "⚙",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Screen::Home => "Home",
            Screen::Search => "Search",
            Screen::Music => "Music",
            Screen::Videos => "Videos",
            Screen::Playlists => "Playlists",
            Screen::Queue => "Queue",
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

pub fn item_row_line<'a>(item: &'a BaseItem, selected: bool) -> Line<'a> {
    let icon = item.kind().icon();
    let (icon_col, main_style) = if selected {
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
    let mut spans: Vec<Span<'a>> = vec![
        Span::styled(
            format!(" {} ", icon),
            Style::default().fg(icon_col).add_modifier(Modifier::BOLD),
        ),
        Span::styled(item.name.clone(), main_style),
    ];
    if !sub.is_empty() {
        spans.push(Span::styled(
            format!("   {}", sub),
            Style::default().fg(Palette::MUTED),
        ));
    }
    if let Some(d) = item.duration_secs() {
        let mm = d / 60;
        let ss = d % 60;
        spans.push(Span::styled(
            format!("   {}:{:02}", mm, ss),
            Style::default().fg(Palette::SKY),
        ));
    }
    Line::from(spans)
}
