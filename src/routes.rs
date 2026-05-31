use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::error;

use crate::config::Config;
use crate::db::{Db, RateLimiter, SessionStore};
use crate::error::AppError;

#[derive(Deserialize)]
pub struct InfoQuery {
    pub list: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: Arc<Mutex<Db>>,
    pub sessions: Arc<Mutex<SessionStore>>,
    pub rate_limiter: RateLimiter,
    pub storage_name: Option<String>,
}

// ===================== Pixiv filename parsing =====================

#[derive(Debug, Clone, Serialize)]
struct ParsedPixiv {
    base: String,
    pid: String,
    page: Option<String>,
}

fn parse_pixiv_filename(filename: &str) -> Option<ParsedPixiv> {
    let name = Path::new(filename).file_stem()?.to_str()?;

    // Try with page number: base_pid_p\d+
    if let Some(p_page) = name.rfind("_p") {
        let after_p = &name[p_page + 2..];
        if !after_p.is_empty() && after_p.chars().all(|c| c.is_ascii_digit()) {
            let before_page = &name[..p_page];
            if let Some(last_underscore) = before_page.rfind('_') {
                let pid = &before_page[last_underscore + 1..];
                if !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) {
                    let base = &before_page[..last_underscore];
                    return Some(ParsedPixiv {
                        base: base.to_string(),
                        pid: pid.to_string(),
                        page: Some(after_p.to_string()),
                    });
                }
            }
        }
    }

    // Try without page: base_pid
    if let Some(last_underscore) = name.rfind('_') {
        let pid = &name[last_underscore + 1..];
        if !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()) {
            let base = &name[..last_underscore];
            return Some(ParsedPixiv {
                base: base.to_string(),
                pid: pid.to_string(),
                page: None,
            });
        }
    }

    None
}

// ===================== Utility functions =====================

fn get_host(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next().map(|s| s.trim()))
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("{}://{}", proto, host)
}

fn check_referer(headers: &HeaderMap, config: &Config) -> bool {
    let ref_val = headers.get("referer").and_then(|v| v.to_str().ok());
    match ref_val {
        None => config.allow_empty_referer,
        Some(r) => {
            if config.allowed_referers.is_empty() {
                return true;
            }
            config.allowed_referers.iter().any(|allowed| r.starts_with(allowed))
        }
    }
}

fn validate_referer(headers: &HeaderMap, config: &Config) -> Result<(), AppError> {
    if !check_referer(headers, config) {
        return Err(AppError::with_details(
            StatusCode::FORBIDDEN,
            "REFERER_FORBIDDEN",
            "访问被拒绝，请检查referer设置",
            json!({
                "referer": headers.get("referer").and_then(|v| v.to_str().ok()).unwrap_or("空"),
                "allowedReferers": config.allowed_referers,
                "allowEmpty": config.allow_empty_referer,
            }),
        ));
    }
    Ok(())
}

fn verify_resource_exists(file_path: &Path, res_root: &Path) -> Result<(), AppError> {
    if file_path.strip_prefix(res_root).is_err() {
        return Err(AppError::with_details(
            StatusCode::FORBIDDEN,
            "PATH_TRAVERSAL",
            "路径访问被拒绝",
            json!({ "requestedPath": file_path }),
        ));
    }
    if !file_path.exists() {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "RESOURCE_NOT_FOUND",
            "请求的资源不存在",
        ));
    }
    Ok(())
}

fn sanitize_search_term(term: &str) -> String {
    term.chars().filter(|c| *c != '%' && *c != '_').collect()
}

fn get_session_id(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    if let Some(sid) = query.get("sessionId") {
        return Some(sid.clone());
    }
    if let Some(sid) = headers.get("x-session-id").and_then(|v| v.to_str().ok()) {
        return Some(sid.to_string());
    }
    if let Some(cookie) = headers.get("cookie").and_then(|v| v.to_str().ok()) {
        for pair in cookie.split(';') {
            let pair = pair.trim();
            if let Some(val) = pair.strip_prefix("sessionId=") {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn content_type_for_ext(ext: &str) -> &'static str {
    match ext {
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        _ => "image/jpeg",
    }
}

fn api_response(data: Value, storage_name: &Option<String>) -> Response {
    json_response(json!({ "data": data }), storage_name)
}

fn json_response(mut body: Value, storage_name: &Option<String>) -> Response {
    body["error"] = json!(false);
    body["message"] = json!("success");
    if let Some(name) = storage_name {
        body["storage"] = json!(name);
    }
    let json = serde_json::to_string_pretty(&body).unwrap_or_default();
    Response::builder()
        .header("Content-Type", "application/json; charset=utf-8")
        .body(Body::from(json))
        .unwrap()
}

// ===================== Index & static files =====================

async fn index_handler() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../public/index.html"))
}

async fn css_handler() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/css; charset=utf-8")
        .body(Body::from(include_str!("../public/style.css")))
        .unwrap()
}

async fn js_handler() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "application/javascript; charset=utf-8")
        .body(Body::from(include_str!("../public/script.js")))
        .unwrap()
}

// ===================== Pixiv routes =====================

async fn pixiv_random_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    state.rate_limiter.check("pixiv_random").await?;

    let db = state.db.lock().await;
    let row = db.get_random_pixiv().map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "NO_PIXIV_FILES", "没有找到任何pixiv图片"))?;

    let parsed = parse_pixiv_filename(row.filename.as_deref().unwrap_or(""))
        .ok_or_else(|| AppError::with_details(
            StatusCode::BAD_REQUEST,
            "FILENAME_PARSE_ERROR",
            "文件名解析失败",
            json!({ "filename": row.filename, "expectedFormat": "base_pid[_p页码]" }),
        ))?;

    let prefix = format!("{}_{}", parsed.base, parsed.pid);
    let pages = db.get_pixiv_pages(
        row.dir.as_deref().unwrap_or(""),
        &prefix,
    ).map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?;

    let host = get_host(&headers);

    // Session
    let query_map = query.0;
    let session_id = get_session_id(&headers, &query_map);
    let mut sessions = state.sessions.lock().await;
    let (sid, session) = sessions.get_or_create(session_id.as_deref());
    session.pixiv = Some(row.clone());

    let mut body = json!({
        "data": {
            "illustId": parsed.pid,
            "title": parsed.base,
            "pageCount": pages.len(),
            "urls": pages.iter().map(|p| {
                format!("{}/pixiv/artworks/file/{}", host, urlencoding::encode(p))
            }).collect::<Vec<_>>(),
            "fileInfo": {
                "dir": row.dir,
                "filename": row.filename,
            },
        },
    });
    body["sessionId"] = json!(sid);
    Ok(json_response(body, &state.storage_name))
}

async fn pixiv_info_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Query<InfoQuery>,
) -> Result<Response, AppError> {
    state.rate_limiter.check("pixiv_info").await?;

    let list = query.list.as_deref().ok_or_else(|| {
        AppError::with_details(
            StatusCode::BAD_REQUEST,
            "MISSING_PARAMETER",
            "缺少必要参数",
            json!({ "parameter": "list", "description": "需要提供文件名关键字" }),
        )
    })?;

    let search_term = sanitize_search_term(list);

    let db = state.db.lock().await;
    let row = db.search_pixiv(&search_term).map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?
    .ok_or_else(|| AppError::with_details(
        StatusCode::NOT_FOUND,
        "FILE_NOT_FOUND",
        "未找到匹配的图片",
        json!({ "query": list }),
    ))?;

    let parsed = parse_pixiv_filename(row.filename.as_deref().unwrap_or(""))
        .ok_or_else(|| AppError::with_details(
            StatusCode::BAD_REQUEST,
            "FILENAME_PARSE_ERROR",
            "文件名解析失败",
            json!({ "filename": row.filename, "expectedFormat": "base_pid[_p页码]" }),
        ))?;

    let prefix = format!("{}_{}", parsed.base, parsed.pid);
    let pages = db.get_pixiv_pages(
        row.dir.as_deref().unwrap_or(""),
        &prefix,
    ).map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?;

    if pages.is_empty() {
        return Err(AppError::with_details(
            StatusCode::NOT_FOUND,
            "PAGES_NOT_FOUND",
            "未找到相关页面",
            json!({ "illustId": parsed.pid }),
        ));
    }

    let host = get_host(&headers);

    Ok(api_response(json!({
        "illustId": parsed.pid,
        "title": parsed.base,
        "pageCount": pages.len(),
        "urls": pages.iter().map(|p| {
            format!("{}/pixiv/artworks/file/{}", host, urlencoding::encode(p))
        }).collect::<Vec<_>>(),
        "fileInfo": {
            "dir": row.dir,
            "filename": row.filename,
        },
    }), &state.storage_name))
}

async fn pixiv_view_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    validate_referer(&headers, &state.config)?;
    state.rate_limiter.check("pixiv_view").await?;

    let query_map = query.0;
    let session_id = get_session_id(&headers, &query_map);
    let mut sessions = state.sessions.lock().await;
    let (sid, session) = sessions.get_or_create(session_id.as_deref());

    let db = state.db.lock().await;
    let row = db.get_random_pixiv().map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?;
    session.pixiv = row;
    drop(db);

    let row = session.pixiv.as_ref().ok_or_else(|| {
        AppError::new(StatusCode::NOT_FOUND, "NO_PIXIV_FILES", "没有找到任何pixiv图片")
    })?;

    let file_path = state.config.pixiv_root()
        .join(row.dir.as_deref().unwrap_or(""))
        .join(row.filename.as_deref().unwrap_or(""));

    verify_resource_exists(&file_path, &state.config.resources)?;

    let filename = row.filename.as_deref().unwrap_or("unknown");
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();
    let content_type = content_type_for_ext(&ext);

    let data = tokio::fs::read(&file_path).await.map_err(|_| {
        AppError::new(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND", "请求的资源不存在")
    })?;

    let mut resp = Response::new(Body::from(data));
    resp.headers_mut().insert(
        "Content-Type",
        content_type.parse().unwrap(),
    );
    resp.headers_mut().insert(
        "Cache-Control",
        "public, max-age=86400".parse().unwrap(),
    );
    resp.headers_mut().insert(
        "Content-Disposition",
        format!("inline; filename=\"{}\"", urlencoding::encode(filename))
            .parse()
            .unwrap(),
    );
    resp.headers_mut().insert(
        "Set-Cookie",
        format!("sessionId={}; Max-Age=3600; HttpOnly; SameSite=Strict; Path=/", sid)
            .parse()
            .unwrap(),
    );

    Ok(resp)
}

async fn pixiv_file_by_name_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(filename): AxumPath<String>,
) -> Result<Response, AppError> {
    validate_referer(&headers, &state.config)?;

    serve_media_file(&state, "pixiv", &filename, &headers).await
}

async fn pixiv_file_from_session_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    validate_referer(&headers, &state.config)?;

    let query_map = query.0;
    let session_id = get_session_id(&headers, &query_map);
    let mut sessions = state.sessions.lock().await;
    let (_sid, session) = sessions.get_or_create(session_id.as_deref());

    let row = session.pixiv.as_ref().ok_or_else(|| {
        AppError::new(StatusCode::BAD_REQUEST, "MISSING_FILENAME", "缺少文件名参数，且会话中没有缓存文件")
    })?;

    let filename = row.filename.as_deref().ok_or_else(|| {
        AppError::new(StatusCode::BAD_REQUEST, "MISSING_FILENAME", "缺少文件名参数")
    })?;

    serve_media_file(&state, "pixiv", filename, &headers).await
}

// ===================== Plus routes =====================

async fn plus_random_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    state.rate_limiter.check("plus_random").await?;

    let db = state.db.lock().await;
    let row = db.get_random_plus().map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "NO_PLUS_FILES", "没有找到任何视频文件"))?;

    let host = get_host(&headers);

    let query_map = query.0;
    let session_id = get_session_id(&headers, &query_map);
    let mut sessions = state.sessions.lock().await;
    let (sid, session) = sessions.get_or_create(session_id.as_deref());
    session.plus = None;

    let mut body = json!({
        "data": {
            "authorName": row.authorName,
            "authorId": row.authorId,
            "title": row.title,
            "urls": [
                format!("{}/plus/artworks/file/{}", host, urlencoding::encode(row.filename.as_deref().unwrap_or("")))
            ],
            "fileInfo": {
                "dir": row.dir,
                "filename": row.filename,
            },
        },
    });
    body["sessionId"] = json!(sid);
    Ok(json_response(body, &state.storage_name))
}

async fn plus_info_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Query<InfoQuery>,
) -> Result<Response, AppError> {
    state.rate_limiter.check("plus_info").await?;

    let list = query.list.as_deref().ok_or_else(|| {
        AppError::with_details(
            StatusCode::BAD_REQUEST,
            "MISSING_PARAMETER",
            "缺少必要参数",
            json!({ "parameter": "list", "description": "需要提供作者名或作者ID，或使用'all'获取所有作者" }),
        )
    })?;

    let db = state.db.lock().await;

    if list == "all" {
        let rows = db.get_all_authors().map_err(|e| {
            error!("数据库查询失败: {}", e);
            AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
        })?;

        if rows.is_empty() {
            return Err(AppError::new(StatusCode::NOT_FOUND, "NO_AUTHORS_FOUND", "没有找到任何作者"));
        }

        return Ok(api_response(json!({
            "authors": rows,
            "count": rows.len(),
        }), &state.storage_name));
    }

    let search_term = sanitize_search_term(list);
    let rows = db.search_plus(&search_term).map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?;

    if rows.is_empty() {
        return Err(AppError::with_details(
            StatusCode::NOT_FOUND,
            "AUTHOR_NOT_FOUND",
            "未找到匹配的作者",
            json!({ "query": list }),
        ));
    }

    let host = get_host(&headers);

    Ok(api_response(json!({
        "count": rows.len(),
        "files": rows.iter().map(|r| json!({
            "authorName": r.authorName,
            "authorId": r.authorId,
            "title": r.title,
            "urls": [
                format!("{}/plus/artworks/file/{}", host, urlencoding::encode(r.filename.as_deref().unwrap_or("")))
            ],
            "fileInfo": {
                "dir": r.dir,
                "filename": r.filename,
            }
        })).collect::<Vec<_>>(),
    }), &state.storage_name))
}

async fn plus_view_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Redirect, AppError> {
    validate_referer(&headers, &state.config)?;

    let db = state.db.lock().await;
    let row = db.get_random_plus().map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "NO_PLUS_FILES", "没有找到任何视频文件"))?;

    let host = get_host(&headers);
    let file_url = format!(
        "{}/plus/artworks/file/{}",
        host,
        urlencoding::encode(row.filename.as_deref().unwrap_or(""))
    );

    Ok(Redirect::to(&file_url))
}

async fn plus_file_by_name_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(filename): AxumPath<String>,
) -> Result<Response, AppError> {
    validate_referer(&headers, &state.config)?;

    serve_media_file(&state, "plus", &filename, &headers).await
}

async fn plus_file_from_session_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    validate_referer(&headers, &state.config)?;

    let query_map = query.0;
    let session_id = get_session_id(&headers, &query_map);
    let mut sessions = state.sessions.lock().await;
    let (_sid, session) = sessions.get_or_create(session_id.as_deref());

    let row = session.plus.as_ref().ok_or_else(|| {
        AppError::new(StatusCode::BAD_REQUEST, "MISSING_FILENAME", "缺少文件名参数，且会话中没有缓存文件")
    })?;

    let filename = row.filename.as_deref().ok_or_else(|| {
        AppError::new(StatusCode::BAD_REQUEST, "MISSING_FILENAME", "缺少文件名参数")
    })?;

    serve_media_file(&state, "plus", filename, &headers).await
}

// ===================== File serving =====================

async fn serve_media_file(
    state: &AppState,
    r#type: &str,
    filename: &str,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    if filename.is_empty() {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "MISSING_FILENAME", "缺少文件名参数"));
    }

    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return Err(AppError::new(StatusCode::BAD_REQUEST, "INVALID_FILENAME", "文件名包含非法字符"));
    }

    let decoded = urlencoding::decode(filename)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "INVALID_FILENAME", "文件名解码失败"))?;

    let db = state.db.lock().await;
    let row = db.find_file(r#type, &decoded).map_err(|e| {
        error!("数据库查询失败: {}", e);
        AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "DB_ERROR", "数据库查询失败")
    })?;
    drop(db);

    let row = row.ok_or_else(|| AppError::with_details(
        StatusCode::NOT_FOUND,
        "FILE_NOT_IN_DB",
        "文件不在数据库中",
        json!({ "filename": decoded }),
    ))?;

    let root_dir = if r#type == "pixiv" {
        state.config.pixiv_root()
    } else {
        state.config.plus_root()
    };

    let file_path = root_dir.join(row.dir.as_deref().unwrap_or("")).join(&*decoded);
    verify_resource_exists(&file_path, &state.config.resources)?;

    let ext = Path::new(&*decoded)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();

    if r#type == "pixiv" {
        serve_image_file(&file_path, &decoded, &ext).await
    } else {
        serve_video_file(&file_path, &decoded, headers).await
    }
}

async fn serve_image_file(
    file_path: &Path,
    filename: &str,
    ext: &str,
) -> Result<Response, AppError> {
    let content_type = content_type_for_ext(ext);

    let data = tokio::fs::read(file_path).await.map_err(|_| {
        AppError::new(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND", "请求的资源不存在")
    })?;

    let mut resp = Response::new(Body::from(data));
    resp.headers_mut().insert(
        "Content-Type",
        content_type.parse().unwrap(),
    );
    resp.headers_mut().insert(
        "Cache-Control",
        "public, max-age=86400".parse().unwrap(),
    );
    resp.headers_mut().insert(
        "Content-Disposition",
        format!("inline; filename=\"{}\"", urlencoding::encode(filename))
            .parse()
            .unwrap(),
    );

    Ok(resp)
}

async fn serve_video_file(
    file_path: &Path,
    filename: &str,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let metadata = tokio::fs::metadata(file_path).await.map_err(|_| {
        AppError::new(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND", "请求的资源不存在")
    })?;
    let file_size = metadata.len();

    let resp_builder = Response::builder()
        .header("Accept-Ranges", "bytes")
        .header("Content-Type", "video/mp4")
        .header("Cache-Control", "public, max-age=86400")
        .header(
            "Content-Disposition",
            format!("inline; filename=\"{}\"", urlencoding::encode(filename)),
        );

    if let Some(range_str) = headers.get("range").and_then(|v| v.to_str().ok()) {
        if let Some(range_val) = range_str.strip_prefix("bytes=") {
            let parts: Vec<&str> = range_val.split('-').collect();
            let start: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let end: u64 = parts
                .get(1)
                .and_then(|s| if s.is_empty() { None } else { s.parse().ok() })
                .unwrap_or(file_size - 1);

            if start >= file_size {
                return Err(AppError::with_details(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "RANGE_NOT_SATISFIABLE",
                    "请求的Range不可满足",
                    json!({ "range": range_str, "fileSize": file_size }),
                ));
            }

            let read_start = start as usize;
            let read_end = std::cmp::min(end as usize, file_size as usize - 1);
            let len = read_end - read_start + 1;

            let data = tokio::fs::read(file_path).await.map_err(|_| {
                AppError::new(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND", "请求的资源不存在")
            })?;
            let chunk = data[read_start..=read_end].to_vec();

            let resp = resp_builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header("Content-Range", format!("bytes {}-{}/{}", start, end, file_size))
                .header("Content-Length", len.to_string())
                .body(Body::from(chunk))
                .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "RESPONSE_ERROR", "构建响应失败"))?;
            return Ok(resp);
        }
    }

    let data = tokio::fs::read(file_path).await.map_err(|_| {
        AppError::new(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND", "请求的资源不存在")
    })?;

    let resp = resp_builder
        .status(StatusCode::OK)
        .header("Content-Length", file_size.to_string())
        .body(Body::from(data))
        .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "RESPONSE_ERROR", "构建响应失败"))?;
    Ok(resp)
}

// ===================== 404 handler =====================

async fn not_found_handler() -> impl IntoResponse {
    AppError::with_details(
        StatusCode::NOT_FOUND,
        "ENDPOINT_NOT_FOUND",
        "接口不存在",
        json!({}),
    )
}

// ===================== Build router =====================

pub fn build_router(state: Arc<AppState>) -> Router {
    let pixiv_router = Router::new()
        .route("/random", get(pixiv_random_handler))
        .route("/info", get(pixiv_info_handler))
        .route("/view", get(pixiv_view_handler))
        .route("/file/{filename}", get(pixiv_file_by_name_handler))
        .route("/file", get(pixiv_file_from_session_handler));

    let plus_router = Router::new()
        .route("/random", get(plus_random_handler))
        .route("/info", get(plus_info_handler))
        .route("/view", get(plus_view_handler))
        .route("/file/{filename}", get(plus_file_by_name_handler))
        .route("/file", get(plus_file_from_session_handler));

    Router::new()
        .route("/", get(index_handler))
        .route("/style.css", get(css_handler))
        .route("/script.js", get(js_handler))
        .nest("/pixiv/artworks", pixiv_router)
        .nest("/plus/artworks", plus_router)
        .fallback(not_found_handler)
        .with_state(state)
}
