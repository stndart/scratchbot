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

    pub async fn send_report(
        &self,
        chat_id: i64,
        report: &scratchwall_telegram::delivered::SentReport,
        reply_to: Option<i64>,
    ) -> Result<ReportOutcome, HandleError> {
        let (text, entities) = report_text(report);
        let mut body = serde_json::json!({"chat_id":chat_id,"text":text,"entities":entities});
        if let Some(message_id) = reply_to {
            body["reply_parameters"] =
                serde_json::json!({"message_id":message_id,"allow_sending_without_reply":false});
        }
        let response = self
            .http
            .post(format!("{}/sendMessage", self.api_base))
            .json(&body)
            .send()
            .await
            .map_err(HandleError::transient)?;
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(HandleError::Transient(format!(
                "telegram sendMessage {status}"
            )));
        }
        let payload: ApiResponse<serde_json::Value> =
            response.json().await.map_err(HandleError::transient)?;
        if !payload.ok {
            let description = payload
                .description
                .unwrap_or_else(|| format!("telegram sendMessage {status}"));
            if is_missing_reply(&description) {
                return Ok(ReportOutcome::Missing);
            }
            return Err(HandleError::Permanent(description));
        }
        let message = payload.result.unwrap_or_default();
        let message_id = message
            .get("message_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                HandleError::Permanent("Telegram returned a report without an ID".into())
            })?;
        let returned_target = message
            .pointer("/reply_to_message/message_id")
            .and_then(|v| v.as_i64());
        let date = if returned_target == reply_to {
            message
                .pointer("/reply_to_message/date")
                .and_then(|v| v.as_i64())
        } else {
            None
        };
        tracing::info!(
            chat_id,
            ?reply_to,
            ?returned_target,
            ?date,
            "Telegram saved-report reply result"
        );
        // A useful dated report stays visible even if Telegram omits the shortcut.
        Ok(ReportOutcome::Sent { message_id, date })
    }

    pub async fn edit_report(
        &self,
        chat_id: i64,
        message_id: i64,
        report: &scratchwall_telegram::delivered::SentReport,
    ) -> Result<(), HandleError> {
        let (text, entities) = report_text(report);
        let response=self.http.post(format!("{}/editMessageText",self.api_base)).json(&serde_json::json!({"chat_id":chat_id,"message_id":message_id,"text":text,"entities":entities})).send().await.map_err(HandleError::transient)?;
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(HandleError::Transient(format!(
                "telegram editMessageText {status}"
            )));
        }
        let payload: ApiResponse<serde_json::Value> =
            response.json().await.map_err(HandleError::transient)?;
        if payload.ok
            || payload
                .description
                .as_deref()
                .is_some_and(|s| s.contains("not modified"))
        {
            Ok(())
        } else {
            Err(HandleError::Permanent(payload.description.unwrap_or_else(
                || "Telegram could not update the report".into(),
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

    /// Validate the source through Telegram's forwarding RPC. Permission to edit
    /// and cached reply objects do not establish that a source can still be used.
    /// The silent test copy is immediately removed; never touch the source.
    pub async fn probe_message(
        &self,
        chat_id: i64,
        message_id: i64,
    ) -> Result<MessagePresence, HandleError> {
        let response=self.http.post(format!("{}/forwardMessage",self.api_base)).json(&serde_json::json!({
            "chat_id":chat_id,"from_chat_id":chat_id,"message_id":message_id,"disable_notification":true,
        })).send().await.map_err(HandleError::transient)?;
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(HandleError::Transient(format!(
                "telegram forwardMessage {status}"
            )));
        }
        let payload: ApiResponse<serde_json::Value> =
            response.json().await.map_err(HandleError::transient)?;
        if !payload.ok {
            let description = payload
                .description
                .unwrap_or_else(|| format!("telegram forwardMessage {status}"));
            return match classify_forward_probe(&description) {
                Some(presence) => Ok(presence),
                None => Err(HandleError::Permanent(description)),
            };
        }
        let copy_id = payload
            .result
            .as_ref()
            .and_then(|m| m.get("message_id"))
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                HandleError::Permanent("Telegram returned a test copy without an ID".into())
            })?;
        if copy_id == message_id {
            return Err(HandleError::Permanent(
                "Telegram verification returned the source ID; refusing to delete it".into(),
            ));
        }
        match self.delete_message(chat_id, copy_id).await? {
            DeleteOutcome::Deleted | DeleteOutcome::Gone => Ok(MessagePresence::Present),
            DeleteOutcome::Forbidden => Err(HandleError::Permanent(
                "Telegram would not remove the verification copy".into(),
            )),
        }
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
pub enum ReportOutcome {
    Sent { message_id: i64, date: Option<i64> },
    Missing,
}

fn report_text(
    report: &scratchwall_telegram::delivered::SentReport,
) -> (String, serde_json::Value) {
    let mut text = String::new();
    let mut entities = Vec::new();
    let mut date = |text: &mut String,
                    message: scratchwall_telegram::delivered::DeliveredMessage| {
        match message.date {
            Some(timestamp) => {
                let offset = text.encode_utf16().count();
                text.push_str("this date");
                entities.push(serde_json::json!({"type":"date_time","offset":offset,"length":9,"unix_time":timestamp,"date_time_format":"dT"}));
            }
            None => text.push_str("an unknown date (older history has no timestamp)"),
        }
    };
    if let Some(boundary) = report.boundary {
        let same_second = boundary.date.is_some()
            && report
                .latest_failure
                .is_some_and(|f| f.date == boundary.date);
        if same_second {
            text.push_str("All remaining forwards after the latest failure through this command are saved to Scratchwall. First saved forward: ");
            date(&mut text, boundary);
            text.push('.');
        } else {
            text.push_str("All remaining forwards since ");
            date(&mut text, boundary);
            text.push_str(" through this command are saved to Scratchwall.");
        }
        if report.remaining {
            if boundary.clear_blocked {
                text.push_str(" /clear could not delete the remaining Telegram copies (48-hour limit or protected messages).");
            } else {
                text.push_str(
                    " Telegram copies remain. /clear cannot delete copies older than 48 hours.",
                );
            }
        }
    }
    if !report.remaining {
        if let Some(failure) = report.latest_failure {
            if !text.is_empty() {
                text.push('\n');
            }
            if report.boundary.is_none() {
                text.push_str("No saved forwards follow the latest failure. ");
            }
            text.push_str("Latest forward failure: ");
            date(&mut text, failure);
            text.push_str(
                ". Delete that failed forward for both participants, then run /show-sent again.",
            );
        } else if report.boundary.is_some() {
            text.push_str(" No forward failures remain.");
        } else {
            text.push_str("No recorded forwards remain in this chat.");
        }
    }
    (text, serde_json::Value::Array(entities))
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

pub fn classify_forward_probe(description: &str) -> Option<MessagePresence> {
    let description = description.to_ascii_lowercase();
    if description.contains("message_id_invalid")
        || description.contains("message to forward not found")
        || description.contains("message not found")
    {
        Some(MessagePresence::Gone)
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
    fn report_dates_are_exact_and_equal_seconds_do_not_include_the_failure() {
        use scratchwall_telegram::delivered::{DeliveredMessage, DeliveryStatus, SentReport};
        let failure = DeliveredMessage {
            chat_id: 1,
            message_id: 2,
            status: DeliveryStatus::Failed,
            date: Some(100),
            removed: false,
            clear_blocked: false,
        };
        let boundary = DeliveredMessage {
            message_id: 3,
            status: DeliveryStatus::Sent,
            removed: true,
            ..failure
        };
        let (text, entities) = report_text(&SentReport {
            latest_failure: Some(failure),
            boundary: Some(boundary),
            remaining: false,
        });
        assert!(text.contains("after the latest failure"));
        for entity in entities.as_array().unwrap() {
            let offset = entity["offset"].as_u64().unwrap() as usize;
            let len = entity["length"].as_u64().unwrap() as usize;
            let units: Vec<_> = text.encode_utf16().collect();
            assert_eq!(
                String::from_utf16(&units[offset..offset + len]).unwrap(),
                "this date"
            );
            assert_eq!(entity["unix_time"], 100);
        }
        let (text, _) = report_text(&SentReport {
            latest_failure: Some(failure),
            boundary: None,
            remaining: false,
        });
        assert!(text.contains("No saved forwards follow"));
        let (text, entities) = report_text(&SentReport {
            latest_failure: None,
            boundary: None,
            remaining: false,
        });
        assert!(text.contains("No recorded forwards remain"));
        assert!(entities.as_array().unwrap().is_empty());
    }

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
    fn forward_probe_does_not_mistake_permissions_for_presence() {
        assert_eq!(
            classify_forward_probe("Bad Request: message to forward not found"),
            Some(MessagePresence::Gone)
        );
        assert_eq!(
            classify_forward_probe("Bad Request: MESSAGE_ID_INVALID"),
            Some(MessagePresence::Gone)
        );
        assert_eq!(
            classify_forward_probe("Bad Request: message can't be forwarded"),
            None
        );
        assert_eq!(classify_forward_probe("Bad Request: chat not found"), None);
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
