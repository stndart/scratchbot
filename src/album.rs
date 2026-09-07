use crate::mapping::IncomingItem;
use std::collections::HashMap;

#[derive(Default)]
pub struct AlbumBuffer {
    groups: HashMap<String, HashMap<i64, IncomingItem>>,
    last_seen_ms: HashMap<String, u64>,
}

impl AlbumBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, item: IncomingItem, now_ms: u64) -> Option<IncomingItem> {
        let Some(group) = item.media_group_id.clone() else {
            return Some(item);
        };
        self.groups
            .entry(group.clone())
            .or_default()
            .insert(item.message_id, item);
        self.last_seen_ms.insert(group, now_ms);
        None
    }

    pub fn take_ready(&mut self, now_ms: u64, grace_ms: u64) -> Vec<IncomingItem> {
        let ready: Vec<String> = self
            .last_seen_ms
            .iter()
            .filter(|(_, seen)| now_ms.saturating_sub(**seen) >= grace_ms)
            .map(|(key, _)| key.clone())
            .collect();
        let mut out = Vec::with_capacity(ready.len());
        for key in ready {
            self.last_seen_ms.remove(&key);
            if let Some(items) = self.groups.remove(&key) {
                out.push(merge_album(items.into_values().collect()));
            }
        }
        out
    }

    pub fn has_pending(&self) -> bool {
        !self.groups.is_empty()
    }

    pub fn next_deadline_ms(&self, grace_ms: u64) -> Option<u64> {
        self.last_seen_ms
            .values()
            .min()
            .map(|seen| seen.saturating_add(grace_ms))
    }
}

pub fn merge_album(mut items: Vec<IncomingItem>) -> IncomingItem {
    items.sort_by_key(|item| item.message_id);
    let mut merged = items
        .first()
        .cloned()
        .expect("album merge requires at least one item");
    merged.update_ids.clear();
    merged.files.clear();
    merged.body.clear();
    merged.too_large = None;
    for item in items {
        merged.update_ids.extend(item.update_ids);
        if merged.body.is_empty() {
            merged.body = item.body;
        }
        merged.files.extend(item.files);
        if merged.too_large.is_none() {
            merged.too_large = item.too_large;
        }
        merged.media_group_id = item.media_group_id.or(merged.media_group_id);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::IncomingFile;

    fn item(message_id: i64, caption: &str, file_id: &str) -> IncomingItem {
        IncomingItem {
            update_ids: vec![1000 + message_id],
            chat_id: 42,
            user_id: 42,
            message_id,
            media_group_id: Some("album-1".into()),
            body: caption.into(),
            files: vec![IncomingFile {
                file_id: file_id.into(),
                filename: "photo.jpg".into(),
                mime_type: "image/jpeg".into(),
                size: Some(10),
            }],
            too_large: None,
        }
    }

    #[test]
    fn coalesces_album_after_grace_and_keeps_first_caption() {
        let mut buffer = AlbumBuffer::new();
        assert!(buffer.push(item(1, "first caption", "a"), 0).is_none());
        assert!(buffer.push(item(2, "", "b"), 200).is_none());
        assert!(buffer.push(item(3, "", "c"), 400).is_none());
        assert!(buffer.take_ready(1000, 1500).is_empty());
        let ready = buffer.take_ready(2000, 1500);
        assert_eq!(ready.len(), 1);
        let album = &ready[0];
        assert_eq!(album.body, "first caption");
        assert_eq!(album.files.len(), 3);
        assert_eq!(
            album
                .files
                .iter()
                .map(|file| file.file_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(album.idempotency_key(), "tg:42:album:album-1");
        assert_eq!(album.update_ids, vec![1001, 1002, 1003]);
    }

    #[test]
    fn standalone_messages_flush_immediately() {
        let mut buffer = AlbumBuffer::new();
        let mut item = item(9, "solo", "x");
        item.media_group_id = None;
        let ready = buffer.push(item, 0).unwrap();
        assert_eq!(ready.body, "solo");
        assert!(!buffer.has_pending());
    }
}
