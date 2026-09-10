use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeliveredMessage {
    pub chat_id: i64,
    pub message_id: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct DeliveredFile {
    #[serde(default)]
    messages: HashMap<String, Vec<DeliveredMessage>>,
}

pub struct Delivered {
    path: PathBuf,
    by_user: HashMap<i64, BTreeSet<DeliveredMessage>>,
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
    ) -> Result<()> {
        let entries = self.by_user.entry(user_id).or_default();
        let mut changed = false;
        for message_id in message_ids {
            changed |= entries.insert(DeliveredMessage {
                chat_id,
                message_id,
            });
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    pub fn list_for_chat(&self, user_id: i64, chat_id: i64) -> Vec<DeliveredMessage> {
        self.by_user
            .get(&user_id)
            .into_iter()
            .flatten()
            .filter(|message| message.chat_id == chat_id)
            .copied()
            .collect()
    }

    pub fn remove(&mut self, user_id: i64, messages: &[DeliveredMessage]) -> Result<()> {
        let Some(entries) = self.by_user.get_mut(&user_id) else {
            return Ok(());
        };
        let mut changed = false;
        for message in messages {
            changed |= entries.remove(message);
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
                entries.iter().copied().collect::<Vec<_>>(),
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

fn parse_delivered_file(text: &str) -> Result<HashMap<i64, BTreeSet<DeliveredMessage>>> {
    let file: DeliveredFile = serde_json::from_str(text)?;
    let mut by_user = HashMap::new();
    for (key, messages) in file.messages {
        let Ok(user_id) = key.parse::<i64>() else {
            continue;
        };
        by_user.insert(user_id, messages.into_iter().collect());
    }
    Ok(by_user)
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
        delivered.record(42, 42, [10, 11, 11]).unwrap();
        delivered.record(42, 99, [12]).unwrap();
        delivered.record(7, 42, [13]).unwrap();
        assert_eq!(
            delivered.list_for_chat(42, 42),
            vec![
                DeliveredMessage {
                    chat_id: 42,
                    message_id: 10
                },
                DeliveredMessage {
                    chat_id: 42,
                    message_id: 11
                },
            ]
        );
        delivered
            .remove(
                42,
                &[DeliveredMessage {
                    chat_id: 42,
                    message_id: 10,
                }],
            )
            .unwrap();
        let reloaded = Delivered::load(&path).unwrap();
        assert_eq!(
            reloaded.list_for_chat(42, 42),
            vec![DeliveredMessage {
                chat_id: 42,
                message_id: 11
            }]
        );
        assert_eq!(reloaded.list_for_chat(42, 99).len(), 1);
        assert_eq!(reloaded.list_for_chat(7, 42).len(), 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn empty_chat_has_nothing_to_clear() {
        let delivered =
            Delivered::load(std::env::temp_dir().join("missing-delivered.json")).unwrap();
        assert!(delivered.list_for_chat(1, 1).is_empty());
    }
}
