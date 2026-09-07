use std::{collections::HashSet, fs, io::ErrorKind, path::Path};

pub fn load(path: &Path) -> anyhow::Result<i64> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text.trim().parse().unwrap_or(0)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

pub fn save(path: &Path, offset: i64) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{offset}\n"))?;
    Ok(())
}

pub fn next_offset(saved: i64, processed: &HashSet<i64>, seen: &[i64]) -> i64 {
    let mut ids: Vec<i64> = seen
        .iter()
        .copied()
        .chain(processed.iter().copied())
        .filter(|id| *id >= saved)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let mut candidate = saved;
    for id in ids {
        if processed.contains(&id) {
            candidate = id + 1;
        } else {
            break;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn holds_offset_at_first_unprocessed_update() {
        let processed = HashSet::from([12]);
        assert_eq!(next_offset(10, &processed, &[10, 11, 12]), 10);
        let processed = HashSet::from([10, 11, 12]);
        assert_eq!(next_offset(10, &processed, &[10, 11, 12]), 13);
        let processed = HashSet::from([10, 11]);
        assert_eq!(next_offset(10, &processed, &[]), 12);
    }

    #[test]
    fn round_trips_offset_file() {
        let directory =
            std::env::temp_dir().join(format!("scratchwall-offset-{}", std::process::id()));
        let path = directory.join("telegram-offset");
        save(&path, 42).unwrap();
        assert_eq!(load(&path).unwrap(), 42);
        let _ = fs::remove_dir_all(directory);
    }
}
