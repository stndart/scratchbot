use anyhow::Result;

const BIN_PATH: &str = "/usr/local/bin/scratchwall-telegram";
const UNIT_PATH: &str = "/etc/systemd/system/scratchwall-telegram.service";
const ENV_PATH: &str = "/etc/scratchwall/telegram.env";
const STATE_DIR: &str = "/var/lib/scratchwall-telegram";

pub fn print_help() {
    eprintln!(
        "\
scratchwall-telegram [--install | --uninstall | --help]

  (no args)     run the ingest bot
  --install     copy this binary and enable a systemd service (Linux, root)
  --uninstall   stop the service and remove the unit and installed binary
  --help        show this text

Install paths:
  {BIN_PATH}
  {UNIT_PATH}
  {ENV_PATH}
  {STATE_DIR}"
    );
}

pub fn install() -> Result<()> {
    #[cfg(target_os = "linux")]
    return linux::install();
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("--install is only supported on Linux");
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "linux")]
    return linux::uninstall();
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("--uninstall is only supported on Linux");
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::Context;
    use std::{
        env, fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Command,
    };

    const SERVICE_NAME: &str = "scratchwall-telegram";
    const SERVICE_USER: &str = "scratchwall-telegram";
    const UNIT: &str = "\
[Unit]
Description=Scratchwall Telegram ingest bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=scratchwall-telegram
Group=scratchwall-telegram
WorkingDirectory=/var/lib/scratchwall-telegram
StateDirectory=scratchwall-telegram
EnvironmentFile=/etc/scratchwall/telegram.env
ExecStart=/usr/local/bin/scratchwall-telegram
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
";
    const ENV_TEMPLATE: &str = "\
TELEGRAM_BOT_TOKEN=
TELEGRAM_ALLOW_USER_IDS=
SCRATCHWALL_URL=https://scratch.morad.uk
SCRATCHWALL_INGEST_TOKEN=
STATE_DIR=/var/lib/scratchwall-telegram
RUST_LOG=scratchwall_telegram=info
";

    pub fn install() -> Result<()> {
        require_root()?;
        require_systemctl()?;
        let src = env::current_exe().context("could not resolve this binary")?;
        systemctl_ok(&["stop", SERVICE_NAME]);
        install_binary(&src)?;
        ensure_service_user()?;
        fs::create_dir_all(STATE_DIR).with_context(|| format!("create {STATE_DIR}"))?;
        chown_path(STATE_DIR)?;
        let env_ready = seed_env(&src)?;
        fs::write(UNIT_PATH, UNIT).with_context(|| format!("write {UNIT_PATH}"))?;
        chmod(UNIT_PATH, 0o644)?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", SERVICE_NAME])?;
        println!("installed {SERVICE_NAME}");
        println!("  binary  {BIN_PATH}");
        println!("  unit    {UNIT_PATH}");
        println!("  env     {ENV_PATH}");
        println!("  state   {STATE_DIR}");
        if env_ready {
            systemctl(&["restart", SERVICE_NAME])?;
            println!("  status  enabled and started");
        } else {
            println!("  status  enabled, not started");
            println!("fill {ENV_PATH} then: systemctl start {SERVICE_NAME}");
        }
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        require_root()?;
        require_systemctl()?;
        systemctl_ok(&["disable", "--now", SERVICE_NAME]);
        remove_if_exists(UNIT_PATH)?;
        systemctl_ok(&["daemon-reload"]);
        remove_if_exists(BIN_PATH)?;
        println!("removed {SERVICE_NAME} unit and {BIN_PATH}");
        println!("left {ENV_PATH} and {STATE_DIR} in place");
        Ok(())
    }

    fn install_binary(src: &Path) -> Result<()> {
        let dest = Path::new(BIN_PATH);
        if same_file(src, dest)? {
            chmod(BIN_PATH, 0o755)?;
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let tmp = dest.with_extension("new");
        fs::copy(src, &tmp)
            .with_context(|| format!("copy {} -> {}", src.display(), tmp.display()))?;
        chmod(&tmp, 0o755)?;
        fs::rename(&tmp, dest).with_context(|| format!("replace {BIN_PATH}"))?;
        Ok(())
    }

    fn seed_env(exe: &Path) -> Result<bool> {
        let dest = Path::new(ENV_PATH);
        if dest.is_file() {
            chmod(dest, 0o600)?;
            return Ok(env_looks_ready(dest));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut candidates = vec![PathBuf::from("telegram.env"), PathBuf::from(".env")];
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("telegram.env"));
            candidates.push(dir.join(".env"));
        }
        for candidate in candidates {
            if candidate.is_file() {
                fs::copy(&candidate, dest)
                    .with_context(|| format!("copy {} -> {ENV_PATH}", candidate.display()))?;
                chmod(dest, 0o600)?;
                println!("copied {} to {ENV_PATH}", candidate.display());
                return Ok(env_looks_ready(dest));
            }
        }
        fs::write(dest, ENV_TEMPLATE).with_context(|| format!("write {ENV_PATH}"))?;
        chmod(dest, 0o600)?;
        println!("wrote template {ENV_PATH}");
        Ok(false)
    }

    fn env_looks_ready(path: &Path) -> bool {
        let Ok(text) = fs::read_to_string(path) else {
            return false;
        };
        let mut token = false;
        let mut ingest = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches(['\'', '"']);
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "TELEGRAM_BOT_TOKEN" => token = true,
                "SCRATCHWALL_INGEST_TOKEN" | "INGEST_TOKEN" => ingest = true,
                _ => {}
            }
        }
        token && ingest
    }

    fn ensure_service_user() -> Result<()> {
        if user_exists(SERVICE_USER) {
            return Ok(());
        }
        let status = Command::new("useradd")
            .args([
                "--system",
                "--no-create-home",
                "--home-dir",
                STATE_DIR,
                "--shell",
                "/usr/sbin/nologin",
                SERVICE_USER,
            ])
            .status()
            .context("useradd is required to create the service account")?;
        anyhow::ensure!(status.success(), "useradd {SERVICE_USER} failed");
        Ok(())
    }

    fn user_exists(name: &str) -> bool {
        Command::new("id")
            .arg(name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn chown_path(path: &str) -> Result<()> {
        let status = Command::new("chown")
            .args(["-R", &format!("{SERVICE_USER}:{SERVICE_USER}"), path])
            .status()
            .context("chown failed")?;
        anyhow::ensure!(status.success(), "chown {path} failed");
        Ok(())
    }

    fn chmod(path: impl AsRef<Path>, mode: u32) -> Result<()> {
        fs::set_permissions(path.as_ref(), fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {:o} {}", mode, path.as_ref().display()))
    }

    fn same_file(a: &Path, b: &Path) -> Result<bool> {
        if !b.exists() {
            return Ok(false);
        }
        Ok(fs::canonicalize(a).ok() == fs::canonicalize(b).ok())
    }

    fn remove_if_exists(path: &str) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {path}")),
        }
    }

    fn require_root() -> Result<()> {
        let uid = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|text| text.trim().to_string());
        anyhow::ensure!(
            uid.as_deref() == Some("0"),
            "run as root (sudo ./scratchwall-telegram --install)"
        );
        Ok(())
    }

    fn require_systemctl() -> Result<()> {
        Command::new("systemctl")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()
            .filter(|status| status.success())
            .context("systemctl is required (systemd on Ubuntu)")?;
        Ok(())
    }

    fn systemctl(args: &[&str]) -> Result<()> {
        let status = Command::new("systemctl")
            .args(args)
            .status()
            .with_context(|| format!("systemctl {}", args.join(" ")))?;
        anyhow::ensure!(status.success(), "systemctl {} failed", args.join(" "));
        Ok(())
    }

    fn systemctl_ok(args: &[&str]) {
        let _ = Command::new("systemctl").args(args).status();
    }
}
