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
            CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY);",
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
