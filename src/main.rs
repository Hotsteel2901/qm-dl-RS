mod api;
mod auth;
mod bridge;
mod download;
mod login;
mod mirror;
mod probe;
mod ui;

use anyhow::{bail, Context, Result};
use api::{QqClient, Quality, Song, StreamLookup, StreamUrl};
use auth::Auth;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::Command;
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

    #[command(subcommand)]
    cmd: CliCommand,
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
    // 输出接到 `head` 等提前关闭的管道时，默认会因 EPIPE 触发 println! panic。
    // 恢复系统默认的 SIGPIPE 行为，让进程安静退出（shell 看到 141）。
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    if let Err(e) = run() {
        eprintln!("{} {e:#}", ui::err("错误:"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let auth = if cli.anon {
        Auth::anonymous()
    } else {
        Auth::load(cli.cookie.as_deref(), cli.cookie_file.as_deref())?
    };
    if !matches!(cli.mirror.as_str(), "auto" | "migu" | "kuwo" | "netease" | "off") {
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

    match cli.cmd {
        CliCommand::Auth => {
            print_auth(&auth);
            println!("  {} {}", ui::label("默认 Cookie 文件"), auth::default_cookie_path().display());
            println!("  {} {}", ui::label("匿名镜像源"), cli.mirror);
            println!(
                "  {}",
                ui::dim("提示: 换账号可执行 login qq / login wx / login qqmusic；--anon 强制纯匿名。")
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
                print_json(&songs)?;
            } else {
                print_table(&songs);
            }
            Ok(())
        }

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
                let mid = parse_mid(arg);
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
                if let Err(e) = download_song(
                    &client, song, quality, &out, dry_run, preview, no_preview, play, &opts,
                ) {
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
                bail!("签发直链需要会员账号 Cookie（--cookie / QQMUSIC_COOKIE / 默认 Cookie 文件）");
            }
            for arg in &songs {
                let mid = parse_mid(arg);
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

    // 1) 仅试听模式
    if preview_only {
        let stream = match client.trial_url(song)? {
            StreamLookup::Found(s) => s,
            StreamLookup::Unavailable { result } => {
                bail!("没有可用的官方试听片段（result={}）", result)
            }
        };
        return finish_one(
            client, song, &stream, out, dry_run, play, "QQ音乐 · 官方试听", 0, opts.verify,
        );
    }

    // 2) 票据桥：会员账号签发的直链，本地匿名消费
    if let Some(base) = &opts.bridge {
        for q in quality.fallbacks() {
            match bridge::fetch_via_bridge(client.client(), base, &song.mid, q) {
                Ok(stream) => {
                    println!(
                        "  {} {}{}",
                        ui::label("票据桥签发"),
                        ui::ok(stream.quality.label()),
                        if stream.quality != quality {
                            ui::warn("（降级）")
                        } else {
                            String::new()
                        }
                    );
                    return finish_one(
                        client,
                        song,
                        &stream,
                        out,
                        dry_run,
                        play,
                        &format!("会员票据桥 · {}", stream.quality.label()),
                        song.size_for(stream.quality),
                        opts.verify,
                    );
                }
                Err(e) => println!("  {} {} 不可用: {e}", ui::warn("票据桥"), q.label()),
            }
        }
    }

    // 3) QQ 原生直链
    let mut last_result = 0i64;
    for q in quality.fallbacks() {
        match client.stream_url(song, q)? {
            StreamLookup::Found(stream) => {
                if q != quality {
                    println!(
                        "  {} 请求的 {} 不可用，已降级为 {}",
                        ui::warn("!"),
                        quality.label(),
                        ui::ok(q.label())
                    );
                }
                let source = if client.auth().logged_in {
                    "QQ音乐 · 账号直链"
                } else {
                    "QQ音乐 · 匿名直链"
                };
                return finish_one(
                    client,
                    song,
                    &stream,
                    out,
                    dry_run,
                    play,
                    source,
                    song.size_for(q),
                    opts.verify,
                );
            }
            StreamLookup::Unavailable { result } => {
                last_result = result;
                if q == quality {
                    println!(
                        "  {} {} 被接口拒绝（result={}）",
                        ui::warn("!"),
                        q.label(),
                        result
                    );
                }
            }
        }
    }

    // 4) 匿名镜像：QQ 不给，就换一家没有付费墙的曲库
    if opts.mirror != "off" {
        println!("  {}", ui::dim("QQ 拒绝，切换匿名镜像…"));
        match mirror::resolve(client.client(), song, &opts.mirror) {
            Ok(r) => {
                if r.quality != quality {
                    println!(
                        "  {} 匿名免费源上限 {}（{} 需要账号/票据桥）",
                        ui::warn("!"),
                        ui::ok(r.quality.label()),
                        quality.label()
                    );
                }
                let stream = StreamUrl {
                    url: r.url,
                    quality: r.quality,
                    preview: false,
                };
                let expected = r.expected_size.unwrap_or(0);
                return finish_one(
                    client, song, &stream, out, dry_run, play, &r.source, expected, opts.verify,
                );
            }
            Err(e) => println!("  {e}"),
        }
    }

    // 5) 官方 60 秒试听
    if !no_preview {
        match client.trial_url(song)? {
            StreamLookup::Found(stream) => {
                println!(
                    "  {}",
                    ui::warn("所有整曲通道都失败，回落到官方 60 秒试听")
                );
                return finish_one(
                    client,
                    song,
                    &stream,
                    out,
                    dry_run,
                    play,
                    "QQ音乐 · 官方试听",
                    0,
                    opts.verify,
                );
            }
            StreamLookup::Unavailable { .. } => {}
        }
    }

    let hint = if client.auth().logged_in {
        "当前账号没有该歌曲的播放/下载权限，或歌曲已下架。\n  可尝试：换有会员权益的账号、--bridge，或 --mirror migu。"
    } else {
        "匿名无法获取整曲。\n  可尝试：login qqmusic（QQ音乐 App 扫码）、--bridge，或 --mirror migu。"
    };
    bail!("未获取到下载地址（result={}）。\n  {}", last_result, hint)
}

#[allow(clippy::too_many_arguments)]
fn finish_one(
    client: &QqClient,
    song: &Song,
    stream: &StreamUrl,
    out: &Path,
    dry_run: bool,
    play: bool,
    source: &str,
    expected: u64,
    verify: bool,
) -> Result<()> {
    let path = target_path(out, song, stream);

    println!(
        "  {} {}{}",
        ui::label("音质"),
        ui::ok(stream.quality.label()),
        if stream.preview {
            ui::warn("（官方 60 秒试听）")
        } else {
            String::new()
        }
    );
    println!("  {} {}", ui::label("来源"), ui::id(source));
    println!("  {} {}", ui::label("保存到"), path.display());

    if dry_run {
        println!("  {} {}", ui::label("地址"), stream.url);
        return Ok(());
    }

    let bytes = download::download(client.client(), &stream.url, &path, stream.quality, expected)
        .with_context(|| {
            if stream.preview {
                "试听片段下载失败（该曲目可能没有公开试听）".to_string()
            } else {
                "整曲下载失败".to_string()
            }
        })?;
    println!(
        "  {} {}（{:.2} MB）",
        ui::done("完成"),
        path.display(),
        bytes as f64 / 1024.0 / 1024.0
    );

    if verify && !stream.preview && song.duration > 0 {
        match probe::duration_of(&path) {
            Ok(d) => {
                let diff = (d - song.duration as f64).abs();
                if diff > 2.5 {
                    let _ = std::fs::remove_file(&path);
                    bail!(
                        "时长校验失败：文件 {:.1}s，歌曲 {}s —— 拿到的是片段或别的版本，已删除",
                        d,
                        song.duration
                    );
                }
                println!("  {} {:.1}s / 期望 {}s", ui::done("校验"), d, song.duration);
            }
            Err(e) => println!("  {} {e}", ui::warn("校验跳过:")),
        }
    }

    if play {
        play_file(&path);
    }
    Ok(())
}

fn target_path(out: &Path, song: &Song, stream: &StreamUrl) -> PathBuf {
    let artist = sanitize(&song.artist_str());
    let title = sanitize(&song.name);
    let mut base = if artist.is_empty() {
        title
    } else {
        format!("{} - {}", artist, title)
    };
    if stream.preview {
        base.push_str(" [试听60s]");
    }
    if base.trim().is_empty() {
        base = song.mid.clone();
    }
    // 按 UTF-8 字节截断：中文歌名按“字符数”截到 180 会超过文件名 255 字节上限。
    let base = truncate_bytes(&base, 180);
    out.join(format!("{}.{}", base, stream.quality.ext()))
}

fn play_file(path: &Path) {
    let attempts: Vec<(&str, Vec<&str>)> = vec![
        ("ffplay", vec!["-nodisp", "-autoexit", "-loglevel", "quiet"]),
        ("mpv", vec!["--no-video", "--really-quiet"]),
    ];
    for (cmd, args) in attempts {
        let status = Command::new(cmd).args(&args).arg(path).status();
        match status {
            Ok(s) if s.success() => return,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                eprintln!("  播放失败: {}", e);
                return;
            }
        }
    }
    eprintln!("  未找到 ffplay/mpv，无法自动播放");
}

fn parse_mid(arg: &str) -> String {
    let arg = arg.trim();
    if let Some(i) = arg.find("/songDetail/") {
        let rest = &arg[i + "/songDetail/".len()..];
        return rest
            .split(['?', '#', '/'])
            .next()
            .unwrap_or(rest)
            .to_string();
    }
    if let Some(i) = arg.find("songmid=") {
        let rest = &arg[i + "songmid=".len()..];
        return rest
            .split(['&', '#', '?'])
            .next()
            .unwrap_or(rest)
            .to_string();
    }
    arg.to_string()
}

fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        };
        out.push(c);
    }
    out.trim().trim_matches('.').to_string()
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end().to_string()
}

/// 下载前的一行登录态，不打印完整说明。
fn print_auth_brief(auth: &Auth) {
    if auth.logged_in {
        println!(
            "  {} {}",
            ui::label("登录态"),
            ui::ok(&format!(
                "已登录（uin={}, 来源: {}）",
                mask(&auth.uin),
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
            ui::id(&mask(&auth.uin)),
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

fn mask(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 6 {
        return "*".repeat(chars.len());
    }
    format!(
        "{}***{}",
        chars[..3].iter().collect::<String>(),
        chars[chars.len() - 2..].iter().collect::<String>()
    )
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
        let qtext = quality_flags(s);
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

fn print_json(songs: &[Song]) -> Result<()> {
    let arr: Vec<_> = songs
        .iter()
        .map(|s| {
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

fn quality_flags(s: &Song) -> String {
    let mut v = Vec::new();
    if s.size_320 > 0 {
        v.push("320k");
    }
    if s.size_128 > 0 {
        v.push("128k");
    }
    if s.size_flac > 0 {
        v.push("FLAC");
    }
    if v.is_empty() {
        "-".to_string()
    } else {
        v.join("/")
    }
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
