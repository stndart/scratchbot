mod config;
mod telegram;

use anyhow::Result;
use config::Config;
use scratchwall_telegram::{
    ALBUM_GRACE_MS,
    album::AlbumBuffer,
    mapping::{self, Classified, Command},
    offset, spaces,
};
use std::{
    collections::HashSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use telegram::{HandleError, Telegram, post_ingest, request_telegram_bind};

#[tokio::main]
async fn main() -> Result<()> {
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
    let mut albums = AlbumBuffer::new();
    let mut processed = HashSet::new();
    tracing::info!(url = %config.scratchwall_url, "telegram ingest bot started");

    loop {
        let now = unix_ms();
        let timeout = poll_timeout(&albums, now);
        let updates = match telegram.get_updates(offset_value, timeout).await {
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
                        chat_id,
                        user_id,
                        command,
                    )
                    .await?;
                    processed.insert(update.update_id);
                }
                Classified::Item(item) => {
                    if let Some(ready) = albums.push(item, unix_ms()) {
                        handle_item(&config, &telegram, &http, &spaces, ready).await?;
                        processed.insert(update.update_id);
                    }
                }
            }
        }
        for item in albums.take_ready(unix_ms(), ALBUM_GRACE_MS) {
            let ids = item.update_ids.clone();
            handle_item(&config, &telegram, &http, &spaces, item).await?;
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

async fn handle_command(
    config: &Config,
    telegram: &Telegram,
    http: &reqwest::Client,
    spaces: &mut spaces::SpaceBindings,
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
/space Name — folder; created on first save if missing.",
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
        Command::Unknown(name) => {
            telegram
                .send_message(
                    chat_id,
                    &format!("Unknown command {name}. Try /login and /space Name."),
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
    item: mapping::IncomingItem,
) -> Result<(), HandleError> {
    let Some(space) = spaces.get(item.user_id).map(str::to_owned) else {
        telegram
            .send_message(item.chat_id, "Pick a folder first: /space Name")
            .await?;
        return Ok(());
    };
    if let Some(name) = &item.too_large {
        let text = format!("{name} is larger than 20 MiB");
        telegram.send_message(item.chat_id, &text).await?;
        return Ok(());
    }
    let mut files = Vec::new();
    for file in &item.files {
        match telegram.download(&file.file_id).await {
            Ok(bytes) => files.push((file.filename.clone(), file.mime_type.clone(), bytes)),
            Err(error) => {
                tracing::warn!(error = %error, "telegram download failed");
                telegram
                    .send_message(
                        item.chat_id,
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
        Ok(()) => Ok(()),
        Err(error) => {
            if error
                .message()
                .contains("telegram account is not connected")
            {
                return send_login(config, telegram, http, item.chat_id, item.user_id).await;
            }
            tracing::warn!(error = %error, telegram_user_id = item.user_id, space, "ingest failed");
            telegram
                .send_message(item.chat_id, &ingest_user_message(&error))
                .await?;
            Ok(())
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
