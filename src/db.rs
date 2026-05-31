use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::debug;
use uuid::Uuid;

use crate::config::Config;
use crate::error::AppError;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct FileRow {
    pub id: Option<i64>,
    pub r#type: Option<String>,
    pub dir: Option<String>,
    pub filename: Option<String>,
    pub authorName: Option<String>,
    pub authorId: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub pixiv: Option<FileRow>,
    pub plus: Option<FileRow>,
    pub last_accessed: Instant,
}

impl Session {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            pixiv: None,
            plus: None,
            last_accessed: now,
        }
    }
}

pub struct SessionStore {
    sessions: HashMap<String, Session>,
    last_cleanup: Instant,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            last_cleanup: Instant::now(),
        }
    }

    pub fn get_or_create(&mut self, session_id: Option<&str>) -> (String, &mut Session) {
        self.cleanup_if_needed();

        let id = match session_id {
            Some(id) if self.sessions.contains_key(id) => id.to_string(),
            _ => {
                let new_id = Uuid::new_v4().to_string();
                self.sessions.insert(new_id.clone(), Session::new());
                new_id
            }
        };

        let session = self.sessions.get_mut(&id).unwrap();
        session.last_accessed = Instant::now();

        (id, session)
    }

    pub fn cleanup_if_needed(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_cleanup).as_secs() >= 3600 {
            let cutoff = now - Duration::from_secs(3600);
            let before = self.sessions.len();
            self.sessions.retain(|_, s| s.last_accessed > cutoff);
            let cleaned = before - self.sessions.len();
            if cleaned > 0 {
                debug!("清理了 {} 个过期会话", cleaned);
            }
            self.last_cleanup = now;
        }
    }

}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let db_path = config.db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&db_path)?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                type TEXT,
                dir TEXT,
                filename TEXT,
                authorName TEXT,
                authorId TEXT,
                title TEXT,
                UNIQUE(type, dir, filename)
            );
            ",
        )?;

        conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_files_type ON files(type);
            CREATE INDEX IF NOT EXISTS idx_files_filename ON files(filename);
            CREATE INDEX IF NOT EXISTS idx_files_author ON files(authorName, authorId);
            CREATE INDEX IF NOT EXISTS idx_files_dir ON files(dir);
            ",
        )?;

        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = 1000000;
            PRAGMA temp_store = MEMORY;
            ",
        )?;

        Ok(Self { conn })
    }

    pub fn count_files(&self) -> Result<i64, rusqlite::Error> {
        self.conn
            .query_row("SELECT COUNT(*) as c FROM files", [], |row| row.get(0))
    }

    pub fn get_random_pixiv(&self) -> Result<Option<FileRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM files WHERE type='pixiv' ORDER BY RANDOM() LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], Self::map_file_row)?;
        rows.next().transpose()
    }

    pub fn get_random_plus(&self) -> Result<Option<FileRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM files WHERE type='plus' ORDER BY RANDOM() LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], Self::map_file_row)?;
        rows.next().transpose()
    }

    pub fn find_file(
        &self,
        r#type: &str,
        filename: &str,
    ) -> Result<Option<FileRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT dir FROM files WHERE type=? AND filename=?",
        )?;
        let mut rows = stmt.query_map(params![r#type, filename], |row| {
            Ok(FileRow {
                id: None,
                r#type: None,
                dir: row.get(0)?,
                filename: None,
                authorName: None,
                authorId: None,
                title: None,
            })
        })?;
        rows.next().transpose()
    }

    pub fn search_pixiv(&self, term: &str) -> Result<Option<FileRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM files WHERE type='pixiv' AND filename LIKE ? LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![format!("%{}%", term)], Self::map_file_row)?;
        rows.next().transpose()
    }

    pub fn get_pixiv_pages(
        &self,
        dir: &str,
        prefix: &str,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT filename FROM files WHERE type='pixiv' AND dir=? AND filename LIKE ?",
        )?;
        let rows = stmt.query_map(params![dir, format!("{}%", prefix)], |row| {
            row.get::<_, String>(0)
        })?;
        let mut files: Vec<String> = rows.collect::<Result<Vec<_>, _>>()?;

        files.sort_by(|a, b| {
            let pa = a
                .split("_p")
                .nth(1)
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            let pb = b
                .split("_p")
                .nth(1)
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            pa.cmp(&pb)
        });

        Ok(files)
    }

    pub fn search_plus(
        &self,
        term: &str,
    ) -> Result<Vec<FileRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM files WHERE type='plus' AND (authorName LIKE ? OR authorId LIKE ?)",
        )?;
        let rows = stmt.query_map(
            params![format!("%{}%", term), format!("%{}%", term)],
            Self::map_file_row,
        )?;
        rows.collect()
    }

    pub fn get_all_authors(&self) -> Result<Vec<FileRow>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT authorName, authorId FROM files WHERE type='plus'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(FileRow {
                id: None,
                r#type: None,
                dir: None,
                filename: None,
                authorName: row.get(0)?,
                authorId: row.get(1)?,
                title: None,
            })
        })?;
        rows.collect()
    }

    pub fn insert_file(&self, row: &FileRow) -> Result<(), rusqlite::Error> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO files (type, dir, filename, authorName, authorId, title)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.r#type,
                    row.dir,
                    row.filename,
                    row.authorName,
                    row.authorId,
                    row.title,
                ],
            )
            .map(|_| ())
    }

    pub fn delete_file(
        &self,
        r#type: &str,
        dir: &str,
        filename: &str,
    ) -> Result<(), rusqlite::Error> {
        self.conn
            .execute(
                "DELETE FROM files WHERE type=? AND dir=? AND filename=?",
                params![r#type, dir, filename],
            )
            .map(|_| ())
    }

    fn map_file_row(row: &rusqlite::Row) -> rusqlite::Result<FileRow> {
        Ok(FileRow {
            id: row.get(0)?,
            r#type: row.get(1)?,
            dir: row.get(2)?,
            filename: row.get(3)?,
            authorName: row.get(4)?,
            authorId: row.get(5)?,
            title: row.get(6)?,
        })
    }
}

#[derive(Clone)]
pub struct RateLimiter {
    state: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    window: Duration,
    max_requests: u32,
}

impl RateLimiter {
    pub fn new(window_secs: u64, max_requests: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            window: Duration::from_secs(window_secs),
            max_requests,
        }
    }

    pub async fn check(&self, key: &str) -> Result<(), AppError> {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let cutoff = now - self.window;

        let entries = state.entry(key.to_string()).or_default();
        entries.retain(|t| *t > cutoff);

        if entries.len() >= self.max_requests as usize {
            return Err(AppError::new(
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMIT_EXCEEDED",
                "请求过于频繁，请稍后再试",
            ));
        }

        entries.push(now);
        Ok(())
    }
}
