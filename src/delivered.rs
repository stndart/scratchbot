use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    #[default]
    Sent,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct DeliveredMessage {
    pub chat_id: i64,
    pub message_id: i64,
    #[serde(default)]
    pub status: DeliveryStatus,
    #[serde(default)]
    pub date: Option<i64>,
    #[serde(default)]
    pub removed: bool,
    #[serde(default)]
    pub clear_blocked: bool,
}

#[derive(Default, Deserialize)]
struct LegacyFile {
    #[serde(default)]
    messages: HashMap<String, Vec<DeliveredMessage>>,
    #[serde(default)]
    dates: HashMap<String, i64>,
}

pub struct Delivered {
    db: Connection,
}

impl Delivered {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let database_path = path.with_extension("sqlite3");
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut db = Connection::open(&database_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&database_path, fs::Permissions::from_mode(0o600))?;
        }
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS messages (
                user_id INTEGER NOT NULL, chat_id INTEGER NOT NULL, message_id INTEGER NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('sent','failed')), date INTEGER,
                removed INTEGER NOT NULL DEFAULT 0, clear_blocked INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(user_id,chat_id,message_id));
            CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS album_batches (user_id INTEGER NOT NULL,chat_id INTEGER NOT NULL,batch_key TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(user_id,chat_id,batch_key));
            CREATE TABLE IF NOT EXISTS album_members (user_id INTEGER NOT NULL,chat_id INTEGER NOT NULL,message_id INTEGER NOT NULL,batch_key TEXT NOT NULL,PRIMARY KEY(user_id,chat_id,message_id));",
        )?;
        let migrated: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM migrations WHERE name='legacy-json')",
            [],
            |r| r.get(0),
        )?;
        if !migrated {
            let legacy_path = path.with_extension("json");
            let legacy: LegacyFile = match fs::read_to_string(&legacy_path) {
                Ok(text) => {
                    serde_json::from_str(&text).context("invalid legacy delivered history")?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => LegacyFile::default(),
                Err(e) => return Err(e.into()),
            };
            let tx = db.transaction()?;
            for (user, messages) in legacy.messages {
                let user_id: i64 = user.parse()?;
                for m in messages {
                    let date = m.date.or_else(|| {
                        legacy
                            .dates
                            .get(&format!("{user_id}:{}:{}", m.chat_id, m.message_id))
                            .copied()
                    });
                    tx.execute("INSERT OR IGNORE INTO messages(user_id,chat_id,message_id,status,date,removed,clear_blocked) VALUES(?,?,?,?,?,?,?)",
                        params![user_id,m.chat_id,m.message_id,status_text(m.status),date,m.removed,m.clear_blocked])?;
                }
            }
            tx.execute("INSERT INTO migrations(name) VALUES('legacy-json')", [])?;
            tx.commit()?;
        }
        Ok(Self { db })
    }

    /// Reserve immutable upload batches before any network request. On replay,
    /// reconstruct the entire original batch, even if only one member reappears.
    pub fn prepare_batches(
        &mut self,
        item: &crate::mapping::IncomingItem,
    ) -> Result<Vec<crate::mapping::IncomingItem>> {
        let Some(group) = &item.media_group_id else {
            return Ok(vec![item.clone()]);
        };
        if item.too_large.is_some() {
            return Ok(vec![item.clone()]);
        }
        anyhow::ensure!(
            item.files.len() == item.message_ids.len()
                && item.update_ids.len() == item.message_ids.len()
                && item.message_dates.len() == item.message_ids.len(),
            "album file/message mapping is incomplete"
        );
        let tx = self.db.transaction()?;
        let mut keys = std::collections::BTreeSet::new();
        let mut new_indices = Vec::new();
        for (index, id) in item.message_ids.iter().enumerate() {
            let key:Option<String>=tx.query_row("SELECT batch_key FROM album_members WHERE user_id=? AND chat_id=? AND message_id=?",params![item.user_id,item.chat_id,id],|r|r.get(0)).optional()?;
            if let Some(key) = key {
                keys.insert(key);
            } else {
                let sent:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE user_id=? AND chat_id=? AND message_id=? AND status='sent')",params![item.user_id,item.chat_id,id],|r|r.get(0))?;
                if !sent {
                    new_indices.push(index);
                }
            }
        }
        let mut batches = Vec::new();
        for key in keys {
            let payload: String = tx.query_row(
                "SELECT payload FROM album_batches WHERE user_id=? AND chat_id=? AND batch_key=?",
                params![item.user_id, item.chat_id, key],
                |r| r.get(0),
            )?;
            let batch: crate::mapping::IncomingItem = serde_json::from_str(&payload)?;
            let mut complete = true;
            for id in &batch.message_ids {
                let sent:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE user_id=? AND chat_id=? AND message_id=? AND status='sent')",params![item.user_id,item.chat_id,id],|r|r.get(0))?;
                complete &= sent;
            }
            if !complete {
                batches.push(batch);
            }
        }
        if !new_indices.is_empty() {
            let mut batch = item.clone();
            batch.message_id = item.message_ids[new_indices[0]];
            batch.message_ids = new_indices.iter().map(|&i| item.message_ids[i]).collect();
            batch.message_dates = new_indices.iter().map(|&i| item.message_dates[i]).collect();
            batch.update_ids = new_indices.iter().map(|&i| item.update_ids[i]).collect();
            batch.files = new_indices.iter().map(|&i| item.files[i].clone()).collect();
            let key = format!(
                "tg:{}:album:{}:batch:{}",
                item.chat_id, group, batch.message_id
            );
            batch.ingest_key = Some(key.clone());
            tx.execute(
                "INSERT INTO album_batches(user_id,chat_id,batch_key,payload) VALUES(?,?,?,?)",
                params![
                    item.user_id,
                    item.chat_id,
                    key,
                    serde_json::to_string(&batch)?
                ],
            )?;
            for id in &batch.message_ids {
                tx.execute("INSERT INTO album_members(user_id,chat_id,message_id,batch_key) VALUES(?,?,?,?)",params![item.user_id,item.chat_id,id,key])?;
            }
            batches.push(batch);
        }
        tx.commit()?;
        batches.sort_by_key(|batch| batch.message_id);
        Ok(batches)
    }

    pub fn date(&self, user_id: i64, chat_id: i64, message_id: i64) -> Option<i64> {
        self.db
            .query_row(
                "SELECT date FROM messages WHERE user_id=? AND chat_id=? AND message_id=?",
                params![user_id, chat_id, message_id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    pub fn remember_dates(
        &mut self,
        user_id: i64,
        chat_id: i64,
        dates: &[(i64, Option<i64>)],
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        for &(id, date) in dates {
            if let Some(date) = date {
                tx.execute(
                    "UPDATE messages SET date=? WHERE user_id=? AND chat_id=? AND message_id=?",
                    params![date, user_id, chat_id, id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record(
        &mut self,
        user_id: i64,
        chat_id: i64,
        ids: impl IntoIterator<Item = i64>,
        status: DeliveryStatus,
    ) -> Result<()> {
        let dates: Vec<_> = ids.into_iter().map(|id| (id, None)).collect();
        self.record_dated(user_id, chat_id, &dates, status)
    }

    pub fn record_dated(
        &mut self,
        user_id: i64,
        chat_id: i64,
        messages: &[(i64, Option<i64>)],
        status: DeliveryStatus,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        for &(id, date) in messages {
            tx.execute("INSERT INTO messages(user_id,chat_id,message_id,status,date) VALUES(?,?,?,?,?)
                ON CONFLICT(user_id,chat_id,message_id) DO UPDATE SET status=excluded.status,date=COALESCE(excluded.date,messages.date)",
                params![user_id,chat_id,id,status_text(status),date])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn history_for_chat(&self, user_id: i64, chat_id: i64) -> Result<Vec<DeliveredMessage>> {
        let mut stmt=self.db.prepare("SELECT chat_id,message_id,status,date,removed,clear_blocked FROM messages WHERE user_id=? AND chat_id=? ORDER BY message_id")?;
        Ok(stmt
            .query_map(params![user_id, chat_id], |r| {
                Ok(DeliveredMessage {
                    chat_id: r.get(0)?,
                    message_id: r.get(1)?,
                    status: if r.get::<_, String>(2)? == "sent" {
                        DeliveryStatus::Sent
                    } else {
                        DeliveryStatus::Failed
                    },
                    date: r.get(3)?,
                    removed: r.get(4)?,
                    clear_blocked: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn list_for_chat(&self, user_id: i64, chat_id: i64) -> Result<Vec<DeliveredMessage>> {
        Ok(self
            .history_for_chat(user_id, chat_id)?
            .into_iter()
            .filter(|m| !m.removed)
            .collect())
    }
    pub fn list_sent_for_chat(&self, user_id: i64, chat_id: i64) -> Result<Vec<DeliveredMessage>> {
        Ok(self
            .list_for_chat(user_id, chat_id)?
            .into_iter()
            .filter(|m| m.status == DeliveryStatus::Sent)
            .collect())
    }
    /// Retain the outcome and timestamp permanently, including removed failures.
    pub fn remove(&mut self, user_id: i64, messages: &[DeliveredMessage]) -> Result<()> {
        let tx = self.db.transaction()?;
        for m in messages {
            tx.execute("UPDATE messages SET removed=1,clear_blocked=0 WHERE user_id=? AND chat_id=? AND message_id=?",params![user_id,m.chat_id,m.message_id])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn mark_clear_blocked(&mut self, user_id: i64, m: DeliveredMessage) -> Result<()> {
        self.db.execute(
            "UPDATE messages SET clear_blocked=1 WHERE user_id=? AND chat_id=? AND message_id=?",
            params![user_id, m.chat_id, m.message_id],
        )?;
        Ok(())
    }
}
fn status_text(status: DeliveryStatus) -> &'static str {
    match status {
        DeliveryStatus::Sent => "sent",
        DeliveryStatus::Failed => "failed",
    }
}
pub fn delivered_path(state_dir: &Path) -> PathBuf {
    state_dir.join("delivered.sqlite3")
}

#[derive(Debug, PartialEq, Eq)]
pub struct SentReport {
    pub latest_failure: Option<DeliveredMessage>,
    pub boundary: Option<DeliveredMessage>,
    pub remaining: bool,
}

/// A removed failure no longer blocks earlier successful history. Removed successes
/// still supply the date when the entire successful tail has been cleared.
pub fn sent_report(history: &[DeliveredMessage]) -> SentReport {
    let latest_failure = history
        .iter()
        .filter(|m| !m.removed && m.status == DeliveryStatus::Failed)
        .max_by_key(|m| m.message_id)
        .copied();
    let after = latest_failure.map(|m| m.message_id).unwrap_or(i64::MIN);
    let sent = || {
        history
            .iter()
            .filter(|m| m.message_id > after && m.status == DeliveryStatus::Sent)
    };
    let present = sent()
        .filter(|m| !m.removed)
        .min_by_key(|m| m.message_id)
        .copied();
    SentReport {
        latest_failure,
        boundary: present.or_else(|| sent().min_by_key(|m| m.message_id).copied()),
        remaining: present.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn history() -> Delivered {
        Delivered {
            db: Connection::open_in_memory().unwrap(),
        }
    }
    fn setup() -> Delivered {
        let d = history();
        d.db.execute_batch("CREATE TABLE messages(user_id INTEGER,chat_id INTEGER,message_id INTEGER,status TEXT,date INTEGER,removed INTEGER DEFAULT 0,clear_blocked INTEGER DEFAULT 0,PRIMARY KEY(user_id,chat_id,message_id));").unwrap();
        d
    }
    fn album(ids: &[i64]) -> crate::mapping::IncomingItem {
        crate::mapping::IncomingItem {
            ingest_key: None,
            update_ids: ids.iter().map(|id| id + 1000).collect(),
            chat_id: 1,
            user_id: 1,
            message_id: ids[0],
            message_ids: ids.to_vec(),
            message_dates: ids.iter().map(|&id| (id, Some(id * 100))).collect(),
            media_group_id: Some("group".into()),
            body: "caption".into(),
            files: ids
                .iter()
                .map(|id| crate::mapping::IncomingFile {
                    file_id: format!("file{id}"),
                    filename: "photo.jpg".into(),
                    mime_type: "image/jpeg".into(),
                    size: Some(10),
                })
                .collect(),
            too_large: None,
        }
    }
    fn batch_database(name: &str) -> (Delivered, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("scratchbot-batches-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("delivered.sqlite3");
        (Delivered::load(&path).unwrap(), path)
    }
    #[test]
    fn late_album_parts_get_distinct_keys_and_completed_parts_are_not_reuploaded() {
        let (mut d, path) = batch_database("late");
        let first = d.prepare_batches(&album(&[1])).unwrap();
        d.record_dated(1, 1, &first[0].message_dates, DeliveryStatus::Sent)
            .unwrap();
        let late = d.prepare_batches(&album(&[2, 3])).unwrap();
        assert_ne!(first[0].idempotency_key(), late[0].idempotency_key());
        assert_eq!(late[0].files.len(), 2);
        d.record_dated(1, 1, &late[0].message_dates, DeliveryStatus::Sent)
            .unwrap();
        assert!(d.prepare_batches(&album(&[1, 2, 3])).unwrap().is_empty());
        drop(d);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn restart_reconstructs_the_entire_reserved_album_even_from_one_replayed_member() {
        let (mut d, path) = batch_database("restart");
        let reserved = d.prepare_batches(&album(&[1, 2])).unwrap();
        let key = reserved[0].idempotency_key();
        drop(d);
        let mut d = Delivered::load(&path).unwrap();
        let replay = d.prepare_batches(&album(&[1])).unwrap();
        assert_eq!(replay[0].idempotency_key(), key);
        assert_eq!(replay[0].message_ids, [1, 2]);
        assert_eq!(replay[0].files.len(), 2);
        d.record_dated(1, 1, &replay[0].message_dates, DeliveryStatus::Sent)
            .unwrap();
        assert!(d.prepare_batches(&album(&[2])).unwrap().is_empty());
        drop(d);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn crash_before_ack_and_overlapping_delivery_cannot_discard_new_album_parts() {
        let (mut d, path) = batch_database("overlap");
        let original = d.prepare_batches(&album(&[1])).unwrap()[0].idempotency_key();
        drop(d);
        let mut d = Delivered::load(&path).unwrap();
        let replay = d.prepare_batches(&album(&[1, 2, 3])).unwrap();
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].idempotency_key(), original);
        assert_eq!(replay[0].message_ids, [1]);
        assert_eq!(replay[1].message_ids, [2, 3]);
        assert_ne!(replay[0].idempotency_key(), replay[1].idempotency_key());
        drop(d);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn deletion_keeps_dates_and_outcomes_and_retries_do_not_resurrect() {
        let mut d = setup();
        d.record_dated(
            1,
            1,
            &[(1, Some(123)), (2, Some(124))],
            DeliveryStatus::Sent,
        )
        .unwrap();
        let m = d.list_for_chat(1, 1).unwrap();
        d.remove(1, &m).unwrap();
        assert!(d.list_for_chat(1, 1).unwrap().is_empty());
        assert_eq!(d.date(1, 1, 1), Some(123));
        d.record(1, 1, [1], DeliveryStatus::Sent).unwrap();
        assert!(d.list_for_chat(1, 1).unwrap().is_empty());
        let report = sent_report(&d.history_for_chat(1, 1).unwrap());
        assert!(!report.remaining);
        assert_eq!(report.boundary.unwrap().message_id, 1);
    }
    #[test]
    fn removing_failures_moves_boundary_back_until_empty() {
        let mut d = setup();
        d.record_dated(
            1,
            1,
            &[(1, Some(10)), (3, Some(30)), (5, Some(50))],
            DeliveryStatus::Sent,
        )
        .unwrap();
        d.record_dated(
            1,
            1,
            &[(2, Some(20)), (4, Some(40))],
            DeliveryStatus::Failed,
        )
        .unwrap();
        let report = sent_report(&d.history_for_chat(1, 1).unwrap());
        assert_eq!(report.boundary.unwrap().message_id, 5);
        assert_eq!(report.latest_failure.unwrap().message_id, 4);
        d.remove(1, &[report.latest_failure.unwrap()]).unwrap();
        let report = sent_report(&d.history_for_chat(1, 1).unwrap());
        assert_eq!(report.boundary.unwrap().message_id, 3);
        assert_eq!(report.latest_failure.unwrap().message_id, 2);
        d.remove(1, &[report.latest_failure.unwrap()]).unwrap();
        let report = sent_report(&d.history_for_chat(1, 1).unwrap());
        assert_eq!(report.boundary.unwrap().message_id, 1);
        assert!(report.latest_failure.is_none());
    }
    #[test]
    fn users_chats_and_successful_retry_are_isolated() {
        let mut d = setup();
        d.record(1, 1, [1], DeliveryStatus::Failed).unwrap();
        d.record(1, 1, [1], DeliveryStatus::Sent).unwrap();
        d.record(2, 1, [1], DeliveryStatus::Failed).unwrap();
        d.record(1, 2, [1], DeliveryStatus::Failed).unwrap();
        assert!(
            sent_report(&d.history_for_chat(1, 1).unwrap())
                .latest_failure
                .is_none()
        );
        assert!(
            sent_report(&d.history_for_chat(2, 1).unwrap())
                .latest_failure
                .is_some()
        );
        assert!(
            sent_report(&d.history_for_chat(1, 2).unwrap())
                .latest_failure
                .is_some()
        );
    }
    #[test]
    fn legacy_json_import_is_idempotent_and_preserves_known_dates() {
        let dir = std::env::temp_dir().join(format!("scratchbot-migration-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("delivered.sqlite3");
        fs::write(
            path.with_extension("json"),
            r#"{"messages":{"1":[{"chat_id":1,"message_id":3}]},"dates":{"1:1:3":123}}"#,
        )
        .unwrap();
        let mut d = Delivered::load(&path).unwrap();
        assert_eq!(d.date(1, 1, 3), Some(123));
        let m = d.list_for_chat(1, 1).unwrap();
        d.remove(1, &m).unwrap();
        drop(d);
        let d = Delivered::load(&path).unwrap();
        assert!(d.list_for_chat(1, 1).unwrap().is_empty());
        assert_eq!(d.history_for_chat(1, 1).unwrap().len(), 1);
        drop(d);
        fs::remove_dir_all(dir).unwrap();
    }
}
