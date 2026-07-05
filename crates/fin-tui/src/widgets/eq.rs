//! 10-band EQ visualization — one vertical slider per band with a dB label
//! above the bar, a Hz/kHz label below, and a highlight on the selected
//! band. Used inside the Settings screen.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use fin_config::EqBand;

use crate::theme::{muted_style, Palette};

/// One EQ slider column drawn with vertical block characters.
pub struct EqSliders<'a> {
    pub bands: &'a [EqBand],
    pub enabled: bool,
    pub selected: Option<usize>,
    /// Gain range in dB the vertical slider maps onto (± this many dB).
    pub range_db: i32,
}

impl<'a> Widget for EqSliders<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.bands.is_empty() || area.width < 8 || area.height < 5 {
            let msg = "No EQ bands — add [[eq_band_settings]] to config.toml";
            Paragraph::new(Span::styled(msg, muted_style())).render(area, buf);
            return;
        }

        let n = self.bands.len().min(10);
        let col_w = (area.width as usize / n).max(4) as u16;
        let center_row = area.y + area.height / 2;

        // Reserve top row for dB text, bottom row for Hz text; the middle
        // rows draw the bar.
        let bar_top = area.y + 1;
        let bar_bot = area.y + area.height.saturating_sub(3);
        let bar_h = bar_bot.saturating_sub(bar_top) as i32;
        if bar_h < 1 {
            return;
        }

        for (i, band) in self.bands.iter().take(n).enumerate() {
            let col_x = area.x + (i as u16) * col_w;
            if col_x >= area.right() {
                break;
            }

            // Layout inside this column: 4 char wide "gutter" for the bar,
            // centered in the column.
            let bar_x = col_x + col_w / 2;

            // Compute the vertical position for the band's gain.
            let gain_db = band.gain as f32 / 10.0;
            let clamped = gain_db.clamp(-(self.range_db as f32), self.range_db as f32);
            let ratio = clamped / self.range_db as f32; // −1..=+1
            let half_h = bar_h / 2;
            let offset = ((ratio * half_h as f32).round()) as i32;
            let bar_end_row = (center_row as i32 - offset).clamp(bar_top as i32, bar_bot as i32) as u16;

            // Bar style — muted when EQ off, highlighted on the selected band.
            let is_sel = self.selected == Some(i);
            let bar_color = if !self.enabled {
                Palette::MUTED
            } else if is_sel {
                Palette::HIGHLIGHT
            } else {
                Palette::PRIMARY
            };
            let center_line_color = Palette::SURFACE;

            // Zero-dB axis across every column so the user can eyeball who's
            // pushed above / below flat.
            for r in bar_top..=bar_bot {
                let ch = if r == center_row { '─' } else { '│' };
                let col = if r == center_row {
                    center_line_color
                } else {
                    Palette::SURFACE
                };
                if let Some(cell) = buf.cell_mut((bar_x, r)) {
                    cell.set_char(ch);
                    cell.set_style(Style::default().fg(col));
                }
            }

            // Draw the fill from center toward `bar_end_row`.
            let (from, to) = if bar_end_row < center_row {
                (bar_end_row, center_row) // gain > 0: fill upward
            } else {
                (center_row, bar_end_row) // gain < 0: fill downward
            };
            for r in from..=to {
                if let Some(cell) = buf.cell_mut((bar_x, r)) {
                    cell.set_char('█');
                    cell.set_style(
                        Style::default()
                            .fg(bar_color)
                            .add_modifier(Modifier::BOLD),
                    );
                }
            }

            // dB label above the bar (row area.y).
            let db_style = if is_sel && self.enabled {
                Style::default()
                    .fg(Palette::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD)
            } else if self.enabled {
                Style::default().fg(Palette::FG)
            } else {
                muted_style()
            };
            let db_text = format!("{:+.1}", gain_db);
            let db_area = Rect::new(col_x, area.y, col_w, 1);
            Paragraph::new(Span::styled(db_text, db_style))
                .alignment(ratatui::layout::Alignment::Center)
                .render(db_area, buf);

            // "dB" units row (small hint under the number).
            let unit_row = area.y + area.height.saturating_sub(2);
            let hz_row = area.y + area.height.saturating_sub(1);

            let hz_text = fmt_hz(band.cutoff);
            let hz_style = if is_sel && self.enabled {
                Style::default()
                    .fg(Palette::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD)
            } else {
                muted_style()
            };
            let unit_area = Rect::new(col_x, unit_row, col_w, 1);
            Paragraph::new(Span::styled("dB", muted_style()))
                .alignment(ratatui::layout::Alignment::Center)
                .render(unit_area, buf);
            let hz_area = Rect::new(col_x, hz_row, col_w, 1);
            Paragraph::new(Span::styled(hz_text, hz_style))
                .alignment(ratatui::layout::Alignment::Center)
                .render(hz_area, buf);
        }

        // Top-right corner: axis label so the range is unambiguous.
        let axis_lbl = format!("±{} dB", self.range_db);
        let lbl_area = Rect::new(
            area.right().saturating_sub(axis_lbl.len() as u16),
            area.y,
            axis_lbl.len() as u16,
            1,
        );
        Paragraph::new(Line::from(Span::styled(axis_lbl, muted_style()))).render(lbl_area, buf);
    }
}

/// Format Hz as `60 Hz`, `1.2 kHz`, or `20 kHz`. Compact enough to fit under
/// a narrow slider column.
fn fmt_hz(hz: i32) -> String {
    if hz < 1000 {
        format!("{} Hz", hz)
    } else if hz % 1000 == 0 {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{:.1} kHz", hz as f32 / 1000.0)
    }
}
