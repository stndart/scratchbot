use crate::config::UpdateConfig;
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

const RELEASE_OWNER: &str = "stndart";
const RELEASE_REPO: &str = "scratchbot";
const RELEASE_ASSET: &str = "scratchwall-telegram";
const MAX_RELEASE_BYTES: usize = 32 * 1024 * 1024;
const NEXT_NAME: &str = "scratchwall-telegram.next";
const PREV_NAME: &str = "scratchwall-telegram.prev";

pub fn build_sha() -> &'static str {
    match option_env!("GIT_SHA") {
        Some(value) if !value.is_empty() => value,
        _ => "dev",
    }
}

pub fn exe_sha256() -> Result<String> {
    let path = std::env::current_exe().context("could not resolve this binary")?;
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

#[derive(Clone)]
pub struct RestartGate {
    inner: Arc<GateInner>,
}

struct GateInner {
    notify: Notify,
    busy: AtomicBool,
    staged: Mutex<Option<Staged>>,
}

struct Staged {
    temp: PathBuf,
    exe: PathBuf,
    version: String,
    sha256: String,
}

impl RestartGate {
    fn new() -> Self {
        Self {
            inner: Arc::new(GateInner {
                notify: Notify::new(),
                busy: AtomicBool::new(false),
                staged: Mutex::new(None),
            }),
        }
    }

    pub fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.inner.notify.notified()
    }

    fn try_begin(&self) -> bool {
        self.inner
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn clear_busy(&self) {
        self.inner.busy.store(false, Ordering::SeqCst);
    }

    fn stage(&self, staged: Staged) {
        let mut guard = self
            .inner
            .staged
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        tracing::info!(
            version = %staged.version,
            sha256 = %staged.sha256,
            "update staged"
        );
        *guard = Some(staged);
        self.inner.notify.notify_one();
    }

    pub fn restart_if_staged(&self) -> Result<()> {
        let staged = {
            let mut guard = self
                .inner
                .staged
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            guard.take()
        };
        let Some(staged) = staged else {
            return Ok(());
        };
        let result = swap_and_reexec(&staged);
        self.clear_busy();
        if staged.temp.exists() {
            let _ = fs::remove_file(&staged.temp);
        }
        result
    }
}

pub async fn serve(config: UpdateConfig) -> Result<RestartGate> {
    let gate = RestartGate::new();
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("bind update hook {}", config.listen))?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(github_redirect))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("scratchwall-telegram/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let app = Router::new()
        .route("/update", post(hook))
        .with_state(AppState {
            token: Arc::<str>::from(config.token),
            gate: gate.clone(),
            http,
        });
    tracing::info!(listen = %config.listen, "update hook listening");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!(error = %error, "update hook stopped");
        }
    });
    Ok(gate)
}

#[derive(Clone)]
struct AppState {
    token: Arc<str>,
    gate: RestartGate,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct UpdateRequest {
    url: String,
    sha256: String,
    #[serde(default)]
    version: String,
}

#[derive(Serialize)]
struct HookResponse<'a> {
    status: &'a str,
    build: &'a str,
}

#[derive(Serialize)]
struct HookErrorBody<'a> {
    error: &'a str,
}

async fn hook(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<UpdateRequest>,
) -> Result<Json<HookResponse<'static>>, (StatusCode, Json<HookErrorBody<'static>>)> {
    if !authorized(&headers, &state.token) {
        tracing::warn!("rejected update hook");
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(HookErrorBody {
                error: "unauthorized",
            }),
        ));
    }
    if !allowed_release_url(&payload.url) {
        return Err(bad_request("invalid release url"));
    }
    if !valid_sha256(&payload.sha256) {
        return Err(bad_request("invalid sha256"));
    }
    if payload.version.len() > 128 || payload.version.chars().any(|ch| ch.is_control()) {
        return Err(bad_request("invalid version"));
    }
    if let Ok(current) = exe_sha256() {
        if sha256_eq(&current, &payload.sha256) {
            tracing::info!(sha256 = %current, "update matches the running binary");
            return Ok(Json(HookResponse {
                status: "current",
                build: build_sha(),
            }));
        }
    }
    if !state.gate.try_begin() {
        return Ok(Json(HookResponse {
            status: "in_progress",
            build: build_sha(),
        }));
    }
    let version = if payload.version.is_empty() {
        "unknown".to_owned()
    } else {
        payload.version.clone()
    };
    tracing::info!(version = %version, url = %payload.url, "accepted update hook");
    tokio::spawn(async move {
        // Let the 202 leave the socket before the process might re-exec.
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Err(error) = stage_update(state.http.clone(), state.gate.clone(), payload).await {
            tracing::error!(error = %error, "self-update failed");
            state.gate.clear_busy();
        }
    });
    Ok(Json(HookResponse {
        status: "accepted",
        build: build_sha(),
    }))
}

fn bad_request(error: &'static str) -> (StatusCode, Json<HookErrorBody<'static>>) {
    (StatusCode::BAD_REQUEST, Json(HookErrorBody { error }))
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(provided) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(provided.as_bytes(), token.as_bytes())
}

async fn stage_update(
    http: reqwest::Client,
    gate: RestartGate,
    payload: UpdateRequest,
) -> Result<()> {
    let exe = std::env::current_exe().context("could not resolve this binary")?;
    let bytes = download_release(&http, &payload.url, &payload.sha256).await?;
    let temp = exe.with_file_name(NEXT_NAME);
    write_executable(&temp, &bytes)?;
    match Command::new(&temp).arg("--ready").output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = fs::remove_file(&temp);
            bail!("new binary failed --ready: {stderr}");
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            return Err(error).context("could not run new binary --ready");
        }
    }
    gate.stage(Staged {
        temp,
        exe,
        version: if payload.version.is_empty() {
            "unknown".into()
        } else {
            payload.version
        },
        sha256: payload.sha256.trim().to_ascii_lowercase(),
    });
    Ok(())
}

async fn download_release(
    http: &reqwest::Client,
    url: &str,
    expected_sha256: &str,
) -> Result<Vec<u8>> {
    let mut last_error = None;
    for attempt in 1..=5 {
        match download_once(http, url, expected_sha256).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                tracing::warn!(attempt, error = %error, "release download failed");
                last_error = Some(error);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("release download failed")))
}

async fn download_once(
    http: &reqwest::Client,
    url: &str,
    expected_sha256: &str,
) -> Result<Vec<u8>> {
    let response = http
        .get(url)
        .send()
        .await
        .context("release request failed")?;
    anyhow::ensure!(
        response.status().is_success(),
        "release download returned {}",
        response.status()
    );
    anyhow::ensure!(
        allowed_download_host(response.url()),
        "release redirected off GitHub"
    );
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= MAX_RELEASE_BYTES as u64,
            "release is larger than {MAX_RELEASE_BYTES} bytes"
        );
    }
    let mut body = Vec::new();
    let mut response = response;
    loop {
        let Some(chunk) = response.chunk().await.context("release body")? else {
            break;
        };
        anyhow::ensure!(
            body.len() + chunk.len() <= MAX_RELEASE_BYTES,
            "release is larger than {MAX_RELEASE_BYTES} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    anyhow::ensure!(body.starts_with(b"\x7fELF"), "release is not an ELF binary");
    anyhow::ensure!(
        sha256_matches(&body, expected_sha256),
        "release sha256 does not match"
    );
    Ok(body)
}

fn github_redirect(attempt: reqwest::redirect::Attempt) -> reqwest::redirect::Action {
    if allowed_download_host(attempt.url()) {
        attempt.follow()
    } else {
        attempt.error(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "redirect host refused",
        ))
    }
}

fn allowed_download_host(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    url.scheme() == "https" && (host == "github.com" || host.ends_with(".githubusercontent.com"))
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))?;
    Ok(())
}

fn swap_and_reexec(staged: &Staged) -> Result<()> {
    snapshot_previous(&staged.exe)?;
    fs::rename(&staged.temp, &staged.exe).with_context(|| {
        format!(
            "replace {} with {}",
            staged.exe.display(),
            staged.temp.display()
        )
    })?;
    tracing::info!(
        version = %staged.version,
        path = %staged.exe.display(),
        "replacing binary and re-executing"
    );
    let error = Command::new(&staged.exe)
        .args(std::env::args().skip(1))
        .exec();
    let previous = staged.exe.with_file_name(PREV_NAME);
    if let Err(restore_error) = fs::rename(&previous, &staged.exe) {
        bail!("re-exec failed ({error}) and restore failed ({restore_error})");
    }
    Err(error).context("re-exec")
}

fn snapshot_previous(exe: &Path) -> Result<()> {
    let previous = exe.with_file_name(PREV_NAME);
    if previous.exists() {
        fs::remove_file(&previous).with_context(|| format!("remove {}", previous.display()))?;
    }
    if fs::hard_link(exe, &previous).is_err() {
        fs::copy(exe, &previous).with_context(|| format!("copy {}", previous.display()))?;
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256_matches(bytes: &[u8], expected: &str) -> bool {
    let Some(expected) = normalized_sha256(expected) else {
        return false;
    };
    sha256_eq(&sha256_hex(bytes), &expected)
}

fn sha256_eq(actual: &str, expected: &str) -> bool {
    let Some(expected) = normalized_sha256(expected) else {
        return false;
    };
    let Some(actual) = normalized_sha256(actual) else {
        return false;
    };
    constant_time_eq(actual.as_bytes(), expected.as_bytes())
}

fn normalized_sha256(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    if valid_sha256(&value) {
        Some(value)
    } else {
        None
    }
}

fn valid_sha256(value: &str) -> bool {
    let value = value.trim();
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in left.iter().zip(right) {
        diff |= left ^ right;
    }
    diff == 0
}

pub fn allowed_release_url(raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let segments: Vec<&str> = url.path().split('/').collect();
    if segments.len() != 7 {
        return false;
    }
    let tag = segments[5];
    segments[0].is_empty()
        && segments[1] == RELEASE_OWNER
        && segments[2] == RELEASE_REPO
        && segments[3] == "releases"
        && segments[4] == "download"
        && !tag.is_empty()
        && tag != "."
        && tag != ".."
        && !tag.contains('/')
        && segments[6] == RELEASE_ASSET
}

#[cfg(test)]
mod tests {
    use super::{allowed_release_url, sha256_hex, sha256_matches, valid_sha256};

    #[test]
    fn sha256_of_abc() {
        let hex = sha256_hex(b"abc");
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(sha256_matches(b"abc", &hex.to_ascii_uppercase()));
        assert!(!sha256_matches(b"abd", &hex));
    }

    #[test]
    fn sha256_must_be_64_hex_chars() {
        assert!(valid_sha256(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
        assert!(!valid_sha256("abc"));
        assert!(!valid_sha256(
            "zz7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
    }

    #[test]
    fn release_url_is_this_repos_asset() {
        assert!(allowed_release_url(
            "https://github.com/stndart/scratchbot/releases/download/deploy-abc/scratchwall-telegram"
        ));
        assert!(!allowed_release_url(
            "http://github.com/stndart/scratchbot/releases/download/deploy-abc/scratchwall-telegram"
        ));
        assert!(!allowed_release_url(
            "https://github.com/stndart/scratchbot/releases/download/deploy-abc/other"
        ));
        assert!(!allowed_release_url(
            "https://example.com/stndart/scratchbot/releases/download/deploy-abc/scratchwall-telegram"
        ));
        assert!(!allowed_release_url(
            "https://github.com/other/scratchbot/releases/download/deploy-abc/scratchwall-telegram"
        ));
        assert!(!allowed_release_url(
            "https://github.com/stndart/scratchbot/releases/download/deploy-abc/scratchwall-telegram?x=1"
        ));
        assert!(!allowed_release_url(
            "https://github.com/stndart/scratchbot/releases/download/../scratchwall-telegram"
        ));
    }
}
