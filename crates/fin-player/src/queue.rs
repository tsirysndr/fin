use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Everything a renderer needs to play a single item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub stream_url: String,
    pub image_url: Option<String>,
    pub duration_secs: Option<u64>,
    pub is_video: bool,
    pub content_type: String,
}

/// A thread-safe playback queue. Both the TUI and renderers share the same
/// queue snapshot; the renderer is the source of truth for the current index.
#[derive(Debug, Clone, Default)]
pub struct PlaybackQueue {
    inner: Arc<RwLock<QueueInner>>,
}

#[derive(Debug, Default)]
struct QueueInner {
    items: Vec<QueueItem>,
    index: Option<usize>,
}

impl PlaybackQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> Vec<QueueItem> {
        self.inner.read().items.clone()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.inner.read().index
    }

    pub fn current(&self) -> Option<QueueItem> {
        let g = self.inner.read();
        g.index.and_then(|i| g.items.get(i).cloned())
    }

    pub fn len(&self) -> usize {
        self.inner.read().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().items.is_empty()
    }

    pub fn replace(&self, items: Vec<QueueItem>, index: usize) {
        let mut g = self.inner.write();
        g.items = items;
        g.index = if g.items.is_empty() {
            None
        } else {
            Some(index.min(g.items.len().saturating_sub(1)))
        };
    }

    pub fn append(&self, items: Vec<QueueItem>) {
        let mut g = self.inner.write();
        let was_empty = g.items.is_empty();
        g.items.extend(items);
        if was_empty && !g.items.is_empty() {
            g.index = Some(0);
        }
    }

    pub fn insert_next(&self, items: Vec<QueueItem>) {
        let mut g = self.inner.write();
        let at = g.index.map(|i| i + 1).unwrap_or(0);
        for (offset, item) in items.into_iter().enumerate() {
            g.items.insert(at + offset, item);
        }
        if g.index.is_none() && !g.items.is_empty() {
            g.index = Some(0);
        }
    }

    pub fn advance(&self) -> Option<usize> {
        let mut g = self.inner.write();
        let next = match g.index {
            Some(i) if i + 1 < g.items.len() => Some(i + 1),
            None if !g.items.is_empty() => Some(0),
            _ => None,
        };
        g.index = next;
        next
    }

    pub fn back(&self) -> Option<usize> {
        let mut g = self.inner.write();
        let prev = match g.index {
            Some(i) if i > 0 => Some(i - 1),
            _ => g.index,
        };
        g.index = prev;
        prev
    }

    pub fn set_index(&self, i: usize) {
        let mut g = self.inner.write();
        if i < g.items.len() {
            g.index = Some(i);
        }
    }

    pub fn clear(&self) {
        let mut g = self.inner.write();
        g.items.clear();
        g.index = None;
    }

    pub fn remove(&self, i: usize) {
        let mut g = self.inner.write();
        if i >= g.items.len() {
            return;
        }
        g.items.remove(i);
        if g.items.is_empty() {
            g.index = None;
        } else if let Some(cur) = g.index {
            if i < cur {
                g.index = Some(cur - 1);
            } else if i == cur && cur >= g.items.len() {
                g.index = Some(g.items.len() - 1);
            }
        }
    }
}
