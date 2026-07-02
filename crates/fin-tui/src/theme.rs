use ratatui::style::{Color, Modifier, Style};

/// Neon-electric palette — matches the pocketenv CLI theme.
pub struct Palette;

impl Palette {
    /// Primary accent — electric teal.
    pub const PRIMARY: Color = Color::Rgb(0, 232, 198);
    /// Secondary accent — bright cyan.
    pub const SECONDARY: Color = Color::Rgb(0, 198, 232);
    /// Deep violet neon accent used for selection.
    pub const ACCENT: Color = Color::Rgb(130, 100, 255);
    /// Vivid mint highlight.
    pub const HIGHLIGHT: Color = Color::Rgb(100, 232, 130);
    /// Muted foreground for secondary text.
    pub const MUTED: Color = Color::Rgb(160, 175, 195);
    /// Playful orange used for links / stream indicators.
    pub const LINK: Color = Color::Rgb(255, 160, 100);
    /// Sky blue.
    pub const SKY: Color = Color::Rgb(0, 210, 255);
    /// Neon pink used for warnings.
    pub const WARN: Color = Color::Rgb(255, 110, 220);
    /// Vivid red for errors.
    pub const ERROR: Color = Color::Rgb(255, 100, 100);
    /// Off-black background.
    pub const BG: Color = Color::Rgb(6, 8, 18);
    /// Slightly lighter surface for cards.
    pub const SURFACE: Color = Color::Rgb(12, 16, 30);
    /// Foreground text.
    pub const FG: Color = Color::Rgb(224, 235, 255);
}

pub fn base_style() -> Style {
    Style::default().fg(Palette::FG).bg(Palette::BG)
}

pub fn title_style() -> Style {
    Style::default()
        .fg(Palette::PRIMARY)
        .add_modifier(Modifier::BOLD)
}

pub fn muted_style() -> Style {
    Style::default().fg(Palette::MUTED)
}

pub fn accent_style() -> Style {
    Style::default()
        .fg(Palette::ACCENT)
        .add_modifier(Modifier::BOLD)
}

pub fn highlight_style() -> Style {
    Style::default()
        .fg(Palette::BG)
        .bg(Palette::PRIMARY)
        .add_modifier(Modifier::BOLD)
}

pub fn selection_style() -> Style {
    Style::default()
        .fg(Palette::ACCENT)
        .add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

pub fn tab_active() -> Style {
    Style::default()
        .fg(Palette::PRIMARY)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

pub fn tab_inactive() -> Style {
    Style::default().fg(Palette::MUTED)
}

pub fn border_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Palette::PRIMARY)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Palette::MUTED)
    }
}

pub fn status_playing() -> Style {
    Style::default()
        .fg(Palette::HIGHLIGHT)
        .add_modifier(Modifier::BOLD)
}

pub fn status_paused() -> Style {
    Style::default().fg(Palette::WARN)
}
