//! Pure playback queue and mode state machine.
//!
//! No I/O lives here, which makes the tricky parts (shuffle history, sequential
//! stop, previous after a random jump, heartbeat context) unit-testable.

use std::collections::VecDeque;

use rand::Rng;
use yoruplay_protocol::{PlayMode, PlaySource, PlaylistRef, QueueItem, QueueSnapshot, Song};

const HISTORY_LIMIT: usize = 1024;
const SHUFFLE_MEMORY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// Play this queue index.
    Index(usize),
    /// The queue ended (sequential mode at the last track).
    End,
    /// Heartbeat must fetch the next recommendation; the queue did not move.
    Heartbeat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    RemovedCurrent { stopped: bool },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QueueError {
    #[error("Heartbeat needs a playlist context; start playback from a playlist first")]
    HeartbeatRequiresContext,
    #[error("queue index {0} is out of range")]
    IndexOutOfRange(usize),
}

#[derive(Debug, Clone)]
pub struct Queue {
    items: Vec<QueueItem>,
    current: Option<usize>,
    mode: PlayMode,
    source: PlaySource,
    history: Vec<usize>,
    shuffle_recent: VecDeque<usize>,
    heartbeat_seed: Option<u64>,
}

impl Queue {
    pub fn new(mode: PlayMode) -> Self {
        Self {
            items: Vec::new(),
            current: None,
            mode,
            source: PlaySource::none(),
            history: Vec::new(),
            shuffle_recent: VecDeque::new(),
            heartbeat_seed: None,
        }
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.current.and_then(|i| self.items.get(i))
    }

    pub fn mode(&self) -> PlayMode {
        self.mode
    }

    pub fn source(&self) -> &PlaySource {
        &self.source
    }

    pub fn heartbeat_seed(&self) -> Option<u64> {
        self.heartbeat_seed
    }

    pub fn set_source(&mut self, source: PlaySource) {
        self.source = source;
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            items: self.items.clone(),
            current: self.current,
            source: self.source.clone(),
            mode: self.mode,
        }
    }

    pub fn set_mode(&mut self, mode: PlayMode) -> Result<(), QueueError> {
        if mode == PlayMode::Intelligence {
            let item = self
                .current()
                .or_else(|| self.items.first())
                .ok_or(QueueError::HeartbeatRequiresContext)?;
            let context = item
                .context
                .as_ref()
                .ok_or(QueueError::HeartbeatRequiresContext)?;
            let _ = context;
            self.heartbeat_seed = Some(item.song.id);
        } else {
            self.heartbeat_seed = None;
        }
        self.mode = mode;
        Ok(())
    }

    /// Replace the queue and select `start_index`. The caller is responsible
    /// for actually loading the selected track.
    pub fn replace(
        &mut self,
        items: Vec<QueueItem>,
        start_index: usize,
        source: PlaySource,
    ) -> Option<usize> {
        self.items = items;
        self.source = source;
        self.history.clear();
        self.shuffle_recent.clear();
        self.current = if self.items.is_empty() {
            None
        } else {
            Some(start_index.min(self.items.len() - 1))
        };
        self.heartbeat_seed = if self.mode == PlayMode::Intelligence {
            self.current().map(|i| i.song.id)
        } else {
            None
        };
        self.current
    }

    pub fn append(&mut self, items: impl IntoIterator<Item = QueueItem>) {
        let before = self.items.len();
        self.items.extend(items);
        if self.current.is_none() && self.items.len() > before {
            // Adding to an empty, stopped queue does not start playback. The
            // first item becomes the selected item so Play has a target.
            self.current = Some(0);
        }
    }

    /// Insert a generated Heartbeat track immediately after the current item
    /// and move to it. Returns the new index.
    pub fn insert_after_current(&mut self, item: QueueItem) -> usize {
        let pos = match self.current {
            Some(i) => (i + 1).min(self.items.len()),
            None => self.items.len(),
        };
        self.items.insert(pos, item);
        if let Some(current) = self.current {
            if current != pos {
                self.push_history(current);
            }
        }
        self.current = Some(pos);
        self.shuffle_recent.clear();
        self.history.retain(|i| *i < self.items.len());
        pos
    }

    /// Jump to an existing index, recording the previous index for Previous.
    pub fn jump_to(&mut self, index: usize) -> Result<usize, QueueError> {
        if index >= self.items.len() {
            return Err(QueueError::IndexOutOfRange(index));
        }
        if let Some(current) = self.current {
            if current != index {
                self.push_history(current);
            }
        }
        self.current = Some(index);
        Ok(index)
    }

    pub fn set_current_without_history(&mut self, index: usize) -> Result<(), QueueError> {
        if index >= self.items.len() {
            return Err(QueueError::IndexOutOfRange(index));
        }
        self.current = Some(index);
        Ok(())
    }

    pub fn advance(&mut self, auto: bool, rng: &mut impl Rng) -> Advance {
        if self.items.is_empty() {
            self.current = None;
            return Advance::End;
        }
        let current = self.current.unwrap_or(0).min(self.items.len() - 1);
        let len = self.items.len();
        match self.mode {
            PlayMode::Intelligence => Advance::Heartbeat,
            PlayMode::TrackLoop if auto => {
                self.current = Some(current);
                Advance::Index(current)
            }
            PlayMode::TrackLoop | PlayMode::ListLoop => {
                let next = (current + 1) % len;
                if next != current {
                    self.push_history(current);
                }
                self.current = Some(next);
                Advance::Index(next)
            }
            PlayMode::Sequential => {
                if current + 1 < len {
                    self.push_history(current);
                    self.current = Some(current + 1);
                    Advance::Index(current + 1)
                } else {
                    self.current = Some(current);
                    Advance::End
                }
            }
            PlayMode::Shuffle => {
                let next = self.pick_shuffle(rng);
                if next != current {
                    self.push_history(current);
                }
                self.current = Some(next);
                Advance::Index(next)
            }
        }
    }

    fn pick_shuffle(&mut self, rng: &mut impl Rng) -> usize {
        let len = self.items.len();
        if len <= 1 {
            return 0;
        }
        let current = self.current;
        let memory = SHUFFLE_MEMORY.min(len.saturating_sub(1));
        let recent: Vec<usize> = self
            .shuffle_recent
            .iter()
            .rev()
            .take(memory)
            .copied()
            .collect();
        let mut candidates: Vec<usize> = (0..len)
            .filter(|i| Some(*i) != current && !recent.contains(i))
            .collect();
        if candidates.is_empty() {
            candidates = (0..len).filter(|i| Some(*i) != current).collect();
        }
        if candidates.is_empty() {
            candidates.push(current.unwrap_or(0).min(len - 1));
        }
        let chosen = candidates[rng.gen_range(0..candidates.len())];
        if self.shuffle_recent.len() >= memory.max(1) {
            self.shuffle_recent.pop_front();
        }
        self.shuffle_recent.push_back(chosen);
        chosen
    }

    /// Manual Previous. Uses real playback history first, then a conservative
    /// mode-specific fallback.
    pub fn previous(&mut self) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        while let Some(prev) = self.history.pop() {
            if prev < self.items.len() {
                self.current = Some(prev);
                return Some(prev);
            }
        }
        let current = self.current.unwrap_or(0).min(self.items.len() - 1);
        match self.mode {
            PlayMode::ListLoop | PlayMode::TrackLoop => {
                let prev = if current == 0 {
                    self.items.len() - 1
                } else {
                    current - 1
                };
                self.current = Some(prev);
                Some(prev)
            }
            PlayMode::Sequential if current > 0 => {
                self.current = Some(current - 1);
                Some(current - 1)
            }
            _ => None,
        }
    }

    pub fn remove(&mut self, index: usize) -> Result<RemoveOutcome, QueueError> {
        if index >= self.items.len() {
            return Err(QueueError::IndexOutOfRange(index));
        }
        let was_current = self.current == Some(index);
        self.items.remove(index);
        self.history.clear();
        self.shuffle_recent.clear();
        if self.items.is_empty() {
            self.current = None;
            self.heartbeat_seed = None;
        } else if was_current {
            let next = index.min(self.items.len() - 1);
            self.current = Some(next);
            if self.mode == PlayMode::Intelligence {
                self.heartbeat_seed = self.current().map(|i| i.song.id);
            }
        } else if let Some(current) = self.current {
            if current > index {
                self.current = Some(current - 1);
            }
        }
        Ok(if was_current {
            RemoveOutcome::RemovedCurrent {
                stopped: !self.items.is_empty(),
            }
        } else {
            RemoveOutcome::Removed
        })
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.history.clear();
        self.shuffle_recent.clear();
        self.heartbeat_seed = None;
        self.source = PlaySource::none();
    }

    fn push_history(&mut self, index: usize) {
        if self.history.last() != Some(&index) {
            self.history.push(index);
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
        }
    }

    pub fn context_of_current(&self) -> Option<PlaylistRef> {
        self.current().and_then(|i| i.context.clone())
    }

    pub fn song_of_current(&self) -> Option<&Song> {
        self.current().map(|i| &i.song)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn song(id: u64) -> Song {
        Song {
            id,
            name: format!("song {id}"),
            artists: vec!["artist".into()],
            artist_ids: vec![1],
            album: "album".into(),
            album_id: 1,
            cover_url: None,
            duration_ms: 180_000,
            fee: 0,
            playable: Some(true),
        }
    }

    fn item(id: u64) -> QueueItem {
        QueueItem {
            song: song(id),
            context: None,
        }
    }

    fn ctx_item(id: u64, pid: u64) -> QueueItem {
        QueueItem {
            song: song(id),
            context: Some(PlaylistRef {
                id: pid,
                name: "list".into(),
            }),
        }
    }

    fn queue(mode: PlayMode) -> Queue {
        let mut q = Queue::new(mode);
        q.replace((1..=4).map(item).collect(), 0, PlaySource::none());
        q
    }

    #[test]
    fn list_loop_wraps() {
        let mut q = queue(PlayMode::ListLoop);
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(q.advance(true, &mut rng), Advance::Index(1));
        assert_eq!(q.advance(true, &mut rng), Advance::Index(2));
        assert_eq!(q.advance(true, &mut rng), Advance::Index(3));
        assert_eq!(q.advance(true, &mut rng), Advance::Index(0));
    }

    #[test]
    fn track_loop_repeats_on_end_but_next_advances() {
        let mut q = queue(PlayMode::TrackLoop);
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(q.advance(true, &mut rng), Advance::Index(0));
        assert_eq!(q.advance(false, &mut rng), Advance::Index(1));
    }

    #[test]
    fn sequential_stops_at_end() {
        let mut q = queue(PlayMode::Sequential);
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(q.advance(true, &mut rng), Advance::Index(1));
        assert_eq!(q.advance(true, &mut rng), Advance::Index(2));
        assert_eq!(q.advance(true, &mut rng), Advance::Index(3));
        assert_eq!(q.advance(true, &mut rng), Advance::End);
        assert_eq!(q.current_index(), Some(3));
    }

    #[test]
    fn shuffle_avoids_short_repeats_and_previous_works() {
        let mut q = queue(PlayMode::Shuffle);
        let mut rng = StdRng::seed_from_u64(42);
        let mut seen = vec![0usize];
        for _ in 0..30 {
            let Advance::Index(next) = q.advance(true, &mut rng) else {
                panic!("unexpected end");
            };
            let last = *seen.last().unwrap();
            assert_ne!(next, last);
            assert!(
                !seen.iter().rev().take(3).any(|i| *i == next),
                "repeat in {seen:?}"
            );
            seen.push(next);
        }
        let before = q.current_index();
        let prev = q.previous();
        assert!(prev.is_some());
        assert_ne!(prev, before);
    }

    #[test]
    fn heartbeat_requires_context_and_keeps_seed() {
        let mut q = Queue::new(PlayMode::ListLoop);
        q.replace((1..=3).map(item).collect(), 0, PlaySource::none());
        assert_eq!(
            q.set_mode(PlayMode::Intelligence),
            Err(QueueError::HeartbeatRequiresContext)
        );
        let mut q = Queue::new(PlayMode::ListLoop);
        q.replace(
            (1..=3).map(|id| ctx_item(id, 99)).collect(),
            1,
            PlaySource::playlist(PlaylistRef {
                id: 99,
                name: "list".into(),
            }),
        );
        q.set_mode(PlayMode::Intelligence).unwrap();
        assert_eq!(q.heartbeat_seed(), Some(2));
        let mut rng = StdRng::seed_from_u64(1);
        assert_eq!(q.advance(true, &mut rng), Advance::Heartbeat);
    }

    #[test]
    fn remove_current_selects_next_and_reports_stop() {
        let mut q = queue(PlayMode::ListLoop);
        q.jump_to(2).unwrap();
        let outcome = q.remove(2).unwrap();
        assert_eq!(outcome, RemoveOutcome::RemovedCurrent { stopped: true });
        assert_eq!(q.current_index(), Some(2));
        assert_eq!(q.len(), 3);
        let outcome = q.remove(0).unwrap();
        assert_eq!(outcome, RemoveOutcome::Removed);
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn append_to_empty_selects_without_starting() {
        let mut q = Queue::new(PlayMode::ListLoop);
        q.append(vec![item(1), item(2)]);
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn insert_heartbeat_track_after_current_moves_to_it() {
        let mut q = queue(PlayMode::ListLoop);
        q.jump_to(1).unwrap();
        let idx = q.insert_after_current(ctx_item(50, 7));
        assert_eq!(idx, 2);
        assert_eq!(q.current_index(), Some(2));
        assert_eq!(q.song_of_current().unwrap().id, 50);
        assert_eq!(q.previous(), Some(1));
    }
}
