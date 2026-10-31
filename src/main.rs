mod config;
mod service;
mod telegram;
mod update;

use anyhow::Result;
use config::Config;
use scratchwall_telegram::{
    ALBUM_GRACE_MS,
    album::AlbumBuffer,
    delivered::{self, Delivered, DeliveryStatus},
    mapping::{self, Classified, Command},
    offset, spaces,
};
use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use telegram::{
    DeleteOutcome, HandleError, MessagePresence, ReportOutcome, Telegram, post_ingest,
    request_telegram_bind,
};

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => run().await,
        [flag] if flag == "--install" => service::install(),
        [flag] if flag == "--uninstall" => service::uninstall(),
        [flag] if flag == "--ready" => {
            println!("ok");
            Ok(())
        }
        [flag] if flag == "--help" || flag == "-h" => {
            service::print_help();
            Ok(())
        }
        _ => {
            service::print_help();
            anyhow::bail!("invalid arguments");
        }
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "scratchwall_telegram=info".into()),
        )
        .init();
    let config = Config::from_env()?;
    let telegram = Telegram::new(config.telegram_bot_token.clone())?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut offset_value = offset::load(&config.offset_path())?;
    let mut spaces = spaces::SpaceBindings::load(spaces::spaces_path(&config.state_dir))?;
    let mut delivered = Delivered::load(delivered::delivered_path(&config.state_dir))?;
    let mut albums = AlbumBuffer::new();
    let mut processed = HashSet::new();
    let mut pending_commands = BTreeMap::new();
    let self_update = if let Some(update) = config.update.clone() {
        Some(update::serve(update).await?)
    } else {
        tracing::info!("self-update hook disabled");
        None
    };
    match update::exe_sha256() {
        Ok(sha256) => tracing::info!(
            url = %config.scratchwall_url,
            sha256 = %sha256,
            build = update::build_sha(),
            "telegram ingest bot started"
        ),
        Err(error) => tracing::info!(
            url = %config.scratchwall_url,
            error = %error,
            build = update::build_sha(),
            "telegram ingest bot started"
        ),
    }

    loop {
        let now = unix_ms();
        let timeout = poll_timeout(&albums, now);
        let updates = if let Some(gate) = &self_update {
            tokio::select! {
                biased;
                _ = gate.notified() => {
                    if let Err(error) = gate.restart_if_staged() {
                        tracing::error!(error = %error, "self-update failed");
                    }
                    continue;
                }
                result = telegram.get_updates(offset_value, timeout) => result,
            }
        } else {
            telegram.get_updates(offset_value, timeout).await
        };
        let updates = match updates {
            Ok(updates) => updates,
            Err(error) => {
                tracing::warn!(error = %error, "getUpdates failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let seen: Vec<i64> = updates.iter().map(|update| update.update_id).collect();
        for update in &updates {
            if processed.contains(&update.update_id)
                || pending_commands.contains_key(&update.update_id)
            {
                continue;
            }
            match mapping::classify(update, &config.allow_user_ids) {
                Classified::Ignored => {
                    processed.insert(update.update_id);
                }
                Classified::Command {
                    chat_id,
                    user_id,
                    message_id,
                    command,
                } => {
                    if matches!(command, Command::Clear | Command::ShowSent)
                        && (albums.has_pending() || !pending_commands.is_empty())
                    {
                        pending_commands
                            .insert(update.update_id, (chat_id, user_id, message_id, command));
                        continue;
                    }
                    handle_command(
                        &config,
                        &telegram,
                        &http,
                        &mut spaces,
                        &mut delivered,
                        chat_id,
                        user_id,
                        message_id,
                        command,
                    )
                    .await?;
                    processed.insert(update.update_id);
                }
                Classified::Unsupported {
                    chat_id,
                    user_id,
                    message_id,
                    date,
                } => {
                    delivered.record_dated(
                        user_id,
                        chat_id,
                        &[(message_id, date)],
                        DeliveryStatus::Failed,
                    )?;
                    telegram
                        .reply_message(
                            chat_id,
                            message_id,
                            "This message type is not supported and was not saved to Scratchwall.",
                        )
                        .await?;
                    processed.insert(update.update_id);
                }
                Classified::Item(item) => {
                    if let Some(ready) = albums.push(item, unix_ms()) {
                        handle_item(&config, &telegram, &http, &spaces, &mut delivered, ready)
                            .await?;
                        processed.insert(update.update_id);
                    }
                }
            }
        }
        for item in albums.take_ready(unix_ms(), ALBUM_GRACE_MS) {
            let ids = item.update_ids.clone();
            handle_item(&config, &telegram, &http, &spaces, &mut delivered, item).await?;
            processed.extend(ids);
        }
        if !albums.has_pending() {
            for (update_id, (chat_id, user_id, message_id, command)) in
                std::mem::take(&mut pending_commands)
            {
                handle_command(
                    &config,
                    &telegram,
                    &http,
                    &mut spaces,
                    &mut delivered,
                    chat_id,
                    user_id,
                    message_id,
                    command,
                )
                .await?;
                processed.insert(update_id);
            }
        }
        offset_value = offset::next_offset(offset_value, &processed, &seen);
        offset::save(&config.offset_path(), offset_value)?;
        processed.retain(|id| *id >= offset_value.saturating_sub(1));
    }
}

fn poll_timeout(albums: &AlbumBuffer, now_ms: u64) -> u64 {
    if !albums.has_pending() {
        return 30;
    }
    let deadline = albums.next_deadline_ms(ALBUM_GRACE_MS).unwrap_or(now_ms);
    let wait_secs = deadline.saturating_sub(now_ms).div_ceil(1000);
    wait_secs.clamp(1, 30)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    config: &Config,
    telegram: &Telegram,
    http: &reqwest::Client,
    spaces: &mut spaces::SpaceBindings,
    delivered: &mut Delivered,
    chat_id: i64,
    user_id: i64,
    command_message_id: i64,
    command: Command,
) -> Result<(), HandleError> {
    match command {
        Command::Start => {
            telegram
                .send_message(
                    chat_id,
                    "\
Connect Scratchwall, then pick a folder, then forward memes.\n\
/login — connect your Scratchwall account\n\
/space Name — folder; created on first save if missing.\n\
/clear — delete Telegram copies from the last 48 hours that already reached Scratchwall.\n\
/show-sent — quote the oldest saved forward you can delete by hand down to the bottom.",
                )
                .await
        }
        Command::Login { name: _ } => send_login(config, telegram, http, chat_id, user_id).await,
        Command::Space { name: None } => {
            let text = match spaces.get(user_id) {
                Some(space) => format!("Saving to {space}. Change it with /space Name."),
                None => "No folder yet. Set one with /space Name.".into(),
            };
            telegram.send_message(chat_id, &text).await
        }
        Command::Space { name: Some(name) } => {
            let text = match spaces.set(user_id, &name) {
                Ok(space) => format!("Saving to {space}. Forward a meme whenever."),
                Err(error) => error.to_string(),
            };
            telegram.send_message(chat_id, &text).await
        }
        Command::Clear => {
            clear_delivered(telegram, delivered, chat_id, user_id, command_message_id).await
        }
        Command::ShowSent => {
            show_sent(telegram, delivered, chat_id, user_id, command_message_id).await
        }
        Command::Unknown(name) => {
            telegram
                .send_message(
                    chat_id,
                    &format!(
                        "Unknown command {name}. Try /login, /space Name, /clear, and /show-sent."
                    ),
                )
                .await
        }
    }
}

async fn send_login(
    config: &Config,
    telegram: &Telegram,
    http: &reqwest::Client,
    chat_id: i64,
    user_id: i64,
) -> Result<(), HandleError> {
    match request_telegram_bind(
        http,
        &config.scratchwall_url,
        &config.scratchwall_ingest_token,
        user_id,
    )
    .await
    {
        Ok(status) => match status.url {
            Some(url) => {
                telegram
                    .send_connect(
                        chat_id,
                        "Connect Scratchwall to choose who you save as.",
                        &url,
                    )
                    .await
            }
            None => {
                let name = status.display_name.unwrap_or_else(|| "your account".into());
                telegram
                    .send_message(chat_id, &format!("Saving as {name}."))
                    .await
            }
        },
        Err(error) => {
            tracing::warn!(error = %error, "telegram bind request failed");
            telegram
                .send_message(chat_id, &ingest_user_message(&error))
                .await
        }
    }
}

async fn handle_item(
    config: &Config,
    telegram: &Telegram,
    http: &reqwest::Client,
    spaces: &spaces::SpaceBindings,
    delivered: &mut Delivered,
    item: mapping::IncomingItem,
) -> Result<(), HandleError> {
    let batches = delivered
        .prepare_batches(&item)
        .map_err(HandleError::transient)?;
    for batch in batches {
        handle_batch(config, telegram, http, spaces, delivered, batch).await?;
    }
    Ok(())
}

async fn handle_batch(
    config: &Config,
    telegram: &Telegram,
    http: &reqwest::Client,
    spaces: &spaces::SpaceBindings,
    delivered: &mut Delivered,
    item: mapping::IncomingItem,
) -> Result<(), HandleError> {
    let Some(space) = spaces.get(item.user_id).map(str::to_owned) else {
        remember(delivered, &item, DeliveryStatus::Failed)?;
        telegram
            .reply_message(
                item.chat_id,
                item.message_id,
                "Pick a folder first: /space Name",
            )
            .await?;
        return Ok(());
    };
    if let Some(name) = &item.too_large {
        remember(delivered, &item, DeliveryStatus::Failed)?;
        let text = format!("{name} is larger than 20 MiB");
        telegram
            .reply_message(item.chat_id, item.message_id, &text)
            .await?;
        return Ok(());
    }
    let mut files = Vec::new();
    for file in &item.files {
        match telegram.download(&file.file_id).await {
            Ok(bytes) => files.push((file.filename.clone(), file.mime_type.clone(), bytes)),
            Err(error) => {
                tracing::warn!(error = %error, "telegram download failed");
                remember(delivered, &item, DeliveryStatus::Failed)?;
                telegram
                    .reply_message(
                        item.chat_id,
                        item.message_id,
                        "Could not download that file from Telegram. Forward it again.",
                    )
                    .await?;
                return Ok(());
            }
        }
    }
    let file_count = files.len();
    match post_ingest(
        http,
        &config.scratchwall_url,
        &config.scratchwall_ingest_token,
        item.user_id,
        &space,
        &item,
        files,
    )
    .await
    {
        Ok(()) => {
            remember(delivered, &item, DeliveryStatus::Sent)?;
            tracing::info!(chat_id=item.chat_id,message_ids=?item.message_ids,file_count,key=%item.idempotency_key(),"Telegram forward saved");
            Ok(())
        }
        Err(error) => {
            remember(delivered, &item, DeliveryStatus::Failed)?;
            if error
                .message()
                .contains("telegram account is not connected")
            {
                return send_login(config, telegram, http, item.chat_id, item.user_id).await;
            }
            tracing::warn!(error = %error, telegram_user_id = item.user_id, space, "ingest failed");
            telegram
                .reply_message(item.chat_id, item.message_id, &ingest_user_message(&error))
                .await?;
            Ok(())
        }
    }
}

async fn clear_delivered(
    telegram: &Telegram,
    delivered: &mut Delivered,
    chat_id: i64,
    user_id: i64,
    command_message_id: i64,
) -> Result<(), HandleError> {
    let pending = delivered
        .list_sent_for_chat(user_id, chat_id)
        .map_err(HandleError::transient)?
        .into_iter()
        .filter(|m| m.message_id < command_message_id)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return telegram
            .send_message(
                chat_id,
                "Nothing to clear. Only messages that already reached Scratchwall are removed.",
            )
            .await;
    }
    let mut finished = Vec::new();
    let mut deleted = 0usize;
    let mut already_gone = 0usize;
    let mut skipped = 0usize;
    let mut stopped = false;
    for message in pending {
        match telegram
            .delete_message(message.chat_id, message.message_id)
            .await
        {
            Ok(DeleteOutcome::Deleted) => {
                finished.push(message);
                deleted += 1;
            }
            Ok(DeleteOutcome::Gone) => {
                finished.push(message);
                already_gone += 1;
            }
            Ok(DeleteOutcome::Forbidden) => {
                match telegram
                    .probe_message(message.chat_id, message.message_id)
                    .await
                {
                    Ok(MessagePresence::Gone) => {
                        finished.push(message);
                        already_gone += 1;
                    }
                    Ok(MessagePresence::Present) => {
                        delivered
                            .mark_clear_blocked(user_id, message)
                            .map_err(HandleError::transient)?;
                        skipped += 1;
                    }
                    Err(error) => {
                        tracing::warn!(error=%error,"could not verify undeletable Telegram copy");
                        stopped = true;
                        break;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "telegram deleteMessage failed");
                stopped = true;
                break;
            }
        }
    }
    delivered
        .remove(user_id, &finished)
        .map_err(HandleError::transient)?;
    telegram
        .send_message(
            chat_id,
            &clear_summary(deleted, already_gone, skipped, stopped),
        )
        .await
}

fn clear_summary(deleted: usize, already_gone: usize, skipped: usize, stopped: bool) -> String {
    let mut text = if deleted == 0 {
        "Removed no Telegram messages.".into()
    } else if deleted == 1 {
        "Removed 1 Telegram message that was already in Scratchwall.".into()
    } else {
        format!("Removed {deleted} Telegram messages that were already in Scratchwall.")
    };
    if already_gone > 0 {
        text.push_str(&format!(" {already_gone} were already gone."));
    }
    if skipped > 0 {
        text.push_str(&format!(
            " Left {skipped} that Telegram would not delete (older than 48 hours, or protected)."
        ));
    }
    if stopped {
        text.push_str(" Stopped early; remaining successful copies can be cleared next time.");
    }
    text.push_str(" Failed or unprocessed forwards were not touched.");
    text
}

fn remember(
    delivered: &mut Delivered,
    item: &mapping::IncomingItem,
    status: DeliveryStatus,
) -> Result<(), HandleError> {
    delivered
        .record_dated(item.user_id, item.chat_id, &item.message_dates, status)
        .map_err(HandleError::transient)
}

async fn show_sent(
    telegram: &Telegram,
    delivered: &mut Delivered,
    chat_id: i64,
    user_id: i64,
    command_message_id: i64,
) -> Result<(), HandleError> {
    // A later deletion can invalidate the selected reply between validation and send.
    // Every retry tombstones one record, so this loop is bounded by the history.
    loop {
        let history = delivered
            .history_for_chat(user_id, chat_id)
            .map_err(HandleError::transient)?
            .into_iter()
            .filter(|m| m.message_id < command_message_id)
            .collect::<Vec<_>>();
        if history.is_empty() {
            return telegram
                .send_message(chat_id, "No forwards are recorded in this chat yet.")
                .await;
        }
        telegram.send_typing(chat_id).await;
        let mut latest_failure = None;
        for failure in history
            .iter()
            .rev()
            .filter(|m| !m.removed && m.status == DeliveryStatus::Failed)
        {
            match telegram.probe_message(chat_id, failure.message_id).await {
                Ok(MessagePresence::Gone) => delivered
                    .remove(user_id, &[*failure])
                    .map_err(HandleError::transient)?,
                Ok(MessagePresence::Present) => {
                    latest_failure = Some(*failure);
                    break;
                }
                Err(error) => {
                    tracing::warn!(error=%error,"could not verify failed forward");
                    return telegram.send_message(chat_id,"Telegram could not verify the latest failed forward. Please run /show-sent again.").await;
                }
            }
        }
        let after = latest_failure.map(|m| m.message_id).unwrap_or(i64::MIN);
        for message in history
            .iter()
            .filter(|m| !m.removed && m.status == DeliveryStatus::Sent && m.message_id > after)
        {
            match telegram.probe_message(chat_id, message.message_id).await {
                Ok(MessagePresence::Gone) => delivered
                    .remove(user_id, &[*message])
                    .map_err(HandleError::transient)?,
                Ok(MessagePresence::Present) => break,
                Err(error) => {
                    tracing::warn!(error=%error,"could not verify saved forward");
                    return telegram.send_message(chat_id,"Telegram could not verify the saved forwards. Please run /show-sent again.").await;
                }
            }
        }
        let history = delivered
            .history_for_chat(user_id, chat_id)
            .map_err(HandleError::transient)?
            .into_iter()
            .filter(|m| m.message_id < command_message_id)
            .collect::<Vec<_>>();
        let mut report = delivered::sent_report(&history);
        let target = if report.remaining {
            report.boundary
        } else {
            report.latest_failure
        };
        match telegram
            .send_report(chat_id, &report, target.map(|m| m.message_id))
            .await?
        {
            ReportOutcome::Missing => {
                if let Some(target) = target {
                    delivered
                        .remove(user_id, &[target])
                        .map_err(HandleError::transient)?;
                    continue;
                }
                return Err(HandleError::Permanent(
                    "Telegram rejected a report without a reply target".into(),
                ));
            }
            ReportOutcome::Sent { message_id, date } => {
                if let (Some(target), Some(date)) = (target, date) {
                    if target.date != Some(date) {
                        delivered
                            .remember_dates(user_id, chat_id, &[(target.message_id, Some(date))])
                            .map_err(HandleError::transient)?;
                        let history = delivered
                            .history_for_chat(user_id, chat_id)
                            .map_err(HandleError::transient)?
                            .into_iter()
                            .filter(|m| m.message_id < command_message_id)
                            .collect::<Vec<_>>();
                        report = delivered::sent_report(&history);
                        telegram.edit_report(chat_id, message_id, &report).await?;
                    }
                }
                return Ok(());
            }
        }
    }
}

fn ingest_user_message(error: &HandleError) -> String {
    let text = error.message();
    if text.contains("502") || text.contains("503") || text.contains("504") {
        "Scratchwall is unreachable. Forward the meme again.".into()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::{Arc, Mutex};

    #[test]
    fn clear_summary_leaves_failures_and_mentions_skips() {
        let text = clear_summary(2, 0, 1, false);
        assert!(text.contains("Removed 2 Telegram messages"));
        assert!(text.contains("Left 1"));
        assert!(text.contains("Failed or unprocessed forwards were not touched"));
        assert!(!text.contains("Stopped early"));
        let empty = clear_summary(0, 0, 0, true);
        assert!(empty.contains("Removed no Telegram messages"));
        assert!(empty.contains("Stopped early"));
    }
    #[derive(Default)]
    struct ApiState {
        alive: std::collections::HashMap<i64, i64>,
        copies: std::collections::HashMap<i64, i64>,
        blocked: HashSet<i64>,
        race: HashSet<i64>,
        omit_reply: bool,
        forward_error: Option<String>,
        cleanup_error: bool,
        events: Vec<String>,
        reports: Vec<serde_json::Value>,
        next_copy: i64,
    }
    async fn api(
        State(state): State<Arc<Mutex<ApiState>>>,
        axum::extract::Path(method): axum::extract::Path<String>,
        headers: axum::http::HeaderMap,
        body: axum::body::Bytes,
    ) -> Json<serde_json::Value> {
        let v: serde_json::Value = if headers
            .get("content-type")
            .is_some_and(|h| h.to_str().unwrap_or("").contains("application/json"))
        {
            serde_json::from_slice(&body).unwrap()
        } else {
            let url =
                reqwest::Url::parse(&format!("http://local/?{}", String::from_utf8_lossy(&body)))
                    .unwrap();
            let mut v = serde_json::json!({});
            for (k, value) in url.query_pairs() {
                v[k.as_ref()] = value
                    .parse::<i64>()
                    .map(serde_json::Value::from)
                    .unwrap_or_else(|_| value.into_owned().into());
            }
            v
        };
        let mut state = state.lock().unwrap();
        let id = v["message_id"].as_i64().unwrap_or(0);
        let payload = match method.as_str() {
            "sendChatAction" => serde_json::json!({"ok":true,"result":true}),
            "forwardMessage" => {
                assert_eq!(v["chat_id"], v["from_chat_id"]);
                assert_eq!(v["disable_notification"], true);
                state.events.push(format!("probe {id}"));
                if let Some(error) = &state.forward_error {
                    serde_json::json!({"ok":false,"description":error})
                } else if state.alive.contains_key(&id) {
                    state.next_copy += 1;
                    let copy = 10000 + state.next_copy;
                    state.copies.insert(copy, id);
                    serde_json::json!({"ok":true,"result":{"message_id":copy}})
                } else {
                    serde_json::json!({"ok":false,"description":"Bad Request: message to forward not found"})
                }
            }
            "deleteMessage" => {
                state.events.push(format!("delete {id}"));
                if state.copies.contains_key(&id) {
                    if state.cleanup_error {
                        serde_json::json!({"ok":false,"description":"Forbidden: test cleanup refused"})
                    } else {
                        state.copies.remove(&id);
                        serde_json::json!({"ok":true,"result":true})
                    }
                } else if state.blocked.contains(&id) {
                    serde_json::json!({"ok":false,"description":"Bad Request: message can't be deleted for everyone"})
                } else if state.alive.remove(&id).is_some() {
                    serde_json::json!({"ok":true,"result":true})
                } else {
                    serde_json::json!({"ok":false,"description":"Bad Request: message to delete not found"})
                }
            }
            "sendMessage" => {
                let target = v
                    .pointer("/reply_parameters/message_id")
                    .and_then(|v| v.as_i64());
                state.events.push(format!("report {target:?}"));
                if target.is_some_and(|id| state.race.remove(&id)) {
                    state.alive.remove(&target.unwrap());
                    serde_json::json!({"ok":false,"description":"Bad Request: replied message not found"})
                } else {
                    state.reports.push(v.clone());
                    let reply = if state.omit_reply {
                        serde_json::Value::Null
                    } else {
                        target
                            .and_then(|id| {
                                state
                                    .alive
                                    .get(&id)
                                    .map(|date| serde_json::json!({"message_id":id,"date":date}))
                            })
                            .unwrap_or_default()
                    };
                    serde_json::json!({"ok":true,"result":{"message_id":20000,"reply_to_message":reply}})
                }
            }
            "editMessageText" => {
                state.reports.push(v);
                serde_json::json!({"ok":true,"result":{"message_id":id}})
            }
            _ => panic!("unexpected API method {method}"),
        };
        Json(payload)
    }
    struct Fixture {
        delivered: Delivered,
        telegram: Telegram,
        state: Arc<Mutex<ApiState>>,
        server: tokio::task::JoinHandle<()>,
        directory: std::path::PathBuf,
    }
    impl Fixture {
        async fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "scratchbot-report-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&directory);
            let delivered = Delivered::load(directory.join("delivered.sqlite3")).unwrap();
            let state = Arc::new(Mutex::new(ApiState::default()));
            let app = Router::new()
                .route("/{method}", post(api))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self {
                delivered,
                telegram: Telegram::with_test_api(format!("http://{addr}")),
                state,
                server,
                directory,
            }
        }
        fn seed(&mut self, id: i64, status: DeliveryStatus, present: bool, removed: bool) {
            self.delivered
                .record_dated(1, 1, &[(id, Some(id * 100))], status)
                .unwrap();
            if present {
                self.state.lock().unwrap().alive.insert(id, id * 100);
            }
            if removed {
                let m = self
                    .delivered
                    .history_for_chat(1, 1)
                    .unwrap()
                    .into_iter()
                    .find(|m| m.message_id == id)
                    .unwrap();
                self.delivered.remove(1, &[m]).unwrap();
            }
        }
        async fn show(&mut self) {
            show_sent(&self.telegram, &mut self.delivered, 1, 1, 50)
                .await
                .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[tokio::test]
    async fn legacy_target_date_is_recovered_only_from_its_reply_object() {
        let mut f = Fixture::new().await;
        f.delivered.record(1, 1, [3], DeliveryStatus::Sent).unwrap();
        f.state.lock().unwrap().alive.insert(3, 300);
        f.show().await;
        assert_eq!(f.delivered.date(1, 1, 3), Some(300));
        assert_eq!(
            f.state.lock().unwrap().reports.last().unwrap()["entities"][0]["unix_time"],
            300
        );
    }

    #[tokio::test]
    async fn cleared_tail_keeps_earliest_date_and_replies_to_failure() {
        let mut f = Fixture::new().await;
        f.seed(1, DeliveryStatus::Sent, true, false);
        f.seed(2, DeliveryStatus::Failed, true, false);
        f.seed(3, DeliveryStatus::Sent, false, true);
        f.seed(4, DeliveryStatus::Sent, false, true);
        f.show().await;
        let s = f.state.lock().unwrap();
        let report = s.reports.last().unwrap();
        assert_eq!(report["reply_parameters"]["message_id"], 2);
        assert_eq!(report["entities"][0]["unix_time"], 300);
        assert_eq!(report["entities"][1]["unix_time"], 200);
        assert!(
            report["text"]
                .as_str()
                .unwrap()
                .contains("Latest forward failure")
        );
        assert!(!report["text"].as_str().unwrap().contains("copies remain"));
        assert!(s.copies.is_empty());
    }
    #[tokio::test]
    async fn remaining_tail_skips_deleted_sources_and_excludes_later_updates() {
        let mut f = Fixture::new().await;
        f.seed(2, DeliveryStatus::Failed, true, false);
        f.seed(3, DeliveryStatus::Sent, false, true);
        f.seed(4, DeliveryStatus::Sent, false, false);
        f.seed(5, DeliveryStatus::Sent, true, false);
        f.seed(60, DeliveryStatus::Failed, true, false);
        f.show().await;
        let s = f.state.lock().unwrap();
        let report = s.reports.last().unwrap();
        assert_eq!(report["reply_parameters"]["message_id"], 5);
        assert_eq!(report["entities"][0]["unix_time"], 500);
        assert!(report["text"].as_str().unwrap().contains("48 hours"));
        assert!(!s.events.iter().any(|e| e == "probe 60"));
        assert!(s.copies.is_empty());
        assert!(
            f.delivered
                .history_for_chat(1, 1)
                .unwrap()
                .iter()
                .find(|m| m.message_id == 4)
                .unwrap()
                .removed
        );
    }
    #[tokio::test]
    async fn deleting_failures_for_both_walks_back_through_history() {
        let mut f = Fixture::new().await;
        f.seed(1, DeliveryStatus::Sent, true, false);
        f.seed(2, DeliveryStatus::Failed, true, false);
        f.seed(3, DeliveryStatus::Sent, true, false);
        f.seed(7, DeliveryStatus::Failed, false, false);
        f.show().await;
        assert_eq!(
            f.state.lock().unwrap().reports.last().unwrap()["reply_parameters"]["message_id"],
            3
        );
        f.state.lock().unwrap().alive.remove(&2);
        f.show().await;
        assert_eq!(
            f.state.lock().unwrap().reports.last().unwrap()["reply_parameters"]["message_id"],
            1
        );
        f.state.lock().unwrap().alive.clear();
        f.show().await;
        assert!(f.delivered.list_for_chat(1, 1).unwrap().is_empty());
        assert_eq!(f.delivered.date(1, 1, 1), Some(100));
        assert!(
            f.state.lock().unwrap().reports.last().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("No forward failures remain")
        );
    }
    #[tokio::test]
    async fn deletion_between_probe_and_reply_retries_without_losing_history() {
        let mut f = Fixture::new().await;
        f.seed(3, DeliveryStatus::Sent, true, false);
        f.seed(4, DeliveryStatus::Sent, true, false);
        f.state.lock().unwrap().race.insert(3);
        f.show().await;
        assert_eq!(
            f.state.lock().unwrap().reports.last().unwrap()["reply_parameters"]["message_id"],
            4
        );
        assert_eq!(f.delivered.date(1, 1, 3), Some(300));
        assert!(f.delivered.history_for_chat(1, 1).unwrap()[0].removed);
    }
    #[tokio::test]
    async fn omitted_reply_keeps_the_dated_report_and_does_not_erase_source() {
        let mut f = Fixture::new().await;
        f.seed(3, DeliveryStatus::Sent, true, false);
        f.state.lock().unwrap().omit_reply = true;
        f.show().await;
        let s = f.state.lock().unwrap();
        assert_eq!(s.reports.last().unwrap()["entities"][0]["unix_time"], 300);
        assert!(s.alive.contains_key(&3));
        assert!(!s.events.iter().any(|e| e == "delete 20000"));
    }
    #[tokio::test]
    async fn unverifiable_or_unremovable_test_copy_never_marks_source_gone() {
        for cleanup in [false, true] {
            let mut f = Fixture::new().await;
            f.seed(3, DeliveryStatus::Sent, true, false);
            if cleanup {
                f.state.lock().unwrap().cleanup_error = true;
            } else {
                f.state.lock().unwrap().forward_error =
                    Some("Bad Request: message can't be forwarded".into());
            }
            f.show().await;
            assert!(!f.delivered.history_for_chat(1, 1).unwrap()[0].removed);
            assert!(
                !f.state.lock().unwrap().reports.last().unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("All forwards")
            );
        }
    }
    #[tokio::test]
    async fn clear_preserves_dates_and_never_deletes_failures_or_future_updates() {
        let mut f = Fixture::new().await;
        f.seed(1, DeliveryStatus::Sent, true, false);
        f.seed(2, DeliveryStatus::Failed, true, false);
        f.seed(3, DeliveryStatus::Sent, true, false);
        f.seed(60, DeliveryStatus::Sent, true, false);
        f.state.lock().unwrap().blocked.insert(1);
        clear_delivered(&f.telegram, &mut f.delivered, 1, 1, 50)
            .await
            .unwrap();
        let h = f.delivered.history_for_chat(1, 1).unwrap();
        assert!(h.iter().find(|m| m.message_id == 1).unwrap().clear_blocked);
        assert!(h.iter().find(|m| m.message_id == 3).unwrap().removed);
        assert_eq!(f.delivered.date(1, 1, 3), Some(300));
        let s = f.state.lock().unwrap();
        assert!(s.alive.contains_key(&2));
        assert!(s.alive.contains_key(&60));
        assert!(!s.events.iter().any(|e| e == "delete 2"));
    }
}
