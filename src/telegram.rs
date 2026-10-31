use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    token: String,
    api_base: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteFile {
    file_path: Option<String>,
}

impl Telegram {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(45))
                .build()?,
            api_base: format!("https://api.telegram.org/bot{token}"),
            token,
        })
    }

    #[cfg(test)]
    pub fn with_test_api(api_base: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            token: String::new(),
            api_base,
        }
    }

    pub async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> Result<Vec<scratchwall_telegram::mapping::Update>, HandleError> {
        let url = format!("https://api.telegram.org/bot{}/getUpdates", self.token);
        let response = self
            .http
            .get(url)
            .query(&[
                ("offset", offset.to_string()),
                ("timeout", timeout_secs.to_string()),
                ("allowed_updates", r#"["message"]"#.into()),
            ])
            .timeout(std::time::Duration::from_secs(timeout_secs + 10))
            .send()
            .await
            .map_err(HandleError::transient)?;
        if response.status().is_server_error() || response.status().as_u16() == 429 {
            return Err(HandleError::Transient(format!(
                "telegram getUpdates {}",
                response.status()
            )));
        }
        let payload: ApiResponse<Vec<scratchwall_telegram::mapping::Update>> =
            response.json().await.map_err(HandleError::transient)?;
        if !payload.ok {
            return Err(HandleError::Transient(
                payload
                    .description
                    .unwrap_or_else(|| "telegram getUpdates failed".into()),
            ));
        }
        Ok(payload.result.unwrap_or_default())
    }

    pub async fn download(&self, file_id: &str) -> Result<Vec<u8>, HandleError> {
        let url = format!("https://api.telegram.org/bot{}/getFile", self.token);
        let response = self
            .http
            .get(url)
            .query(&[("file_id", file_id)])
            .send()
            .await
            .map_err(HandleError::transient)?;
        if !response.status().is_success() {
            return Err(classify_status("telegram getFile", response.status()));
        }
        let payload: ApiResponse<RemoteFile> =
            response.json().await.map_err(HandleError::transient)?;
        if !payload.ok {
            return Err(HandleError::Permanent(
                payload
                    .description
                    .unwrap_or_else(|| "telegram getFile failed".into()),
            ));
        }
        let path = payload
            .result
            .and_then(|file| file.file_path)
            .ok_or_else(|| HandleError::Permanent("telegram file path missing".into()))?;
        let file_url = format!("https://api.telegram.org/file/bot{}/{path}", self.token);
        let bytes = self
            .http
            .get(file_url)
            .send()
            .await
            .map_err(HandleError::transient)?;
        if !bytes.status().is_success() {
            return Err(classify_status("telegram file download", bytes.status()));
        }
        bytes
            .bytes()
            .await
            .map(|value| value.to_vec())
            .map_err(HandleError::transient)
    }

    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), HandleError> {
        self.send_message_quietly(chat_id, text, None, None).await
    }

    pub async fn reply_message(
        &self,
        chat_id: i64,
        reply_to_message_id: i64,
        text: &str,
    ) -> Result<(), HandleError> {
        self.send_message_quietly(chat_id, text, Some(reply_to_message_id), None)
            .await
    }

    pub async fn quote_message(
        &self,
        chat_id: i64,
        reply_to_message_id: i64,
        text: &str,
    ) -> Result<QuoteOutcome, HandleError> {
        match self
            .send_message_opts(chat_id, text, Some(reply_to_message_id), None)
            .await?
        {
            SendOutcome::Sent(message) => {
                if message
                    .pointer("/reply_to_message/message_id")
                    .and_then(|id| id.as_i64())
                    == Some(reply_to_message_id)
                {
                    let message_id = message
                        .get("message_id")
                        .and_then(|id| id.as_i64())
                        .ok_or_else(|| {
                            HandleError::Permanent(
                                "Telegram returned a reply without a message ID".into(),
                            )
                        })?;
                    let date = message
                        .pointer("/reply_to_message/date")
                        .and_then(|v| v.as_i64());
                    tracing::info!(reply_target = reply_to_message_id, reply_message_id = message_id, target_date = ?date, "Telegram confirmed reply target");
                    return Ok(QuoteOutcome::Attached { message_id, date });
                }
                let message_id = message
                    .get("message_id")
                    .and_then(|id| id.as_i64())
                    .ok_or_else(|| {
                        HandleError::Permanent(
                            "Telegram returned a message without an ID or reply".into(),
                        )
                    })?;
                match self.delete_message(chat_id, message_id).await? {
                    DeleteOutcome::Deleted | DeleteOutcome::Gone => Ok(QuoteOutcome::Missing),
                    DeleteOutcome::Forbidden => Err(HandleError::Permanent(
                        "Telegram sent an unquoted message and would not remove it".into(),
                    )),
                }
            }
            SendOutcome::Rejected(description) if is_missing_reply(&description) => {
                Ok(QuoteOutcome::Missing)
            }
            SendOutcome::Rejected(description) => Err(HandleError::Permanent(description)),
        }
    }

    pub async fn edit_sent_boundary(
        &self,
        chat_id: i64,
        message_id: i64,
        date: Option<i64>,
    ) -> Result<(), HandleError> {
        let (text, entities) = boundary_text(date);
        let url = format!("{}/editMessageText", self.api_base);
        let payload: ApiResponse<serde_json::Value> = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "chat_id": chat_id, "message_id": message_id, "text": text, "entities": entities,
            }))
            .send()
            .await
            .map_err(HandleError::transient)?
            .json()
            .await
            .map_err(HandleError::transient)?;
        if payload.ok {
            Ok(())
        } else {
            Err(HandleError::Permanent(payload.description.unwrap_or_else(
                || "Telegram could not update the saved boundary".into(),
            )))
        }
    }

    pub async fn send_sent_boundary(&self, chat_id: i64, date: i64) -> Result<(), HandleError> {
        let (text, entities) = boundary_text(Some(date));
        let url = format!("{}/sendMessage", self.api_base);
        let payload: ApiResponse<serde_json::Value> = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "chat_id": chat_id, "text": text, "entities": entities,
            }))
            .send()
            .await
            .map_err(HandleError::transient)?
            .json()
            .await
            .map_err(HandleError::transient)?;
        if payload.ok {
            Ok(())
        } else {
            Err(HandleError::Permanent(payload.description.unwrap_or_else(
                || "Telegram could not send the saved boundary".into(),
            )))
        }
    }

    pub async fn send_connect(
        &self,
        chat_id: i64,
        text: &str,
        url: &str,
    ) -> Result<(), HandleError> {
        self.send_message_quietly(chat_id, text, None, Some(url))
            .await
    }

    pub async fn send_typing(&self, chat_id: i64) {
        let url = format!("{}/sendChatAction", self.api_base);
        let _ = self
            .http
            .post(url)
            .form(&[
                ("chat_id", chat_id.to_string()),
                ("action", "typing".into()),
            ])
            .send()
            .await;
    }

    async fn send_message_quietly(
        &self,
        chat_id: i64,
        text: &str,
        reply_to_message_id: Option<i64>,
        connect_url: Option<&str>,
    ) -> Result<(), HandleError> {
        match self
            .send_message_opts(chat_id, text, reply_to_message_id, connect_url)
            .await?
        {
            SendOutcome::Sent(_) => Ok(()),
            SendOutcome::Rejected(description) => {
                tracing::warn!(description, "telegram sendMessage failed");
                Ok(())
            }
        }
    }

    async fn send_message_opts(
        &self,
        chat_id: i64,
        text: &str,
        reply_to_message_id: Option<i64>,
        connect_url: Option<&str>,
    ) -> Result<SendOutcome, HandleError> {
        let url = format!("{}/sendMessage", self.api_base);
        let form = message_form(chat_id, text, reply_to_message_id, connect_url);
        let response = self
            .http
            .post(url)
            .form(&form)
            .send()
            .await
            .map_err(HandleError::transient)?;
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(HandleError::Transient(format!(
                "telegram sendMessage {status}"
            )));
        }
        let payload: ApiResponse<serde_json::Value> = match response.json().await {
            Ok(payload) => payload,
            Err(_) => {
                return Ok(SendOutcome::Rejected(format!(
                    "telegram sendMessage {status}"
                )));
            }
        };
        if payload.ok {
            return Ok(SendOutcome::Sent(payload.result.unwrap_or_default()));
        }
        Ok(SendOutcome::Rejected(payload.description.unwrap_or_else(
            || format!("telegram sendMessage {status}"),
        )))
    }

    /// Asks Telegram whether this message is still in the chat.
    /// Editing a user's message is refused when it exists, and reported missing when it was deleted.
    pub async fn probe_message(
        &self,
        chat_id: i64,
        message_id: i64,
    ) -> Result<MessagePresence, HandleError> {
        let url = format!("{}/editMessageReplyMarkup", self.api_base);
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
            }))
            .send()
            .await
            .map_err(HandleError::transient)?;
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(HandleError::Transient(format!(
                "telegram editMessageReplyMarkup {status}"
            )));
        }
        let payload: ApiResponse<serde_json::Value> = match response.json().await {
            Ok(payload) => payload,
            Err(_) => {
                return Err(HandleError::Permanent(format!(
                    "telegram editMessageReplyMarkup {status}"
                )));
            }
        };
        if payload.ok {
            return Ok(MessagePresence::Present);
        }
        let description = payload
            .description
            .unwrap_or_else(|| "telegram editMessageReplyMarkup failed".into());
        classify_edit_probe(&description).ok_or(HandleError::Permanent(description))
    }

    pub async fn delete_message(
        &self,
        chat_id: i64,
        message_id: i64,
    ) -> Result<DeleteOutcome, HandleError> {
        let url = format!("{}/deleteMessage", self.api_base);
        let response = self
            .http
            .post(url)
            .form(&[
                ("chat_id", chat_id.to_string()),
                ("message_id", message_id.to_string()),
            ])
            .send()
            .await
            .map_err(HandleError::transient)?;
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(HandleError::Transient(format!(
                "telegram deleteMessage {status}"
            )));
        }
        let payload: ApiResponse<bool> = response.json().await.map_err(HandleError::transient)?;
        if payload.ok {
            return Ok(DeleteOutcome::Deleted);
        }
        Ok(classify_delete_error(
            payload
                .description
                .as_deref()
                .unwrap_or("telegram deleteMessage failed"),
        ))
    }
}

fn message_form(
    chat_id: i64,
    text: &str,
    reply_to_message_id: Option<i64>,
    connect_url: Option<&str>,
) -> Vec<(String, String)> {
    let mut form = vec![
        ("chat_id".into(), chat_id.to_string()),
        ("text".into(), text.to_owned()),
    ];
    if let Some(reply_to) = reply_to_message_id {
        form.push((
            "reply_parameters".into(),
            serde_json::json!({ "message_id": reply_to }).to_string(),
        ));
    }
    if let Some(connect) = connect_url {
        form.push((
            "reply_markup".into(),
            serde_json::json!({
                "inline_keyboard": [[{"text": "Connect Scratchwall", "url": connect}]]
            })
            .to_string(),
        ));
    }
    form
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SendOutcome {
    Sent(serde_json::Value),
    Rejected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteOutcome {
    Attached { message_id: i64, date: Option<i64> },
    Missing,
}

fn boundary_text(date: Option<i64>) -> (String, serde_json::Value) {
    match date {
        Some(date) => {
            let prefix = "Forwards I recorded after ";
            let placeholder = "this date";
            let text = format!("{prefix}{placeholder} and before this command are saved to Scratchwall. Check other messages and those sent at that exact time before deleting.");
            let entities = serde_json::json!([{ "type": "date_time", "offset": prefix.len(), "length": placeholder.len(), "unix_time": date, "date_time_format": "dT" }]);
            (text, entities)
        }
        None => ("The replied-to forward and later forwards I recorded before this command are saved to Scratchwall. The date is unavailable; check other messages before deleting.".into(), serde_json::json!([])),
    }
}

fn is_missing_reply(description: &str) -> bool {
    let description = description.to_ascii_lowercase();
    description.contains("reply message not found")
        || description.contains("replied message not found")
        || description.contains("message to reply not found")
        || description.contains("message_id_invalid")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePresence {
    Present,
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    Gone,
    Forbidden,
}

pub fn classify_edit_probe(description: &str) -> Option<MessagePresence> {
    let description = description.to_ascii_lowercase();
    if description.contains("message_id_invalid")
        || description.contains("message to edit not found")
        || description.contains("message not found")
    {
        Some(MessagePresence::Gone)
    } else if description.contains("can't be edited") || description.contains("not modified") {
        Some(MessagePresence::Present)
    } else {
        None
    }
}

pub fn classify_delete_error(description: &str) -> DeleteOutcome {
    let description = description.to_ascii_lowercase();
    if description.contains("not found")
        || description.contains("message to delete not found")
        || description.contains("message identifier is not specified")
    {
        DeleteOutcome::Gone
    } else {
        DeleteOutcome::Forbidden
    }
}

#[derive(Debug)]
pub enum HandleError {
    Transient(String),
    Permanent(String),
}

impl HandleError {
    pub fn transient(error: impl std::fmt::Display) -> Self {
        Self::Transient(error.to_string())
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Transient(value) | Self::Permanent(value) => value,
        }
    }
}

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for HandleError {}

pub fn classify_status(op: &str, status: reqwest::StatusCode) -> HandleError {
    let text = format!("{op} {status}");
    if status.is_server_error() || status.as_u16() == 429 {
        HandleError::Transient(text)
    } else {
        HandleError::Permanent(text)
    }
}

pub async fn post_ingest(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    telegram_user_id: i64,
    space: &str,
    item: &scratchwall_telegram::mapping::IncomingItem,
    files: Vec<(String, String, Vec<u8>)>,
) -> Result<(), HandleError> {
    let mut form = reqwest::multipart::Form::new()
        .text("telegramUserId", telegram_user_id.to_string())
        .text("space", space.to_owned())
        .text("body", item.body.clone())
        .text("idempotencyKey", item.idempotency_key());
    for (filename, mime, bytes) in files {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(&mime)
            .context("invalid mime type")
            .map_err(|error| HandleError::Permanent(error.to_string()))?;
        form = form.part("file", part);
    }
    let response = http
        .post(format!("{base_url}/api/v1/ingest"))
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(HandleError::transient)?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let text = if body.is_empty() {
        format!("scratchwall ingest {status}")
    } else {
        format!("scratchwall ingest {status}: {body}")
    };
    Err(classify_status(&text, status).pipe_message(text))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramBindResponse {
    pub url: Option<String>,
    pub display_name: Option<String>,
}

pub async fn request_telegram_bind(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    telegram_user_id: i64,
) -> Result<TelegramBindResponse, HandleError> {
    let response = http
        .post(format!("{base_url}/api/v1/ingest/telegram/bind"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "telegramUserId": telegram_user_id }))
        .send()
        .await
        .map_err(HandleError::transient)?;
    if response.status().is_success() {
        return response.json().await.map_err(HandleError::transient);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let text = if body.is_empty() {
        format!("scratchwall bind {status}")
    } else {
        format!("scratchwall bind {status}: {body}")
    };
    Err(classify_status(&text, status).pipe_message(text))
}

trait PipeMessage {
    fn pipe_message(self, text: String) -> HandleError;
}

impl PipeMessage for HandleError {
    fn pipe_message(self, text: String) -> HandleError {
        match self {
            HandleError::Transient(_) => HandleError::Transient(text),
            HandleError::Permanent(_) => HandleError::Permanent(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_error_form_replies_to_the_source_message() {
        let form = message_form(42, "scratchwall ingest 500: boom", Some(99), None);
        assert_eq!(
            form,
            vec![
                ("chat_id".into(), "42".into()),
                ("text".into(), "scratchwall ingest 500: boom".into()),
                ("reply_parameters".into(), "{\"message_id\":99}".into()),
            ]
        );
        let command = message_form(42, "Saving to memes.", None, None);
        assert!(command.iter().all(|(key, _)| key != "reply_parameters"));
    }

    #[test]
    fn edit_probe_treats_a_refused_edit_as_still_present() {
        assert_eq!(
            classify_edit_probe("Bad Request: message can't be edited"),
            Some(MessagePresence::Present)
        );
        assert_eq!(
            classify_edit_probe("Bad Request: message is not modified"),
            Some(MessagePresence::Present)
        );
        assert_eq!(
            classify_edit_probe("Bad Request: message to edit not found"),
            Some(MessagePresence::Gone)
        );
        assert_eq!(
            classify_edit_probe("Bad Request: MESSAGE_ID_INVALID"),
            Some(MessagePresence::Gone)
        );
        assert_eq!(
            classify_edit_probe("Bad Request: message not found"),
            Some(MessagePresence::Gone)
        );
        assert_eq!(classify_edit_probe("Bad Request: chat not found"), None);
    }

    #[test]
    fn delete_errors_keep_undeletable_messages_out_of_the_success_path() {
        assert_eq!(
            classify_delete_error("Bad Request: message to delete not found"),
            DeleteOutcome::Gone
        );
        assert_eq!(
            classify_delete_error("Bad Request: message can't be deleted"),
            DeleteOutcome::Forbidden
        );
    }
}
