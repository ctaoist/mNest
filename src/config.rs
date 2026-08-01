use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub server: ServerSettings,
    pub database: DatabaseSettings,
    pub queue: QueueSettings,
    pub admin: AdminSettings,
    pub tools: ToolSettings,
    pub cover_cache: CoverCacheSettings,
    pub auth: AuthSettings,
    pub scraper: ScraperSettings,
}

impl Settings {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let expanded = expand_env(&raw)?;
        let mut settings: Self = serde_yaml::from_str(&expanded)
            .with_context(|| format!("invalid yaml in {}", path.display()))?;
        settings.apply_environment_overrides(|key| std::env::var(key).ok());
        Ok(settings)
    }

    fn apply_environment_overrides(&mut self, mut get: impl FnMut(&str) -> Option<String>) {
        if let Some(value) = non_empty_override(get("MNEST_DATABASE_DRIVER")) {
            self.database.driver = value;
        }
        if let Some(value) = non_empty_override(get("MNEST_DATABASE_URL")) {
            self.database.url = value;
        }
        if let Some(value) = non_empty_override(get("MNEST_QUEUE_DRIVER")) {
            self.queue.driver = value;
        }
        if let Some(value) = non_empty_override(get("MNEST_REDIS_URL")) {
            self.queue.redis_url = Some(value);
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.admin.username.trim().is_empty() {
            bail!("admin.username cannot be empty");
        }
        if self.admin.password.len() < 8 {
            bail!("admin.password must contain at least 8 characters");
        }
        if self.auth.jwt_secret.len() < 32 {
            bail!("auth.jwt_secret must contain at least 32 characters");
        }
        if !(1..=512).contains(&self.database.max_connections) {
            bail!("database.max_connections must be between 1 and 512");
        }
        if !(1..=64).contains(&self.queue.workers) {
            bail!("queue.workers must be between 1 and 64");
        }
        if !(1..=100).contains(&self.queue.max_attempts) {
            bail!("queue.max_attempts must be between 1 and 100");
        }
        if !(1..=10_080).contains(&self.auth.access_token_minutes) {
            bail!("auth.access_token_minutes must be between 1 and 10080");
        }
        if !(1..=3_650).contains(&self.auth.refresh_token_days) {
            bail!("auth.refresh_token_days must be between 1 and 3650");
        }
        if !(1..=300).contains(&self.scraper.timeout_seconds) {
            bail!("scraper.timeout_seconds must be between 1 and 300");
        }
        if self.cover_cache.enabled {
            if self.cover_cache.path.as_os_str().is_empty() {
                bail!("cover_cache.path cannot be empty when cover caching is enabled");
            }
            if !self.cover_cache.path.is_absolute() {
                bail!("cover_cache.path must be an absolute path");
            }
            if self.cover_cache.path == Path::new("/") {
                bail!("cover_cache.path cannot be the filesystem root");
            }
        }
        if !(1..=64).contains(&self.cover_cache.concurrency) {
            bail!("cover_cache.concurrency must be between 1 and 64");
        }
        match self.database.driver.as_str() {
            "sqlite" => {
                if !cfg!(feature = "sqlite") {
                    bail!("SQLite support is not enabled in this build; enable the sqlite feature")
                }
                if !self.database.url.starts_with("sqlite:") {
                    bail!("sqlite database.url must start with sqlite:")
                }
            }
            "postgres" => {
                if !cfg!(feature = "postgres") {
                    bail!(
                        "PostgreSQL support is not enabled in this build; enable the postgres feature"
                    )
                }
                if !self.database.url.starts_with("postgres://")
                    && !self.database.url.starts_with("postgresql://")
                {
                    bail!("postgres database.url must be a PostgreSQL DSN")
                }
            }
            other => bail!("unsupported database.driver: {other}"),
        }
        match self.queue.driver.as_str() {
            "database" => {}
            "redis"
                if self
                    .queue
                    .redis_url
                    .as_deref()
                    .is_none_or(|url| url.trim().is_empty()) =>
            {
                bail!("queue.redis_url is required")
            }
            "redis" => {}
            other => bail!("unsupported queue.driver: {other}"),
        }
        Ok(())
    }

    pub fn prepare_runtime(&self) -> anyhow::Result<()> {
        if self.cover_cache.enabled {
            fs::create_dir_all(&self.cover_cache.path).with_context(|| {
                format!(
                    "failed to create cover cache directory {}",
                    self.cover_cache.path.display()
                )
            })?;
            if !self.cover_cache.path.is_dir() {
                bail!(
                    "cover_cache.path is not a directory: {}",
                    self.cover_cache.path.display()
                );
            }
        }
        Ok(())
    }
}

fn non_empty_override(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn expand_env(input: &str) -> anyhow::Result<String> {
    let re = Regex::new(r"\$\{([A-Z0-9_]+)\}")?;
    let mut missing = Vec::new();
    let output = re.replace_all(input, |caps: &regex::Captures<'_>| {
        let key = &caps[1];
        match std::env::var(key) {
            Ok(value) => value,
            Err(_) => {
                missing.push(key.to_owned());
                String::new()
            }
        }
    });
    if !missing.is_empty() {
        bail!("missing environment variables: {}", missing.join(", "));
    }
    Ok(output.into_owned())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub public_url: Option<String>,
}
impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 4535,
            public_url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DatabaseSettings {
    pub driver: String,
    pub url: String,
    pub max_connections: u32,
}
impl Default for DatabaseSettings {
    fn default() -> Self {
        Self {
            driver: "sqlite".into(),
            url: "sqlite:///data/mNest.db?mode=rwc".into(),
            max_connections: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QueueSettings {
    pub driver: String,
    pub redis_url: Option<String>,
    pub workers: usize,
    pub max_attempts: u32,
}
impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            driver: "database".into(),
            redis_url: None,
            workers: 4,
            max_attempts: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdminSettings {
    pub username: String,
    pub password: String,
    pub email: String,
    pub overwrite_existing: bool,
}
impl Default for AdminSettings {
    fn default() -> Self {
        Self {
            username: "admin".into(),
            password: "change-me-now".into(),
            email: "admin@localhost".into(),
            overwrite_existing: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolSettings {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub fpcalc: PathBuf,
    pub taglib: Option<PathBuf>,
}
impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            ffmpeg: "/usr/bin/ffmpeg".into(),
            ffprobe: "/usr/bin/ffprobe".into(),
            fpcalc: "/usr/bin/fpcalc".into(),
            taglib: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CoverCacheSettings {
    pub enabled: bool,
    pub path: PathBuf,
    pub concurrency: usize,
}
impl Default for CoverCacheSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "/data/cache/covers".into(),
            concurrency: 4,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AuthSettings {
    pub jwt_secret: String,
    pub access_token_minutes: i64,
    pub refresh_token_days: i64,
}
impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            jwt_secret: "replace-this-development-secret-32-chars".into(),
            access_token_minutes: 60,
            refresh_token_days: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ScraperSettings {
    pub enabled: Vec<String>,
    pub timeout_seconds: u64,
    pub acoustid_api_key: Option<String>,
    pub provider_options: HashMap<String, serde_yaml::Value>,
}
impl Default for ScraperSettings {
    fn default() -> Self {
        Self {
            enabled: vec![
                "netease".into(),
                "qmusic".into(),
                "migu".into(),
                "kuwo".into(),
                "kugou".into(),
                "acoustid".into(),
            ],
            timeout_seconds: 12,
            acoustid_api_key: None,
            provider_options: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let yaml = r#"
database: { driver: sqlite, url: "sqlite::memory:" }
admin: { username: admin, password: long-enough-password }
auth: { jwt_secret: "12345678901234567890123456789012" }
"#;
        let settings: Settings = serde_yaml::from_str(yaml).unwrap();
        settings.validate().unwrap();
        assert_eq!(settings.server.port, 4535);
        assert!(!settings.cover_cache.enabled);
        assert_eq!(settings.cover_cache.concurrency, 4);
    }

    #[test]
    fn environment_overrides_database_and_queue_connections() {
        let mut settings = Settings::default();
        settings.apply_environment_overrides(|key| {
            Some(
                match key {
                    "MNEST_DATABASE_DRIVER" => "postgres",
                    "MNEST_DATABASE_URL" => "postgres://mNest:secret@postgres/mNest",
                    "MNEST_QUEUE_DRIVER" => "redis",
                    "MNEST_REDIS_URL" => "redis://redis:6379/0",
                    _ => return None,
                }
                .to_owned(),
            )
        });

        assert_eq!(settings.database.driver, "postgres");
        assert_eq!(
            settings.database.url,
            "postgres://mNest:secret@postgres/mNest"
        );
        assert_eq!(settings.queue.driver, "redis");
        assert_eq!(
            settings.queue.redis_url.as_deref(),
            Some("redis://redis:6379/0")
        );
    }

    #[test]
    fn validates_and_prepares_the_cover_cache_directory() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.cover_cache.enabled = true;
        settings.cover_cache.path = directory.path().join("covers");
        settings.validate().unwrap();
        settings.prepare_runtime().unwrap();
        assert!(settings.cover_cache.path.is_dir());

        settings.cover_cache.concurrency = 0;
        assert!(settings.validate().is_err());
        settings.cover_cache.concurrency = 4;

        settings.cover_cache.path = PathBuf::from("relative/covers");
        assert!(settings.validate().is_err());
    }
}
