use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub resources: PathBuf,
    pub storage_name: Option<String>,
    pub allow_empty_referer: bool,
    pub allowed_referers: Vec<String>,
    pub cors_origin: String,
    pub rate_limit_window_secs: u64,
    pub rate_limit_max: u32,
    pub log_level: String,
}

impl Config {
    pub fn from_env() -> Self {
        let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let port = std::env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000);
        let resources = std::env::var("RESOURCES")
            .unwrap_or_else(|_| "./resources".into());
        let storage_name = std::env::var("STORAGE_NAME").ok();
        let allow_empty_referer = std::env::var("ALLOW_EMPTY_REFERER")
            .unwrap_or_default()
            == "true";
        let referers = std::env::var("REFERERS").unwrap_or_default();
        let allowed_referers: Vec<String> = referers
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let cors_origin = std::env::var("CORS_ORIGIN")
            .unwrap_or_else(|_| "http://localhost:3000".into());
        let rate_limit_window = std::env::var("RATE_LIMIT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15);
        let rate_limit_max = std::env::var("RATE_LIMIT_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100);
        let log_level = std::env::var("LOG_LEVEL")
            .unwrap_or_else(|_| "info".into());

        Self {
            host,
            port,
            resources: PathBuf::from(resources),
            storage_name,
            allow_empty_referer,
            allowed_referers,
            cors_origin,
            rate_limit_window_secs: rate_limit_window * 60,
            rate_limit_max,
            log_level,
        }
    }

    pub fn pixiv_root(&self) -> PathBuf {
        self.resources.join("pixiv")
    }

    pub fn plus_root(&self) -> PathBuf {
        self.resources.join("plus")
    }

    pub fn db_path(&self) -> PathBuf {
        PathBuf::from("./data/cache.db")
    }

    pub fn log_dir(&self) -> PathBuf {
        PathBuf::from("./data/logs")
    }
}
