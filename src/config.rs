use crate::install::Result;
use serde::Deserialize;
use std::{
    env,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct Config {
    pub download_dir: PathBuf,
    pub max_cache_count: usize,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    download_dir: Option<String>,
    max_cache_count: Option<usize>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or("HOME must be set to an absolute directory")?;
        let config_home = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty());
        Self::load_at(Path::new(&home), config_home.as_deref())
    }

    fn load_at(home: &Path, config_home: Option<&OsStr>) -> Result<Self> {
        if !home.is_absolute() {
            return Err("HOME must be an absolute directory".into());
        }
        let config_home = config_home
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        if !config_home.is_absolute() {
            return Err("XDG_CONFIG_HOME must be an absolute directory".into());
        }
        let path = config_home.join("vscu/config.toml");
        let settings = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|error| format!("Invalid configuration {}: {error}", path.display()))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Settings::default(),
            Err(error) => {
                return Err(
                    format!("Cannot read configuration {}: {error}", path.display()).into(),
                );
            }
        };
        Self::resolve(home, settings)
    }

    fn resolve(home: &Path, settings: Settings) -> Result<Self> {
        let download_dir = match settings.download_dir {
            None => home.join(".vscu"),
            Some(path) => match path.strip_prefix("~/") {
                Some(relative) => home.join(relative.trim_start_matches('/')),
                None => PathBuf::from(path),
            },
        };
        if !download_dir.is_absolute() {
            return Err("download_dir must be an absolute path or start with ~/".into());
        }
        let max_cache_count = settings.max_cache_count.unwrap_or(3);
        if max_cache_count == 0 {
            return Err("max_cache_count must be at least 1".into());
        }
        Ok(Self {
            download_dir,
            max_cache_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_uses_defaults_without_creating_files() {
        let home = tempfile::tempdir().unwrap();
        let config = Config::load_at(home.path(), None).unwrap();
        assert_eq!(config.download_dir, home.path().join(".vscu"));
        assert_eq!(config.max_cache_count, 3);
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[test]
    fn reads_xdg_config_and_expands_home_with_partial_defaults() {
        let home = tempfile::tempdir().unwrap();
        let xdg = home.path().join("config-elsewhere");
        fs::create_dir_all(xdg.join("vscu")).unwrap();
        let path = xdg.join("vscu/config.toml");
        fs::write(&path, "download_dir = '~/packages'\nmax_cache_count = 5\n").unwrap();
        let config = Config::load_at(home.path(), Some(xdg.as_os_str())).unwrap();
        assert_eq!(config.download_dir, home.path().join("packages"));
        assert_eq!(config.max_cache_count, 5);
        fs::write(&path, "max_cache_count = 1").unwrap();
        let config = Config::load_at(home.path(), Some(xdg.as_os_str())).unwrap();
        assert_eq!(config.download_dir, home.path().join(".vscu"));
        assert_eq!(config.max_cache_count, 1);
        fs::write(&path, "download_dir = '/somewhere/packages'").unwrap();
        let config = Config::load_at(home.path(), Some(xdg.as_os_str())).unwrap();
        assert_eq!(config.download_dir, Path::new("/somewhere/packages"));
        assert_eq!(config.max_cache_count, 3);
        fs::write(&path, "download_dir = '~//packages'").unwrap();
        let config = Config::load_at(home.path(), Some(xdg.as_os_str())).unwrap();
        assert_eq!(config.download_dir, home.path().join("packages"));
    }

    #[test]
    fn rejects_invalid_settings_instead_of_silently_using_defaults() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/vscu")).unwrap();
        for text in [
            "download_dir = 'relative'",
            "download_dir = ''",
            "download_dir = '~someone/cache'",
            "max_cache_count = 0",
            "max_cache_count = -1",
            "max_cache_count = 1.5",
            "max_cache_count = '3'",
            "max_cahce_count = 5",
            "invalid toml",
        ] {
            fs::write(home.path().join(".config/vscu/config.toml"), text).unwrap();
            assert!(
                Config::load_at(home.path(), None).is_err(),
                "accepted {text}"
            );
        }
        assert!(Config::load_at(Path::new("relative"), None).is_err());
        assert!(Config::load_at(home.path(), Some(OsStr::new("relative"))).is_err());
    }
}
