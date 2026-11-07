mod api;
mod auth;
mod bridge;
mod download;
mod login;
mod mirror;
mod probe;

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
    about = "QQ音乐下载器：账号直链 / 匿名镜像 / 会员票据桥"
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
    /// 搜索歌曲，只列出结果
    Search {
        keyword: String,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// 以 JSON 输出
        #[arg(long)]
        json: bool,
    },

    /// 按 songmid 或 QQ音乐歌曲链接下载
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
    Mint {
        #[arg(required = true)]
        songs: Vec<String>,
        #[arg(short = 'q', long, default_value = "320")]
        quality: Quality,
        #[arg(long)]
        json: bool,
    },

    /// 启动本机票据桥：会员账号换直链，其它机器匿名消费
    Serve {
        #[arg(short = 'p', long, default_value_t = 8080)]
        port: u16,
    },

    /// 扫码登录（qq / wx），Cookie 自动写入 ~/.config/qqmusic-dl/cookie.txt
    Login {
        /// 登录方式：qq | wx
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
        eprintln!("错误: {e:#}");
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
            println!("默认 Cookie 文件: {}", auth::default_cookie_path().display());
            println!("匿名镜像源: {}（--mirror 可切换，off 关闭）", cli.mirror);
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
                println!("没有找到结果");
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
            print_auth(client.auth());
            let opts = Opts {
                bridge: bridge_base,
                ..opts
            };
            let total = songs.len();
            for (i, arg) in songs.iter().enumerate() {
                let mid = parse_mid(arg);
                println!("\n[{}/{}] 获取详情: {}", i + 1, total, mid);
                let song = client.detail(&mid)?;
                download_song(
                    &client, &song, quality, &out, dry_run, preview, no_preview, play, &opts,
                )?;
            }
            Ok(())
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
            print_auth(client.auth());
            let opts = Opts {
                bridge: bridge_base,
                ..opts
            };
            let songs = client.search(&keyword, limit)?;
            if songs.is_empty() {
                println!("没有找到结果");
                return Ok(());
            }
            let total = songs.len();
            for (i, song) in songs.iter().enumerate() {
                println!(
                    "\n[{}/{}] {} - {}",
                    i + 1,
                    total,
                    song.artist_str(),
                    song.name
                );
                download_song(
                    &client, song, quality, &out, dry_run, preview, no_preview, play, &opts,
                )?;
            }
            Ok(())
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

        CliCommand::Serve { port } => {
            bridge::serve(port, auth, cli.proxy.as_deref(), cli.insecure)
        }

        CliCommand::Login {
            kind,
            no_open,
            timeout,
        } => {
            let opts = login::LoginOpts {
                open_browser: !no_open,
                timeout: std::time::Duration::from_secs(timeout.max(30)),
            };
            let auth = login::login(kind, &opts)?;
            print_auth(&auth);
            Ok(())
        }
    }
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
        "  {} | {} | {}",
        song.artist_str(),
        song.name,
        quality.label()
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
                        "  票据桥签发: {}{}",
                        stream.quality.label(),
                        if stream.quality != quality { "（降级）" } else { "" }
                    );
                    return finish_one(
                        client,
                        song,
                        &stream,
                        out,
                        dry_run,
                        play,
                        &format!("会员票据桥 · {}", stream.quality.label()),
                        song.size_for(q),
                        opts.verify,
                    );
                }
                Err(e) => println!("  票据桥 {} 不可用: {e}", q.label()),
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
                        "  请求的 {} 不可用，已降级为 {}",
                        quality.label(),
                        q.label()
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
                    println!("  {} 被接口拒绝（result={}）", q.label(), result);
                }
            }
        }
    }

    // 4) 匿名镜像：QQ 不给，就换一家没有付费墙的曲库
    if opts.mirror != "off" {
        match mirror::resolve(client.client(), song, &opts.mirror) {
            Ok(r) => {
                println!("  QQ 拒绝，切换匿名镜像…");
                if r.quality != quality {
                    println!(
                        "  匿名免费源上限 {}（{} 需要账号/票据桥）",
                        r.quality.label(),
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
                println!("  所有整曲通道都失败，回落到官方 60 秒试听");
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
        "当前账号没有该歌曲的播放/下载权限，或歌曲已下架。"
    } else {
        "匿名无法获取整曲；可试 --mirror migu，或用会员 Cookie / 票据桥。"
    };
    bail!("未获取到下载地址（result={}）。{}", last_result, hint)
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
        "  音质: {}{}",
        stream.quality.label(),
        if stream.preview { "（官方 60 秒试听）" } else { "" }
    );
    println!("  来源: {}", source);
    println!("  保存到: {}", path.display());

    if dry_run {
        println!("  地址: {}", stream.url);
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
        "  完成: {} ({:.2} MB)",
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
                println!("  校验: {:.1}s / 期望 {}s ✓", d, song.duration);
            }
            Err(e) => println!("  校验跳过: {e}"),
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
    let base = truncate_chars(&base, 180);
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

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn print_auth(auth: &Auth) {
    if auth.logged_in {
        println!(
            "登录态: 已登录（uin={}, g_tk={}, 来源: {}）",
            mask(&auth.uin),
            auth.g_tk,
            auth.source
        );
    } else {
        println!("登录态: 匿名 —— QQ 免费曲目直链可用，会员曲目自动走匿名镜像");
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
    println!(
        "{:>3}  {} {} {} {:>5}  曲库音质",
        "#",
        pad("歌名", 30),
        pad("歌手", 18),
        pad("专辑", 22),
        "时长",
    );
    for (i, s) in songs.iter().enumerate() {
        println!(
            "{:>3}  {} {} {} {:>5}  {}",
            i + 1,
            pad(&s.name, 30),
            pad(&s.artist_str(), 18),
            pad(&s.album, 22),
            s.duration_str(),
            quality_flags(s)
        );
    }
    println!("\n可用 qqmusic-dl download <songmid> 或 qqmusic-dl get 关键词 下载");
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
