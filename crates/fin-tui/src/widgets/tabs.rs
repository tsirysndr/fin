use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::theme::Palette;

pub struct NeonTabs<'a> {
    labels: &'a [(&'a str, &'a str)], // (icon, label)
    selected: usize,
}

impl<'a> NeonTabs<'a> {
    pub fn new(labels: &'a [(&'a str, &'a str)], selected: usize) -> Self {
        Self { labels, selected }
    }
}

impl<'a> Widget for NeonTabs<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut spans: Vec<Span<'_>> = Vec::new();
        for (i, (icon, label)) in self.labels.iter().enumerate() {
            let is_sel = i == self.selected;
            let bracket = if is_sel { "▍" } else { " " };
            let bracket_col = if is_sel {
                Palette::PRIMARY
            } else {
                Palette::BG
            };
            spans.push(Span::styled(bracket, Style::default().fg(bracket_col)));
            let (icon_col, text_style) = if is_sel {
                (
                    Palette::PRIMARY,
                    Style::default()
                        .fg(Palette::FG)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (Palette::MUTED, Style::default().fg(Palette::MUTED))
            };
            spans.push(Span::styled(
                format!(" {} ", icon),
                Style::default().fg(icon_col).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!("{}  ", label), text_style));
            if i + 1 < self.labels.len() {
                spans.push(Span::styled(
                    "│  ",
                    Style::default().fg(Color::Rgb(30, 40, 60)),
                ));
            }
        }
        Line::from(spans).render(area, buf);
    }
}
