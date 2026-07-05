use std::sync::Arc;

use parking_lot::RwLock;
use rand::seq::SliceRandom;
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

/// How the queue behaves once the last item finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    Off,
    One,
    All,
}

impl RepeatMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::One => "one",
            Self::All => "all",
        }
    }

    /// Cycle order matches what users expect from most players:
    /// off → all → one → off.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
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
    shuffle: bool,
    repeat: RepeatMode,
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
        let next = match (g.repeat, g.index) {
            // Repeat-one: keep the playhead on the same track.
            (RepeatMode::One, Some(i)) if i < g.items.len() => Some(i),
            (_, Some(i)) if i + 1 < g.items.len() => Some(i + 1),
            // Repeat-all: wrap around at the end.
            (RepeatMode::All, _) if !g.items.is_empty() => Some(0),
            (_, None) if !g.items.is_empty() => Some(0),
            _ => None,
        };
        g.index = next;
        next
    }

    pub fn back(&self) -> Option<usize> {
        let mut g = self.inner.write();
        let prev = match (g.repeat, g.index) {
            (RepeatMode::One, Some(i)) => Some(i),
            (_, Some(i)) if i > 0 => Some(i - 1),
            // Repeat-all wraps to the end when Prev is hit at index 0.
            (RepeatMode::All, Some(0)) if !g.items.is_empty() => Some(g.items.len() - 1),
            _ => g.index,
        };
        g.index = prev;
        prev
    }

    pub fn shuffle_enabled(&self) -> bool {
        self.inner.read().shuffle
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        self.inner.read().repeat
    }

    pub fn set_repeat(&self, mode: RepeatMode) {
        self.inner.write().repeat = mode;
    }

    /// Toggle shuffle. Turning shuffle ON reshuffles every item AFTER the
    /// currently-playing one — the current item stays where it is so the
    /// track doesn't jump. Turning shuffle OFF is a no-op on the order
    /// (we don't stash the original permutation to restore).
    pub fn set_shuffle(&self, on: bool) {
        let mut g = self.inner.write();
        g.shuffle = on;
        if !on {
            return;
        }
        let start = g.index.map(|i| i + 1).unwrap_or(0);
        if start < g.items.len() {
            let mut rng = rand::rng();
            g.items[start..].shuffle(&mut rng);
        }
    }

    /// Full-queue reshuffle — used on restore and by tests. Preserves the
    /// current index by swapping the currently-playing item to position 0.
    pub fn reshuffle_all(&self) {
        let mut g = self.inner.write();
        if g.items.is_empty() {
            return;
        }
        let mut rng = rand::rng();
        g.items.shuffle(&mut rng);
        if let Some(_) = g.index {
            g.index = Some(0);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> QueueItem {
        QueueItem {
            id: id.into(),
            title: id.into(),
            subtitle: String::new(),
            stream_url: format!("http://example/{id}"),
            image_url: None,
            duration_secs: Some(180),
            is_video: false,
            content_type: "audio/mpeg".into(),
        }
    }

    fn ids(q: &PlaybackQueue) -> Vec<String> {
        q.items().into_iter().map(|i| i.id).collect()
    }

    #[test]
    fn empty_queue_has_no_current() {
        let q = PlaybackQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert_eq!(q.current_index(), None);
        assert!(q.current().is_none());
    }

    #[test]
    fn replace_sets_index_and_clamps() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b"), item("c")], 1);
        assert_eq!(q.current_index(), Some(1));
        assert_eq!(q.current().unwrap().id, "b");
        // Out-of-range start clamps to the last valid index.
        q.replace(vec![item("x"), item("y")], 99);
        assert_eq!(q.current_index(), Some(1));
        assert_eq!(q.current().unwrap().id, "y");
    }

    #[test]
    fn replace_with_empty_clears_index() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a")], 0);
        q.replace(vec![], 0);
        assert_eq!(q.current_index(), None);
        assert!(q.current().is_none());
    }

    #[test]
    fn append_sets_index_when_starting_empty() {
        let q = PlaybackQueue::new();
        q.append(vec![item("a"), item("b")]);
        assert_eq!(q.current_index(), Some(0));
        // Appending again keeps the current index.
        q.append(vec![item("c")]);
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(ids(&q), vec!["a", "b", "c"]);
    }

    #[test]
    fn insert_next_places_items_after_current() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b"), item("c")], 1);
        q.insert_next(vec![item("x"), item("y")]);
        assert_eq!(ids(&q), vec!["a", "b", "x", "y", "c"]);
        // Current stays on "b".
        assert_eq!(q.current().unwrap().id, "b");
    }

    #[test]
    fn insert_next_into_empty_becomes_first() {
        let q = PlaybackQueue::new();
        q.insert_next(vec![item("a"), item("b")]);
        assert_eq!(ids(&q), vec!["a", "b"]);
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn advance_walks_to_end_then_stops() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b"), item("c")], 0);
        assert_eq!(q.advance(), Some(1));
        assert_eq!(q.advance(), Some(2));
        assert_eq!(q.advance(), None);
        // Once past the end, current becomes None.
        assert_eq!(q.current_index(), None);
    }

    #[test]
    fn back_walks_to_zero_and_stays() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b"), item("c")], 2);
        assert_eq!(q.back(), Some(1));
        assert_eq!(q.back(), Some(0));
        assert_eq!(q.back(), Some(0));
    }

    #[test]
    fn set_index_within_bounds_moves_cursor() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b"), item("c")], 0);
        q.set_index(2);
        assert_eq!(q.current().unwrap().id, "c");
        // Out-of-bounds set_index is a no-op.
        q.set_index(99);
        assert_eq!(q.current().unwrap().id, "c");
    }

    #[test]
    fn remove_before_current_shifts_index_down() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b"), item("c")], 2);
        q.remove(0);
        assert_eq!(ids(&q), vec!["b", "c"]);
        assert_eq!(q.current().unwrap().id, "c");
    }

    #[test]
    fn remove_current_last_clamps() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b")], 1);
        q.remove(1);
        assert_eq!(ids(&q), vec!["a"]);
        assert_eq!(q.current().unwrap().id, "a");
    }

    #[test]
    fn remove_only_item_clears_index() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a")], 0);
        q.remove(0);
        assert!(q.is_empty());
        assert_eq!(q.current_index(), None);
    }

    #[test]
    fn remove_out_of_bounds_is_noop() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b")], 0);
        q.remove(99);
        assert_eq!(ids(&q), vec!["a", "b"]);
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn clear_resets_everything() {
        let q = PlaybackQueue::new();
        q.replace(vec![item("a"), item("b")], 1);
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.current_index(), None);
    }
}
