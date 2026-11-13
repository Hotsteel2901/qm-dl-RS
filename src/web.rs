//! 本地 WebUI。
//!
//! 前端是 `src/web/index.html` 单文件（内联 CSS/JS，经 `include_str!` 编进二进制），
//! 不需要 Node / npm / CDN，离线也能用。
//!
//! 后端是同步 tiny_http：每个请求一个线程，下载任务交给一个串行 worker，
//! 与 CLI / TUI 共用 [`crate::flow`] 的取流回退与整曲校验。
//!
//! 安全默认值：
//!   - 默认只监听 127.0.0.1；
//!   - POST 必须是 application/json，并校验 Origin/Host 同源；
//!   - 只允许读取任务自己记录的文件，不接受客户端传路径；
//!   - 登录通过固定参数的子进程执行，不拼接 shell。

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::api::{QqClient, Quality, Song};
use crate::auth::Auth;
use crate::flow::{self, FlowEvent, Level};
use crate::login::{self, LoginKind};
use crate::util;

const INDEX_HTML: &str = include_str!("web/index.html");
const FAVICON_SVG: &str = include_str!("web/favicon.svg");
const MAX_BODY: usize = 1024 * 1024;
const MAX_BATCH: usize = 50;

pub struct Options {
    pub port: u16,
    pub bind: String,
    pub out: PathBuf,
    pub quality: Quality,
    pub open: bool,
    pub bridge: Option<String>,
    pub mirror: String,
    pub verify: bool,
    pub proxy: Option<String>,
    pub insecure: bool,
    pub anon: bool,
    pub cookie: Option<String>,
    pub cookie_file: Option<PathBuf>,
}

// ==================== 任务模型 ====================

struct JobSpec {
    mid: String,
    quality: Quality,
    preview: bool,
    no_preview: bool,
    mirror: String,
    verify: bool,
    bridge: Option<String>,
    out: PathBuf,
}

struct JobState {
    name: String,
    artist: String,
    status: &'static str,
    stage: String,
    quality: Quality,
    preview: bool,
    source: String,
    path: Option<PathBuf>,
    url: Option<String>,
    downloaded: u64,
    total: u64,
    speed: f64,
    messages: Vec<String>,
    error: Option<String>,
    verified: Option<f64>,
    created: Instant,
    finished: Option<Instant>,
}

struct Job {
    id: u64,
    spec: JobSpec,
    state: Mutex<JobState>,
    cancel: Arc<AtomicBool>,
}

struct State {
    options: Arc<Options>,
    jobs: Mutex<HashMap<u64, Arc<Job>>>,
    next_id: AtomicU64,
    queue: Mutex<Sender<u64>>,
    login: Arc<Mutex<LoginState>>,
}

#[derive(Default)]
struct LoginState {
    kind: Option<LoginKind>,
    running: bool,
    canceled: bool,
    success: bool,
    error: Option<String>,
    lines: Vec<String>,
    pid: Option<u32>,
}

// ==================== 启动 ====================

pub fn serve(options: Options) -> Result<()> {
    let addr = if options.bind.contains(':') && !options.bind.starts_with('[') {
        format!("[{}]:{}", options.bind, options.port)
    } else {
        format!("{}:{}", options.bind, options.port)
    };
    let server = Server::http(&addr).map_err(|e| anyhow::anyhow!("监听 {addr} 失败: {e}"))?;

    let (tx, rx) = mpsc::channel::<u64>();
    let state = Arc::new(State {
        options: Arc::new(options),
        jobs: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        queue: Mutex::new(tx),
        login: Arc::new(Mutex::new(LoginState::default())),
    });
    spawn_worker(state.clone(), rx);

    print_startup(&state);

    if state.options.open {
        let url = browser_url(&state.options.bind, state.options.port);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(350));
            let _ = util::open_path(Path::new(&url));
        });
    }

    loop {
        match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(request)) => {
                let state = state.clone();
                thread::spawn(move || handle(state, request));
            }
            Ok(None) => {}
            // accept 的瞬时错误（如 EMFILE）不应让 WebUI 静默退出。
            Err(e) => {
                eprintln!("WebUI accept 错误: {e}");
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn browser_url(bind: &str, port: u16) -> String {
    let host = match bind {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "[::]" => "[::1]".to_string(),
        b if b.contains(':') && !b.starts_with('[') => format!("[{b}]"),
        b => b.to_string(),
    };
    format!("http://{host}:{port}/")
}

fn print_startup(state: &State) {
    let o = &state.options;
    println!("WebUI 已启动: {}", browser_url(&o.bind, o.port));
    if o.bind == "0.0.0.0" || o.bind == "::" || o.bind == "[::]" {
        println!(
            "监听: {}（本机可用上面的地址，局域网用 http://<本机IP>:{}/）",
            o.bind, o.port
        );
        println!(
            "{}",
            crate::ui::warn(
                "注意: 任何能访问该端口的人都能使用当前登录态搜索/下载，请只在可信网络使用。"
            )
        );
    }
    println!("  {} {}", crate::ui::label("下载目录"), o.out.display());
    println!("  {} {}", crate::ui::label("默认音质"), o.quality.label());
    println!("  {} {}", crate::ui::label("匿名镜像"), o.mirror);
    if let Some(bridge) = &o.bridge {
        println!("  {} {}", crate::ui::label("票据桥"), bridge);
    }
    match load_auth(o) {
        Ok(auth) if auth.logged_in => println!(
            "  {} {}",
            crate::ui::label("登录态"),
            crate::ui::ok(&format!(
                "已登录（uin={}, 来源: {}）",
                util::mask(&auth.uin),
                auth.source
            ))
        ),
        Ok(_) => println!(
            "  {} {}",
            crate::ui::label("登录态"),
            crate::ui::warn("匿名（VIP 曲目自动尝试匿名镜像）")
        ),
        Err(e) => println!(
            "  {} {}",
            crate::ui::label("登录态"),
            crate::ui::err(&format!("{e:#}"))
        ),
    }
    println!(
        "  {}",
        crate::ui::dim("前端为内嵌单文件，无 CDN / 无构建依赖；在该页面可直接扫码登录。")
    );
    println!("  {}", crate::ui::dim("按 Ctrl+C 停止服务。"));
}

fn load_auth(options: &Options) -> Result<Auth> {
    if options.anon {
        Ok(Auth::anonymous())
    } else {
        Auth::load(options.cookie.as_deref(), options.cookie_file.as_deref())
    }
}

// ==================== 下载 worker ====================

fn spawn_worker(state: Arc<State>, rx: Receiver<u64>) {
    thread::spawn(move || {
        while let Ok(id) = rx.recv() {
            let job = state
                .jobs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .cloned();
            if let Some(job) = job {
                let for_panic = job.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_job(&state, &job)
                }));
                if result.is_err() {
                    finish_job(
                        &for_panic,
                        "failed",
                        "下载线程异常",
                        Some("下载线程异常退出（详见 qqmusic-dl web 服务端输出）".to_string()),
                    );
                }
            }
        }
    });
}

fn run_job(state: &State, job: &Arc<Job>) {
    if job.cancel.load(Ordering::Relaxed) {
        finish_job(job, "cancelled", "已取消（.part 保留，可续传）", None);
        return;
    }
    {
        let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
        st.status = "resolving";
        st.stage = "读取登录态…".to_string();
    }

    let outcome = (|| -> Result<flow::Outcome> {
        let auth = load_auth(&state.options)?;
        let client = QqClient::new(auth, state.options.proxy.as_deref(), state.options.insecure)?;
        let song = client.detail(&job.spec.mid)?;
        {
            let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
            st.name = song.name.clone();
            st.artist = song.artist_str();
            st.stage = "解析下载地址…".to_string();
        }

        let request = flow::Request {
            quality: job.spec.quality,
            out: job.spec.out.clone(),
            dry_run: false,
            preview_only: job.spec.preview,
            no_preview: job.spec.no_preview,
            mirror: job.spec.mirror.clone(),
            bridge: job.spec.bridge.clone(),
            verify: job.spec.verify,
            cancel: Some(job.cancel.clone()),
        };
        let mut meter = flow::SpeedMeter::new();
        let mut on_event = |event: FlowEvent| {
            let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
            match event {
                FlowEvent::Message { level, text } => {
                    st.stage = text.clone();
                    push_message(&mut st, text, level);
                }
                FlowEvent::Resolved {
                    quality,
                    source,
                    preview,
                    degraded,
                    expected_size,
                    ..
                } => {
                    st.quality = quality;
                    st.preview = preview;
                    st.source = source;
                    if st.total == 0 && expected_size > 0 {
                        st.total = expected_size;
                    }
                    st.stage = if degraded && !preview {
                        "已降级，准备下载…".to_string()
                    } else {
                        "准备下载…".to_string()
                    };
                }
                FlowEvent::Target { path, url } => {
                    st.path = Some(path);
                    st.url = Some(url);
                }
                FlowEvent::Progress { downloaded, total } => {
                    st.status = "downloading";
                    st.downloaded = downloaded;
                    // total=0 表示服务端没给 Content-Length，保留 Resolved
                    // 阶段的 expected_size，避免进度条从已知总长退回不确定态。
                    if total > 0 {
                        st.total = total;
                    }
                    st.speed = meter.update(downloaded);
                    st.stage = "下载中…".to_string();
                }
                FlowEvent::Saved { bytes, .. } => {
                    st.status = "verifying";
                    st.downloaded = bytes;
                    if st.total == 0 {
                        st.total = bytes;
                    }
                    st.speed = 0.0;
                    st.stage = "下载完成，校验时长…".to_string();
                }
                FlowEvent::Verified { actual, .. } => {
                    st.verified = Some(actual);
                    st.stage = "时长校验通过".to_string();
                }
            }
        };
        flow::execute(&client, &song, &request, &mut on_event)
    })();

    match outcome {
        Ok(flow::Outcome::Downloaded {
            path,
            bytes,
            quality,
            source,
            preview,
            verified,
        }) => {
            let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
            st.status = "done";
            st.stage = "完成".to_string();
            st.path = Some(path);
            st.downloaded = bytes;
            if st.total == 0 {
                st.total = bytes;
            }
            st.quality = quality;
            st.source = source;
            st.preview = preview;
            st.verified = verified;
            st.speed = 0.0;
            st.finished = Some(Instant::now());
        }
        Ok(flow::Outcome::DryRun { .. }) => {
            finish_job(job, "done", "完成（dry-run）", None);
        }
        Err(e) => {
            if e.downcast_ref::<crate::download::Cancelled>().is_some() {
                finish_job(job, "cancelled", "已取消（.part 保留，可续传）", None);
            } else {
                let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
                st.status = "failed";
                st.stage = "失败".to_string();
                st.speed = 0.0;
                st.error = Some(format!("{e:#}"));
                push_message(&mut st, format!("{e:#}"), Level::Warn);
                st.finished = Some(Instant::now());
            }
        }
    }
}

fn finish_job(job: &Arc<Job>, status: &'static str, stage: &str, error: Option<String>) {
    let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
    st.status = status;
    st.stage = stage.to_string();
    st.speed = 0.0;
    if let Some(error) = error {
        st.error = Some(error);
    }
    st.finished = Some(Instant::now());
}

fn push_message(st: &mut JobState, text: String, _level: Level) {
    if text.trim().is_empty() {
        return;
    }
    if st.messages.last() == Some(&text) {
        return;
    }
    st.messages.push(text);
    if st.messages.len() > 40 {
        st.messages.remove(0);
    }
}

// ==================== HTTP 路由 ====================

fn handle(state: Arc<State>, mut request: Request) {
    if !host_allowed_request(&request) {
        respond_error(
            request,
            403,
            "Host 校验失败：请用 http://127.0.0.1 或 IP 访问，不要用自定义域名",
        );
        return;
    }
    let method = request.method().clone();
    let (path, query_text) = {
        let (path, query) = split_target(request.url());
        (path.to_string(), query.to_string())
    };
    let query = util::parse_query(&query_text);

    // 预检请求不提供 CORS 允许头：本服务只接受同源页面。
    if method == Method::Options {
        let _ = request.respond(Response::empty(StatusCode(204)));
        return;
    }

    match (method, path.as_str()) {
        // path 的所有权在本函数内，避免与 read_json(&mut request) 的借用冲突。
        (Method::Get | Method::Head, "/") => {
            respond_bytes(
                request,
                200,
                "text/html; charset=utf-8",
                INDEX_HTML.as_bytes().to_vec(),
                vec![
                    header("Cache-Control", "no-store"),
                    header("X-Content-Type-Options", "nosniff"),
                    header("X-Frame-Options", "DENY"),
                    header("Referrer-Policy", "no-referrer"),
                    header(
                        "Content-Security-Policy",
                        "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self'; media-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
                    ),
                ],
            );
        }
        (Method::Get | Method::Head, "/favicon.svg") => {
            respond_bytes(
                request,
                200,
                "image/svg+xml",
                FAVICON_SVG.as_bytes().to_vec(),
                vec![header("Cache-Control", "public, max-age=86400")],
            );
        }
        (Method::Get, "/api/health") => {
            let value = health_json(&state);
            respond_json(request, 200, value);
        }
        (Method::Get, "/api/search") => {
            let value = search_json(&state, &query);
            let status = if value.get("ok").and_then(Value::as_bool) == Some(true) {
                200
            } else {
                400
            };
            respond_json(request, status, value);
        }
        (Method::Get, "/api/jobs") => {
            let value = jobs_json(&state);
            respond_json(request, 200, value);
        }
        (Method::Post, "/api/jobs") => {
            let body = match read_json(&mut request) {
                Ok(body) => body,
                Err(e) => {
                    respond_error(request, 400, format!("{e:#}"));
                    return;
                }
            };
            match create_jobs(&state, &body) {
                Ok(ids) => respond_json(request, 200, json!({"ok": true, "ids": ids})),
                Err(e) => respond_error(request, 400, format!("{e:#}")),
            }
        }
        (Method::Post, "/api/jobs/clear") => {
            if let Err(e) = read_json(&mut request) {
                respond_error(request, 400, format!("{e:#}"));
                return;
            }
            let removed = clear_finished(&state);
            respond_json(request, 200, json!({"ok": true, "removed": removed}));
        }
        (Method::Post, "/api/login/start") => {
            let body = match read_json(&mut request) {
                Ok(body) => body,
                Err(e) => {
                    respond_error(request, 400, format!("{e:#}"));
                    return;
                }
            };
            let kind = body
                .get("kind")
                .and_then(Value::as_str)
                .and_then(parse_login_kind);
            let Some(kind) = kind else {
                respond_error(request, 400, "kind 必须是 qqmusic / wx / qq");
                return;
            };
            match start_login(&state, kind) {
                Ok(()) => respond_json(request, 200, json!({"ok": true, "kind": kind.slug()})),
                Err(e) => respond_error(request, 409, format!("{e:#}")),
            }
        }
        (Method::Get, "/api/login/status") => {
            respond_json(request, 200, login_status_json(&state));
        }
        (Method::Get, "/api/login/qr") => {
            serve_login_qr(request, &query);
        }
        (Method::Post, "/api/login/cancel") => {
            if let Err(e) = read_json(&mut request) {
                respond_error(request, 400, format!("{e:#}"));
                return;
            }
            cancel_login(&state);
            respond_json(request, 200, json!({"ok": true}));
        }
        (method, path) if method == Method::Post && path.starts_with("/api/jobs/") => {
            if path.ends_with("/cancel") {
                if let Err(e) = read_json(&mut request) {
                    respond_error(request, 400, format!("{e:#}"));
                    return;
                }
                match parse_job_id(path, "/cancel") {
                    Some(id) => match cancel_job(&state, id) {
                        Some(cancelled) => {
                            respond_json(request, 200, json!({"ok": true, "cancelled": cancelled}))
                        }
                        None => respond_error(request, 404, "任务不存在"),
                    },
                    None => respond_error(request, 404, "任务不存在"),
                }
            } else if path.ends_with("/open") {
                if let Err(e) = read_json(&mut request) {
                    respond_error(request, 400, format!("{e:#}"));
                    return;
                }
                match parse_job_id(path, "/open") {
                    Some(id) => match open_job_dir(&state, id) {
                        Ok(dir) => respond_json(
                            request,
                            200,
                            json!({"ok": true, "path": dir.display().to_string()}),
                        ),
                        Err(e) => respond_error(request, 400, format!("{e:#}")),
                    },
                    None => respond_error(request, 404, "任务不存在"),
                }
            } else {
                respond_error(request, 404, "not found");
            }
        }
        (Method::Get | Method::Head, path) if path.starts_with("/api/jobs/") => {
            if let Some(id) = parse_job_id(path, "/file") {
                serve_job_file(request, &state, id, query.contains_key("download"));
            } else if let Some(id) = parse_job_id(path, "") {
                match job_json_by_id(&state, id) {
                    Some(value) => respond_json(request, 200, json!({"ok": true, "job": value})),
                    None => respond_error(request, 404, "任务不存在"),
                }
            } else {
                respond_error(request, 404, "not found");
            }
        }
        (Method::Get | Method::Head, path) if path.starts_with("/api/file/") => {
            match path["/api/file/".len()..].parse::<u64>() {
                Ok(id) => serve_job_file(request, &state, id, query.contains_key("download")),
                Err(_) => respond_error(request, 404, "任务不存在"),
            }
        }
        _ => respond_error(request, 404, "not found"),
    }
}

fn split_target(url: &str) -> (&str, &str) {
    url.split_once('?').unwrap_or((url, ""))
}

/// 防 DNS rebinding：只接受 localhost / IPv4 / IPv6 字面量作为 Host。
/// 恶意域名即使解析到 127.0.0.1，浏览器发出的 Host 仍是该域名，会被拒绝。
fn host_allowed_request(request: &Request) -> bool {
    let Some(host) = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str().to_string())
    else {
        // HTTP/1.0 没有 Host；本机工具仍可访问。
        return true;
    };
    host_allowed(&host)
}

fn host_allowed(host: &str) -> bool {
    let host = host.trim();
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok()
}

fn parse_job_id(path: &str, suffix: &str) -> Option<u64> {
    let rest = path.strip_prefix("/api/jobs/")?;
    let id = rest.strip_suffix(suffix)?;
    id.parse().ok()
}

// ==================== API 实现 ====================

fn health_json(state: &State) -> Value {
    let o = &state.options;
    let auth = load_auth(o);
    let (logged_in, uin, source, auth_error) = match auth {
        Ok(auth) => (
            auth.logged_in,
            if auth.logged_in {
                util::mask(&auth.uin)
            } else {
                String::new()
            },
            auth.source,
            Value::Null,
        ),
        Err(e) => (
            false,
            String::new(),
            "读取 Cookie 失败".to_string(),
            json!(format!("{e:#}")),
        ),
    };
    let login_block = login_block_reason(o);
    json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "anon": o.anon,
        "login_available": login_block.is_none(),
        "login_blocked_reason": login_block,
        "auth": {
            "logged_in": logged_in,
            "uin": uin,
            "source": source,
            "error": auth_error,
        },
        "out_dir": o.out.display().to_string(),
        "bridge": o.bridge,
        "default_quality": o.quality.slug(),
        "default_quality_label": o.quality.label(),
        "mirror": o.mirror,
        "verify": o.verify,
    })
}

fn search_json(state: &State, query: &HashMap<String, String>) -> Value {
    let keyword = query
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if keyword.is_empty() {
        return json!({"ok": false, "error": "缺少搜索关键词 q"});
    }
    let limit = query
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20)
        .clamp(1, 50);
    let result = (|| -> Result<Vec<Song>> {
        let auth = load_auth(&state.options)?;
        let client = QqClient::new(auth, state.options.proxy.as_deref(), state.options.insecure)?;
        client.search(&keyword, limit)
    })();
    match result {
        Ok(songs) => json!({
            "ok": true,
            "songs": songs.iter().map(song_json).collect::<Vec<_>>(),
        }),
        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
    }
}

fn song_json(song: &Song) -> Value {
    json!({
        "songmid": song.mid,
        "media_mid": song.media_mid,
        "name": song.name,
        "artists": song.artists,
        "artist": song.artist_str(),
        "album": song.album,
        "duration": song.duration,
        "duration_text": song.duration_str(),
        "size_128": song.size_128,
        "size_320": song.size_320,
        "size_flac": song.size_flac,
        "preview_size": song.preview_size,
        "quality_flags": song.quality_flags(),
    })
}

struct PendingJob {
    mid: String,
    name: String,
    artist: String,
}

fn create_jobs(state: &State, body: &Value) -> Result<Vec<u64>> {
    let quality = match body.get("quality").and_then(Value::as_str) {
        Some(q) => Quality::from_str_loose(q).context("quality 必须是 128 / 320 / flac")?,
        None => state.options.quality,
    };
    let preview = body
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let no_preview = body
        .get("no_preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mirror = body
        .get("mirror")
        .and_then(Value::as_str)
        .unwrap_or(&state.options.mirror)
        .to_string();
    if !crate::mirror::POLICIES.contains(&mirror.as_str()) {
        bail!("mirror 必须是 auto / migu / kuwo / netease / off");
    }
    let verify = body
        .get("verify")
        .and_then(Value::as_bool)
        .unwrap_or(state.options.verify);

    let songs = body
        .get("songs")
        .and_then(Value::as_array)
        .context("缺少 songs 数组")?;
    if songs.is_empty() {
        bail!("songs 不能为空");
    }
    if songs.len() > MAX_BATCH {
        bail!("一次最多提交 {MAX_BATCH} 首");
    }

    let mut pending = Vec::with_capacity(songs.len());
    for item in songs {
        let (mid, name, artist) = match item {
            Value::String(mid) => (mid.clone(), String::new(), String::new()),
            Value::Object(obj) => {
                let mid = obj
                    .get("mid")
                    .or_else(|| obj.get("songmid"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let artist = obj
                    .get("artist")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        obj.get("artists").and_then(Value::as_array).map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("/")
                        })
                    })
                    .unwrap_or_default();
                (mid, name, artist)
            }
            _ => bail!("songs 里的每一项必须是 songmid 字符串或对象"),
        };
        if !valid_mid(&mid) {
            bail!("非法 songmid: {mid}");
        }
        pending.push(PendingJob { mid, name, artist });
    }

    let mut ids = Vec::with_capacity(pending.len());
    let queue = state.queue.lock().unwrap_or_else(|e| e.into_inner());
    for item in pending {
        let id = state.next_id.fetch_add(1, Ordering::SeqCst);
        let name = if item.name.is_empty() {
            item.mid.clone()
        } else {
            item.name.clone()
        };
        let job = Arc::new(Job {
            id,
            spec: JobSpec {
                mid: item.mid,
                quality,
                preview,
                no_preview,
                mirror: mirror.clone(),
                verify,
                bridge: state.options.bridge.clone(),
                out: state.options.out.clone(),
            },
            state: Mutex::new(JobState {
                name,
                artist: item.artist,
                status: "queued",
                stage: "等待中…".to_string(),
                quality,
                preview,
                source: String::new(),
                path: None,
                url: None,
                downloaded: 0,
                total: 0,
                speed: 0.0,
                messages: Vec::new(),
                error: None,
                verified: None,
                created: Instant::now(),
                finished: None,
            }),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        state
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, job);
        if queue.send(id).is_err() {
            // worker 已退出：别在任务列表里留下永远 queued 的孤儿。
            state
                .jobs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            bail!("下载队列已关闭");
        }
        ids.push(id);
    }
    Ok(ids)
}

fn valid_mid(mid: &str) -> bool {
    !mid.is_empty() && mid.len() <= 64 && mid.chars().all(|c| c.is_ascii_alphanumeric())
}

fn jobs_json(state: &State) -> Value {
    let jobs = state.jobs.lock().unwrap_or_else(|e| e.into_inner());
    let mut values: Vec<Value> = jobs.values().map(|job| job_json(job)).collect();
    values.sort_by_key(|v| v.get("id").and_then(Value::as_u64).unwrap_or(0));
    json!({"ok": true, "jobs": values})
}

fn job_json_by_id(state: &State, id: u64) -> Option<Value> {
    let job = state
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()?;
    Some(job_json(&job))
}

fn job_json(job: &Job) -> Value {
    let st = job.state.lock().unwrap_or_else(|e| e.into_inner());
    let elapsed = st
        .finished
        .unwrap_or_else(Instant::now)
        .duration_since(st.created)
        .as_secs_f64();
    let file_name = st
        .path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string());
    json!({
        "id": job.id,
        "mid": job.spec.mid,
        "name": st.name,
        "artist": st.artist,
        "requested_quality": job.spec.quality.slug(),
        "quality": st.quality.slug(),
        "quality_label": st.quality.label(),
        "preview": st.preview,
        "requested_preview": job.spec.preview,
        "status": st.status,
        "stage": st.stage,
        "source": st.source,
        "downloaded": st.downloaded,
        "total": st.total,
        "speed": st.speed,
        "path": st.path.as_ref().map(|p| p.display().to_string()),
        "file_name": file_name,
        "url": st.url,
        "error": st.error,
        "verified": st.verified,
        "messages": st.messages,
        "elapsed": elapsed,
    })
}

fn cancel_job(state: &State, id: u64) -> Option<bool> {
    let job = state
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()?;
    let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
    let active = matches!(st.status, "queued" | "resolving" | "downloading");
    if active {
        st.stage = "正在取消…".to_string();
    }
    drop(st);
    if active {
        job.cancel.store(true, Ordering::SeqCst);
        Some(true)
    } else {
        Some(false)
    }
}

fn open_job_dir(state: &State, id: u64) -> Result<PathBuf> {
    let job = state
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
        .context("任务不存在")?;
    let path = job
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .path
        .clone()
        .context("任务还没有文件")?;
    let dir = path.parent().unwrap_or(&path).to_path_buf();
    let dir = dir.canonicalize().context("下载目录不存在")?;
    let out_root = state
        .options
        .out
        .canonicalize()
        .unwrap_or_else(|_| state.options.out.clone());
    if !dir.starts_with(&out_root) {
        bail!("目录路径越界，已拒绝");
    }
    util::open_path(&dir)?;
    Ok(dir)
}

fn clear_finished(state: &State) -> usize {
    let mut jobs = state.jobs.lock().unwrap_or_else(|e| e.into_inner());
    let before = jobs.len();
    jobs.retain(|_, job| {
        let st = job.state.lock().unwrap_or_else(|e| e.into_inner());
        !matches!(st.status, "done" | "failed" | "cancelled")
    });
    before - jobs.len()
}

// ==================== 文件服务（支持 Range） ====================

fn serve_job_file(request: Request, state: &State, id: u64, download: bool) {
    let job = match state
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned()
    {
        Some(job) => job,
        None => {
            respond_error(request, 404, "任务不存在");
            return;
        }
    };
    let path = match job
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .path
        .clone()
    {
        Some(path) => path,
        None => {
            respond_error(request, 404, "任务还没有生成文件");
            return;
        }
    };
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(e) => {
            respond_error(request, 404, format!("文件不存在或不可访问: {e}"));
            return;
        }
    };
    let out_root = state
        .options
        .out
        .canonicalize()
        .unwrap_or_else(|_| state.options.out.clone());
    if !path.starts_with(&out_root) {
        respond_error(request, 403, "文件路径越界，已拒绝");
        return;
    }
    let meta = match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => {
            respond_error(request, 404, "不是文件");
            return;
        }
        Err(e) => {
            respond_error(request, 404, format!("读取文件失败: {e}"));
            return;
        }
    };
    let len = meta.len();
    let range_header = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());

    let (start, end, partial) = match range_header.as_deref() {
        Some(value) => match parse_range(value, len) {
            Some((start, end)) => (start, end, true),
            None => {
                let response = Response::empty(StatusCode(416))
                    .with_header(header("Content-Range", &format!("bytes */{len}")));
                let _ = request.respond(response);
                return;
            }
        },
        None => (0, len.saturating_sub(1), false),
    };

    let count = if len == 0 { 0 } else { end - start + 1 };
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) => {
            respond_error(request, 500, format!("打开文件失败: {e}"));
            return;
        }
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        respond_error(request, 500, "定位文件失败");
        return;
    }

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    let ascii_name: String = file_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let disposition = if download { "attachment" } else { "inline" };

    let mut headers = vec![
        header("Content-Type", audio_mime(&path)),
        header("Accept-Ranges", "bytes"),
        header("Cache-Control", "no-store"),
        header("X-Content-Type-Options", "nosniff"),
        header("Cross-Origin-Resource-Policy", "same-origin"),
        header(
            "Content-Disposition",
            &format!(
                "{disposition}; filename=\"{ascii_name}\"; filename*=UTF-8''{}",
                util::url_encode(&file_name)
            ),
        ),
    ];
    if partial {
        headers.push(header(
            "Content-Range",
            &format!("bytes {start}-{end}/{len}"),
        ));
    }
    let reader = file.take(count);
    let response = Response::new(
        StatusCode(if partial { 206 } else { 200 }),
        headers,
        reader,
        Some(count as usize),
        None,
    )
    // 强制写出精确的 Content-Length；tiny_http 默认 >=32KiB 会改用 chunked，
    // 某些播放器/下载器对 chunked 的音频 Range 支持不佳。
    .with_chunked_threshold(usize::MAX);
    let _ = request.respond(response);
}

fn audio_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("m4a") | Some("mp4") => "audio/mp4",
        _ => "application/octet-stream",
    }
}

/// 单区间 `Range: bytes=...`。多区间或非法值返回 None（调用方回 416）。
fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    if a.is_empty() {
        let suffix: u64 = b.trim().parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = len.saturating_sub(suffix);
        return Some((start, len - 1));
    }
    let start: u64 = a.trim().parse().ok()?;
    if start >= len {
        return None;
    }
    let end = if b.trim().is_empty() {
        len - 1
    } else {
        b.trim().parse::<u64>().ok()?.min(len - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

// ==================== 登录子进程 ====================

fn parse_login_kind(s: &str) -> Option<LoginKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "qq" => Some(LoginKind::Qq),
        "wx" | "wechat" => Some(LoginKind::Wx),
        "qqmusic" | "app" | "mobile" | "client" => Some(LoginKind::QqMusic),
        _ => None,
    }
}

fn login_block_reason(options: &Options) -> Option<&'static str> {
    if options.anon {
        Some("服务以 --anon 启动，登录不会生效")
    } else if options
        .cookie
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        Some("--cookie 指定的 Cookie 优先于扫码登录")
    } else if std::env::var_os("QQMUSIC_COOKIE")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        Some("QQMUSIC_COOKIE 环境变量优先于扫码登录")
    } else {
        None
    }
}

/// 登录子进程固定写默认 Cookie 路径；服务用 --cookie-file 启动时同步一份。
fn copy_cookie_to(path: &Path) -> std::io::Result<()> {
    let default = crate::auth::default_cookie_path();
    let bytes = std::fs::read(&default)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".cookie-{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        std::fs::write(&tmp, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(path);
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn fail_login_start(state: &State, message: String) {
    let mut st = state.login.lock().unwrap_or_else(|e| e.into_inner());
    st.running = false;
    st.pid = None;
    st.error = Some(message.clone());
    st.lines.push(message);
}

fn start_login(state: &State, kind: LoginKind) -> Result<()> {
    if let Some(reason) = login_block_reason(&state.options) {
        bail!("{reason}");
    }
    {
        let mut login = state.login.lock().unwrap_or_else(|e| e.into_inner());
        if login.running {
            bail!("已有登录流程正在进行");
        }
        login::clear_qr(kind);
        *login = LoginState {
            kind: Some(kind),
            running: true,
            lines: vec!["正在申请二维码…".to_string()],
            ..LoginState::default()
        };
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            fail_login_start(state, format!("获取可执行文件路径失败: {e}"));
            return Err(e.into());
        }
    };
    let mut command = Command::new(exe);
    command
        .arg("login")
        .arg(kind.slug())
        .arg("--no-open")
        .arg("--timeout")
        .arg("240");
    if let Some(proxy) = &state.options.proxy {
        command.arg("--proxy").arg(proxy);
    }
    if state.options.insecure {
        command.arg("--insecure");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            fail_login_start(state, format!("启动登录子进程失败: {e}"));
            return Err(e.into());
        }
    };
    let pid = child.id();
    {
        let mut login = state.login.lock().unwrap_or_else(|e| e.into_inner());
        login.pid = Some(pid);
        login
            .lines
            .push("二维码生成后会显示在下方；请用手机扫码。".to_string());
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let login_state = state.login.clone();
    let cookie_file = state.options.cookie_file.clone();
    let out_reader = stdout.map(|s| spawn_login_reader(s, login_state.clone()));
    let err_reader = stderr.map(|s| spawn_login_reader(s, login_state.clone()));

    thread::spawn(move || {
        let status = child.wait();
        let mut st = login_state.lock().unwrap_or_else(|e| e.into_inner());
        st.running = false;
        st.pid = None;
        match status {
            Ok(status) if status.success() => {
                st.success = true;
                st.lines.push("✓ 登录成功，Cookie 已写入".to_string());
                if let Some(path) = &cookie_file {
                    if path != &crate::auth::default_cookie_path() {
                        match copy_cookie_to(path) {
                            Ok(()) => st
                                .lines
                                .push(format!("已同步 Cookie 到 {}", path.display())),
                            Err(e) => {
                                let msg = format!("同步 Cookie 到 {} 失败: {e}", path.display());
                                st.error = Some(msg.clone());
                                st.lines.push(msg);
                            }
                        }
                    }
                }
            }
            Ok(status) => {
                if st.canceled {
                    st.lines.push("登录已取消".to_string());
                } else {
                    let msg = format!("登录子进程退出: {status}");
                    st.error = Some(msg.clone());
                    st.lines.push(msg);
                }
            }
            Err(e) => {
                let msg = format!("等待登录子进程失败: {e}");
                st.error = Some(msg.clone());
                st.lines.push(msg);
            }
        }
        if st.lines.len() > 80 {
            let drain = st.lines.len() - 80;
            st.lines.drain(0..drain);
        }
        drop(st);
        // 登录子进程可能被取消/异常终止，CurlJar 的 Drop 不一定会跑；
        // 按子进程 PID 精确清掉它的临时 Cookie 罐（不等待可能被孙进程占住的管道）。
        login::cleanup_login_dirs_for_pid(pid);
        // 子进程可能留下持有管道的 curl 孙进程；不要让 running 一直挂住，
        // 先把状态置为结束，再在后台等读取线程收尾。
        if let Some(handle) = out_reader {
            let _ = handle.join();
        }
        if let Some(handle) = err_reader {
            let _ = handle.join();
        }
    });
    Ok(())
}

fn spawn_login_reader<R: Read + Send + 'static>(
    reader: R,
    login: Arc<Mutex<LoginState>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            let line = util::strip_ansi(line.trim_end_matches('\r'));
            let line = line.trim().to_string();
            if line.is_empty() || is_qr_art(&line) {
                continue;
            }
            let mut st = login.lock().unwrap_or_else(|e| e.into_inner());
            if st.lines.last() != Some(&line) {
                st.lines.push(line);
                if st.lines.len() > 80 {
                    st.lines.remove(0);
                }
            }
        }
    })
}

/// 终端二维码由半块字符组成，WebUI 展示的是 PNG 图片，这些行直接丢弃。
fn is_qr_art(line: &str) -> bool {
    let blocks = line
        .chars()
        .filter(|c| matches!(c, '█' | '▀' | '▄' | '▌' | '▐'))
        .count();
    blocks >= 4
}

fn login_status_json(state: &State) -> Value {
    let st = state.login.lock().unwrap_or_else(|e| e.into_inner());
    let qr = st.kind.and_then(login::qr_image);
    let qr_value = match (st.kind, qr) {
        (Some(kind), Some((_, mime, mtime))) => json!({
            "url": format!("/api/login/qr?kind={}&t={}", kind.slug(), mtime),
            "mime": mime,
            "mtime": mtime,
        }),
        _ => Value::Null,
    };
    let lines: Vec<&String> = st.lines.iter().rev().take(8).collect();
    let lines: Vec<&String> = lines.into_iter().rev().collect();
    json!({
        "ok": true,
        "running": st.running,
        "success": st.success,
        "kind": st.kind.map(|k| k.slug()),
        "canceled": st.canceled,
        "error": st.error,
        "lines": lines,
        "qr": qr_value,
    })
}

fn serve_login_qr(request: Request, query: &HashMap<String, String>) {
    let kind = query.get("kind").and_then(|s| parse_login_kind(s));
    let Some(kind) = kind else {
        respond_error(request, 400, "kind 必须是 qqmusic / wx / qq");
        return;
    };
    match login::qr_image(kind) {
        Some((bytes, mime, _)) => respond_bytes(
            request,
            200,
            mime,
            bytes,
            vec![header("Cache-Control", "no-store")],
        ),
        None => respond_error(request, 404, "二维码还没生成"),
    }
}

fn cancel_login(state: &State) {
    let pid = {
        let mut st = state.login.lock().unwrap_or_else(|e| e.into_inner());
        if !st.running {
            return;
        }
        st.canceled = true;
        st.pid
    };
    if let Some(pid) = pid {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status();
        }
    }
}

// ==================== HTTP 小工具 ====================

fn header(name: &'static str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("HTTP header 必须是 ASCII")
}

fn respond_json(request: Request, status: u16, value: Value) {
    let _ = request.respond(json_response(status, &value));
}

fn respond_error(request: Request, status: u16, message: impl std::fmt::Display) {
    respond_json(
        request,
        status,
        json!({"ok": false, "error": message.to_string()}),
    );
}

fn json_response(status: u16, value: &Value) -> Response<Cursor<Vec<u8>>> {
    let bytes = value.to_string().into_bytes();
    let len = bytes.len();
    Response::new(
        StatusCode(status),
        vec![
            header("Content-Type", "application/json; charset=utf-8"),
            header("Cache-Control", "no-store"),
            header("X-Content-Type-Options", "nosniff"),
            header("Cross-Origin-Resource-Policy", "same-origin"),
        ],
        Cursor::new(bytes),
        Some(len),
        None,
    )
    .with_chunked_threshold(usize::MAX)
}

fn respond_bytes(
    request: Request,
    status: u16,
    content_type: &str,
    bytes: Vec<u8>,
    extra: Vec<Header>,
) {
    let len = bytes.len();
    let mut headers = vec![
        header("Content-Type", content_type),
        header("X-Content-Type-Options", "nosniff"),
        header("Cross-Origin-Resource-Policy", "same-origin"),
    ];
    headers.extend(extra);
    let response = Response::new(
        StatusCode(status),
        headers,
        Cursor::new(bytes),
        Some(len),
        None,
    )
    .with_chunked_threshold(usize::MAX);
    let _ = request.respond(response);
}

fn read_json(request: &mut Request) -> Result<Value> {
    let content_type = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .map(|h| h.value.as_str().to_ascii_lowercase())
        .unwrap_or_default();
    if !content_type.starts_with("application/json") {
        bail!("POST 需要 Content-Type: application/json");
    }
    check_same_origin(request)?;
    if request.body_length().unwrap_or(0) > MAX_BODY {
        bail!("请求体过大");
    }
    let mut text = String::new();
    request
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_string(&mut text)
        .context("读取请求体失败")?;
    if text.len() > MAX_BODY {
        bail!("请求体过大");
    }
    serde_json::from_str(&text).context("请求体不是合法 JSON")
}

/// 只接受同源页面发起的写操作。浏览器跨站请求会带 Origin，直接拒绝；
/// curl 等本地工具通常不带 Origin，放行。
fn check_same_origin(request: &Request) -> Result<()> {
    let origin = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_string());
    let Some(origin) = origin else {
        return Ok(());
    };
    let host = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str())
        .unwrap_or("");
    let expected = format!("http://{host}");
    if origin == expected {
        Ok(())
    } else {
        bail!("跨域写请求被拒绝")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-9999", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=10-5", 1000), None);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=0-0", 0), None);
    }

    #[test]
    fn job_id_paths() {
        assert_eq!(parse_job_id("/api/jobs/12", ""), Some(12));
        assert_eq!(parse_job_id("/api/jobs/12/cancel", "/cancel"), Some(12));
        assert_eq!(parse_job_id("/api/jobs/12/file", "/file"), Some(12));
        assert_eq!(parse_job_id("/api/jobs/abc", ""), None);
        assert_eq!(parse_job_id("/api/file/12", ""), None);
    }

    #[test]
    fn login_kinds() {
        assert!(matches!(
            parse_login_kind("qqmusic"),
            Some(LoginKind::QqMusic)
        ));
        assert!(matches!(parse_login_kind("app"), Some(LoginKind::QqMusic)));
        assert!(matches!(parse_login_kind("wx"), Some(LoginKind::Wx)));
        assert!(matches!(parse_login_kind("qq"), Some(LoginKind::Qq)));
        assert!(parse_login_kind("weibo").is_none());
    }

    #[test]
    fn mids_are_strict() {
        assert!(valid_mid("0048Ts0Q0oi3Ti"));
        assert!(!valid_mid(""));
        assert!(!valid_mid("../etc/passwd"));
        assert!(!valid_mid("中文"));
    }

    #[test]
    fn qr_art_detection() {
        assert!(is_qr_art("  ██████  ▄▄▄▄"));
        assert!(!is_qr_art("  等待扫码…"));
    }

    #[test]
    fn host_validation_blocks_dns_rebinding() {
        assert!(host_allowed("127.0.0.1:8088"));
        assert!(host_allowed("localhost:8088"));
        assert!(host_allowed("[::1]:8088"));
        assert!(host_allowed("192.168.1.5:8088"));
        assert!(!host_allowed("evil.example:8088"));
        assert!(!host_allowed("evil.example"));
    }
}
