use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    #[default]
    Sent,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredMessage {
    pub chat_id: i64,
    pub message_id: i64,
    #[serde(default)]
    pub status: DeliveryStatus,
}

#[derive(Default, Serialize, Deserialize)]
struct DeliveredFile {
    #[serde(default)]
    messages: HashMap<String, Vec<DeliveredMessage>>,
}

type MessageOutcomes = BTreeMap<(i64, i64), DeliveryStatus>;

pub struct Delivered {
    path: PathBuf,
    by_user: HashMap<i64, MessageOutcomes>,
}

impl Delivered {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let by_user = match fs::read_to_string(&path) {
            Ok(text) => parse_delivered_file(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, by_user })
    }

    pub fn record(
        &mut self,
        user_id: i64,
        chat_id: i64,
        message_ids: impl IntoIterator<Item = i64>,
        status: DeliveryStatus,
    ) -> Result<()> {
        let entries = self.by_user.entry(user_id).or_default();
        let mut changed = false;
        for message_id in message_ids {
            match entries.get(&(chat_id, message_id)) {
                Some(current) if *current == status => {}
                _ => {
                    entries.insert((chat_id, message_id), status);
                    changed = true;
                }
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    pub fn list_for_chat(&self, user_id: i64, chat_id: i64) -> Vec<DeliveredMessage> {
        self.messages_for_chat(user_id, chat_id, |_| true)
    }

    pub fn list_sent_for_chat(&self, user_id: i64, chat_id: i64) -> Vec<DeliveredMessage> {
        self.messages_for_chat(user_id, chat_id, |status| status == DeliveryStatus::Sent)
    }

    pub fn remove(&mut self, user_id: i64, messages: &[DeliveredMessage]) -> Result<()> {
        let Some(entries) = self.by_user.get_mut(&user_id) else {
            return Ok(());
        };
        let mut changed = false;
        for message in messages {
            changed |= entries
                .remove(&(message.chat_id, message.message_id))
                .is_some();
        }
        if entries.is_empty() {
            self.by_user.remove(&user_id);
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut messages = HashMap::new();
        for (user_id, entries) in &self.by_user {
            messages.insert(
                user_id.to_string(),
                entries
                    .iter()
                    .map(|(&(chat_id, message_id), &status)| DeliveredMessage {
                        chat_id,
                        message_id,
                        status,
                    })
                    .collect(),
            );
        }
        fs::write(
            &self.path,
            serde_json::to_string_pretty(&DeliveredFile { messages })?,
        )?;
        Ok(())
    }
}

pub fn delivered_path(state_dir: &Path) -> PathBuf {
    state_dir.join("delivered.json")
}

fn parse_delivered_file(text: &str) -> Result<HashMap<i64, MessageOutcomes>> {
    let file: DeliveredFile = serde_json::from_str(text)?;
    let mut by_user = HashMap::new();
    for (key, messages) in file.messages {
        let Ok(user_id) = key.parse::<i64>() else {
            continue;
        };
        let entries = messages
            .into_iter()
            .map(|message| ((message.chat_id, message.message_id), message.status))
            .collect();
        by_user.insert(user_id, entries);
    }
    Ok(by_user)
}

impl Delivered {
    fn messages_for_chat(
        &self,
        user_id: i64,
        chat_id: i64,
        keep: impl Fn(DeliveryStatus) -> bool,
    ) -> Vec<DeliveredMessage> {
        let Some(entries) = self.by_user.get(&user_id) else {
            return Vec::new();
        };
        let mut messages = Vec::new();
        for (&(entry_chat, message_id), &status) in entries {
            if entry_chat == chat_id && keep(status) {
                messages.push(DeliveredMessage {
                    chat_id: entry_chat,
                    message_id,
                    status,
                });
            }
        }
        messages
    }
}

/// Oldest sent message such that every later message in `existing` was also sent.
/// `existing` is the messages still present in the chat, in any order.
pub fn oldest_sent_tail(existing: &[DeliveredMessage]) -> Option<DeliveredMessage> {
    let mut ordered = existing.to_vec();
    ordered.sort_by_key(|message| message.message_id);
    let start = ordered
        .iter()
        .rposition(|message| message.status == DeliveryStatus::Failed)
        .map(|index| index + 1)
        .unwrap_or(0);
    ordered[start..]
        .iter()
        .find(|message| message.status == DeliveryStatus::Sent)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_successful_messages_per_chat() {
        let directory =
            std::env::temp_dir().join(format!("scratchwall-delivered-{}", std::process::id()));
        let path = directory.join("delivered.json");
        let _ = fs::remove_dir_all(&directory);
        let mut delivered = Delivered::load(&path).unwrap();
        delivered
            .record(42, 42, [10, 11, 11], DeliveryStatus::Sent)
            .unwrap();
        delivered
            .record(42, 99, [12], DeliveryStatus::Sent)
            .unwrap();
        delivered
            .record(7, 42, [13], DeliveryStatus::Failed)
            .unwrap();
        assert_eq!(
            delivered.list_sent_for_chat(42, 42),
            vec![
                DeliveredMessage {
                    chat_id: 42,
                    message_id: 10,
                    status: DeliveryStatus::Sent
                },
                DeliveredMessage {
                    chat_id: 42,
                    message_id: 11,
                    status: DeliveryStatus::Sent
                },
            ]
        );
        assert!(delivered.list_sent_for_chat(7, 42).is_empty());
        delivered
            .remove(
                42,
                &[DeliveredMessage {
                    chat_id: 42,
                    message_id: 10,
                    status: DeliveryStatus::Sent,
                }],
            )
            .unwrap();
        let reloaded = Delivered::load(&path).unwrap();
        assert_eq!(
            reloaded.list_for_chat(42, 42),
            vec![DeliveredMessage {
                chat_id: 42,
                message_id: 11,
                status: DeliveryStatus::Sent
            }]
        );
        assert_eq!(reloaded.list_for_chat(42, 99).len(), 1);
        assert_eq!(reloaded.list_for_chat(7, 42).len(), 1);
        assert_eq!(
            reloaded.list_for_chat(7, 42)[0].status,
            DeliveryStatus::Failed
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn empty_chat_has_nothing_to_clear() {
        let delivered =
            Delivered::load(std::env::temp_dir().join("missing-delivered.json")).unwrap();
        assert!(delivered.list_for_chat(1, 1).is_empty());
    }

    #[test]
    fn older_file_without_status_counts_as_sent() {
        let directory =
            std::env::temp_dir().join(format!("scratchwall-delivered-old-{}", std::process::id()));
        let path = directory.join("delivered.json");
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &path,
            r#"{"messages":{"42":[{"chat_id":42,"message_id":3}]}}"#,
        )
        .unwrap();
        let delivered = Delivered::load(&path).unwrap();
        assert_eq!(
            delivered.list_sent_for_chat(42, 42),
            vec![DeliveredMessage {
                chat_id: 42,
                message_id: 3,
                status: DeliveryStatus::Sent
            }]
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn oldest_sent_tail_stops_at_a_later_failure() {
        let sent = |message_id| DeliveredMessage {
            chat_id: 1,
            message_id,
            status: DeliveryStatus::Sent,
        };
        let failed = |message_id| DeliveredMessage {
            chat_id: 1,
            message_id,
            status: DeliveryStatus::Failed,
        };
        assert_eq!(
            oldest_sent_tail(&[sent(1), failed(2), sent(4), sent(3)])
                .map(|message| message.message_id),
            Some(3)
        );
        assert_eq!(
            oldest_sent_tail(&[sent(1), sent(3), sent(4)]).map(|message| message.message_id),
            Some(1)
        );
        assert_eq!(oldest_sent_tail(&[sent(1), failed(5)]), None);
        assert_eq!(oldest_sent_tail(&[]), None);
    }
}
