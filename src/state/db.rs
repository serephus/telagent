use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use redb::{ConcurrencyMode, Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const KV: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");
const BINDINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("bindings");
const PAIRING: TableDefinition<&str, &[u8]> = TableDefinition::new("pairing_codes");
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
const TELEGRAM_MESSAGES: TableDefinition<&str, &[u8]> = TableDefinition::new("telegram_messages");
const PENDING: TableDefinition<&str, &[u8]> = TableDefinition::new("pending_requests");
const PROCESSED: TableDefinition<i64, ()> = TableDefinition::new("processed_updates");
const OUTBOX: TableDefinition<i64, &[u8]> = TableDefinition::new("outbox");

const SEPARATOR: char = '\u{1f}';

#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub instance_id: String,
    pub session_id: String,
    pub title: String,
    pub directory: String,
    pub project_id: Option<String>,
    pub parent_id: Option<String>,
    pub state: String,
    pub last_assistant_message_id: Option<String>,
    pub last_notified_message_id: Option<String>,
    pub muted: bool,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    pub request_id: String,
    pub kind: String,
    pub session_id: String,
    pub chat_id: Option<i64>,
    pub message_id: Option<i32>,
    pub payload: Option<String>,
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxItem {
    pub id: i64,
    pub session_id: String,
    pub text: String,
    pub attempts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BindingRecord {
    chat_id: i64,
    user_id: i64,
    created_at: i64,
    revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairingRecord {
    instance_id: String,
    expires_at: i64,
    used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramMessageRecord {
    session_id: String,
    kind: String,
    created_at: i64,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }

        let mut builder = Database::builder();
        builder.set_concurrency_mode(ConcurrencyMode::MultiWriter);
        let db = builder
            .create(path)
            .with_context(|| format!("failed to open redb database {}", path.display()))?;
        initialize(&db)?;

        Ok(Self { db: Arc::new(db) })
    }

    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Database) -> Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .context("redb task panicked")?
    }

    pub async fn get_or_create_instance_id(&self, configured: Option<&str>) -> Result<String> {
        if let Some(value) = configured {
            return Ok(value.to_string());
        }

        self.run(move |db| {
            let write = db.begin_write()?;
            let mut table = write.open_table(KV)?;
            let existing = table
                .get("instance_id")?
                .map(|value| value.value().to_vec());

            let id = match existing {
                Some(bytes) => {
                    String::from_utf8(bytes).context("instance_id is not valid UTF-8")?
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    table.insert("instance_id", id.as_bytes())?;
                    id
                }
            };

            drop(table);
            write.commit()?;
            Ok(id)
        })
        .await
    }

    pub async fn is_bound(&self, chat_id: i64) -> Result<bool> {
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(BINDINGS)?;
            for entry in table.iter()? {
                let (_, value) = entry?;
                let record: BindingRecord = decode(value.value())?;
                if record.chat_id == chat_id && record.revoked_at.is_none() {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
    }

    pub async fn bind(&self, instance_id: &str, chat_id: i64, user_id: i64) -> Result<()> {
        let instance_id = instance_id.to_string();
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(BINDINGS)?;
                let record = BindingRecord {
                    chat_id,
                    user_id,
                    created_at: now(),
                    revoked_at: None,
                };
                let key = binding_key(&instance_id, chat_id);
                let bytes = encode(&record)?;
                table.insert(key.as_str(), bytes.as_slice())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn unbind(&self, chat_id: i64) -> Result<()> {
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(BINDINGS)?;
                let keys = table
                    .iter()?
                    .filter_map(|entry| entry.ok().map(|(key, _)| key.value().to_string()))
                    .collect::<Vec<_>>();
                for key in keys {
                    let record = table
                        .get(key.as_str())?
                        .map(|value| decode::<BindingRecord>(value.value()))
                        .transpose()?;
                    let Some(mut record) = record else {
                        continue;
                    };
                    if record.chat_id == chat_id && record.revoked_at.is_none() {
                        record.revoked_at = Some(now());
                        let bytes = encode(&record)?;
                        table.insert(key.as_str(), bytes.as_slice())?;
                    }
                }
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn bound_chats(&self, instance_id: &str) -> Result<Vec<i64>> {
        let instance_id = instance_id.to_string();
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(BINDINGS)?;
            let prefix = format!("{instance_id}{SEPARATOR}");
            let mut chats = Vec::new();
            for entry in table.iter()? {
                let (key, value) = entry?;
                if !key.value().starts_with(&prefix) {
                    continue;
                }
                let record: BindingRecord = decode(value.value())?;
                if record.revoked_at.is_none() {
                    chats.push(record.chat_id);
                }
            }
            Ok(chats)
        })
        .await
    }

    pub async fn create_pairing_code(
        &self,
        instance_id: &str,
        code_hash: &str,
        expires_at: i64,
    ) -> Result<()> {
        let instance_id = instance_id.to_string();
        let code_hash = code_hash.to_string();
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(PAIRING)?;
                let record = PairingRecord {
                    instance_id,
                    expires_at,
                    used_at: None,
                };
                let bytes = encode(&record)?;
                table.insert(code_hash.as_str(), bytes.as_slice())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn consume_pairing_code(&self, code_hash: &str) -> Result<Option<String>> {
        let code_hash = code_hash.to_string();
        self.run(move |db| {
            let now = now();
            let write = db.begin_write()?;
            let mut table = write.open_table(PAIRING)?;
            let Some(value) = table.get(code_hash.as_str())? else {
                drop(table);
                write.commit()?;
                return Ok(None);
            };
            let mut record: PairingRecord = decode(value.value())?;
            drop(value);

            if record.used_at.is_some() || record.expires_at <= now {
                drop(table);
                write.commit()?;
                return Ok(None);
            }

            record.used_at = Some(now);
            let instance_id = record.instance_id.clone();
            let bytes = encode(&record)?;
            table.insert(code_hash.as_str(), bytes.as_slice())?;
            drop(table);
            write.commit()?;
            Ok(Some(instance_id))
        })
        .await
    }

    pub async fn upsert_session(&self, session: &SessionRecord) -> Result<()> {
        let mut session = session.clone();
        session.updated_at = now();
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(SESSIONS)?;
                let key = session_key(&session.instance_id, &session.session_id);
                let bytes = encode(&session)?;
                table.insert(key.as_str(), bytes.as_slice())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn get_session(
        &self,
        instance_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionRecord>> {
        let key = session_key(instance_id, session_id);
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(SESSIONS)?;
            match table.get(key.as_str())? {
                Some(value) => Ok(Some(decode(value.value())?)),
                None => Ok(None),
            }
        })
        .await
    }

    pub async fn list_sessions(
        &self,
        instance_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionRecord>> {
        let instance_id = instance_id.to_string();
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(SESSIONS)?;
            let prefix = format!("{instance_id}{SEPARATOR}");
            let mut sessions = Vec::new();
            for entry in table.iter()? {
                let (key, value) = entry?;
                if key.value().starts_with(&prefix) {
                    sessions.push(decode::<SessionRecord>(value.value())?);
                }
            }
            sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
            sessions.truncate(limit);
            Ok(sessions)
        })
        .await
    }

    pub async fn update_state(&self, session_id: &str, state: &str) -> Result<()> {
        let session_id = session_id.to_string();
        let state = state.to_string();
        self.run(move |db| {
            self::update_session(db, &session_id, |session| {
                session.state = state.clone();
            })
        })
        .await
    }

    pub async fn set_last_assistant_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<()> {
        let session_id = session_id.to_string();
        let message_id = message_id.to_string();
        self.run(move |db| {
            self::update_session(db, &session_id, |session| {
                session.last_assistant_message_id = Some(message_id.clone());
            })
        })
        .await
    }

    pub async fn set_last_notified_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<()> {
        let session_id = session_id.to_string();
        let message_id = message_id.to_string();
        self.run(move |db| {
            self::update_session(db, &session_id, |session| {
                session.last_notified_message_id = Some(message_id.clone());
            })
        })
        .await
    }

    pub async fn set_muted(&self, session_id: &str, muted: bool) -> Result<()> {
        let session_id = session_id.to_string();
        self.run(move |db| {
            self::update_session(db, &session_id, |session| {
                session.muted = muted;
            })
        })
        .await
    }

    pub async fn set_active_session(&self, chat_id: i64, session_id: &str) -> Result<()> {
        let key = active_session_key(chat_id);
        let session_id = session_id.to_string();
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(KV)?;
                table.insert(key.as_str(), session_id.as_bytes())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn active_session(&self, chat_id: i64) -> Result<Option<String>> {
        let key = active_session_key(chat_id);
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(KV)?;
            match table.get(key.as_str())? {
                Some(value) => Ok(Some(
                    String::from_utf8(value.value().to_vec())
                        .context("active session is not valid UTF-8")?,
                )),
                None => Ok(None),
            }
        })
        .await
    }

    pub async fn clear_active_session(&self, chat_id: i64) -> Result<()> {
        let key = active_session_key(chat_id);
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(KV)?;
                table.remove(key.as_str())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn pending_request_count(&self, chat_id: i64) -> Result<usize> {
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(PENDING)?;
            let mut count = 0;
            for entry in table.iter()? {
                let (_, value) = entry?;
                let record: PendingRequest = decode(value.value())?;
                if record.chat_id == Some(chat_id) {
                    count += 1;
                }
            }
            Ok(count)
        })
        .await
    }

    pub async fn store_telegram_message(
        &self,
        chat_id: i64,
        message_id: i32,
        session_id: &str,
        kind: &str,
    ) -> Result<()> {
        let key = telegram_message_key(chat_id, message_id);
        let session_id = session_id.to_string();
        let kind = kind.to_string();
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(TELEGRAM_MESSAGES)?;
                let record = TelegramMessageRecord {
                    session_id,
                    kind,
                    created_at: now(),
                };
                let bytes = encode(&record)?;
                table.insert(key.as_str(), bytes.as_slice())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn session_for_telegram_message(
        &self,
        chat_id: i64,
        message_id: i32,
    ) -> Result<Option<String>> {
        let key = telegram_message_key(chat_id, message_id);
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(TELEGRAM_MESSAGES)?;
            match table.get(key.as_str())? {
                Some(value) => {
                    let record: TelegramMessageRecord = decode(value.value())?;
                    Ok(Some(record.session_id))
                }
                None => Ok(None),
            }
        })
        .await
    }

    pub async fn add_pending_request(
        &self,
        request_id: &str,
        kind: &str,
        session_id: &str,
        chat_id: Option<i64>,
        message_id: Option<i32>,
        payload: Option<&str>,
    ) -> Result<()> {
        let request_id = request_id.to_string();
        let kind = kind.to_string();
        let session_id = session_id.to_string();
        let payload = payload.map(ToString::to_string);
        self.run(move |db| {
            let chat_id = chat_id.unwrap_or(0);
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(PENDING)?;
                let record = PendingRequest {
                    request_id: request_id.clone(),
                    kind,
                    session_id,
                    chat_id: Some(chat_id),
                    message_id,
                    payload,
                    created_at: now(),
                };
                let key = pending_key(&request_id, chat_id);
                let bytes = encode(&record)?;
                table.insert(key.as_str(), bytes.as_slice())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn get_pending_request_for_chat(
        &self,
        request_id: &str,
        chat_id: i64,
    ) -> Result<Option<PendingRequest>> {
        let key = pending_key(request_id, chat_id);
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(PENDING)?;
            match table.get(key.as_str())? {
                Some(value) => Ok(Some(decode(value.value())?)),
                None => Ok(None),
            }
        })
        .await
    }

    pub async fn pending_request_for_message(
        &self,
        chat_id: i64,
        message_id: i32,
    ) -> Result<Option<PendingRequest>> {
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(PENDING)?;
            let mut found: Option<PendingRequest> = None;
            for entry in table.iter()? {
                let (_, value) = entry?;
                let record: PendingRequest = decode(value.value())?;
                if record.chat_id == Some(chat_id)
                    && record.message_id == Some(message_id)
                    && found
                        .as_ref()
                        .map(|current| record.created_at > current.created_at)
                        .unwrap_or(true)
                {
                    found = Some(record);
                }
            }
            Ok(found)
        })
        .await
    }

    pub async fn remove_pending_request(&self, request_id: &str) -> Result<()> {
        let request_id = request_id.to_string();
        self.run(move |db| {
            let prefix = format!("{request_id}{SEPARATOR}");
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(PENDING)?;
                let keys = table
                    .iter()?
                    .filter_map(|entry| entry.ok().map(|(key, _)| key.value().to_string()))
                    .filter(|key| key.starts_with(&prefix))
                    .collect::<Vec<_>>();
                for key in keys {
                    table.remove(key.as_str())?;
                }
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn is_processed_update(&self, update_id: i64) -> Result<bool> {
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(PROCESSED)?;
            Ok(table.get(update_id)?.is_some())
        })
        .await
    }

    pub async fn mark_update(&self, update_id: i64) -> Result<()> {
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(PROCESSED)?;
                table.insert(update_id, ())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn enqueue_outbox(&self, session_id: &str, text: &str) -> Result<()> {
        let session_id = session_id.to_string();
        let text = text.to_string();
        self.run(move |db| {
            let write = db.begin_write()?;
            let id;
            {
                let mut kv = write.open_table(KV)?;
                let next = kv
                    .get("outbox_seq")?
                    .and_then(|value| {
                        std::str::from_utf8(value.value())
                            .ok()
                            .and_then(|value| value.parse::<u64>().ok())
                    })
                    .unwrap_or(0)
                    + 1;
                let next_bytes = next.to_string();
                kv.insert("outbox_seq", next_bytes.as_bytes())?;
                id = next as i64;
            }
            {
                let mut table = write.open_table(OUTBOX)?;
                let item = OutboxItem {
                    id,
                    session_id,
                    text,
                    attempts: 0,
                };
                let bytes = encode(&item)?;
                table.insert(id, bytes.as_slice())?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn list_outbox(&self, limit: usize) -> Result<Vec<OutboxItem>> {
        self.run(move |db| {
            let read = db.begin_read()?;
            let table = read.open_table(OUTBOX)?;
            let mut items = Vec::new();
            for entry in table.iter()? {
                let (_, value) = entry?;
                items.push(decode::<OutboxItem>(value.value())?);
            }
            items.sort_by_key(|item| item.id);
            items.truncate(limit);
            Ok(items)
        })
        .await
    }

    pub async fn remove_outbox(&self, id: i64) -> Result<()> {
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(OUTBOX)?;
                table.remove(id)?;
            }
            write.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn increment_outbox_attempts(&self, id: i64) -> Result<()> {
        self.run(move |db| {
            let write = db.begin_write()?;
            {
                let mut table = write.open_table(OUTBOX)?;
                let item = table
                    .get(id)?
                    .map(|value| decode::<OutboxItem>(value.value()))
                    .transpose()?;
                if let Some(mut item) = item {
                    item.attempts += 1;
                    let bytes = encode(&item)?;
                    table.insert(id, bytes.as_slice())?;
                }
            }
            write.commit()?;
            Ok(())
        })
        .await
    }
}

fn initialize(db: &Database) -> Result<()> {
    let write = db.begin_write()?;
    {
        let _ = write.open_table(KV)?;
        let _ = write.open_table(BINDINGS)?;
        let _ = write.open_table(PAIRING)?;
        let _ = write.open_table(SESSIONS)?;
        let _ = write.open_table(TELEGRAM_MESSAGES)?;
        let _ = write.open_table(PENDING)?;
        let _ = write.open_table(PROCESSED)?;
        let _ = write.open_table(OUTBOX)?;
    }
    write.commit()?;
    Ok(())
}

fn update_session<F>(db: &Database, session_id: &str, mut update: F) -> Result<()>
where
    F: FnMut(&mut SessionRecord),
{
    let write = db.begin_write()?;
    {
        let mut table = write.open_table(SESSIONS)?;
        let keys = table
            .iter()?
            .filter_map(|entry| entry.ok().map(|(key, _)| key.value().to_string()))
            .filter(|key| key.ends_with(&format!("{SEPARATOR}{session_id}")))
            .collect::<Vec<_>>();
        for key in keys {
            let Some(value) = table.get(key.as_str())? else {
                continue;
            };
            let mut session: SessionRecord = decode(value.value())?;
            drop(value);
            update(&mut session);
            session.updated_at = now();
            let bytes = encode(&session)?;
            table.insert(key.as_str(), bytes.as_slice())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn binding_key(instance_id: &str, chat_id: i64) -> String {
    format!("{instance_id}{SEPARATOR}{chat_id}")
}

fn session_key(instance_id: &str, session_id: &str) -> String {
    format!("{instance_id}{SEPARATOR}{session_id}")
}

fn telegram_message_key(chat_id: i64, message_id: i32) -> String {
    format!("{chat_id}{SEPARATOR}{message_id}")
}

fn pending_key(request_id: &str, chat_id: i64) -> String {
    format!("{request_id}{SEPARATOR}{chat_id}")
}

fn active_session_key(chat_id: i64) -> String {
    format!("active_session:{chat_id}")
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(bytes)?)
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.redb")).unwrap();
        (dir, db)
    }

    fn session(instance_id: &str, session_id: &str) -> SessionRecord {
        SessionRecord {
            instance_id: instance_id.to_string(),
            session_id: session_id.to_string(),
            title: "Title".to_string(),
            directory: "/tmp".to_string(),
            project_id: None,
            parent_id: None,
            state: "busy".to_string(),
            last_assistant_message_id: None,
            last_notified_message_id: None,
            muted: false,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn binds_and_consumes_pairing_codes() {
        let (_dir, db) = temp_db().await;
        let instance = db.get_or_create_instance_id(None).await.unwrap();
        db.create_pairing_code(&instance, "hash", now() + 60)
            .await
            .unwrap();

        let consumed = db.consume_pairing_code("hash").await.unwrap();
        assert_eq!(consumed.as_deref(), Some(instance.as_str()));
        assert!(db.consume_pairing_code("hash").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_roundtrip() {
        let (_dir, db) = temp_db().await;
        db.upsert_session(&session("i", "ses_1")).await.unwrap();
        let loaded = db.get_session("i", "ses_1").await.unwrap().unwrap();
        assert_eq!(loaded.title, "Title");
        assert_eq!(loaded.state, "busy");
    }

    #[tokio::test]
    async fn updates_session_by_id() {
        let (_dir, db) = temp_db().await;
        db.upsert_session(&session("i", "ses_1")).await.unwrap();
        db.update_state("ses_1", "idle").await.unwrap();
        db.set_muted("ses_1", true).await.unwrap();
        let loaded = db.get_session("i", "ses_1").await.unwrap().unwrap();
        assert_eq!(loaded.state, "idle");
        assert!(loaded.muted);
    }

    #[tokio::test]
    async fn outbox_is_processed_in_order() {
        let (_dir, db) = temp_db().await;
        db.enqueue_outbox("ses_1", "one").await.unwrap();
        db.enqueue_outbox("ses_1", "two").await.unwrap();
        let items = db.list_outbox(10).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "one");
        assert_eq!(items[1].text, "two");
        db.increment_outbox_attempts(items[0].id).await.unwrap();
        let items = db.list_outbox(10).await.unwrap();
        assert_eq!(items[0].attempts, 1);
        db.remove_outbox(items[0].id).await.unwrap();
        assert_eq!(db.list_outbox(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pending_requests_are_scoped_per_chat() {
        let (_dir, db) = temp_db().await;
        db.add_pending_request("per_1", "permission", "ses_1", Some(1), Some(10), None)
            .await
            .unwrap();
        db.add_pending_request("per_1", "permission", "ses_1", Some(2), Some(20), None)
            .await
            .unwrap();

        assert!(
            db.get_pending_request_for_chat("per_1", 1)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_pending_request_for_chat("per_1", 2)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_pending_request_for_chat("per_1", 3)
                .await
                .unwrap()
                .is_none()
        );

        let by_message = db.pending_request_for_message(2, 20).await.unwrap();
        assert_eq!(by_message.unwrap().request_id, "per_1");

        db.remove_pending_request("per_1").await.unwrap();
        assert!(
            db.get_pending_request_for_chat("per_1", 1)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_pending_request_for_chat("per_1", 2)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn multiple_handles_can_open_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let first = Db::open(&path).unwrap();
        let second = Db::open(&path).unwrap();

        first.bind("i", 42, 7).await.unwrap();
        assert!(second.is_bound(42).await.unwrap());

        second.unbind(42).await.unwrap();
        assert!(!first.is_bound(42).await.unwrap());
    }
}
