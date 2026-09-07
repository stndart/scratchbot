use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

const MAX_SPACE_NAME: usize = 200;

#[derive(Default, Serialize, Deserialize)]
struct SpaceFile {
    #[serde(default)]
    spaces: HashMap<String, String>,
}

pub struct SpaceBindings {
    path: PathBuf,
    by_user: HashMap<i64, String>,
}

impl SpaceBindings {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let by_user = match fs::read_to_string(&path) {
            Ok(text) => parse_space_file(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, by_user })
    }

    pub fn get(&self, user_id: i64) -> Option<&str> {
        self.by_user.get(&user_id).map(String::as_str)
    }

    pub fn set(&mut self, user_id: i64, name: &str) -> Result<String> {
        let name = normalize_space_name(name)?;
        self.by_user.insert(user_id, name.clone());
        self.save()?;
        Ok(name)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = SpaceFile {
            spaces: self
                .by_user
                .iter()
                .map(|(id, name)| (id.to_string(), name.clone()))
                .collect(),
        };
        fs::write(&self.path, serde_json::to_string_pretty(&file)?)?;
        Ok(())
    }
}

pub fn normalize_space_name(name: &str) -> Result<String> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "space name is required");
    anyhow::ensure!(name.len() <= MAX_SPACE_NAME, "space name is too long");
    anyhow::ensure!(!name.starts_with('/'), "space name cannot start with /");
    Ok(name.to_owned())
}

fn parse_space_file(text: &str) -> Result<HashMap<i64, String>> {
    let file: SpaceFile = serde_json::from_str(text)?;
    let mut by_user = HashMap::new();
    for (key, name) in file.spaces {
        if let Ok(id) = key.parse::<i64>()
            && let Ok(name) = normalize_space_name(&name)
        {
            by_user.insert(id, name);
        }
    }
    Ok(by_user)
}

pub fn spaces_path(state_dir: &Path) -> PathBuf {
    state_dir.join("spaces.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_rejects_empty_names() {
        assert_eq!(normalize_space_name("  Memes  ").unwrap(), "Memes");
        assert!(normalize_space_name("   ").is_err());
        assert!(normalize_space_name("/etc").is_err());
    }

    #[test]
    fn round_trips_per_user_space() {
        let directory =
            std::env::temp_dir().join(format!("scratchwall-spaces-{}", std::process::id()));
        let path = directory.join("spaces.json");
        let _ = fs::remove_dir_all(&directory);
        let mut bindings = SpaceBindings::load(&path).unwrap();
        assert!(bindings.get(42).is_none());
        bindings.set(42, "  Dump  ").unwrap();
        let reloaded = SpaceBindings::load(&path).unwrap();
        assert_eq!(reloaded.get(42), Some("Dump"));
        let _ = fs::remove_dir_all(directory);
    }
}
