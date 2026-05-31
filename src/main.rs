mod config;
mod db;
mod error;
mod routes;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::event::EventKind;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::Mutex;
use axum::http::{header, HeaderValue, Method};
use tower::ServiceBuilder;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::compression::CompressionLayer;
use tracing::{error, info, warn};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::prelude::*;

use crate::config::Config;
use crate::db::{Db, FileRow, RateLimiter, SessionStore};
use crate::routes::AppState;

// ===================== Logging =====================

fn init_logging(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let log_dir = config.log_dir();
    std::fs::create_dir_all(&log_dir)?;

    let log_level = config.log_level.clone();
    let env_filter = EnvFilter::builder()
        .with_default_directive(log_level.parse()?)
        .from_env_lossy();

    // Console layer (with color)
    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(true)
        .with_target(false)
        .with_filter(env_filter.clone());

    // File layer (JSON format)
    let error_file = tracing_appender::rolling::never(
        &log_dir,
        "error.log",
    );
    let combined_file = tracing_appender::rolling::never(
        &log_dir,
        "combined.log",
    );

    let (non_blocking_error, _guard1) = tracing_appender::non_blocking(error_file);
    let (non_blocking_combined, _guard2) = tracing_appender::non_blocking(combined_file);

    let error_file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(non_blocking_error)
        .with_target(true)
        .with_filter(
            EnvFilter::builder()
                .with_default_directive("error".parse()?)
                .from_env_lossy(),
        );

    let combined_file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(non_blocking_combined)
        .with_target(true);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(error_file_layer)
        .with(combined_file_layer)
        .init();

    Ok(())
}

// ===================== File watching =====================

fn start_file_watching(
    config: &Config,
    db: Arc<Mutex<Db>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let pixiv_root = config.pixiv_root();
    let plus_root = config.plus_root();

    if pixiv_root.exists() {
        spawn_watcher("pixiv", pixiv_root, db.clone());
    } else {
        warn!("监听目录不存在: {:?}", pixiv_root);
    }

    if plus_root.exists() {
        spawn_watcher("plus", plus_root, db.clone());
    } else {
        warn!("监听目录不存在: {:?}", plus_root);
    }

    Ok(())
}

fn spawn_watcher(r#type: &'static str, root: PathBuf, db: Arc<Mutex<Db>>) {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Event, notify::Error>>();

        let mut watcher = match RecommendedWatcher::new(
            move |event| {
                let _ = tx.send(event);
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                error!("文件监听器创建失败 ({}): {}", r#type, e);
                return;
            }
        };

        if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
            error!("开始监听目录失败 ({:?}): {}", root, e);
            return;
        }

        info!("开始监听目录: {:?} ({})", root, r#type);

        let mut last_event: HashMap<PathBuf, Instant> = HashMap::new();

        loop {
            match rx.recv() {
                Ok(Ok(event)) => {
                    // 只处理创建和修改事件，忽略访问事件
                    let is_create = matches!(event.kind, EventKind::Create(_));
                    let is_modify = matches!(event.kind, EventKind::Modify(_));
                    if !is_create && !is_modify {
                        continue;
                    }

                    let now = Instant::now();

                    for path in &event.paths {
                        if let Ok(relative) = path.strip_prefix(&root) {
                            let components: Vec<_> = relative.components().collect();
                            if components.len() < 2 {
                                continue;
                            }

                            // Check if directory is numeric (dir name)
                            let dir_name = components[0]
                                .as_os_str()
                                .to_str()
                                .unwrap_or("")
                                .to_string();
                            let file_name = components[1]
                                .as_os_str()
                                .to_str()
                                .unwrap_or("")
                                .to_string();

                            if !dir_name.chars().all(|c| c.is_ascii_digit()) {
                                continue;
                            }

                            // Filter by extension
                            let lower = file_name.to_lowercase();
                            if r#type == "pixiv"
                                && !lower.ends_with(".jpg")
                                && !lower.ends_with(".jpeg")
                                && !lower.ends_with(".png")
                                && !lower.ends_with(".gif")
                                && !lower.ends_with(".webp")
                            {
                                continue;
                            }
                            if r#type == "plus" && !lower.ends_with(".mp4") {
                                continue;
                            }

                            // Debounce check
                            if let Some(last) = last_event.get(path) {
                                if now.duration_since(*last) < Duration::from_secs(1) {
                                    continue;
                                }
                            }
                            last_event.insert(path.clone(), now);

                            // Small sleep for debounce
                            std::thread::sleep(Duration::from_millis(1000));

                            let full_path = root.join(&dir_name).join(&file_name);

                            if let Ok(db) = db.try_lock() {
                                if full_path.exists() {
                                    if r#type == "plus" {
                                        let parsed = parse_plus_filename(&file_name);
                                        let row = FileRow {
                                            id: None,
                                            r#type: Some(r#type.to_string()),
                                            dir: Some(dir_name),
                                            filename: Some(file_name.clone()),
                                            authorName: parsed.as_ref().map(|p| p.0.clone()),
                                            authorId: parsed.as_ref().map(|p| p.1.clone()),
                                            title: parsed.map(|p| p.2),
                                        };
                                        if let Err(e) = db.insert_file(&row) {
                                            error!("文件插入失败: {}", e);
                                        }
                                    } else {
                                        let row = FileRow {
                                            id: None,
                                            r#type: Some(r#type.to_string()),
                                            dir: Some(dir_name),
                                            filename: Some(file_name.clone()),
                                            authorName: None,
                                            authorId: None,
                                            title: None,
                                        };
                                        if let Err(e) = db.insert_file(&row) {
                                            error!("文件插入失败: {}", e);
                                        }
                                    }
                                    info!("文件新增: {} ({})", file_name, r#type);
                                } else {
                                    if let Err(e) = db.delete_file(r#type, &dir_name, &file_name) {
                                        error!("文件删除失败: {}", e);
                                    }
                                    info!("文件删除: {} ({})", file_name, r#type);
                                }
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!("文件监听器错误 ({}): {}", r#type, e);
                }
                Err(_) => {
                    info!("文件监听通道关闭 ({})", r#type);
                    break;
                }
            }
        }
    });
}

fn parse_plus_filename(filename: &str) -> Option<(String, String, String)> {
    let name = Path::new(filename).file_stem()?.to_str()?;
    let parts: Vec<&str> = name.split('_').collect();
    if parts.len() < 3 {
        return None;
    }
    Some((
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2..].join("_"),
    ))
}

// ===================== Cache initialization =====================

async fn init_cache(config: &Config, db: &Mutex<Db>) -> Result<(), Box<dyn std::error::Error>> {
    {
        let conn = db.lock().await;
        let count = conn.count_files()?;
        if count > 0 {
            info!("缓存已存在，跳过初始化");
            return Ok(());
        }
    }

    info!("初始化缓存...");

    let mut rows: Vec<FileRow> = Vec::new();

    async fn scan_dir(
        root: &Path,
        r#type: &str,
        rows: &mut Vec<FileRow>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !root.exists() {
            warn!("资源目录不存在: {:?}", root);
            return Ok(());
        }

        let mut dir_entries = tokio::fs::read_dir(root).await?;
        while let Some(entry) = dir_entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }

            let dir_name = entry.file_name();
            let dir_name_str = dir_name.to_str().unwrap_or("");
            if !dir_name_str.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }

            let mut file_entries = tokio::fs::read_dir(entry.path()).await?;
            while let Some(file_entry) = file_entries.next_entry().await? {
                let file_name = file_entry.file_name();
                let file_name_str = file_name.to_str().unwrap_or("").to_string();
                let lower = file_name_str.to_lowercase();

                if r#type == "pixiv"
                    && !lower.ends_with(".jpg")
                    && !lower.ends_with(".jpeg")
                    && !lower.ends_with(".png")
                    && !lower.ends_with(".gif")
                    && !lower.ends_with(".webp")
                {
                    continue;
                }
                if r#type == "plus" && !lower.ends_with(".mp4") {
                    continue;
                }

                let (author_name, author_id, title) = if r#type == "plus" {
                    parse_plus_filename(&file_name_str)
                        .map(|(a, i, t)| (Some(a), Some(i), Some(t)))
                        .unwrap_or((None, None, None))
                } else {
                    (None, None, None)
                };

                rows.push(FileRow {
                    id: None,
                    r#type: Some(r#type.to_string()),
                    dir: Some(dir_name_str.to_string()),
                    filename: Some(file_name_str),
                    authorName: author_name,
                    authorId: author_id,
                    title,
                });
            }
        }
        Ok(())
    }

    scan_dir(&config.pixiv_root(), "pixiv", &mut rows).await?;
    scan_dir(&config.plus_root(), "plus", &mut rows).await?;

    let conn = db.lock().await;
    if !rows.is_empty() {
        for row in &rows {
            conn.insert_file(row)?;
        }
        info!("缓存 {} 条记录", rows.len());
    } else {
        warn!("没有找到任何资源文件");
    }

    Ok(())
}

// ===================== Main =====================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = std::env::current_dir()?.join(".env");
    if config_path.exists() {
        dotenvy::from_path(&config_path).ok();
    }
    dotenvy::dotenv().ok();

    let config = Config::from_env();

    init_logging(&config)?;

    let db = Arc::new(Mutex::new(Db::open(&config)?));
    let sessions = Arc::new(Mutex::new(SessionStore::new()));
    let rate_limiter = RateLimiter::new(config.rate_limit_window_secs, config.rate_limit_max);

    // Cache initialization
    if let Err(e) = init_cache(&config, &db).await {
        error!("缓存初始化失败: {}", e);
        return Err(e);
    }

    // File watching
    if let Err(e) = start_file_watching(&config, db.clone()) {
        error!("文件监听启动失败: {}", e);
        return Err(e);
    }

    // Build CORS layer
    let cors_origins: Vec<_> = config
        .cors_origin
        .split(',')
        .filter_map(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.parse::<HeaderValue>().unwrap())
            }
        })
        .collect();

    let cors = if cors_origins.len() == 1 {
        CorsLayer::new()
            .allow_origin(cors_origins.into_iter().next().unwrap())
            .allow_credentials(true)
            .allow_methods([Method::GET])
            .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
    } else {
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(cors_origins))
            .allow_credentials(true)
            .allow_methods([Method::GET])
            .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
    };

    let compression = CompressionLayer::new();

    let storage_name = config.storage_name.clone();

    let state = Arc::new(AppState {
        config: config.clone(),
        db,
        sessions,
        rate_limiter,
        storage_name,
    });

    let app = routes::build_router(state)
        .layer(ServiceBuilder::new()
            .layer(cors)
            .layer(compression));

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("PixHub服务器启动成功");
    info!("访问地址: http://{}", addr);
    info!("存储地: {}", config.storage_name.as_deref().unwrap_or("未设置"));
    info!(
        "速率限制: {} 请求/{}分钟",
        config.rate_limit_max,
        config.rate_limit_window_secs / 60
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("无法安装Ctrl+C信号处理器");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("无法安装SIGTERM信号处理器")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("收到SIGINT信号，正在优雅关闭...");
        }
        _ = terminate => {
            info!("收到SIGTERM信号，正在优雅关闭...");
        }
    }
}
