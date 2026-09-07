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
use telegram::{HandleError, Telegram, post_ingest};

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
        let mut transient = false;
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
                    handle_command(&telegram, &mut spaces, chat_id, user_id, command).await?;
                    processed.insert(update.update_id);
                }
                Classified::Item(item) => {
                    if let Some(ready) = albums.push(item, unix_ms()) {
                        match handle_item(&config, &telegram, &http, &spaces, ready).await {
                            Ok(()) => {
                                processed.insert(update.update_id);
                            }
                            Err(error) if error.is_transient() => {
                                tracing::warn!(error = %error, "transient ingest failure");
                                transient = true;
                                break;
                            }
                            Err(error) => {
                                tracing::error!(error = %error, "ingest failed");
                                processed.insert(update.update_id);
                            }
                        }
                    }
                }
            }
        }
        if !transient {
            for item in albums.take_ready(unix_ms(), ALBUM_GRACE_MS) {
                let ids = item.update_ids.clone();
                let chat_id = item.chat_id;
                match handle_item(&config, &telegram, &http, &spaces, item).await {
                    Ok(()) => processed.extend(ids),
                    Err(error) if error.is_transient() => {
                        tracing::warn!(error = %error, chat_id, "transient album ingest failure");
                        transient = true;
                        break;
                    }
                    Err(error) => {
                        tracing::error!(error = %error, chat_id, "album ingest failed");
                        processed.extend(ids);
                    }
                }
            }
        }
        if !transient {
            offset_value = offset::next_offset(offset_value, &processed, &seen);
            offset::save(&config.offset_path(), offset_value)?;
        }
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
    telegram: &Telegram,
    spaces: &mut spaces::SpaceBindings,
    chat_id: i64,
    user_id: i64,
    command: Command,
) -> Result<(), HandleError> {
    let text = match command {
        Command::Start => "Pick a Scratchwall folder with /space Name, then forward memes here.\n\
             If that folder does not exist yet, it is created on the first save."
            .to_owned(),
        Command::Space { name: None } => match spaces.get(user_id) {
            Some(space) => format!("Saving to {space}. Change it with /space Name."),
            None => "No folder yet. Set one with /space Name.".into(),
        },
        Command::Space { name: Some(name) } => match spaces.set(user_id, &name) {
            Ok(space) => format!("Saving to {space}. Forward a meme whenever."),
            Err(error) => error.to_string(),
        },
        Command::Unknown(name) => format!("Unknown command {name}. Try /space Name."),
    };
    telegram.send_message(chat_id, &text).await
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
        let bytes = telegram.download(&file.file_id).await?;
        files.push((file.filename.clone(), file.mime_type.clone(), bytes));
    }
    match post_ingest(
        http,
        &config.scratchwall_url,
        &config.scratchwall_ingest_token,
        &space,
        &item,
        files,
    )
    .await
    {
        Ok(()) => {
            telegram
                .send_message(item.chat_id, &format!("saved to {space}"))
                .await?;
            Ok(())
        }
        Err(error) => {
            let _ = telegram.send_message(item.chat_id, error.message()).await;
            Err(error)
        }
    }
}
