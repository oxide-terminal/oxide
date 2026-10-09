//! Where user state lives, and the one-time move from the old name.
//!
//! The app was called Oxide until 0.9.0 and kept its files under
//! `~/.config/oxide` and `~/.cache/oxide`. The first launch of a renamed
//! build copies what matters into the `omnipty` directories; the originals
//! are left alone so an older copy that still runs keeps working. Delete
//! this migration once no installed copy predates the rename.

use std::path::{Path, PathBuf};

const NEW: &str = "omnipty";
const OLD: &str = "oxide";

/// The files worth carrying over. Everything else in the cache is generated
/// on launch or is a per-session handoff file.
const CACHE_FILES: &[&str] = &["workspaces.json", "last_version.txt", "window.txt"];

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `~/.config/omnipty/config.toml`, or the `OMNIPTY_CONFIG` override
/// (`OXIDE_CONFIG` still works).
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("OMNIPTY_CONFIG").or_else(|| std::env::var_os("OXIDE_CONFIG")) {
        return PathBuf::from(p);
    }
    config_path_in(&home())
}

/// `~/.cache/omnipty`, created and seeded from `~/.cache/oxide` on first use.
pub fn cache_dir() -> PathBuf {
    cache_dir_in(&home())
}

fn config_path_in(home: &Path) -> PathBuf {
    let new = home.join(".config").join(NEW).join("config.toml");
    let old = home.join(".config").join(OLD).join("config.toml");
    if !new.exists() && old.is_file() {
        if let Some(dir) = new.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::copy(&old, &new) {
            Ok(_) => eprintln!("omnipty: copied {} to {}", old.display(), new.display()),
            Err(e) => eprintln!("omnipty: couldn't copy {}: {e}", old.display()),
        }
    }
    new
}

fn cache_dir_in(home: &Path) -> PathBuf {
    let new = home.join(".cache").join(NEW);
    let old = home.join(".cache").join(OLD);
    // The new directory existing is the "already migrated" marker: never
    // overwrite newer omnipty state with older oxide state.
    if !new.exists() && old.is_dir() && std::fs::create_dir_all(&new).is_ok() {
        for name in CACHE_FILES {
            let from = old.join(name);
            if from.is_file() {
                let _ = std::fs::copy(&from, new.join(name));
            }
        }
    }
    new
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omnipty-paths-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn old_state_is_copied_once_and_never_overwrites() {
        let home = scratch("migrate");
        std::fs::create_dir_all(home.join(".config/oxide")).unwrap();
        std::fs::write(home.join(".config/oxide/config.toml"), "a = 1").unwrap();
        std::fs::create_dir_all(home.join(".cache/oxide/cd")).unwrap();
        std::fs::write(home.join(".cache/oxide/workspaces.json"), "[]").unwrap();
        std::fs::write(home.join(".cache/oxide/cd/42"), "/tmp").unwrap();

        assert_eq!(
            std::fs::read_to_string(config_path_in(&home)).unwrap(),
            "a = 1"
        );
        let cache = cache_dir_in(&home);
        assert_eq!(std::fs::read_to_string(cache.join("workspaces.json")).unwrap(), "[]");
        assert!(!cache.join("cd").exists(), "per-session files aren't migrated");
        assert!(home.join(".config/oxide/config.toml").exists(), "old copy left in place");

        // Newer state under the new name wins over the old directory.
        std::fs::write(home.join(".config/omnipty/config.toml"), "a = 2").unwrap();
        std::fs::write(cache.join("workspaces.json"), "[1]").unwrap();
        assert_eq!(std::fs::read_to_string(config_path_in(&home)).unwrap(), "a = 2");
        assert_eq!(
            std::fs::read_to_string(cache_dir_in(&home).join("workspaces.json")).unwrap(),
            "[1]"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn fresh_install_has_nothing_to_migrate() {
        let home = scratch("fresh");
        assert_eq!(config_path_in(&home), home.join(".config/omnipty/config.toml"));
        assert!(!home.join(".config/omnipty").exists(), "nothing created until written");
        assert_eq!(cache_dir_in(&home), home.join(".cache/omnipty"));
        let _ = std::fs::remove_dir_all(home);
    }
}
