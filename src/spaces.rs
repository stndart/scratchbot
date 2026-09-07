use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

const MAX_NAME: usize = 200;

#[derive(Default, Serialize, Deserialize)]
struct BindingsFile {
    #[serde(default)]
    spaces: HashMap<String, String>,
    #[serde(default)]
    logins: HashMap<String, String>,
}

pub struct SpaceBindings {
    path: PathBuf,
    spaces: HashMap<i64, String>,
    logins: HashMap<i64, String>,
}

impl SpaceBindings {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let (spaces, logins) = match fs::read_to_string(&path) {
            Ok(text) => parse_bindings_file(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (HashMap::new(), HashMap::new())
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            spaces,
            logins,
        })
    }

    pub fn get(&self, user_id: i64) -> Option<&str> {
        self.spaces.get(&user_id).map(String::as_str)
    }

    pub fn login(&self, user_id: i64) -> Option<&str> {
        self.logins.get(&user_id).map(String::as_str)
    }

    pub fn set(&mut self, user_id: i64, name: &str) -> Result<String> {
        let name = normalize_name("folder", name)?;
        self.spaces.insert(user_id, name.clone());
        self.save()?;
        Ok(name)
    }

    pub fn set_login(&mut self, user_id: i64, name: &str) -> Result<String> {
        let name = normalize_name("login", name)?;
        self.logins.insert(user_id, name.clone());
        self.save()?;
        Ok(name)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = BindingsFile {
            spaces: stringify_map(&self.spaces),
            logins: stringify_map(&self.logins),
        };
        fs::write(&self.path, serde_json::to_string_pretty(&file)?)?;
        Ok(())
    }
}

pub fn normalize_space_name(name: &str) -> Result<String> {
    normalize_name("folder", name)
}

fn normalize_name(kind: &str, name: &str) -> Result<String> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "{kind} is required");
    anyhow::ensure!(name.len() <= MAX_NAME, "{kind} is too long");
    anyhow::ensure!(!name.starts_with('/'), "{kind} cannot start with /");
    Ok(name.to_owned())
}

fn stringify_map(map: &HashMap<i64, String>) -> HashMap<String, String> {
    map.iter()
        .map(|(id, name)| (id.to_string(), name.clone()))
        .collect()
}

fn parse_id_map(raw: HashMap<String, String>, kind: &str) -> HashMap<i64, String> {
    let mut by_user = HashMap::new();
    for (key, name) in raw {
        if let Ok(id) = key.parse::<i64>()
            && let Ok(name) = normalize_name(kind, &name)
        {
            by_user.insert(id, name);
        }
    }
    by_user
}

fn parse_bindings_file(text: &str) -> Result<(HashMap<i64, String>, HashMap<i64, String>)> {
    let file: BindingsFile = serde_json::from_str(text)?;
    Ok((
        parse_id_map(file.spaces, "folder"),
        parse_id_map(file.logins, "login"),
    ))
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
    fn round_trips_per_user_space_and_login() {
        let directory =
            std::env::temp_dir().join(format!("scratchwall-spaces-{}", std::process::id()));
        let path = directory.join("spaces.json");
        let _ = fs::remove_dir_all(&directory);
        let mut bindings = SpaceBindings::load(&path).unwrap();
        assert!(bindings.get(42).is_none());
        assert!(bindings.login(42).is_none());
        bindings.set(42, "  Dump  ").unwrap();
        bindings.set_login(42, "  Svyat  ").unwrap();
        let reloaded = SpaceBindings::load(&path).unwrap();
        assert_eq!(reloaded.get(42), Some("Dump"));
        assert_eq!(reloaded.login(42), Some("Svyat"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn loads_legacy_spaces_file_without_logins() {
        let text = r#"{"spaces":{"7":"Memes"}}"#;
        let (spaces, logins) = parse_bindings_file(text).unwrap();
        assert_eq!(spaces.get(&7).map(String::as_str), Some("Memes"));
        assert!(logins.is_empty());
    }
}
