use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Widget};

use fin_player::{PlaybackState, PlaybackStatus, RendererKind, RepeatMode};

use crate::theme::{border_style, muted_style, title_style, Palette};

pub struct PlayerBar<'a> {
    pub state: &'a PlaybackState,
    pub renderer: RendererKind,
    pub renderer_label: &'a str,
}

fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0:00".into();
    }
    let s = secs as u64;
    let (h, rem) = (s / 3600, s % 3600);
    let (m, sec) = (rem / 60, rem % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, sec)
    } else {
        format!("{}:{:02}", m, sec)
    }
}

fn status_icon(status: PlaybackStatus) -> (&'static str, Style) {
    match status {
        PlaybackStatus::Playing => (
            "▶",
            Style::default()
                .fg(Palette::HIGHLIGHT)
                .add_modifier(Modifier::BOLD),
        ),
        PlaybackStatus::Paused => (
            "⏸",
            Style::default()
                .fg(Palette::WARN)
                .add_modifier(Modifier::BOLD),
        ),
        PlaybackStatus::Buffering => (
            "⋯",
            Style::default()
                .fg(Palette::SKY)
                .add_modifier(Modifier::BOLD),
        ),
        PlaybackStatus::Stopped | PlaybackStatus::Idle => (
            "⏹",
            Style::default()
                .fg(Palette::MUTED)
                .add_modifier(Modifier::BOLD),
        ),
    }
}

impl<'a> Widget for PlayerBar<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(border_style(true))
            .title_style(title_style())
            .style(Style::default().bg(Palette::BG))
            .title(" ⚡ Now Playing ");
        let inner = block.inner(area);
        block.render(area, buf);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(inner);

        let (icon, icon_style) = status_icon(self.state.status);
        let renderer_icon = match self.renderer {
            RendererKind::Mpv => "󰐋 local",
            RendererKind::Chromecast => "󰓐 chromecast",
            RendererKind::Upnp => "◈ upnp",
        };
        let (title_text, subtitle_text) = match &self.state.now_playing {
            Some(item) => (item.title.clone(), item.subtitle.clone()),
            None => ("Nothing playing".to_string(), String::new()),
        };

        // Incoming-cast badge — this track was pushed at us by an external
        // UPnP control point (fin acting as a MediaRenderer device), not
        // picked in the TUI. Flag it so unexpected audio is attributable.
        let cast_in_span = match &self.state.now_playing {
            Some(item) if item.is_upnp_cast() => Span::styled(
                "⇊ UPnP ",
                Style::default()
                    .fg(Palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            _ => Span::raw(""),
        };

        // Row 1: status icon + track title + right-aligned renderer + volume
        let title_line = Line::from(vec![
            Span::styled(format!("{} ", icon), icon_style),
            cast_in_span,
            Span::styled(
                title_text,
                Style::default()
                    .fg(Palette::FG)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        // Compact "modes" glyph — only rendered when either mode is active.
        // Colored when on / dim when off keeps the row balanced without
        // gaining/losing columns as the user toggles.
        let shuffle_span = Span::styled(
            "⇄ ",
            if self.state.shuffle {
                Style::default()
                    .fg(Palette::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Palette::MUTED)
            },
        );
        let (repeat_glyph, repeat_active) = match self.state.repeat {
            RepeatMode::Off => ("↻", false),
            RepeatMode::All => ("↻", true),
            RepeatMode::One => ("↺", true),
        };
        let repeat_span = Span::styled(
            format!("{} ", repeat_glyph),
            if repeat_active {
                Style::default()
                    .fg(Palette::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Palette::MUTED)
            },
        );
        let rg_span = if self.state.replaygain.mode.is_active() {
            Span::styled(
                format!("RG:{} ", self.state.replaygain.mode.label()),
                Style::default()
                    .fg(Palette::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        };
        let xf_span = if self.state.crossfade.mode.is_active() {
            let glyph = match self.state.crossfade.mode {
                fin_player::CrossfadeMode::Crossfade => "⋈",
                fin_player::CrossfadeMode::Mixed => "≈",
                fin_player::CrossfadeMode::Off => unreachable!(),
            };
            Span::styled(
                format!("{} {:.0}s ", glyph, self.state.crossfade.duration_secs),
                Style::default()
                    .fg(Palette::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        };
        let eq_span = if self.state.eq_enabled {
            Span::styled(
                "EQ ",
                Style::default()
                    .fg(Palette::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        };
        let tone_span = if self.state.bass_db != 0 || self.state.treble_db != 0 {
            Span::styled(
                format!("B{:+}/T{:+} ", self.state.bass_db, self.state.treble_db),
                Style::default()
                    .fg(Palette::HIGHLIGHT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        };
        let right_line = Line::from(vec![
            shuffle_span,
            repeat_span,
            rg_span,
            xf_span,
            eq_span,
            tone_span,
            Span::styled(
                format!("♪ {}%   ", (self.state.volume * 100.0) as i32),
                Style::default().fg(Palette::HIGHLIGHT),
            ),
            Span::styled(
                format!("{}  ", renderer_icon),
                Style::default()
                    .fg(Palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("on {}", self.renderer_label),
                Style::default().fg(Palette::MUTED),
            ),
        ]);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(rows[0]);
        Paragraph::new(title_line).render(cols[0], buf);
        Paragraph::new(right_line)
            .alignment(ratatui::layout::Alignment::Right)
            .render(cols[1], buf);

        // Row 2: subtitle
        let sub_line = Line::from(vec![Span::styled(subtitle_text, muted_style())]);
        Paragraph::new(sub_line).render(rows[1], buf);

        // Row 3: progress bar with elapsed/total on either side
        let cols2 = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(8),
                Constraint::Min(10),
                Constraint::Length(8),
            ])
            .split(rows[2]);
        let elapsed = fmt_time(self.state.position_secs);
        let total = if self.state.duration_secs > 0.0 {
            fmt_time(self.state.duration_secs)
        } else if let Some(item) = &self.state.now_playing {
            item.duration_secs
                .map(|s| fmt_time(s as f64))
                .unwrap_or_else(|| "--:--".into())
        } else {
            "--:--".into()
        };
        Paragraph::new(Line::from(Span::styled(
            elapsed,
            Style::default().fg(Palette::SKY),
        )))
        .render(cols2[0], buf);
        let ratio = if self.state.duration_secs > 0.0 {
            (self.state.position_secs / self.state.duration_secs).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Gauge::default()
            .gauge_style(
                Style::default()
                    .fg(Palette::PRIMARY)
                    .bg(Palette::SURFACE)
                    .add_modifier(Modifier::BOLD),
            )
            .ratio(ratio)
            .label("")
            .use_unicode(true)
            .render(cols2[1], buf);
        Paragraph::new(Line::from(Span::styled(
            total,
            Style::default().fg(Palette::SKY),
        )))
        .alignment(ratatui::layout::Alignment::Right)
        .render(cols2[2], buf);
    }
}
