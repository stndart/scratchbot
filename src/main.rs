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
    collections::HashSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use telegram::{
    DeleteOutcome, HandleError, MessagePresence, Telegram, post_ingest, request_telegram_bind,
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
            if processed.contains(&update.update_id) {
                continue;
            }
            match mapping::classify(update, &config.allow_user_ids) {
                Classified::Ignored => {
                    processed.insert(update.update_id);
                }
                Classified::Command {
                    chat_id,
                    user_id,
                    command,
                } => {
                    handle_command(
                        &config,
                        &telegram,
                        &http,
                        &mut spaces,
                        &mut delivered,
                        chat_id,
                        user_id,
                        command,
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
        Command::Clear => clear_delivered(telegram, delivered, chat_id, user_id).await,
        Command::ShowSent => show_sent(telegram, delivered, chat_id, user_id).await,
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
    let Some(space) = spaces.get(item.user_id).map(str::to_owned) else {
        remember(delivered, &item, DeliveryStatus::Failed);
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
        remember(delivered, &item, DeliveryStatus::Failed);
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
                remember(delivered, &item, DeliveryStatus::Failed);
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
            remember(delivered, &item, DeliveryStatus::Sent);
            Ok(())
        }
        Err(error) => {
            remember(delivered, &item, DeliveryStatus::Failed);
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
) -> Result<(), HandleError> {
    let pending = delivered.list_sent_for_chat(user_id, chat_id);
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
    let mut skipped = 0usize;
    let mut stopped = false;
    for message in pending {
        match telegram
            .delete_message(message.chat_id, message.message_id)
            .await
        {
            Ok(DeleteOutcome::Deleted | DeleteOutcome::Gone) => {
                finished.push(message);
                deleted += 1;
            }
            Ok(DeleteOutcome::Forbidden) => {
                skipped += 1;
            }
            Err(error) => {
                tracing::warn!(error = %error, "telegram deleteMessage failed");
                stopped = true;
                break;
            }
        }
    }
    if let Err(error) = delivered.remove(user_id, &finished) {
        tracing::warn!(error = %error, "could not update delivered message list");
    }
    telegram
        .send_message(chat_id, &clear_summary(deleted, skipped, stopped))
        .await
}

fn clear_summary(deleted: usize, skipped: usize, stopped: bool) -> String {
    let mut text = if deleted == 0 {
        "Removed no Telegram messages.".into()
    } else if deleted == 1 {
        "Removed 1 Telegram message that was already in Scratchwall.".into()
    } else {
        format!("Removed {deleted} Telegram messages that were already in Scratchwall.")
    };
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

fn remember(delivered: &mut Delivered, item: &mapping::IncomingItem, status: DeliveryStatus) {
    if let Err(error) = delivered.record(
        item.user_id,
        item.chat_id,
        item.message_ids.iter().copied(),
        status,
    ) {
        tracing::warn!(error = %error, "could not remember telegram message outcome");
    }
}

const SHOW_SENT_QUOTE: &str = "\
Select from the quoted message to the bottom. It is the oldest forward that reached Scratchwall, \
and every forward after it that is still in the chat also reached Scratchwall.";

async fn show_sent(
    telegram: &Telegram,
    delivered: &mut Delivered,
    chat_id: i64,
    user_id: i64,
) -> Result<(), HandleError> {
    let mut tracked = delivered.list_for_chat(user_id, chat_id);
    if tracked.is_empty() {
        return telegram
            .send_message(
                chat_id,
                "Nothing recorded in this chat yet. Forwards show up here after they reach Scratchwall or fail to.",
            )
            .await;
    }
    tracked.sort_by_key(|message| message.message_id);
    let mut gone = Vec::new();
    let mut tail = Vec::new();
    let mut saw_failure = false;
    for (index, message) in tracked.into_iter().rev().enumerate() {
        if index % 20 == 0 {
            telegram.send_typing(chat_id).await;
        }
        match telegram
            .probe_message(message.chat_id, message.message_id)
            .await
        {
            Ok(MessagePresence::Gone) => gone.push(message),
            Ok(MessagePresence::Present) if message.status == DeliveryStatus::Failed => {
                saw_failure = true;
                break;
            }
            Ok(MessagePresence::Present) => tail.push(message),
            Err(error) => {
                tracing::warn!(error = %error, "could not check whether a telegram message is still in the chat");
                if let Err(error) = delivered.remove(user_id, &gone) {
                    tracing::warn!(error = %error, "could not update delivered message list");
                }
                return telegram
                    .send_message(
                        chat_id,
                        "Could not refresh this chat from Telegram. Run /show-sent again.",
                    )
                    .await;
            }
        }
    }
    if let Err(error) = delivered.remove(user_id, &gone) {
        tracing::warn!(error = %error, "could not update delivered message list");
    }
    let Some(marker) = delivered::oldest_sent_tail(&tail) else {
        let text = if saw_failure {
            "A forward that did not reach Scratchwall is still in this chat. Delete it, then run /show-sent again."
        } else {
            "Every recorded forward is already gone from this chat."
        };
        return telegram.send_message(chat_id, text).await;
    };
    match telegram
        .quote_message(chat_id, marker.message_id, SHOW_SENT_QUOTE)
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::warn!(error = %error, "could not quote the oldest saved forward");
            let text = if error.message().to_ascii_lowercase().contains("not found") {
                "That forward disappeared while I was checking. Run /show-sent again."
            } else {
                "I found the forward, but Telegram would not let me quote it. Run /show-sent again."
            };
            telegram.send_message(chat_id, text).await
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
    use super::clear_summary;

    #[test]
    fn clear_summary_leaves_failures_and_mentions_skips() {
        let text = clear_summary(2, 1, false);
        assert!(text.contains("Removed 2 Telegram messages"));
        assert!(text.contains("Left 1"));
        assert!(text.contains("Failed or unprocessed forwards were not touched"));
        assert!(!text.contains("Stopped early"));
        let empty = clear_summary(0, 0, true);
        assert!(empty.contains("Removed no Telegram messages"));
        assert!(empty.contains("Stopped early"));
    }
}
