//! Bounded, window-local formatting cache. Arrow remains the source of truth;
//! cached text is shared with GPUI and charged to that window's local cache quota.
use std::collections::{BTreeMap, HashMap};

use gpui::SharedString;

use crate::resource_budget::{Client, MemoryLease};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub row: usize,
    pub column: usize,
    pub wrapped: bool,
}

struct Entry {
    text: SharedString,
    touched: u64,
    bytes: usize,
    _lease: MemoryLease,
}

#[derive(Default)]
pub struct DisplayCache {
    entries: HashMap<Key, Entry>,
    age: BTreeMap<u64, Key>,
    clock: u64,
    bytes: usize,
}

impl DisplayCache {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.age.clear();
        self.bytes = 0;
    }

    pub fn trim(&mut self, limit: usize) {
        while self.bytes > limit && self.evict() {}
    }

    fn evict(&mut self) -> bool {
        let Some((_, key)) = self.age.pop_first() else {
            return false;
        };
        if let Some(entry) = self.entries.remove(&key) {
            self.bytes -= entry.bytes;
        }
        true
    }

    fn tick(&mut self) -> u64 {
        if self.clock == u64::MAX {
            self.clear();
            self.clock = 0;
        }
        self.clock += 1;
        self.clock
    }

    pub fn get(&mut self, key: Key) -> Option<SharedString> {
        let touched = self.tick();
        let entry = self.entries.get_mut(&key)?;
        self.age.remove(&entry.touched);
        self.age.insert(touched, key);
        entry.touched = touched;
        Some(entry.text.clone())
    }

    pub fn insert(&mut self, key: Key, text: String, client: &Client) -> SharedString {
        let text: SharedString = text.into();
        // Include conservative hash/B-tree node and shared-string overhead.
        let bytes = text.len().saturating_add(192);
        let limit = client.cache_limit_bytes();
        self.trim(limit.saturating_sub(bytes));
        if bytes > limit {
            return text;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.age.remove(&previous.touched);
            self.bytes -= previous.bytes;
        }
        let lease = loop {
            if let Some(lease) = client.try_reserve_cache(bytes) {
                break lease;
            }
            if !self.evict() {
                return text;
            }
        };
        let touched = self.tick();
        self.age.insert(touched, key);
        self.bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                text: text.clone(),
                touched,
                bytes,
                _lease: lease,
            },
        );
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(row: usize) -> Key {
        Key {
            row,
            column: 0,
            wrapped: false,
        }
    }

    #[test]
    fn hits_update_lru_without_growing_age_metadata() {
        let client = Client::new();
        let mut cache = DisplayCache::default();
        cache.insert(key(1), "one".into(), &client);
        cache.insert(key(2), "two".into(), &client);
        for _ in 0..10_000 {
            assert_eq!(cache.get(key(1)).unwrap().as_ref(), "one");
        }
        assert_eq!(cache.age.len(), 2);
        cache.trim(195);
        assert!(cache.get(key(2)).is_none());
        assert!(cache.get(key(1)).is_some());
    }

    #[test]
    fn evicting_cached_text_keeps_frame_shared_string_valid() {
        let client = Client::new();
        let mut cache = DisplayCache::default();
        let held = cache.insert(key(1), "中文😀".into(), &client);
        cache.clear();
        assert_eq!(held.as_ref(), "中文😀");
        assert_eq!(cache.bytes, 0);
        assert!(cache.age.is_empty());
    }

    #[test]
    fn exceeding_window_allowance_is_not_retained() {
        let client = Client::new();
        client.set_active(false);
        let mut cache = DisplayCache::default();
        let text = "x".repeat(client.cache_limit_bytes() + 1);
        let held = cache.insert(key(0), text.clone(), &client);
        assert_eq!(held.as_ref(), text);
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }
}
