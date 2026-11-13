//! 全屏终端界面（ratatui）。
//!
//! 设计目标：搜索、结果多选、下载队列实时进度、扫码登录、播放/打开文件，
//! 全部在一个进程里完成，下载流程与 CLI / WebUI 共用 [`crate::flow`]。
//!
//! 线程模型：
//!   - 主线程只负责输入、状态和渲染；
//!   - 搜索每次起一个短线程；
//!   - 下载只有一个常驻 worker，按队列串行处理，避免同时压垮接口；
//!   - worker 通过 mpsc 发结构化事件，绝不直接打印到终端。

use anyhow::{bail, Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{DefaultTerminal, Frame, Terminal};
use std::collections::HashSet;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Once, OnceLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::api::{QqClient, Quality, Song};
use crate::auth::Auth;
use crate::download::Cancelled;
use crate::flow;
use crate::login::{self, LoginKind, LoginOpts};
use crate::util;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Rgb(49, 194, 124);
const ACCENT_DIM: Color = Color::Rgb(35, 110, 78);
const BG: Color = Color::Rgb(10, 16, 14);
const FG: Color = Color::Rgb(224, 236, 230);
const BG_SELECTED: Color = Color::Rgb(24, 48, 38);
const FG_DIM: Color = Color::Rgb(120, 130, 126);
const FG_WARN: Color = Color::Rgb(230, 180, 80);
const FG_ERR: Color = Color::Rgb(235, 100, 100);

pub struct Options {
    pub out: PathBuf,
    pub quality: Quality,
    pub mirror: String,
    pub verify: bool,
    pub bridge: Option<String>,
    pub proxy: Option<String>,
    pub insecure: bool,
    pub anon: bool,
    pub cookie: Option<String>,
    pub cookie_file: Option<PathBuf>,
}

impl Options {
    fn load_auth(&self) -> Result<Auth> {
        if self.anon {
            Ok(Auth::anonymous())
        } else {
            Auth::load(self.cookie.as_deref(), self.cookie_file.as_deref())
        }
    }
}

pub fn run(options: Options) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("TUI 需要在交互式终端中运行；脚本环境请用 CLI 子命令，或使用 `qqmusic-dl web`");
    }
    // 在进入备用屏之前先校验 Cookie，避免错误信息被 alt-screen 吃掉。
    let auth = options.load_auth()?;
    let mut app = App::new(options, auth)?;

    // 只安装一次钩子。ratatui::init 每次调用都会再包一层 panic hook，
    // 登录挂起/恢复反复 init 会让钩子无限嵌套；这里手工管理终端生命周期。
    UI_THREAD.set(thread::current().id()).ok();
    install_panic_hook();
    install_signal_handlers();

    let mut terminal = match init_terminal() {
        Ok(terminal) => terminal,
        Err(e) => {
            leave_terminal();
            return Err(e);
        }
    };
    let result = app.run(&mut terminal);
    leave_terminal();
    result
}

/// UI 线程 id。panic 钩子只在 UI 线程恢复终端；下载 worker 崩溃时写日志，
/// 不能把正在运行的 TUI 切回主屏。
static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();
/// SIGTERM / SIGHUP 置位，让事件循环走正常退出路径并恢复终端。
static TERMINATE: AtomicBool = AtomicBool::new(false);

fn init_terminal() -> Result<DefaultTerminal> {
    enable_raw_mode().context("启用终端 raw mode 失败")?;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste
    )
    .context("进入备用屏幕失败")?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend).context("创建终端失败")?;
    terminal.hide_cursor().context("隐藏光标失败")?;
    terminal.clear().context("清屏失败")?;
    Ok(terminal)
}

fn leave_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
}

fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if UI_THREAD.get() == Some(&thread::current().id()) {
                leave_terminal();
                original(info);
            } else {
                log_worker_panic(info);
            }
        }));
    });
}

fn log_worker_panic(info: &std::panic::PanicHookInfo<'_>) {
    use std::io::Write;
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        });
    let path = base.join("qqmusic-dl").join("tui-panic.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            file,
            "[{}] worker panic: {info}",
            thread::current().name().unwrap_or("unnamed")
        );
    }
}

#[cfg(unix)]
fn install_signal_handlers() {
    unsafe {
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
            libc::signal(sig, handle_signal as *const () as libc::sighandler_t);
        }
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

#[cfg(unix)]
extern "C" fn handle_signal(_: libc::c_int) {
    TERMINATE.store(true, Ordering::Relaxed);
}

// ==================== 状态 ====================

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Focus {
    Search,
    Results,
    Jobs,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ToastLevel {
    Info,
    Ok,
    Warn,
    Err,
}

struct Toast {
    text: String,
    level: ToastLevel,
    until: Instant,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum JobStatus {
    Queued,
    Resolving,
    Downloading,
    Verifying,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn is_active(self) -> bool {
        matches!(
            self,
            JobStatus::Queued
                | JobStatus::Resolving
                | JobStatus::Downloading
                | JobStatus::Verifying
        )
    }

    /// 校验阶段下载已完成，取消已无意义；只对真正会中断的任务显示/接受取消。
    fn can_cancel(self) -> bool {
        matches!(
            self,
            JobStatus::Queued | JobStatus::Resolving | JobStatus::Downloading
        )
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled
        )
    }
}

struct TuiJob {
    id: u64,
    mid: String,
    name: String,
    artist: String,
    quality: Quality,
    status: JobStatus,
    downloaded: u64,
    total: u64,
    speed: f64,
    source: String,
    preview: bool,
    path: Option<PathBuf>,
    url: Option<String>,
    verified: Option<f64>,
    error: Option<String>,
    messages: Vec<String>,
    cancel: Arc<AtomicBool>,
}

impl TuiJob {
    fn new(id: u64, song: &Song, quality: Quality, cancel: Arc<AtomicBool>) -> Self {
        Self {
            id,
            mid: song.mid.clone(),
            name: song.name.clone(),
            artist: song.artist_str(),
            quality,
            status: JobStatus::Queued,
            downloaded: 0,
            total: 0,
            speed: 0.0,
            source: String::new(),
            preview: false,
            path: None,
            url: None,
            verified: None,
            error: None,
            messages: Vec::new(),
            cancel,
        }
    }

    fn display_name(&self) -> String {
        if self.artist.is_empty() {
            self.name.clone()
        } else {
            format!("{} · {}", self.name, self.artist)
        }
    }

    fn ratio(&self) -> Option<f64> {
        if self.total > 0 {
            Some((self.downloaded as f64 / self.total as f64).clamp(0.0, 1.0))
        } else if self.status == JobStatus::Done {
            Some(1.0)
        } else {
            None
        }
    }

    fn push_message(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if self.messages.last().map(String::as_str) == Some(text.as_str()) {
            return;
        }
        self.messages.push(text);
        if self.messages.len() > 40 {
            self.messages.remove(0);
        }
    }
}

enum Popup {
    Help,
    Login { index: usize },
    OutDir(TextInput),
    JobDetail,
    ConfirmQuit,
}

// ==================== 输入框 ====================

struct TextInput {
    text: String,
    /// 字节位置，始终落在 char 边界。
    cursor: usize,
}

impl TextInput {
    fn new(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn insert_str(&mut self, s: &str) {
        let s: String = s
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.text.len());
        self.text.replace_range(self.cursor..next, "");
    }

    fn left(&mut self) {
        if let Some((i, _)) = self.text[..self.cursor].char_indices().last() {
            self.cursor = i;
        }
    }

    fn right(&mut self) {
        if let Some((i, c)) = self.text[self.cursor..].char_indices().next() {
            self.cursor = self.cursor + i + c.len_utf8();
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.text.len();
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn word_backspace(&mut self) {
        // 先删尾部空白，再删一个单词；与 shell/readline 的 Ctrl+W 一致。
        while self.cursor > 0 {
            let prev = self.previous_char_start();
            let ch = self.text[prev..self.cursor].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
        while self.cursor > 0 {
            let prev = self.previous_char_start();
            let ch = self.text[prev..self.cursor].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    fn previous_char_start(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
}

// ==================== worker 消息 ====================

struct JobRequest {
    id: u64,
    mid: String,
    name: String,
    artist: String,
    quality: Quality,
    preview_only: bool,
    mirror: String,
    verify: bool,
    bridge: Option<String>,
    anon: bool,
    cookie: Option<String>,
    cookie_file: Option<PathBuf>,
    proxy: Option<String>,
    insecure: bool,
    out: PathBuf,
    cancel: Arc<AtomicBool>,
}

enum WorkerCommand {
    Job(JobRequest),
}

enum WorkerEvent {
    SearchDone(Result<Vec<Song>, String>),
    Job { id: u64, update: JobUpdate },
}

enum JobUpdate {
    Started {
        name: String,
        artist: String,
    },
    SongInfo {
        name: String,
        artist: String,
    },
    Message {
        text: String,
    },
    Source {
        quality: Quality,
        source: String,
        preview: bool,
        expected_size: u64,
    },
    Path {
        path: PathBuf,
        url: String,
    },
    Progress {
        downloaded: u64,
        total: u64,
        speed: f64,
    },
    Saved {
        bytes: u64,
    },
    Verified {
        actual: f64,
    },
    Finished {
        path: PathBuf,
        bytes: u64,
        verified: Option<f64>,
    },
    Cancelled,
    Failed(String),
}

fn spawn_worker(event_tx: Sender<WorkerEvent>, cmd_rx: Receiver<WorkerCommand>) {
    thread::spawn(move || {
        while let Ok(WorkerCommand::Job(req)) = cmd_rx.recv() {
            let id = req.id;
            let tx = event_tx.clone();
            // 单个任务的 panic 不能带走整个队列；钩子会把详情写进
            // ~/.cache/qqmusic-dl/tui-panic.log，不会撕坏终端。
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(req, &event_tx)));
            if result.is_err() {
                let _ = tx.send(WorkerEvent::Job {
                    id,
                    update: JobUpdate::Failed(
                        "下载线程异常退出（详见 ~/.cache/qqmusic-dl/tui-panic.log）".to_string(),
                    ),
                });
            }
        }
    });
}

fn run_job(req: JobRequest, event_tx: &Sender<WorkerEvent>) {
    let send = |update: JobUpdate| {
        let _ = event_tx.send(WorkerEvent::Job { id: req.id, update });
    };

    if req.cancel.load(Ordering::Relaxed) {
        send(JobUpdate::Cancelled);
        return;
    }
    send(JobUpdate::Started {
        name: req.name.clone(),
        artist: req.artist.clone(),
    });

    let outcome = (|| -> Result<flow::Outcome> {
        // 每个任务开始时重新读登录态：入队后扫码登录/刷新 Cookie 能立即生效。
        let auth = if req.anon {
            Auth::anonymous()
        } else {
            Auth::load(req.cookie.as_deref(), req.cookie_file.as_deref())?
        };
        let client = QqClient::new(auth, req.proxy.as_deref(), req.insecure)?;
        let song = client.detail(&req.mid)?;
        send(JobUpdate::SongInfo {
            name: song.name.clone(),
            artist: song.artist_str(),
        });

        let request = flow::Request {
            quality: req.quality,
            out: req.out.clone(),
            dry_run: false,
            preview_only: req.preview_only,
            no_preview: false,
            mirror: req.mirror.clone(),
            bridge: req.bridge.clone(),
            verify: req.verify,
            cancel: Some(req.cancel.clone()),
        };
        let mut meter = flow::SpeedMeter::new();
        let mut last_progress = Instant::now();
        let mut on_event = |ev: flow::FlowEvent| match ev {
            flow::FlowEvent::Message { text, .. } => send(JobUpdate::Message { text }),
            flow::FlowEvent::Resolved {
                quality,
                source,
                preview,
                expected_size,
                ..
            } => send(JobUpdate::Source {
                quality,
                source,
                preview,
                expected_size,
            }),
            flow::FlowEvent::Target { path, url } => send(JobUpdate::Path { path, url }),
            flow::FlowEvent::Progress { downloaded, total } => {
                let speed = meter.update(downloaded);
                // 每 64KB 一次回调；10MB/s 时约 160 次/秒，全部转发会让
                // UI 事件队列和重绘压力过大。按 100ms 节流，末尾强制送一次。
                let finished = total > 0 && downloaded >= total;
                if last_progress.elapsed() >= Duration::from_millis(100) || finished {
                    last_progress = Instant::now();
                    send(JobUpdate::Progress {
                        downloaded,
                        total,
                        speed,
                    });
                }
            }
            flow::FlowEvent::Saved { bytes, .. } => send(JobUpdate::Saved { bytes }),
            flow::FlowEvent::Verified { actual, .. } => send(JobUpdate::Verified { actual }),
        };
        flow::execute(&client, &song, &request, &mut on_event)
    })();

    match outcome {
        Ok(flow::Outcome::Downloaded {
            path,
            bytes,
            verified,
            ..
        }) => send(JobUpdate::Finished {
            path,
            bytes,
            verified,
        }),
        Ok(flow::Outcome::DryRun { path, .. }) => send(JobUpdate::Finished {
            path,
            bytes: 0,
            verified: None,
        }),
        Err(e) => {
            if e.downcast_ref::<Cancelled>().is_some() {
                send(JobUpdate::Cancelled);
            } else {
                send(JobUpdate::Failed(format!("{e:#}")));
            }
        }
    }
}

// ==================== App ====================

struct App {
    options: Options,
    auth: Auth,
    input: TextInput,
    focus: Focus,
    results: Vec<Song>,
    selected: HashSet<String>,
    table_state: TableState,
    jobs: Vec<TuiJob>,
    job_state: ListState,
    quality: Quality,
    mirror: String,
    preview_only: bool,
    searching: bool,
    toast: Option<Toast>,
    last_hint: String,
    popup: Option<Popup>,
    worker_tx: Sender<WorkerCommand>,
    event_tx: Sender<WorkerEvent>,
    event_rx: Receiver<WorkerEvent>,
    next_job_id: u64,
    spin: usize,
    quit: bool,
}

impl App {
    fn new(options: Options, auth: Auth) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        let (worker_tx, worker_rx) = mpsc::channel();
        spawn_worker(event_tx.clone(), worker_rx);
        let mirror = options.mirror.clone();
        let quality = options.quality;
        Ok(Self {
            options,
            auth,
            input: TextInput::new(String::new()),
            focus: Focus::Search,
            results: Vec::new(),
            selected: HashSet::new(),
            table_state: TableState::default(),
            jobs: Vec::new(),
            job_state: ListState::default(),
            quality,
            mirror,
            preview_only: false,
            searching: false,
            toast: None,
            last_hint: "输入歌名或「歌名 歌手」，Enter 搜索".to_string(),
            popup: None,
            worker_tx,
            event_tx,
            event_rx,
            next_job_id: 1,
            spin: 0,
            quit: false,
        })
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            if TERMINATE.load(Ordering::Relaxed) {
                self.quit = true;
                break;
            }
            self.drain_events();
            terminal.draw(|frame| self.draw(frame))?;
            self.spin = self.spin.wrapping_add(1);

            if event::poll(Duration::from_millis(90))? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        match self.on_key(key) {
                            Action::None => {}
                            Action::Quit => self.quit = true,
                            Action::Login(kind) => self.suspend_and_login(terminal, kind)?,
                        }
                    }
                    Event::Paste(text) => self.on_paste(&text),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                WorkerEvent::SearchDone(result) => {
                    self.searching = false;
                    match result {
                        Ok(songs) => {
                            let count = songs.len();
                            let keep: HashSet<String> =
                                songs.iter().map(|s| s.mid.clone()).collect();
                            self.selected.retain(|mid| keep.contains(mid));
                            self.results = songs;
                            if self.results.is_empty() {
                                self.table_state.select(None);
                                self.set_toast(ToastLevel::Warn, "没有找到结果，试试更短的关键词");
                            } else {
                                self.table_state.select(Some(0));
                                self.focus = Focus::Results;
                                self.set_toast(ToastLevel::Ok, format!("找到 {count} 首"));
                            }
                        }
                        Err(e) => {
                            self.set_toast(ToastLevel::Err, format!("搜索失败: {e}"));
                        }
                    }
                }
                WorkerEvent::Job { id, update } => self.on_job_update(id, update),
            }
        }
    }

    fn on_job_update(&mut self, id: u64, update: JobUpdate) {
        let mut toast: Option<(ToastLevel, String)> = None;
        let mut hint: Option<String> = None;
        {
            let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) else {
                return;
            };
            match update {
                JobUpdate::Started { name, artist } => {
                    if !name.is_empty() {
                        job.name = name;
                    }
                    if !artist.is_empty() {
                        job.artist = artist;
                    }
                    job.status = JobStatus::Resolving;
                    hint = Some(format!("正在解析: {}", job.display_name()));
                }
                JobUpdate::SongInfo { name, artist } => {
                    job.name = name;
                    job.artist = artist;
                }
                JobUpdate::Message { text } => {
                    job.push_message(text.clone());
                    hint = Some(text);
                }
                JobUpdate::Source {
                    quality,
                    source,
                    preview,
                    expected_size,
                } => {
                    job.quality = quality;
                    job.source = source;
                    job.preview = preview;
                    if job.total == 0 && expected_size > 0 {
                        job.total = expected_size;
                    }
                }
                JobUpdate::Path { path, url } => {
                    job.path = Some(path);
                    job.url = Some(url);
                }
                JobUpdate::Progress {
                    downloaded,
                    total,
                    speed,
                } => {
                    job.status = JobStatus::Downloading;
                    job.downloaded = downloaded;
                    // 服务端没给 Content-Length 时 total=0；不要覆盖 Resolved
                    // 阶段已经拿到的 expected_size。
                    if total > 0 {
                        job.total = total;
                    }
                    job.speed = speed;
                }
                JobUpdate::Saved { bytes } => {
                    job.status = JobStatus::Verifying;
                    job.downloaded = bytes;
                    if job.total == 0 {
                        job.total = bytes;
                    }
                }
                JobUpdate::Verified { actual } => job.verified = Some(actual),
                JobUpdate::Finished {
                    path,
                    bytes,
                    verified,
                } => {
                    job.status = JobStatus::Done;
                    job.path = Some(path.clone());
                    job.downloaded = bytes;
                    if job.total == 0 {
                        job.total = bytes;
                    }
                    job.verified = verified;
                    job.speed = 0.0;
                    let skipped = job.messages.iter().any(|m| m.starts_with("已存在，跳过"));
                    toast = Some(if skipped {
                        (
                            ToastLevel::Info,
                            format!("文件已存在，未重新下载: {}", path.display()),
                        )
                    } else {
                        (ToastLevel::Ok, format!("下载完成: {}", path.display()))
                    });
                    hint = Some(format!("完成: {}", job.display_name()));
                }
                JobUpdate::Cancelled => {
                    job.status = JobStatus::Cancelled;
                    job.speed = 0.0;
                    hint = Some(format!("已取消: {}", job.display_name()));
                }
                JobUpdate::Failed(error) => {
                    job.status = JobStatus::Failed;
                    job.error = Some(error.clone());
                    job.speed = 0.0;
                    job.push_message(error);
                    toast = Some((ToastLevel::Err, format!("下载失败: {}", job.display_name())));
                }
            }
        }
        if let Some(text) = hint {
            self.last_hint = text;
        }
        if let Some((level, text)) = toast {
            self.set_toast(level, text);
        }
    }

    fn set_toast(&mut self, level: ToastLevel, text: impl Into<String>) {
        let secs = match level {
            ToastLevel::Info | ToastLevel::Ok => 4,
            ToastLevel::Warn => 6,
            ToastLevel::Err => 9,
        };
        self.toast = Some(Toast {
            text: text.into(),
            level,
            until: Instant::now() + Duration::from_secs(secs),
        });
    }

    // ==================== 输入 ====================

    fn on_paste(&mut self, text: &str) {
        match &mut self.popup {
            Some(Popup::OutDir(input)) => input.insert_str(text),
            Some(_) => {}
            None if self.focus == Focus::Search => self.input.insert_str(text),
            None => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if key.code == KeyCode::F(1) {
            self.popup = Some(Popup::Help);
            return Action::None;
        }
        if self.popup.is_some() {
            return self.on_popup_key(key);
        }
        if key.code == KeyCode::Char('?') && key.modifiers.is_empty() {
            self.popup = Some(Popup::Help);
            return Action::None;
        }
        if self.focus == Focus::Search {
            return self.on_search_key(key);
        }
        self.on_global_key(key)
    }

    fn on_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter => self.start_search(),
            KeyCode::Esc => self.focus = Focus::Results,
            KeyCode::Tab => self.focus = Focus::Results,
            KeyCode::BackTab => self.focus = Focus::Jobs,
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.home()
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => self.input.end(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear()
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.word_backspace()
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert(c)
            }
            _ => {}
        }
        Action::None
    }

    fn on_global_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => return self.request_quit(),
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
            KeyCode::Char('L') => return self.start_login_popup(),
            KeyCode::Char('r') => self.refresh_auth(),
            KeyCode::Char('t') => {
                self.quality = self.quality.cycle_next();
                self.set_toast(
                    ToastLevel::Info,
                    format!("下载音质: {}", self.quality.label()),
                );
            }
            KeyCode::Char('m') => {
                self.mirror = next_mirror(&self.mirror);
                self.set_toast(ToastLevel::Info, format!("匿名镜像: {}", self.mirror));
            }
            KeyCode::Char('v') => {
                self.preview_only = !self.preview_only;
                self.set_toast(
                    ToastLevel::Info,
                    if self.preview_only {
                        "仅下载官方 60 秒试听"
                    } else {
                        "下载整曲（失败时自动回落试听）"
                    },
                );
            }
            KeyCode::Char('o') => {
                self.popup = Some(Popup::OutDir(TextInput::new(
                    self.options.out.display().to_string(),
                )));
            }
            KeyCode::Char('/') => self.focus = Focus::Search,
            KeyCode::Tab => self.focus = self.next_focus(),
            KeyCode::BackTab => self.focus = self.prev_focus(),
            KeyCode::Esc => self.focus = Focus::Results,
            _ => match self.focus {
                Focus::Results => self.on_results_key(key),
                Focus::Jobs => self.on_jobs_key(key),
                Focus::Search => {}
            },
        }
        Action::None
    }

    fn on_results_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_result(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_result(1),
            KeyCode::PageUp => self.move_result(-10),
            KeyCode::PageDown => self.move_result(10),
            KeyCode::Home | KeyCode::Char('g') => self.select_result(0),
            KeyCode::End | KeyCode::Char('G') => {
                if !self.results.is_empty() {
                    self.select_result(self.results.len() - 1);
                }
            }
            KeyCode::Char(' ') => self.toggle_current(),
            KeyCode::Char('a') => self.toggle_all(),
            KeyCode::Char('d') | KeyCode::Enter => self.enqueue_downloads(),
            KeyCode::Char('/') => self.focus = Focus::Search,
            _ => {}
        }
    }

    fn on_jobs_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                let i = self.job_state.selected().unwrap_or(0);
                if !self.jobs.is_empty() {
                    self.job_state.select(Some(i.saturating_sub(1)));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = self.job_state.selected().unwrap_or(0);
                if i + 1 < self.jobs.len() {
                    self.job_state.select(Some(i + 1));
                }
            }
            KeyCode::Char('g') => {
                if !self.jobs.is_empty() {
                    self.job_state.select(Some(0));
                }
            }
            KeyCode::Char('G') => {
                if !self.jobs.is_empty() {
                    self.job_state.select(Some(self.jobs.len() - 1));
                }
            }
            KeyCode::Enter => {
                if self.current_job().is_some() {
                    self.popup = Some(Popup::JobDetail);
                }
            }
            KeyCode::Char('x') => self.cancel_current_job(),
            KeyCode::Char('p') => self.play_current_job(),
            KeyCode::Char('c') => self.clear_finished_jobs(),
            _ => {}
        }
    }

    fn on_popup_key(&mut self, key: KeyEvent) -> Action {
        // 先 take 出来，避免在 &mut self.popup 借用期间再调用 self 的其它方法。
        let Some(popup) = self.popup.take() else {
            return Action::None;
        };
        match popup {
            Popup::Help => {
                if !matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
                    self.popup = Some(Popup::Help);
                }
                Action::None
            }
            Popup::Login { mut index } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Action::None,
                KeyCode::Up | KeyCode::Char('k') => {
                    index = index.saturating_sub(1);
                    self.popup = Some(Popup::Login { index });
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    index = (index + 1).min(2);
                    self.popup = Some(Popup::Login { index });
                    Action::None
                }
                KeyCode::Enter => {
                    let kind = match index {
                        0 => LoginKind::QqMusic,
                        1 => LoginKind::Wx,
                        _ => LoginKind::Qq,
                    };
                    Action::Login(kind)
                }
                _ => {
                    self.popup = Some(Popup::Login { index });
                    Action::None
                }
            },
            Popup::OutDir(mut input) => {
                match key.code {
                    KeyCode::Esc => return Action::None,
                    KeyCode::Enter => {
                        let path = util::expand_tilde(&input.text);
                        if path.as_os_str().is_empty() {
                            self.set_toast(ToastLevel::Warn, "下载目录不能为空");
                            self.popup = Some(Popup::OutDir(input));
                            return Action::None;
                        }
                        if path.exists() && !path.is_dir() {
                            self.set_toast(
                                ToastLevel::Err,
                                format!("不是目录: {}", path.display()),
                            );
                            self.popup = Some(Popup::OutDir(input));
                            return Action::None;
                        }
                        self.options.out = path.clone();
                        self.set_toast(ToastLevel::Ok, format!("下载目录: {}", path.display()));
                        return Action::None;
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.home()
                    }
                    KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.end()
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.clear()
                    }
                    KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.word_backspace()
                    }
                    KeyCode::Backspace => input.backspace(),
                    KeyCode::Delete => input.delete(),
                    KeyCode::Left => input.left(),
                    KeyCode::Right => input.right(),
                    KeyCode::Home => input.home(),
                    KeyCode::End => input.end(),
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        input.insert(c)
                    }
                    _ => {}
                }
                self.popup = Some(Popup::OutDir(input));
                Action::None
            }
            Popup::JobDetail => {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                        return Action::None;
                    }
                    KeyCode::Char('p') => self.play_current_job(),
                    KeyCode::Char('o') => self.open_current_job_dir(),
                    _ => {}
                }
                self.popup = Some(Popup::JobDetail);
                Action::None
            }
            Popup::ConfirmQuit => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Action::Quit,
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => Action::None,
                _ => {
                    self.popup = Some(Popup::ConfirmQuit);
                    Action::None
                }
            },
        }
    }

    // ==================== 动作 ====================

    fn start_search(&mut self) {
        let keyword = self.input.text.trim().to_string();
        if keyword.is_empty() {
            self.set_toast(ToastLevel::Warn, "请输入歌名或歌手");
            return;
        }
        if self.searching {
            return;
        }
        self.searching = true;
        self.set_toast(ToastLevel::Info, "正在搜索…");
        let tx = self.event_sender();
        let auth = self.auth.clone();
        let proxy = self.options.proxy.clone();
        let insecure = self.options.insecure;
        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let client = QqClient::new(auth, proxy.as_deref(), insecure)?;
                client.search(&keyword, 20)
            }));
            let result = match result {
                Ok(Ok(songs)) => Ok(songs),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("搜索线程异常退出".to_string()),
            };
            let _ = tx.send(WorkerEvent::SearchDone(result));
        });
    }

    fn event_sender(&self) -> Sender<WorkerEvent> {
        // App 与下载 worker 各持一个发送端；搜索短线程再克隆一份。
        self.event_tx.clone()
    }

    fn refresh_auth(&mut self) {
        match self.options.load_auth() {
            Ok(auth) => {
                let label = if auth.logged_in {
                    format!("已登录（uin={}）", util::mask(&auth.uin))
                } else {
                    "匿名".to_string()
                };
                self.auth = auth;
                self.set_toast(ToastLevel::Ok, format!("登录态已刷新: {label}"));
            }
            Err(e) => self.set_toast(ToastLevel::Err, format!("读取 Cookie 失败: {e:#}")),
        }
    }

    fn start_login_popup(&mut self) -> Action {
        if self.options.anon {
            self.set_toast(ToastLevel::Warn, "当前是 --anon 纯匿名模式，登录不会生效");
            return Action::None;
        }
        if self
            .options
            .cookie
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            self.set_toast(
                ToastLevel::Warn,
                "--cookie 指定的 Cookie 优先于扫码登录，请去掉该参数后再登录",
            );
            return Action::None;
        }
        if std::env::var_os("QQMUSIC_COOKIE")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.set_toast(
                ToastLevel::Warn,
                "QQMUSIC_COOKIE 环境变量优先于扫码登录，请先 unset 再登录",
            );
            return Action::None;
        }
        self.popup = Some(Popup::Login { index: 0 });
        Action::None
    }

    fn suspend_and_login(&mut self, terminal: &mut DefaultTerminal, kind: LoginKind) -> Result<()> {
        leave_terminal();
        println!();
        println!("{}", crate::ui::bold("扫码登录（完成后按 Enter 返回界面）"));
        println!();
        let opts = LoginOpts {
            open_browser: true,
            timeout: Duration::from_secs(180),
            proxy: self.options.proxy.clone(),
            insecure: self.options.insecure,
        };
        // 挂起期间处于 cooked mode，终端已经恢复；让 SIGTERM/SIGHUP 回到默认行为，
        // 避免登录等待扫码时进程对 kill 无响应（最多 180 秒）。
        #[cfg(unix)]
        unsafe {
            // cooked mode 下 Ctrl+C 必须能立刻终止登录；恢复 TUI 后再装回处理函数。
            for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
        let result = login::login(kind, &opts);
        install_signal_handlers();
        if let Err(e) = &result {
            eprintln!("{} {e:#}", crate::ui::err("登录失败:"));
        }
        println!();
        println!("{}", crate::ui::dim("按 Enter 返回 TUI…"));
        let mut buf = [0u8; 1];
        let _ = std::io::stdin().read(&mut buf);

        *terminal = init_terminal()?;

        match result {
            Ok(auth) => {
                // login() 固定写默认路径；使用 --cookie-file 时同步一份到用户指定位置，
                // 否则刷新登录态会读回旧 Cookie。
                let extra = if let Some(path) = &self.options.cookie_file {
                    match write_cookie_file(path, &auth.cookie) {
                        Ok(()) => Some(format!("已写入 {}", path.display())),
                        Err(e) => Some(format!("自定义 Cookie 文件写入失败: {e}")),
                    }
                } else {
                    None
                };
                self.auth = auth;
                match extra {
                    Some(extra) => self.set_toast(ToastLevel::Warn, format!("登录成功；{extra}")),
                    None => self.set_toast(ToastLevel::Ok, "登录成功，Cookie 已更新"),
                }
            }
            Err(e) => self.set_toast(ToastLevel::Err, format!("登录失败: {e:#}")),
        }
        Ok(())
    }

    fn request_quit(&mut self) -> Action {
        let active = self.jobs.iter().filter(|j| j.status.is_active()).count();
        if active > 0 {
            self.popup = Some(Popup::ConfirmQuit);
            Action::None
        } else {
            Action::Quit
        }
    }

    fn next_focus(&self) -> Focus {
        match self.focus {
            Focus::Search => Focus::Results,
            Focus::Results => Focus::Jobs,
            Focus::Jobs => Focus::Search,
        }
    }

    fn prev_focus(&self) -> Focus {
        match self.focus {
            Focus::Search => Focus::Jobs,
            Focus::Results => Focus::Search,
            Focus::Jobs => Focus::Results,
        }
    }

    fn move_result(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let current = self.table_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.results.len() as isize - 1);
        self.table_state.select(Some(next as usize));
    }

    fn select_result(&mut self, index: usize) {
        if self.results.is_empty() {
            self.table_state.select(None);
        } else {
            self.table_state
                .select(Some(index.min(self.results.len() - 1)));
        }
    }

    fn toggle_current(&mut self) {
        let Some(index) = self.table_state.selected() else {
            return;
        };
        let Some(song) = self.results.get(index) else {
            return;
        };
        let mid = song.mid.clone();
        if !self.selected.remove(&mid) {
            self.selected.insert(mid);
        }
    }

    fn toggle_all(&mut self) {
        if self.results.is_empty() {
            return;
        }
        if self.selected.len() == self.results.len() {
            self.selected.clear();
        } else {
            self.selected = self.results.iter().map(|s| s.mid.clone()).collect();
        }
    }

    fn enqueue_downloads(&mut self) {
        let indices: Vec<usize> = if !self.selected.is_empty() {
            self.results
                .iter()
                .enumerate()
                .filter(|(_, s)| self.selected.contains(&s.mid))
                .map(|(i, _)| i)
                .collect()
        } else if let Some(i) = self.table_state.selected() {
            vec![i]
        } else {
            Vec::new()
        };
        if indices.is_empty() {
            self.set_toast(ToastLevel::Warn, "先选中歌曲（Space 选择 / ↑↓ 移动）");
            return;
        }

        let mut count = 0;
        for index in indices {
            let Some(song) = self.results.get(index).cloned() else {
                continue;
            };
            let id = self.next_job_id;
            self.next_job_id += 1;
            let cancel = Arc::new(AtomicBool::new(false));
            let job = TuiJob::new(id, &song, self.quality, cancel.clone());
            // 队列按「最新在前」存储，与 draw_jobs 的显示顺序一致；
            // current_job() 用 ListState 的索引直接取，避免动作作用到别的任务上。
            self.jobs.insert(0, job);
            let request = JobRequest {
                id,
                mid: song.mid.clone(),
                name: song.name.clone(),
                artist: song.artist_str(),
                quality: self.quality,
                preview_only: self.preview_only,
                mirror: self.mirror.clone(),
                verify: self.options.verify,
                bridge: self.options.bridge.clone(),
                anon: self.options.anon,
                cookie: self.options.cookie.clone(),
                cookie_file: self.options.cookie_file.clone(),
                proxy: self.options.proxy.clone(),
                insecure: self.options.insecure,
                out: self.options.out.clone(),
                cancel,
            };
            if self.worker_tx.send(WorkerCommand::Job(request)).is_err() {
                self.set_toast(ToastLevel::Err, "下载 worker 已退出");
                break;
            }
            count += 1;
        }

        self.selected.clear();
        if !self.jobs.is_empty() {
            self.job_state.select(Some(0));
        }
        if count > 0 {
            self.set_toast(
                ToastLevel::Ok,
                format!("已加入队列: {count} 首（{}）", self.quality.label()),
            );
        }
    }

    fn current_job(&self) -> Option<&TuiJob> {
        self.job_state.selected().and_then(|i| self.jobs.get(i))
    }

    fn current_job_mut(&mut self) -> Option<&mut TuiJob> {
        self.job_state
            .selected()
            .and_then(move |i| self.jobs.get_mut(i))
    }

    fn cancel_current_job(&mut self) {
        let (name, status) = match self.current_job_mut() {
            Some(job) if job.status.can_cancel() => {
                job.cancel.store(true, Ordering::Relaxed);
                (job.display_name(), job.status)
            }
            Some(job) => (job.display_name(), job.status),
            None => return,
        };
        match status {
            JobStatus::Verifying => self.set_toast(
                ToastLevel::Info,
                format!("{name} 已下载完成，正在校验，无法取消"),
            ),
            JobStatus::Queued | JobStatus::Resolving | JobStatus::Downloading => {
                self.set_toast(ToastLevel::Info, format!("正在取消: {name}"))
            }
            _ => self.set_toast(ToastLevel::Info, "该任务已经结束"),
        }
    }

    fn play_current_job(&mut self) {
        let Some(job) = self.current_job() else {
            return;
        };
        if job.status != JobStatus::Done {
            self.set_toast(ToastLevel::Warn, "只有已完成的任务可以播放");
            return;
        }
        let Some(path) = job.path.clone() else {
            return;
        };
        match util::spawn_player(&path) {
            Ok(player) => self.set_toast(
                ToastLevel::Ok,
                format!("正在用 {player} 播放: {}", path.display()),
            ),
            Err(e) => self.set_toast(ToastLevel::Err, format!("播放失败: {e:#}")),
        }
    }

    fn open_current_job_dir(&mut self) {
        let Some(path) = self.current_job().and_then(|j| j.path.clone()) else {
            return;
        };
        let dir = path.parent().unwrap_or(&path).to_path_buf();
        match util::open_path(&dir) {
            Ok(()) => self.set_toast(ToastLevel::Ok, format!("已打开: {}", dir.display())),
            Err(e) => self.set_toast(ToastLevel::Err, format!("打开目录失败: {e:#}")),
        }
    }

    fn clear_finished_jobs(&mut self) {
        let before = self.jobs.len();
        self.jobs.retain(|j| !j.status.is_terminal());
        if self.jobs.len() != before {
            self.job_state
                .select(if self.jobs.is_empty() { None } else { Some(0) });
            self.set_toast(
                ToastLevel::Info,
                format!("已清理 {} 个结束的任务", before - self.jobs.len()),
            );
        }
    }

    // ==================== 绘制 ====================

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // 固定深色底：否则在浅色终端里，白色/默认前景会与终端背景撞色。
        fill_bg(frame, area);
        if area.width < 52 || area.height < 16 {
            let msg = Paragraph::new(vec![
                Line::from(Span::styled(
                    "终端窗口太小",
                    Style::default().fg(FG_WARN).add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "当前 {}x{}，至少需要 52x16",
                    area.width, area.height
                )),
                Line::from(Span::styled("Ctrl+C 退出", Style::default().fg(FG_DIM))),
            ])
            .style(Style::default().fg(FG))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(FG_DIM)),
            );
            frame.render_widget(msg, area);
            return;
        }

        let [header, input, main, status, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .areas(area);

        self.draw_header(frame, header);
        self.draw_input(frame, input);
        if area.width >= 100 {
            let [left, right] =
                Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(main);
            self.draw_results(frame, left);
            self.draw_jobs(frame, right);
        } else {
            let [top, bottom] =
                Layout::vertical([Constraint::Percentage(56), Constraint::Percentage(44)])
                    .areas(main);
            self.draw_results(frame, top);
            self.draw_jobs(frame, bottom);
        }
        self.draw_status(frame, status);
        self.draw_footer(frame, footer);

        match &self.popup {
            Some(Popup::Help) => self.draw_help(frame, area),
            Some(Popup::Login { index }) => self.draw_login(frame, area, *index),
            Some(Popup::OutDir(_)) => self.draw_outdir(frame, area),
            Some(Popup::JobDetail) => self.draw_job_detail(frame, area),
            Some(Popup::ConfirmQuit) => self.draw_confirm_quit(frame, area),
            None => {}
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(area);

        let auth = if self.auth.logged_in {
            Span::styled(
                format!("已登录 {}", util::mask(&self.auth.uin)),
                Style::default().fg(ACCENT),
            )
        } else {
            Span::styled("匿名", Style::default().fg(FG_WARN))
        };
        let mut spans = vec![
            Span::styled(
                "♫ qqmusic-dl",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default()),
            auth,
            Span::styled("  ·  ", Style::default().fg(FG_DIM)),
            Span::styled(self.quality.label(), Style::default().fg(Color::Cyan)),
            Span::styled("  ·  镜像 ", Style::default().fg(FG_DIM)),
            Span::styled(self.mirror.clone(), Style::default().fg(Color::Cyan)),
        ];
        if self.preview_only {
            spans.push(Span::styled("  ·  试听", Style::default().fg(FG_WARN)));
        }
        if self.options.bridge.is_some() {
            spans.push(Span::styled(
                "  ·  票据桥",
                Style::default().fg(Color::Cyan),
            ));
        }
        if self.searching || self.jobs.iter().any(|j| j.status.is_active()) {
            spans.push(Span::styled("  ", Style::default()));
            spans.push(Span::styled(
                SPINNER[self.spin % SPINNER.len()],
                Style::default().fg(ACCENT),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), left);

        let dir = self.options.out.display().to_string();
        let dir = shorten_left(&dir, right.width.saturating_sub(3) as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("目录 {}", dir),
                Style::default().fg(FG_DIM),
            )))
            .alignment(Alignment::Right),
            right,
        );
    }

    fn draw_input(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Search;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(if focused {
                Style::default().fg(ACCENT)
            } else {
                Style::default().fg(ACCENT_DIM)
            })
            .title(Line::from(vec![
                Span::styled(
                    " 搜索 ",
                    Style::default()
                        .fg(if focused { ACCENT } else { FG_DIM })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if focused { "Enter 搜索" } else { "/ 聚焦" },
                    Style::default().fg(FG_DIM),
                ),
            ]));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let width = inner.width as usize;
        if width == 0 {
            return;
        }
        if self.input.text.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "输入歌名 / 歌手，例如「晴天 周杰伦」",
                    Style::default().fg(FG_DIM),
                )),
                inner,
            );
            if focused {
                frame.set_cursor_position((inner.x, inner.y));
            }
            return;
        }

        let cursor_cols = self.input.text[..self.input.cursor].width();
        let scroll = cursor_cols.saturating_sub(width.saturating_sub(1));
        let visible = display_slice_scrolled(&self.input.text, scroll, width);
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().fg(FG)),
            inner,
        );
        if focused {
            let x = inner.x + (cursor_cols - scroll).min(width.saturating_sub(1)) as u16;
            frame.set_cursor_position((x, inner.y));
        }
    }

    fn draw_results(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Results;
        let title = if self.searching {
            format!(" 搜索结果  搜索中{} ", SPINNER[self.spin % SPINNER.len()])
        } else if self.selected.is_empty() {
            format!(" 搜索结果 · {} 首 ", self.results.len())
        } else {
            format!(
                " 搜索结果 · {} 首 · 已选 {} ",
                self.results.len(),
                self.selected.len()
            )
        };
        let block = panel_block(title, focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.results.is_empty() {
            let hint = if self.searching {
                "正在搜索…"
            } else {
                "还没有结果\n\n输入关键词后按 Enter；结果里 Space 选择、d 下载"
            };
            frame.render_widget(
                Paragraph::new(hint)
                    .style(Style::default().fg(FG_DIM))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }

        let show_album = inner.width >= 90;
        let show_id = inner.width >= 72;
        let mut widths = vec![
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Min(12),
            Constraint::Length(if inner.width >= 80 { 14 } else { 10 }),
        ];
        if show_album {
            widths.push(Constraint::Length(16));
        }
        widths.push(Constraint::Length(6));
        widths.push(Constraint::Length(14));
        if show_id {
            widths.push(Constraint::Length(15));
        }

        let mut header_cells = vec!["", "#", "歌名", "歌手"];
        if show_album {
            header_cells.push("专辑");
        }
        header_cells.push("时长");
        header_cells.push("音质");
        if show_id {
            header_cells.push("歌曲ID");
        }
        let header = Row::new(header_cells)
            .style(Style::default().fg(FG_DIM).add_modifier(Modifier::BOLD))
            .height(1);

        let rows: Vec<Row> = self
            .results
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let selected = self.selected.contains(&s.mid);
                let mut cells = vec![
                    ratatui::widgets::Cell::from(if selected { "●" } else { "○" })
                        .style(Style::default().fg(if selected { ACCENT } else { FG_DIM })),
                    ratatui::widgets::Cell::from((i + 1).to_string()),
                    ratatui::widgets::Cell::from(s.name.clone()),
                    ratatui::widgets::Cell::from(s.artist_str()),
                ];
                if show_album {
                    cells.push(ratatui::widgets::Cell::from(s.album.clone()));
                }
                cells.push(ratatui::widgets::Cell::from(s.duration_str()));
                let q = s.quality_flags();
                let q_style = if q.contains("FLAC") {
                    Style::default().fg(Color::Magenta)
                } else if q.contains("320") {
                    Style::default().fg(ACCENT)
                } else if q == "-" {
                    Style::default().fg(FG_DIM)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                cells.push(ratatui::widgets::Cell::from(q).style(q_style));
                if show_id {
                    cells.push(
                        ratatui::widgets::Cell::from(s.mid.clone())
                            .style(Style::default().fg(FG_DIM)),
                    );
                }
                let row = Row::new(cells);
                row.style(Style::default().fg(if selected { ACCENT } else { FG }))
            })
            .collect();

        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .row_highlight_style(
                Style::default()
                    .bg(BG_SELECTED)
                    .fg(FG)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        frame.render_stateful_widget(table, inner, &mut self.table_state);
    }

    fn draw_jobs(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Jobs;
        let active = self.jobs.iter().filter(|j| j.status.is_active()).count();
        let title = if active > 0 {
            format!(" 下载队列 · {} 个 · {} 进行中 ", self.jobs.len(), active)
        } else {
            format!(" 下载队列 · {} 个 ", self.jobs.len())
        };
        let block = panel_block(title, focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.jobs.is_empty() {
            frame.render_widget(
                Paragraph::new("队列为空\n\n在搜索结果里按 d 或 Enter 加入下载")
                    .style(Style::default().fg(FG_DIM))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }

        let items: Vec<ListItem> = self
            .jobs
            .iter()
            .map(|job| {
                let (icon, color) = match job.status {
                    JobStatus::Queued => ("○", FG_DIM),
                    JobStatus::Resolving => (SPINNER[self.spin % SPINNER.len()], Color::Cyan),
                    JobStatus::Downloading => (SPINNER[self.spin % SPINNER.len()], ACCENT),
                    JobStatus::Verifying => (SPINNER[self.spin % SPINNER.len()], Color::Cyan),
                    JobStatus::Done => ("✓", ACCENT),
                    JobStatus::Failed => ("✗", FG_ERR),
                    JobStatus::Cancelled => ("⊘", FG_WARN),
                };
                let quality = job.quality.label();
                let mut line1 = vec![
                    Span::styled(format!("{icon} "), Style::default().fg(color)),
                    Span::styled(format!("{:02}  ", job.id), Style::default().fg(FG_DIM)),
                    Span::styled(
                        job.display_name(),
                        Style::default()
                            .fg(
                                if job.status.is_terminal() && job.status != JobStatus::Done {
                                    FG_DIM
                                } else {
                                    FG
                                },
                            )
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  [{quality}]{}", if job.preview { " 试听" } else { "" }),
                        Style::default().fg(FG_DIM),
                    ),
                ];
                if job.status == JobStatus::Done {
                    if let Some(v) = job.verified {
                        line1.push(Span::styled(
                            format!("  {v:.0}s ✓"),
                            Style::default().fg(ACCENT),
                        ));
                    }
                }
                let line2 = job_detail_line(job);
                ListItem::new(vec![Line::from(line1), line2])
            })
            .collect();

        let list = List::new(items)
            .highlight_style(Style::default().bg(BG_SELECTED))
            .highlight_symbol("▌ ");
        frame.render_stateful_widget(list, inner, &mut self.job_state);
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let (text, style) = if let Some(toast) = &self.toast {
            if toast.until > Instant::now() {
                let color = match toast.level {
                    ToastLevel::Info => Color::Cyan,
                    ToastLevel::Ok => ACCENT,
                    ToastLevel::Warn => FG_WARN,
                    ToastLevel::Err => FG_ERR,
                };
                (
                    toast.text.clone(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )
            } else {
                (self.last_hint.clone(), Style::default().fg(FG_DIM))
            }
        } else {
            (self.last_hint.clone(), Style::default().fg(FG_DIM))
        };
        let text = shorten_right(&text, area.width as usize);
        frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let focus = match self.focus {
            Focus::Search => "搜索",
            Focus::Results => "结果",
            Focus::Jobs => "队列",
        };
        let line1 = Line::from(vec![
            Span::styled("Tab", Style::default().fg(ACCENT)),
            Span::styled(" 切换 · ", Style::default().fg(FG_DIM)),
            Span::styled("Space", Style::default().fg(ACCENT)),
            Span::styled(" 选择 · ", Style::default().fg(FG_DIM)),
            Span::styled("d/Enter", Style::default().fg(ACCENT)),
            Span::styled(" 下载 · ", Style::default().fg(FG_DIM)),
            Span::styled("t", Style::default().fg(ACCENT)),
            Span::styled(" 音质 · ", Style::default().fg(FG_DIM)),
            Span::styled("m", Style::default().fg(ACCENT)),
            Span::styled(" 镜像 · ", Style::default().fg(FG_DIM)),
            Span::styled("v", Style::default().fg(ACCENT)),
            Span::styled(" 试听 · ", Style::default().fg(FG_DIM)),
            Span::styled("L", Style::default().fg(ACCENT)),
            Span::styled(" 登录 · ", Style::default().fg(FG_DIM)),
            Span::styled("o", Style::default().fg(ACCENT)),
            Span::styled(" 目录 · ", Style::default().fg(FG_DIM)),
            Span::styled("?", Style::default().fg(ACCENT)),
            Span::styled(" 帮助 · ", Style::default().fg(FG_DIM)),
            Span::styled("q", Style::default().fg(ACCENT)),
            Span::styled(" 退出", Style::default().fg(FG_DIM)),
        ]);
        let active = self.jobs.iter().filter(|j| j.status.is_active()).count();
        let line2 = Line::from(vec![
            Span::styled(format!("焦点: {focus}"), Style::default().fg(FG_DIM)),
            Span::styled(
                format!(
                    " · 已选 {} 首 · 队列 {} 个（{} 进行中）",
                    self.selected.len(),
                    self.jobs.len(),
                    active
                ),
                Style::default().fg(FG_DIM),
            ),
        ]);
        frame.render_widget(Paragraph::new(vec![line1, line2]), area);
    }

    // ==================== 弹窗 ====================

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(area, 80, 94);
        frame.render_widget(Clear, popup);
        fill_bg(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " 帮助 · 快捷键 ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let lines = vec![
            Line::from(Span::styled(
                "搜索与结果",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from("  /            聚焦搜索框"),
            Line::from("  Enter        搜索（输入框）/ 下载光标歌曲（结果表）"),
            Line::from("  Space        选择 / 取消选择；a 全选或取消全选"),
            Line::from("  d            下载选中歌曲（没有选中则下载光标行）"),
            Line::from("  ↑ ↓ j k      移动；g / G 跳到顶部 / 底部"),
            Line::from(""),
            Line::from(Span::styled(
                "下载队列",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from("  Tab          搜索 → 结果 → 队列 → 搜索"),
            Line::from("  Enter        查看任务详情（详情里 p 播放、o 打开目录）"),
            Line::from("  x            取消选中任务（保留 .part，可续传）"),
            Line::from("  p            播放已完成文件（ffplay / mpv）"),
            Line::from("  c            清空已完成 / 失败 / 取消的任务"),
            Line::from(""),
            Line::from(Span::styled(
                "全局",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from("  t            音质 128 → 320 → FLAC"),
            Line::from("  m            匿名镜像 auto / migu / kuwo / netease / off"),
            Line::from("  v            仅下载官方 60 秒试听"),
            Line::from("  L            扫码登录（QQ音乐 App / 微信 / QQ）"),
            Line::from("  r            刷新本地 Cookie 登录态"),
            Line::from("  o            设置下载目录"),
            Line::from("  q            退出；Ctrl+C 立即退出"),
            Line::from(""),
            Line::from(Span::styled(
                "提示: 搜索框聚焦时可打印字符优先输入；先按 Tab 离开搜索框再使用全局键。",
                Style::default().fg(FG_DIM),
            )),
            Line::from(Span::styled(
                "Esc / Enter / q 关闭帮助",
                Style::default().fg(FG_DIM),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(Color::Gray))
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn draw_login(&self, frame: &mut Frame, area: Rect, index: usize) {
        let popup = centered_rect(area, 58, 42);
        frame.render_widget(Clear, popup);
        fill_bg(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " 扫码登录 ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let options = [
            ("QQ音乐 App 扫码", "推荐 · 同一个码也支持手机 QQ / 微信"),
            ("微信扫码", "open.weixin.qq.com"),
            ("QQ 扫码", "手机 QQ 扫一扫"),
        ];
        let mut lines = vec![Line::from(Span::styled(
            "登录后，VIP 曲目可走账号直链（320k / FLAC）。",
            Style::default().fg(FG_DIM),
        ))];
        lines.push(Line::from(""));
        for (i, (name, hint)) in options.iter().enumerate() {
            let selected = i == index;
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "› " } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    *name,
                    if selected {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(FG)
                    },
                ),
                Span::styled(format!("  {hint}"), Style::default().fg(FG_DIM)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "↑↓ 选择 · Enter 开始 · Esc 取消",
            Style::default().fg(FG_DIM),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn draw_outdir(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(area, 70, 34);
        frame.render_widget(Clear, popup);
        fill_bg(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " 下载目录 ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let input = match &self.popup {
            Some(Popup::OutDir(input)) => input,
            _ => return,
        };
        let width = inner.width as usize;
        let cursor_cols = input.text[..input.cursor].width();
        let scroll = cursor_cols.saturating_sub(width.saturating_sub(1));
        let visible = display_slice_scrolled(&input.text, scroll, width);
        frame.render_widget(
            Paragraph::new(visible).style(Style::default().fg(FG)),
            inner,
        );
        if inner.height > 2 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "支持 ~ 展开；Enter 确认 · Esc 取消",
                    Style::default().fg(FG_DIM),
                )),
                Rect {
                    x: inner.x,
                    y: inner.y + inner.height - 1,
                    width: inner.width,
                    height: 1,
                },
            );
        }
        let x = inner.x + (cursor_cols - scroll).min(width.saturating_sub(1)) as u16;
        frame.set_cursor_position((x, inner.y));
    }

    fn draw_job_detail(&self, frame: &mut Frame, area: Rect) {
        let Some(job) = self.current_job() else {
            return;
        };
        let popup = centered_rect(area, 76, 80);
        frame.render_widget(Clear, popup);
        fill_bg(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                format!(" 任务 #{} · 详情 ", job.id),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let (status_text, color) = match job.status {
            JobStatus::Queued => ("等待中", FG_DIM),
            JobStatus::Resolving => ("解析地址中", Color::Cyan),
            JobStatus::Downloading => ("下载中", ACCENT),
            JobStatus::Verifying => ("校验中", Color::Cyan),
            JobStatus::Done => ("完成", ACCENT),
            JobStatus::Failed => ("失败", FG_ERR),
            JobStatus::Cancelled => ("已取消", FG_WARN),
        };
        let mut lines = vec![
            Line::from(Span::styled(
                job.display_name(),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("状态: ", Style::default().fg(FG_DIM)),
                Span::styled(status_text, Style::default().fg(color)),
                Span::styled(
                    format!("  ·  {}", job.quality.label()),
                    Style::default().fg(FG_DIM),
                ),
                if job.preview {
                    Span::styled("  ·  官方试听", Style::default().fg(FG_WARN))
                } else {
                    Span::styled("", Style::default())
                },
            ]),
            Line::from(vec![
                Span::styled("歌曲ID: ", Style::default().fg(FG_DIM)),
                Span::styled(job.mid.clone(), Style::default().fg(Color::Cyan)),
            ]),
        ];
        if !job.source.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("来源: ", Style::default().fg(FG_DIM)),
                Span::styled(job.source.clone(), Style::default().fg(Color::Gray)),
            ]));
        }
        if job.total > 0 {
            lines.push(Line::from(format!(
                "进度: {} / {}  ({:.0}%)  {}",
                util::human_bytes(job.downloaded),
                util::human_bytes(job.total),
                job.ratio().unwrap_or(0.0) * 100.0,
                util::human_speed(job.speed)
            )));
        }
        if let Some(path) = &job.path {
            lines.push(Line::from(vec![
                Span::styled("文件: ", Style::default().fg(FG_DIM)),
                Span::styled(path.display().to_string(), Style::default().fg(Color::Gray)),
            ]));
        }
        if let Some(url) = &job.url {
            lines.push(Line::from(vec![
                Span::styled("直链: ", Style::default().fg(FG_DIM)),
                Span::styled(url.clone(), Style::default().fg(FG_DIM)),
            ]));
            lines.push(Line::from(Span::styled(
                "      直链约 2 小时有效，请勿公开分享",
                Style::default().fg(FG_DIM),
            )));
        }
        if let Some(v) = job.verified {
            lines.push(Line::from(vec![
                Span::styled("时长校验: ", Style::default().fg(FG_DIM)),
                Span::styled(format!("{v:.1}s ✓"), Style::default().fg(ACCENT)),
            ]));
        }
        if let Some(err) = &job.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(FG_ERR),
            )));
        }
        if !job.messages.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "过程日志",
                Style::default().fg(FG_DIM).add_modifier(Modifier::BOLD),
            )));
            for message in job.messages.iter().rev().take(12).rev() {
                lines.push(Line::from(Span::styled(
                    format!("  {message}"),
                    Style::default().fg(FG_DIM),
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "p 播放 · o 打开目录 · Esc 关闭",
            Style::default().fg(FG_DIM),
        )));
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(Color::Gray))
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn draw_confirm_quit(&self, frame: &mut Frame, area: Rect) {
        let active = self.jobs.iter().filter(|j| j.status.is_active()).count();
        let popup = centered_rect(area, 56, 30);
        frame.render_widget(Clear, popup);
        fill_bg(frame, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(FG_WARN))
            .title(Span::styled(
                " 确认退出 ",
                Style::default().fg(FG_WARN).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("还有 {active} 个任务未完成。")),
                Line::from(Span::styled(
                    "退出会中断下载；.part 文件会保留，下次可续传。",
                    Style::default().fg(FG_DIM),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "y / Enter 退出 · n / Esc 返回",
                    Style::default().fg(FG_WARN),
                )),
            ])
            .style(Style::default().fg(FG))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
            inner,
        );
    }
}

enum Action {
    None,
    Quit,
    Login(LoginKind),
}

// ==================== 小工具 ====================

fn panel_block(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(ACCENT_DIM)
        })
        .title(Span::styled(
            title,
            Style::default()
                .fg(if focused { ACCENT } else { FG_DIM })
                .add_modifier(Modifier::BOLD),
        ))
}

fn next_mirror(current: &str) -> String {
    let i = crate::mirror::POLICIES
        .iter()
        .position(|p| *p == current)
        .unwrap_or(0);
    crate::mirror::POLICIES[(i + 1) % crate::mirror::POLICIES.len()].to_string()
}

/// 登录流程固定写默认 Cookie 路径；`--cookie-file` 场景需要同步一份。
fn write_cookie_file(path: &Path, cookie: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".cookie-{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        std::fs::write(&tmp, cookie)?;
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

/// 队列行只显示来源的第一段（QQ音乐 / 咪咕音乐 / 会员票据桥…），
/// 完整来源留在任务详情里，避免把进度数字挤出面板。
fn short_source(source: &str) -> &str {
    source.split(" · ").next().unwrap_or(source)
}

fn job_detail_line(job: &TuiJob) -> Line<'static> {
    let dim = Style::default().fg(FG_DIM);
    match job.status {
        JobStatus::Queued => Line::from(Span::styled("     等待中…", dim)),
        JobStatus::Resolving => Line::from(Span::styled(
            format!(
                "     解析地址{}",
                if job.source.is_empty() { "" } else { "…" }
            ),
            Style::default().fg(Color::Cyan),
        )),
        JobStatus::Downloading => {
            let ratio = job.ratio().unwrap_or(0.0);
            let filled = (ratio * 10.0).round() as usize;
            let bar = format!(
                "[{}{}]",
                "█".repeat(filled.min(10)),
                "░".repeat(10 - filled.min(10))
            );
            let pct = format!("{:>3.0}%", ratio * 100.0);
            let speed = util::human_speed(job.speed);
            let mut spans = vec![
                Span::styled(format!("     {bar} "), Style::default().fg(ACCENT)),
                Span::styled(pct, dim),
                Span::styled(format!("  {}", util::human_bytes(job.downloaded)), dim),
                Span::styled(format!("  {speed}"), Style::default().fg(Color::Cyan)),
            ];
            let source = short_source(&job.source);
            if !source.is_empty() {
                spans.push(Span::styled(
                    format!("  ·  {source}"),
                    Style::default().fg(FG_DIM),
                ));
            }
            Line::from(spans)
        }
        JobStatus::Verifying => Line::from(Span::styled(
            "     下载完成，校验时长…",
            Style::default().fg(Color::Cyan),
        )),
        JobStatus::Done => {
            let path = job
                .path
                .as_ref()
                .map(|p| {
                    p.file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.display().to_string())
                })
                .unwrap_or_default();
            Line::from(vec![
                Span::styled("     ✓ 完成", Style::default().fg(ACCENT)),
                Span::styled(
                    format!("  {}  {}", util::human_bytes(job.downloaded), path),
                    dim,
                ),
            ])
        }
        JobStatus::Failed => Line::from(Span::styled(
            format!(
                "     ✗ {}",
                job.error
                    .as_deref()
                    .unwrap_or("未知错误")
                    .lines()
                    .next()
                    .unwrap_or("未知错误")
            ),
            Style::default().fg(FG_ERR),
        )),
        JobStatus::Cancelled => Line::from(Span::styled(
            "     ⊘ 已取消（.part 保留，可续传）",
            Style::default().fg(FG_WARN),
        )),
    }
}

fn fill_bg(frame: &mut Frame, area: Rect) {
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let [_, mid_v, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(area);
    let [_, mid_h, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(mid_v);
    mid_h
}

/// 从左边裁掉 `scroll` 个显示列，再截到 `width` 列。
fn display_slice_scrolled(s: &str, scroll: usize, width: usize) -> String {
    let mut out = String::new();
    let mut skipped = 0usize;
    let mut used = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if skipped + w <= scroll {
            skipped += w;
            continue;
        }
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

fn shorten_left(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    let target = max.saturating_sub(1);
    let tail = display_slice_scrolled(s, s.width().saturating_sub(target), target);
    format!("…{tail}")
}

fn shorten_right(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let options = Options {
            out: PathBuf::from("."),
            quality: Quality::Mp3_320,
            mirror: "auto".to_string(),
            verify: true,
            bridge: None,
            proxy: None,
            insecure: false,
            anon: true,
            cookie: None,
            cookie_file: None,
        };
        App::new(options, Auth::anonymous()).expect("app")
    }

    #[test]
    fn question_mark_opens_help_even_in_search_focus() {
        let mut app = test_app();
        assert_eq!(app.focus, Focus::Search);
        app.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(matches!(app.popup, Some(Popup::Help)));
    }

    #[test]
    fn tab_cycles_focus_from_search_to_results() {
        let mut app = test_app();
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Results);
    }

    #[test]
    fn text_input_handles_chinese_cursor() {
        let mut input = TextInput::new(String::new());
        for c in "晴天".chars() {
            input.insert(c);
        }
        assert_eq!(input.text, "晴天");
        input.left();
        input.insert('A');
        assert_eq!(input.text, "晴A天");
        input.backspace();
        assert_eq!(input.text, "晴天");
        input.delete();
        assert_eq!(input.text, "晴");
    }

    #[test]
    fn ctrl_w_deletes_previous_word() {
        let mut input = TextInput::new("foo bar".into());
        input.word_backspace();
        assert_eq!(input.text, "foo ");
        let mut input = TextInput::new("晴天 周杰伦".into());
        input.word_backspace();
        assert_eq!(input.text, "晴天 ");
        let mut input = TextInput::new("abc".into());
        input.word_backspace();
        assert_eq!(input.text, "");
        let mut input = TextInput::new("foo bar  ".into());
        input.word_backspace();
        assert_eq!(input.text, "foo ");
    }

    #[test]
    fn text_input_paste_flattens_newlines() {
        let mut input = TextInput::new("a".into());
        input.insert_str("\r\nb");
        assert_eq!(input.text, "a  b");
    }

    #[test]
    fn mirror_cycles_through_all_policies() {
        let mut value = "auto".to_string();
        let mut seen = vec![value.clone()];
        for _ in 0..crate::mirror::POLICIES.len() {
            value = next_mirror(&value);
            seen.push(value.clone());
        }
        assert_eq!(seen.first(), seen.last());
        assert!(seen.contains(&"off".to_string()));
    }

    #[test]
    fn scrolling_slice_keeps_tail() {
        let s = "abcdef";
        assert_eq!(display_slice_scrolled(s, 2, 3), "cde");
        assert_eq!(display_slice_scrolled(s, 0, 10), "abcdef");
    }

    #[test]
    fn shorten_does_not_split_chinese() {
        let s = shorten_right("周杰伦的晴天哦", 5);
        assert!(s.width() <= 5);
        assert!(s.ends_with('…'));
    }
}
