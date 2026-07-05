pub mod eq;
pub mod player_bar;
pub mod tabs;

pub use eq::EqSliders;
pub use player_bar::PlayerBar;
pub use tabs::NeonTabs;

use ratatui::style::Style;
use ratatui::symbols::border;
use ratatui::widgets::{Block, Borders};

use crate::theme::{border_style, title_style, Palette};

/// Consistent rounded neon block used across screens.
pub fn neon_block(title: &str, active: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style(active))
        .title_style(title_style())
        .style(Style::default().bg(Palette::BG))
        .title(title.to_string())
}
