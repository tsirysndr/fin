use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream, KeyEvent, KeyEventKind};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::interval;

/// Application-level events driving the render loop.
#[derive(Debug, Clone)]
pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    Resize(u16, u16),
}

/// Spawn a task that muxes crossterm input events and a periodic tick.
pub fn spawn_event_loop(tick: Duration) -> mpsc::UnboundedReceiver<AppEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let tx_tick = tx.clone();
    tokio::spawn(async move {
        let mut ticker = interval(tick);
        loop {
            ticker.tick().await;
            if tx_tick.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(Ok(ev)) = events.next().await {
            let out = match ev {
                CtEvent::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                    Some(AppEvent::Key(k))
                }
                CtEvent::Resize(w, h) => Some(AppEvent::Resize(w, h)),
                _ => None,
            };
            if let Some(e) = out {
                if tx.send(e).is_err() {
                    break;
                }
            }
        }
    });
    rx
}

pub async fn drain_all(rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> Result<Vec<AppEvent>> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    Ok(out)
}
