mod api;
mod auth;
mod bridge;
mod download;
mod flow;
mod login;
mod mirror;
mod probe;
mod tui;
mod ui;
mod util;
mod web;

use anyhow::{bail, Result};
use api::{QqClient, Quality, Song};
use auth::Auth;
use clap::{CommandFactory, Parser, Subcommand};
use flow::FlowEvent;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Parser)]
#[command(
    name = "qqmusic-dl",
    version,
    about = "QQ音乐下载器：账号直链 / 匿名镜像 / 会员票据桥",
    long_about = "QQ音乐下载器：账号直链 / 匿名镜像 / 会员票据桥。\n\n\
                  取流顺序：QQ 原生 vkey 直链 → 匿名镜像（咪咕/酷我/网易，128k）→ 官方 60 秒试听。\n\
                  需要 320k / FLAC 的 VIP 曲目时，请先 login，或使用会员票据桥。",
    after_help = "常用示例：\n  \
                  qqmusic-dl search \"晴天 周杰伦\" -n 5\n  \
                  qqmusic-dl download 0039MnYb0qxYhV -q flac -o ~/Music\n  \
                  qqmusic-dl --anon get \"晴天 周杰伦\" -n 1\n  \
                  qqmusic-dl login qqmusic        # QQ音乐 App 扫码\n  \
                  qqmusic-dl tui                  # 全屏终端界面\n  \
                  qqmusic-dl web --open           # 本地 WebUI\n  \
                  qqmusic-dl serve -p 8080        # 会员票据桥\n  \
                  qqmusic-dl mint 000Jd2XU1i8Dpj -q 320 --json"
)]
struct Cli {
    /// 直接传入 Cookie 字符串（仅在你本人使用自己账号时提供）
    #[arg(long, global = true)]
    cookie: Option<String>,

    /// Cookie 文件路径（默认 ~/.config/qqmusic-dl/cookie.txt）
    #[arg(long, global = true)]
    cookie_file: Option<PathBuf>,

    /// HTTP 代理，例如 http://127.0.0.1:9000（配合 Reqable 抓包）
    #[arg(long, global = true)]
    proxy: Option<String>,

    /// 忽略 TLS 证书错误（配合 Reqable 代理时使用）
    #[arg(long, global = true)]
    insecure: bool,

    /// 完全忽略本地 Cookie / 环境变量，强制纯匿名
    #[arg(long, global = true)]
    anon: bool,

    /// 匿名镜像源：auto | migu | kuwo | netease | off
    #[arg(long, global = true, default_value = "auto")]
    mirror: String,

    /// 跳过下载后的整曲时长校验
    #[arg(long, global = true)]
    no_verify: bool,

    /// 不指定子命令时：交互式终端进入 TUI，非交互环境打印帮助。
    #[command(subcommand)]
    cmd: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// 搜索歌曲，输出歌曲 ID、歌手、专辑、时长与可用音质
    Search {
        keyword: String,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },

    /// 按歌曲 ID / QQ音乐链接下载（支持一次传多首）
    ///
    /// 自动按 320k → 128k → 匿名镜像 → 官方试听的顺序取流；单首失败不会中断其余歌曲。
    /// 匿名下不到 VIP 曲目时，可先 `login qqmusic`，或使用 `--bridge` 消费会员票据。
    Download {
        #[arg(required = true)]
        songs: Vec<String>,
        #[arg(short = 'q', long, default_value = "320")]
        quality: Quality,
        #[arg(short = 'o', long, default_value = ".")]
        out: PathBuf,
        /// 只解析并打印地址，不下载
        #[arg(long)]
        dry_run: bool,
        /// 只下载官方 60 秒试听片段
        #[arg(long, conflicts_with = "no_preview")]
        preview: bool,
        /// 整曲不可用时，不自动下载试听片段
        #[arg(long)]
        no_preview: bool,
        /// 下载完成后调用 ffplay/mpv 播放
        #[arg(long)]
        play: bool,
        /// 从本机/远端"票据桥"取直链（本地无需账号）
        #[arg(long)]
        bridge: Option<String>,
    },

    /// 搜索关键词并下载前 N 首
    ///
    /// 会先列出搜索结果（含歌曲 ID），再逐首下载；某首失败不会影响其余歌曲。
    Get {
        keyword: String,
        #[arg(short = 'n', long, default_value_t = 1)]
        limit: usize,
        #[arg(short = 'q', long, default_value = "320")]
        quality: Quality,
        #[arg(short = 'o', long, default_value = ".")]
        out: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, conflicts_with = "no_preview")]
        preview: bool,
        #[arg(long)]
        no_preview: bool,
        #[arg(long)]
        play: bool,
        #[arg(long)]
        bridge: Option<String>,
    },

    /// 启动全屏终端界面：搜索、多选、下载队列与扫码登录
    ///
    /// 也支持直接运行 `qqmusic-dl`（无子命令）进入 TUI。
    Tui {
        /// 默认下载目录
        #[arg(short = 'o', long, default_value = ".")]
        out: PathBuf,
        /// 默认音质
        #[arg(short = 'q', long, default_value = "320")]
        quality: Quality,
        /// 从票据桥取直链（例如 http://A:8080），本地无需会员账号
        #[arg(long)]
        bridge: Option<String>,
    },

    /// 启动本地 WebUI：浏览器里搜索、批量下载、播放与扫码登录
    ///
    /// 前端完全内嵌在二进制里（单文件、无 CDN、无构建步骤）。
    /// 默认只监听 127.0.0.1；供局域网使用时加 `--bind 0.0.0.0`。
    Web {
        #[arg(short = 'p', long, default_value_t = 8088)]
        port: u16,
        /// 监听地址（默认仅本机）
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// 默认下载目录
        #[arg(short = 'o', long, default_value = ".")]
        out: PathBuf,
        /// 默认音质
        #[arg(short = 'q', long, default_value = "320")]
        quality: Quality,
        /// 启动后自动用浏览器打开
        #[arg(long)]
        open: bool,
        /// 从票据桥取直链（例如 http://A:8080），本地无需会员账号
        #[arg(long)]
        bridge: Option<String>,
    },

    /// 用会员账号签发一条 vkey 直链（bearer token，2 小时内任何人可用）
    ///
    /// 直链本身不校验 Cookie / IP，可贴到任何下载器；请勿发到公开渠道。
    Mint {
        #[arg(required = true)]
        songs: Vec<String>,
        #[arg(short = 'q', long, default_value = "320")]
        quality: Quality,
        #[arg(long)]
        json: bool,
    },

    /// 启动会员票据桥：A 机持号签发，B 机零账号消费
    ///
    /// 默认监听 0.0.0.0:8080，供局域网其它机器访问；仅本机使用可加 `--bind 127.0.0.1`。
    Serve {
        #[arg(short = 'p', long, default_value_t = 8080)]
        port: u16,
        /// 监听地址（默认 0.0.0.0，供局域网其它机器消费；仅本机可用 127.0.0.1）
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
    },

    /// 扫码登录（qq / wx / qqmusic），Cookie 自动写入 ~/.config/qqmusic-dl/cookie.txt
    ///
    /// qqmusic = QQ音乐 App 扫码（同一个码也支持手机 QQ / 微信）。
    Login {
        /// 登录方式：qq | wx | qqmusic（QQ音乐 App）
        #[arg(default_value = "qq")]
        kind: login::LoginKind,
        /// 不自动用 xdg-open 打开二维码图片
        #[arg(long)]
        no_open: bool,
        /// 等待扫码的秒数（默认 180，最少 30）
        #[arg(long, default_value_t = 180)]
        timeout: u64,
    },

    /// 查看 Cookie/登录状态
    Auth,
}

struct Opts {
    mirror: String,
    verify: bool,
    bridge: Option<String>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{} {e:#}", ui::err("错误:"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // 输出接到 `head` 等提前关闭的管道时，默认会因 EPIPE 触发 println! panic。
    // CLI 恢复系统默认 SIGPIPE；TUI/Web 则必须忽略，否则浏览器断开或终端关闭
    // 会让进程直接消失，来不及恢复终端/释放端口。
    #[cfg(unix)]
    unsafe {
        let ui_mode = match cli.cmd.as_ref() {
            Some(CliCommand::Tui { .. })
            | Some(CliCommand::Web { .. })
            // 票据桥是长驻 HTTP 服务，客户端断开时不能让 SIGPIPE 杀掉进程。
            | Some(CliCommand::Serve { .. }) => true,
            // 无子命令只有确实会进入 TUI 时才忽略 SIGPIPE；
            // 非交互环境会改成打印帮助，保持 CLI 的 SIG_DFL 行为。
            None => std::io::stdout().is_terminal() && std::io::stdin().is_terminal(),
            _ => false,
        };
        libc::signal(
            libc::SIGPIPE,
            if ui_mode {
                libc::SIG_IGN
            } else {
                libc::SIG_DFL
            },
        );
    }
    let auth = if cli.anon {
        Auth::anonymous()
    } else {
        Auth::load(cli.cookie.as_deref(), cli.cookie_file.as_deref())?
    };
    if !matches!(
        cli.mirror.as_str(),
        "auto" | "migu" | "kuwo" | "netease" | "off"
    ) {
        bail!(
            "未知镜像源: {}（可选 auto / migu / kuwo / netease / off）",
            cli.mirror
        );
    }
    let opts = Opts {
        mirror: cli.mirror.clone(),
        verify: !cli.no_verify,
        bridge: None,
    };

    let cmd = match cli.cmd {
        Some(cmd) => cmd,
        None => {
            // 无子命令：交互式终端直接进 TUI；管道/脚本环境打印帮助，绝不悄无声息。
            if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
                CliCommand::Tui {
                    out: PathBuf::from("."),
                    quality: Quality::Mp3_320,
                    bridge: None,
                }
            } else {
                let mut command = Cli::command();
                command.print_help()?;
                println!();
                // 非交互环境没有子命令属于用法错误；保留 clap 的退出码 2，
                // 便于脚本区分「没写命令」和「正常执行」。
                std::process::exit(2);
            }
        }
    };

    match cmd {
        CliCommand::Auth => {
            print_auth(&auth);
            println!(
                "  {} {}",
                ui::label("默认 Cookie 文件"),
                auth::default_cookie_path().display()
            );
            println!("  {} {}", ui::label("匿名镜像源"), cli.mirror);
            println!(
                "  {}",
                ui::dim(
                    "提示: 换账号可执行 login qq / login wx / login qqmusic；--anon 强制纯匿名。"
                )
            );
            Ok(())
        }

        CliCommand::Search {
            keyword,
            limit,
            json,
        } => {
            let client = QqClient::new(auth, cli.proxy.as_deref(), cli.insecure)?;
            let songs = client.search(&keyword, limit)?;
            if songs.is_empty() {
                println!("{}", ui::warn("没有找到结果"));
                println!("  {}", ui::dim("试试更短的关键词，或「歌名 歌手」。"));
                return Ok(());
            }
            if json {
                print_json(&songs, Some(&client))?;
            } else {
                print_table(&songs);
            }
            Ok(())
        }

        CliCommand::Tui {
            out,
            quality,
            bridge,
        } => tui::run(tui::Options {
            out,
            quality,
            mirror: cli.mirror,
            verify: !cli.no_verify,
            bridge,
            proxy: cli.proxy,
            insecure: cli.insecure,
            anon: cli.anon,
            cookie: cli.cookie,
            cookie_file: cli.cookie_file,
        }),

        CliCommand::Web {
            port,
            bind,
            out,
            quality,
            open,
            bridge,
        } => web::serve(web::Options {
            port,
            bind,
            out,
            quality,
            open,
            bridge,
            mirror: cli.mirror,
            verify: !cli.no_verify,
            proxy: cli.proxy,
            insecure: cli.insecure,
            anon: cli.anon,
            cookie: cli.cookie,
            cookie_file: cli.cookie_file,
        }),

        CliCommand::Download {
            songs,
            quality,
            out,
            dry_run,
            preview,
            no_preview,
            play,
            bridge: bridge_base,
        } => {
            let client = QqClient::new(auth, cli.proxy.as_deref(), cli.insecure)?;
            print_auth_brief(client.auth());
            let opts = Opts {
                bridge: bridge_base,
                ..opts
            };
            let total = songs.len();
            let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
            for (i, arg) in songs.iter().enumerate() {
                let mid = util::parse_mid(arg);
                println!("\n[{}/{}] 获取详情: {}", i + 1, total, mid);
                let result = client.detail(&mid).and_then(|song| {
                    download_song(
                        &client, &song, quality, &out, dry_run, preview, no_preview, play, &opts,
                    )
                });
                if let Err(e) = result {
                    if total > 1 {
                        eprintln!("  {}", ui::failed(&format!("{mid}（{e:#}）")));
                    }
                    failures.push((mid, e));
                }
            }
            finish_batch(failures, total)
        }

        CliCommand::Get {
            keyword,
            limit,
            quality,
            out,
            dry_run,
            preview,
            no_preview,
            play,
            bridge: bridge_base,
        } => {
            let client = QqClient::new(auth, cli.proxy.as_deref(), cli.insecure)?;
            print_auth_brief(client.auth());
            let opts = Opts {
                bridge: bridge_base,
                ..opts
            };
            let songs = client.search(&keyword, limit)?;
            if songs.is_empty() {
                println!("{}", ui::warn("没有找到结果"));
                println!("  {}", ui::dim("试试更短的关键词，或「歌名 歌手」。"));
                return Ok(());
            }
            let total = songs.len();
            let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
            for (i, song) in songs.iter().enumerate() {
                println!(
                    "\n[{}/{}] {} - {}  {}",
                    i + 1,
                    total,
                    song.artist_str(),
                    song.name,
                    ui::id(&song.mid)
                );
                // search_for_qq_cp 不返回 strMediaMid；下载前补一次详情，
                // 拿到真正的 media_mid 与各音质大小（旧搜索接口才带这两个字段）。
                let result = client.detail(&song.mid).and_then(|detail| {
                    download_song(
                        &client, &detail, quality, &out, dry_run, preview, no_preview, play, &opts,
                    )
                });
                if let Err(e) = result {
                    if total > 1 {
                        eprintln!("  {}", ui::failed(&format!("{}（{e:#}）", song.name)));
                    }
                    failures.push((song.name.clone(), e));
                }
            }
            finish_batch(failures, total)
        }

        CliCommand::Mint {
            songs,
            quality,
            json,
        } => {
            let client = QqClient::new(auth, cli.proxy.as_deref(), cli.insecure)?;
            if !client.auth().logged_in {
                bail!(
                    "签发直链需要会员账号 Cookie（--cookie / QQMUSIC_COOKIE / 默认 Cookie 文件）"
                );
            }
            for arg in &songs {
                let mid = util::parse_mid(arg);
                let song = client.detail(&mid)?;
                let (url, used) = bridge::mint(&client, &mid, quality)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "songmid": song.mid,
                            "name": song.name,
                            "artists": song.artists,
                            "quality": used.label(),
                            "expires_in": 7200,
                            "url": url,
                        }))?
                    );
                } else {
                    println!("{} - {}", song.artist_str(), song.name);
                    println!("  音质: {}", used.label());
                    println!("  直链: {}", url);
                }
            }
            Ok(())
        }

        CliCommand::Serve { port, bind } => {
            bridge::serve(port, &bind, auth, cli.proxy.as_deref(), cli.insecure)
        }

        CliCommand::Login {
            kind,
            no_open,
            timeout,
        } => {
            let opts = login::LoginOpts {
                open_browser: !no_open,
                timeout: std::time::Duration::from_secs(timeout.max(30)),
                proxy: cli.proxy.clone(),
                insecure: cli.insecure,
            };
            let auth = login::login(kind, &opts)?;
            if let Some(path) = &cli.cookie_file {
                if path != &auth::default_cookie_path() {
                    auth::write_cookie_file(path, &auth.cookie)?;
                    println!("  {} {}", ui::label("Cookie 已同步"), path.display());
                }
            }
            let explicit_cookie = cli
                .cookie
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            let env_cookie = std::env::var_os("QQMUSIC_COOKIE")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if explicit_cookie || env_cookie {
                println!(
                    "  {}",
                    ui::warn(
                        "注意: --cookie / QQMUSIC_COOKIE 的优先级高于新写入的 Cookie 文件，后续命令请去掉或更新它们。"
                    )
                );
            }
            print_auth(&auth);
            Ok(())
        }
    }
}

/// 批量下载的收尾：单曲保持原始错误，批量则继续处理其它歌曲并汇总结果。
fn finish_batch(mut failures: Vec<(String, anyhow::Error)>, total: usize) -> Result<()> {
    if failures.is_empty() {
        if total > 1 {
            println!();
            println!("  {}", ui::done(&format!("全部 {total} 首处理完成")));
        }
        return Ok(());
    }
    if total == 1 {
        return Err(failures.pop().expect("non-empty").1);
    }
    println!();
    println!(
        "  {} 共 {} 首，成功 {}，失败 {}",
        ui::bold("批量结果"),
        total,
        total - failures.len(),
        failures.len()
    );
    bail!("{}/{} 首歌曲处理失败", failures.len(), total)
}

#[allow(clippy::too_many_arguments)]
fn download_song(
    client: &QqClient,
    song: &Song,
    quality: Quality,
    out: &Path,
    dry_run: bool,
    preview_only: bool,
    no_preview: bool,
    play: bool,
    opts: &Opts,
) -> Result<()> {
    println!(
        "  {}  {}  {}",
        ui::bold(&song.name),
        ui::dim(&song.artist_str()),
        ui::dim(quality.label())
    );

    let request = flow::Request {
        quality,
        out: out.to_path_buf(),
        dry_run,
        preview_only,
        no_preview,
        mirror: opts.mirror.clone(),
        bridge: opts.bridge.clone(),
        verify: opts.verify,
        cancel: None,
    };
    let mut reporter = CliReporter::new();
    let result = flow::execute(client, song, &request, &mut |event| reporter.event(event));
    reporter.finish();
    let outcome = result?;

    match &outcome {
        flow::Outcome::DryRun { url, .. } => {
            println!("  {} {}", ui::label("地址"), url);
        }
        flow::Outcome::Downloaded { path, .. } if play => {
            util::play_file(path);
        }
        flow::Outcome::Downloaded { .. } => {}
    }
    Ok(())
}

/// 把公共流程事件渲染成 CLI 原有风格；下载进度仍用 indicatif。
struct CliReporter {
    bar: Option<ProgressBar>,
    skipped: bool,
}

impl CliReporter {
    fn new() -> Self {
        Self {
            bar: None,
            skipped: false,
        }
    }

    fn event(&mut self, event: FlowEvent) {
        match event {
            FlowEvent::Message { level, text } => {
                if text.starts_with("已存在，跳过") {
                    self.skipped = true;
                }
                match level {
                    flow::Level::Info => println!("  {text}"),
                    flow::Level::Dim => println!("  {}", ui::dim(&text)),
                    flow::Level::Warn => println!("  {} {text}", ui::warn("!")),
                }
            }
            FlowEvent::Resolved {
                quality,
                source,
                preview,
                ..
            } => {
                println!(
                    "  {} {}{}",
                    ui::label("音质"),
                    ui::ok(quality.label()),
                    if preview {
                        ui::warn("（官方 60 秒试听）")
                    } else {
                        String::new()
                    }
                );
                println!("  {} {}", ui::label("来源"), ui::id(&source));
            }
            FlowEvent::Target { path, .. } => {
                println!("  {} {}", ui::label("保存到"), path.display());
            }
            FlowEvent::Progress { downloaded, total } => self.progress(downloaded, total),
            FlowEvent::Saved { path, bytes, .. } => {
                self.finish();
                println!(
                    "  {} {}（{:.2} MB）",
                    if self.skipped {
                        ui::warn("已存在")
                    } else {
                        ui::done("完成")
                    },
                    path.display(),
                    bytes as f64 / 1024.0 / 1024.0
                );
            }
            FlowEvent::Verified { actual, expected } => {
                println!("  {} {:.1}s / 期望 {}s", ui::done("校验"), actual, expected);
            }
        }
    }

    fn progress(&mut self, downloaded: u64, total: u64) {
        if self.bar.is_none() {
            let bar = if total > 0 {
                let bar = ProgressBar::new(total);
                bar.set_style(
                    ProgressStyle::with_template(
                        "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] \
                         {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
                    )
                    .unwrap()
                    .progress_chars("#>-"),
                );
                bar
            } else {
                let bar = ProgressBar::new_spinner();
                bar.set_style(
                    ProgressStyle::with_template(
                        "{spinner:.green} [{elapsed_precise}] {bytes} ({bytes_per_sec})",
                    )
                    .unwrap(),
                );
                bar
            };
            self.bar = Some(bar);
        }
        if let Some(bar) = &self.bar {
            bar.set_position(downloaded);
        }
    }

    fn finish(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }
}

/// 下载前的一行登录态，不打印完整说明。
fn print_auth_brief(auth: &Auth) {
    if auth.logged_in {
        println!(
            "  {} {}",
            ui::label("登录态"),
            ui::ok(&format!(
                "已登录（uin={}, 来源: {}）",
                util::mask(&auth.uin),
                auth.source
            ))
        );
    } else {
        println!(
            "  {} {}",
            ui::label("登录态"),
            ui::warn("匿名（VIP 曲目自动尝试匿名镜像）")
        );
    }
}

fn print_auth(auth: &Auth) {
    if auth.logged_in {
        println!(
            "  {} {}（uin={}, g_tk={}, 来源: {}）",
            ui::label("登录态"),
            ui::ok("已登录"),
            ui::id(&util::mask(&auth.uin)),
            auth.g_tk,
            auth.source
        );
        println!(
            "  {}",
            ui::dim("提示: 320k / FLAC 直链是否可用取决于账号本身的会员权益。")
        );
    } else {
        println!("  {} {}", ui::label("登录态"), ui::warn("匿名"));
        println!(
            "  {}",
            ui::dim("免费曲目可直接下载；VIP 曲目会自动尝试匿名镜像（128k 整曲）。")
        );
        println!(
            "  {} 获取 320k/FLAC：{}",
            ui::dim("下一步"),
            ui::bold("qqmusic-dl login qqmusic")
        );
        println!(
            "  {} 或使用会员票据桥：{}",
            ui::dim("     "),
            ui::bold("qqmusic-dl serve -p 8080")
        );
    }
}

fn print_table(songs: &[Song]) {
    // 窄终端自动隐藏专辑列，避免长行折返；歌曲 ID 永远保留。
    let term_width = console::Term::stdout().size().1 as usize;
    let (name_w, artist_w, album_w) = if term_width >= 110 {
        (28, 16, 20)
    } else if term_width >= 88 {
        (18, 12, 12)
    } else {
        (16, 10, 0)
    };

    let mut header = format!(
        "{:>3}  {} {} {}",
        "#",
        pad("歌曲ID", 15),
        pad("歌名", name_w),
        pad("歌手", artist_w)
    );
    if album_w > 0 {
        header.push_str(&format!(" {} ", pad("专辑", album_w)));
    }
    header.push_str(&format!("{:>5}  {}", "时长", "曲库音质"));
    println!("{}", ui::bold(&header));

    for (i, s) in songs.iter().enumerate() {
        let mut line = format!(
            "{:>3}  {} {} {}",
            i + 1,
            ui::id(&pad(&s.mid, 15)),
            pad(&s.name, name_w),
            pad(&s.artist_str(), artist_w)
        );
        if album_w > 0 {
            line.push_str(&format!(" {} ", pad(&s.album, album_w)));
        }
        let qtext = s.quality_flags();
        let qdisp = if qtext == "-" {
            ui::dim("-")
        } else {
            ui::ok(&qtext)
        };
        line.push_str(&format!("{:>5}  {}", s.duration_str(), qdisp));
        println!("{line}");
    }
    println!();
    println!(
        "  {} 共 {} 首 · 直接下载：{}",
        ui::dim("提示:"),
        songs.len(),
        ui::bold("qqmusic-dl download <歌曲ID>")
    );
}

fn print_json(songs: &[Song], client: Option<&QqClient>) -> Result<()> {
    let arr: Vec<_> = songs
        .iter()
        .map(|s| {
            // 新搜索接口不带 strMediaMid，搜索结果里的 media_mid 只是
            // songmid 的兜底值；--json 面向脚本，补一次详情拿准确值。
            let owned = match client {
                Some(client) if s.media_mid == s.mid => {
                    client.detail(&s.mid).unwrap_or_else(|_| s.clone())
                }
                _ => s.clone(),
            };
            let s = &owned;
            serde_json::json!({
                "songmid": s.mid,
                "media_mid": s.media_mid,
                "name": s.name,
                "artists": s.artists,
                "album": s.album,
                "duration": s.duration,
                "size_128": s.size_128,
                "size_320": s.size_320,
                "size_flac": s.size_flac,
                "preview_size": s.preview_size,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr)?);
    Ok(())
}

fn pad(s: &str, width: usize) -> String {
    let t = truncate_display(s, width);
    let used = t.width();
    format!("{}{}", t, " ".repeat(width.saturating_sub(used)))
}

fn truncate_display(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}
