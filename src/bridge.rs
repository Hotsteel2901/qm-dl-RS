use anyhow::{bail, Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

use crate::api::{QqClient, Quality, StreamLookup, StreamUrl};
use crate::auth::Auth;

/// 启动一个"票据桥"：用会员账号把 songmid 换成 vkey 直链。
/// 直链本身是 bearer token（不校验 Cookie / IP），任何没有账号的机器都能直接下载。
pub fn serve(port: u16, bind: &str, auth: Auth, proxy: Option<&str>, insecure: bool) -> Result<()> {
    if !auth.logged_in {
        bail!("票据桥需要登录 Cookie（扫码/粘贴你本人的 Cookie 后再启动）");
    }
    let client = QqClient::new(auth, proxy, insecure)?;
    let listener =
        TcpListener::bind((bind, port)).with_context(|| format!("监听 {bind}:{port} 失败"))?;
    let local = listener.local_addr().context("读取票据桥监听地址失败")?;
    if local.ip().is_unspecified() {
        println!(
            "票据桥已启动: http://<本机IP>:{}/url?mid=<songmid>&q=320",
            local.port()
        );
        println!("监听: {local}");
        println!(
            "注意: 任何能访问该端口的人都能签发/使用直链，请只在可信网络使用；\
             仅本机访问可加 --bind 127.0.0.1。"
        );
    } else {
        println!("票据桥已启动: http://{local}/url?mid=<songmid>&q=320");
    }
    println!("直链有效期约 2 小时；无账号的机器可用 --bridge 消费。");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = handle(&client, stream) {
                    eprintln!("桥接错误: {e:#}");
                }
            }
            Err(e) => eprintln!("连接失败: {e}"),
        }
    }
    Ok(())
}

fn handle(client: &QqClient, mut stream: TcpStream) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    // 吃掉请求头
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }

    let (route, query) = match path.split_once('?') {
        Some((r, q)) => (r, q),
        None => (path, ""),
    };
    let params = crate::util::parse_query(query);

    let body = if method != "GET" {
        respond(&mut stream, 405, json!({"ok": false, "error": "only GET"}))?;
        return Ok(());
    } else if route == "/health" {
        json!({"ok": true, "logged_in": client.auth().logged_in})
    } else if route == "/url" {
        let mid = params.get("mid").cloned().unwrap_or_default();
        let q = params
            .get("q")
            .cloned()
            .unwrap_or_else(|| "320".to_string());
        if mid.is_empty() {
            json!({"ok": false, "error": "缺少 mid"})
        } else {
            let quality = match Quality::from_str_loose(&q) {
                Some(q) => q,
                None => {
                    respond(
                        &mut stream,
                        400,
                        json!({"ok": false, "error": format!("未知音质 {q}")}),
                    )?;
                    return Ok(());
                }
            };
            match mint(client, &mid, quality) {
                Ok((url, used)) => json!({
                    "ok": true,
                    "songmid": mid,
                    "quality": used.label(),
                    "url": url,
                    "expires_in": 7200,
                }),
                Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
            }
        }
    } else {
        respond(
            &mut stream,
            404,
            json!({"ok": false, "error": "use /url?mid=..&q=320"}),
        )?;
        return Ok(());
    };

    respond(&mut stream, 200, body)
}

fn respond(stream: &mut TcpStream, status: u16, body: serde_json::Value) -> Result<()> {
    let text = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Error" },
        text.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(text.as_bytes())?;
    stream.flush()?;
    Ok(())
}

pub fn mint(client: &QqClient, mid: &str, quality: Quality) -> Result<(String, Quality)> {
    let song = client.detail(mid)?;
    for q in quality.fallbacks() {
        if let StreamLookup::Found(StreamUrl { url, .. }) = client.stream_url(&song, q)? {
            return Ok((url, q));
        }
    }
    bail!("账号无权签发该曲目（不是超级会员，或歌曲已下架）")
}

/// 客户端：从票据桥拿直链，本地完全不需要账号。
pub fn fetch_via_bridge(
    http: &reqwest::blocking::Client,
    base: &str,
    mid: &str,
    quality: Quality,
) -> Result<StreamUrl> {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/url?mid={mid}&q={}", quality_name(quality));
    let text = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .with_context(|| format!("连接票据桥失败: {base}"))?
        .text()
        .context("读取票据桥响应失败")?;
    let v: serde_json::Value = serde_json::from_str(&text).context("票据桥返回非 JSON")?;
    if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
        let err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("未知错误");
        bail!("票据桥拒绝: {err}");
    }
    let purl = v.get("url").and_then(|u| u.as_str()).unwrap_or("");
    if !purl.starts_with("http") {
        bail!("票据桥未返回有效直链");
    }
    let used = match v.get("quality").and_then(|q| q.as_str()) {
        Some("128k MP3") => Quality::Mp3_128,
        Some("320k MP3") => Quality::Mp3_320,
        Some("FLAC 无损") => Quality::Flac,
        _ => quality,
    };
    Ok(StreamUrl {
        url: purl.to_string(),
        quality: used,
        preview: false,
    })
}

fn quality_name(q: Quality) -> &'static str {
    q.slug()
}
