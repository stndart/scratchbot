use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_bot_token: String,
    pub allow_user_ids: Vec<i64>,
    pub scratchwall_url: String,
    pub scratchwall_ingest_token: String,
    pub state_dir: PathBuf,
    pub update: Option<UpdateConfig>,
}

#[derive(Clone)]
pub struct UpdateConfig {
    pub listen: String,
    pub token: String,
}

impl std::fmt::Debug for UpdateConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpdateConfig")
            .field("listen", &self.listen)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        load_env_files();
        let allow_user_ids = std::env::var("TELEGRAM_ALLOW_USER_IDS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse::<i64>()
                    .with_context(|| format!("invalid Telegram user id {value}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let scratchwall_url = std::env::var("SCRATCHWALL_URL")
            .or_else(|_| std::env::var("PUBLIC_BASE_URL"))
            .unwrap_or_else(|_| "https://scratch.morad.uk".into())
            .trim_end_matches('/')
            .to_owned();
        let scratchwall_ingest_token = std::env::var("SCRATCHWALL_INGEST_TOKEN")
            .or_else(|_| std::env::var("INGEST_TOKEN"))
            .context("INGEST_TOKEN or SCRATCHWALL_INGEST_TOKEN is required")?;
        anyhow::ensure!(
            !scratchwall_ingest_token.is_empty(),
            "INGEST_TOKEN or SCRATCHWALL_INGEST_TOKEN is required"
        );
        let update_token = std::env::var("UPDATE_TOKEN")
            .unwrap_or_default()
            .trim()
            .to_owned();
        let update = if update_token.is_empty() {
            None
        } else {
            Some(UpdateConfig {
                listen: std::env::var("UPDATE_LISTEN").unwrap_or_else(|_| "127.0.0.1:8765".into()),
                token: update_token,
            })
        };
        Ok(Self {
            telegram_bot_token: std::env::var("TELEGRAM_BOT_TOKEN")
                .context("TELEGRAM_BOT_TOKEN is required")?,
            allow_user_ids,
            scratchwall_url,
            scratchwall_ingest_token,
            state_dir: PathBuf::from(std::env::var("STATE_DIR").unwrap_or_else(|_| {
                if Path::new("bot").is_dir() {
                    "bot/data".into()
                } else {
                    "./data".into()
                }
            })),
            update,
        })
    }

    pub fn offset_path(&self) -> PathBuf {
        self.state_dir.join("telegram-offset")
    }
}

fn load_env_files() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for path in [
        manifest.join(".env"),
        PathBuf::from("bot/.env"),
        PathBuf::from(".env"),
        manifest.join("../.env"),
    ] {
        if path.is_file() {
            let _ = dotenvy::from_path(&path);
        }
    }
}
