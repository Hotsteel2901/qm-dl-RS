use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::api::USER_AGENT;
use crate::auth::{self, Auth};

const QQ_APPID: &str = "716027609";
const QQ_DAID: &str = "383";
const QQ_PT_3RD_AID: &str = "100497308";
const WX_APPID: &str = "wx48db31d50e334801";
const WX_REDIRECT: &str =
    "https://y.qq.com/portal/wx_redirect.html?login_type=2&surl=https%3A%2F%2Fy.qq.com%2F";
const MUSICU: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const PT_REFERER: &str = "https://xui.ptlogin2.qq.com/";

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LoginKind {
    #[value(name = "qq")]
    Qq,
    #[value(name = "wx", alias = "wechat")]
    Wx,
}

pub struct LoginOpts {
    pub open_browser: bool,
    pub timeout: Duration,
}

/// 扫码登录入口：拿到 Cookie 后写入默认配置并返回 Auth。
pub fn login(kind: LoginKind, opts: &LoginOpts) -> Result<Auth> {
    let http = CurlJar::new()?;
    let (auth, nick) = match kind {
        LoginKind::Qq => qq_login(&http, opts)?,
        LoginKind::Wx => wx_login(&http, opts)?,
    };

    let path = write_cookie(&auth.cookie)?;
    println!();
    println!(
        "登录成功{}",
        if nick.is_empty() {
            String::new()
        } else {
            format!("（{nick}）")
        }
    );
    println!("  账号 uin: {}", mask_uin(&auth.uin));
    println!("  Cookie 已写入: {}", path.display());
    Ok(auth)
}

// ==================== QQ 扫码 ====================

fn qq_login(http: &CurlJar, opts: &LoginOpts) -> Result<(Auth, String)> {
    for attempt in 1..=3 {
        let t = format!("{:.6}", rand_f64());
        let url = format!(
            "https://ssl.ptlogin2.qq.com/ptqrshow?appid={QQ_APPID}&e=2&l=M&s=3&d=72&v=4&t={t}&daid={QQ_DAID}&pt_3rd_aid={QQ_PT_3RD_AID}"
        );
        let qr = http.get(&url, &[("Referer", PT_REFERER)])?;
        if qr.status != 200 {
            bail!("获取 QQ 二维码失败：HTTP {}", qr.status);
        }
        let qrsig = http
            .cookies()?
            .get("qrsig")
            .cloned()
            .context("服务端没有下发 qrsig")?;
        show_qr(&qr.body, "qq", opts, "手机 QQ")?;

        let started = Instant::now();
        let tty = std::io::stdout().is_terminal();
        let mut scanned = false;
        loop {
            if started.elapsed() > opts.timeout {
                bail!("等待扫码超时（{}s）", opts.timeout.as_secs());
            }
            std::thread::sleep(Duration::from_secs(2));
            let (state, cb) = qq_poll(http, &qrsig)?;
            match state {
                66 => {
                    if !scanned && tty {
                        print!("\r  等待扫码…            ");
                        let _ = std::io::stdout().flush();
                    }
                }
                67 => {
                    if !scanned {
                        scanned = true;
                        progress_line(tty, "已扫码，请在手机上确认…");
                    }
                }
                65 => {
                    println!("  二维码已过期，刷新（第 {attempt} 次）…");
                    if attempt == 3 {
                        bail!("二维码多次过期，请稍后再试");
                    }
                    break;
                }
                0 => {
                    progress_line(tty, "手机端已确认，正在换取登录态…");
                    return qq_exchange(http, &cb);
                }
                other => println!("\r  状态码 {other}，继续等待…"),
            }
        }
    }
    bail!("登录失败：二维码多次过期")
}

fn qq_poll(http: &CurlJar, qrsig: &str) -> Result<(u8, String)> {
    // ptqrtoken 用的是种子为 0 的 hash33；跟 g_tk（种子 5381）不是同一个。
    // token 算错时服务端不会给 ptuiCB，而是直接 WAF 403（很难排查，记一笔）。
    let token = ptqrtoken(qrsig);
    let action = format!("0-0-{}", now_ms());
    let url = format!(
        "https://ssl.ptlogin2.qq.com/ptqrlogin?u1=https%3A%2F%2Fgraph.qq.com%2Foauth2.0%2Flogin_jump\
         &ptqrtoken={token}&ptredirect=0&h=1&t=1&g=1&from_ui=1&ptlang=2052&action={action}\
         &js_ver=20102616&js_type=1&pt_uistyle=40&aid={QQ_APPID}&daid={QQ_DAID}\
         &pt_3rd_aid={QQ_PT_3RD_AID}&has_onekey=1"
    );
    let resp = http.get(
        &url,
        &[("Referer", PT_REFERER), ("Cookie", &format!("qrsig={qrsig}"))],
    )?;
    let text = resp.text();
    if std::env::var_os("QQMUSIC_DEBUG").is_some() {
        eprintln!("[poll] status={} body={text}", resp.status);
    }
    let args = parse_ptuicb(&text).with_context(|| {
        format!(
            "无法解析二维码状态响应（HTTP {}）: {}",
            resp.status,
            text.chars().take(160).collect::<String>()
        )
    })?;
    let state = args
        .first()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .context("二维码状态码异常")?;
    Ok((state, args.get(2).cloned().unwrap_or_default()))
}

fn qq_exchange(http: &CurlJar, cb_url: &str) -> Result<(Auth, String)> {
    let sigx = qs_param(cb_url, "ptsigx").context("回调地址缺少 ptsigx")?;
    let uin = qs_param(cb_url, "uin").context("回调地址缺少 uin")?;

    // 1) check_sig：换 p_skey 等登录态 Cookie
    let check_url = format!(
        "https://ssl.ptlogin2.graph.qq.com/check_sig?uin={uin}&pttype=1&service=ptqrlogin\
         &nodirect=0&ptsigx={sigx}&s_url=https%3A%2F%2Fgraph.qq.com%2Foauth2.0%2Flogin_jump\
         &ptlang=2052&ptredirect=100&aid={QQ_APPID}&daid={QQ_DAID}&j_later=0&low_login_hour=0\
         &regmaster=0&pt_login_type=3&pt_aid=0&pt_aaid=16&pt_light=0&pt_3rd_aid={QQ_PT_3RD_AID}"
    );
    let check = http.get(&check_url, &[("Referer", PT_REFERER)])?;
    let jar = http.cookies()?;
    let p_skey = jar
        .get("p_skey")
        .or_else(|| jar.get("skey"))
        .cloned()
        .unwrap_or_default();
    if p_skey.is_empty() {
        bail!(
            "check_sig 没拿到 p_skey（HTTP {}）：{}",
            check.status,
            check.text().chars().take(120).collect::<String>()
        );
    }

    // 2) graph 授权：换 code
    let g_tk = hash33(&p_skey).to_string();
    let auth_time = now_ms().to_string();
    let ui = pseudo_uuid();
    let form = [
        ("response_type", "code"),
        ("client_id", QQ_PT_3RD_AID),
        (
            "redirect_uri",
            "https://y.qq.com/portal/wx_redirect.html?login_type=1&surl=https://y.qq.com/",
        ),
        ("scope", "get_user_info,get_app_friends"),
        ("state", "state"),
        ("switch", ""),
        ("from_ptlogin", "1"),
        ("src", "1"),
        ("update_auth", "1"),
        ("openapi", "1010_1030"),
        ("g_tk", g_tk.as_str()),
        ("auth_time", auth_time.as_str()),
        ("ui", ui.as_str()),
    ];
    let auth = http.post_form(
        "https://graph.qq.com/oauth2.0/authorize",
        &[("Referer", PT_REFERER)],
        &form,
    )?;
    let location = auth.header("location").unwrap_or_default();
    let code = qs_param(&location, "code")
        .or_else(|| {
            location
                .split("code=")
                .nth(1)
                .map(|s| s.split('&').next().unwrap_or("").to_string())
        })
        .filter(|c| !c.is_empty())
        .with_context(|| format!("graph 未返回 code（HTTP {}, Location: {location}）", auth.status))?;

    // 3) code 换 QQ 音乐票据（跟 wx_redirect.html 里的官方调用完全一致）
    let body = exchange_body(
        http,
        "2",
        "QQConnectLogin.LoginServer",
        "QQLogin",
        json!({ "code": code }),
    )?;
    let resp = musicu_post(http, &body)?;
    let (nick, data) = parse_login_response(&resp)?;
    let auth = build_auth(&http.cookies()?, Some(&data), "QQ 扫码登录")?;
    Ok((auth, nick.unwrap_or_default()))
}

// ==================== 微信扫码 ====================

fn wx_login(http: &CurlJar, opts: &LoginOpts) -> Result<(Auth, String)> {
    let state = format!("st{}", now_ms());
    let url = format!(
        "https://open.weixin.qq.com/connect/qrconnect?appid={WX_APPID}&redirect_uri={}&response_type=code&scope=snsapi_login&state={state}",
        urlencode(WX_REDIRECT)
    );
    let page = http.get(&url, &[("Referer", "https://y.qq.com/")])?;
    if page.status != 200 {
        bail!("获取微信二维码失败：HTTP {}", page.status);
    }
    let uuid = extract_wx_uuid(&page.text())
        .context("微信二维码页里没找到 uuid（appid 或 redirect_uri 可能变了）")?;

    let qr = http.get(
        &format!("https://open.weixin.qq.com/connect/qrcode/{uuid}"),
        &[("Referer", "https://open.weixin.qq.com/")],
    )?;
    show_qr(&qr.body, "wx", opts, "微信")?;

    let started = Instant::now();
    let tty = std::io::stdout().is_terminal();
    let mut scanned = false;
    let code = loop {
        if started.elapsed() > opts.timeout {
            bail!("等待扫码超时（{}s）", opts.timeout.as_secs());
        }
        std::thread::sleep(Duration::from_secs(2));
        let stamp = now_ms().to_string();
        let resp = http.get(
            &format!(
                "https://long.open.weixin.qq.com/connect/l/qrconnect?uuid={uuid}&_={stamp}"
            ),
            &[("Referer", "https://open.weixin.qq.com/")],
        )?;
        let text = resp.text();
        let errcode = field_num(&text, "wx_errcode").unwrap_or(-1);
        match errcode {
            408 => {
                if !scanned && tty {
                    print!("\r  等待扫码…            ");
                    let _ = std::io::stdout().flush();
                }
            }
            404 => {
                if !scanned {
                    scanned = true;
                    progress_line(tty, "已扫码，请在手机上确认…");
                }
            }
            405 => {
                let code = text
                    .split("wx_code='")
                    .nth(1)
                    .map(|s| s.split('\'').next().unwrap_or("").to_string())
                    .unwrap_or_default();
                if code.is_empty() {
                    bail!("微信已确认，但没返回 code");
                }
                progress_line(tty, "手机端已确认，正在换取登录态…");
                break code;
            }
            402 => bail!("二维码已过期，请重新执行 login"),
            403 => bail!("已在手机端取消登录"),
            other => println!("\r  状态 {other}，继续等待…"),
        }
    };

    let body = exchange_body(
        http,
        "1",
        "music.login.LoginServer",
        "Login",
        json!({ "strAppid": WX_APPID, "code": code }),
    )?;
    let resp = musicu_post(http, &body)?;
    let (nick, data) = parse_login_response(&resp)?;
    // 微信通道的票据既可能在 JSON data 里，也可能直接由 Set-Cookie 下发
    let auth = build_auth(&http.cookies()?, Some(&data), "微信扫码登录")?;
    Ok((auth, nick.unwrap_or_default()))
}

fn extract_wx_uuid(html: &str) -> Option<String> {
    let marker = "/connect/qrcode/";
    let idx = html.find(marker)? + marker.len();
    let rest = &html[idx..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    let uuid = &rest[..end];
    (!uuid.is_empty()).then(|| uuid.to_string())
}

// ==================== 换票与 Cookie ====================

struct HttpOut {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpOut {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// 复刻 wx_redirect.html 里那个 s() 函数：comm 里补 g_tk / platform / ct / cv。
fn exchange_body(
    http: &CurlJar,
    tme_login_type: &str,
    module: &str,
    method: &str,
    param: Value,
) -> Result<String> {
    let jar = http.cookies()?;
    let seed = ["qqmusic_key", "p_skey", "skey", "p_lskey", "lskey"]
        .iter()
        .find_map(|k| jar.get(*k).filter(|v| !v.is_empty()).cloned())
        .unwrap_or_default();
    let g_tk = hash33(&seed);
    Ok(json!({
        "comm": {
            "tmeAppID": "qqmusic",
            "tmeLoginType": tme_login_type,
            "g_tk": g_tk,
            "g_tk_new_20200303": g_tk,
            "platform": "yqq",
            "ct": 24,
            "cv": 0,
            "format": "json"
        },
        "req": { "module": module, "method": method, "param": param }
    })
    .to_string())
}

fn musicu_post(http: &CurlJar, body: &str) -> Result<HttpOut> {
    http.post_body(
        MUSICU,
        &[
            ("Referer", "https://y.qq.com/"),
            ("Origin", "https://y.qq.com"),
            ("Accept", "*/*"),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ],
        body,
    )
}

fn parse_login_response(resp: &HttpOut) -> Result<(Option<String>, Value)> {
    let value: Value = serde_json::from_slice(&resp.body).with_context(|| {
        format!(
            "登录响应不是 JSON（HTTP {}）：{}",
            resp.status,
            resp.text().chars().take(160).collect::<String>()
        )
    })?;
    let node = value
        .get("req")
        .or_else(|| value.get("req_0"))
        .context("登录响应缺少 req 字段")?;
    let code = node.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        bail!("登录换票失败，code={code}");
    }
    let data = node.get("data").cloned().unwrap_or(Value::Null);
    let nick = data
        .get("nick")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok((nick, data))
}

fn build_auth(
    jar: &BTreeMap<String, String>,
    data: Option<&Value>,
    source: &str,
) -> Result<Auth> {
    let mut pairs: BTreeMap<String, String> = BTreeMap::new();
    const KEEP: [&str; 10] = [
        "qm_keyst",
        "qqmusic_key",
        "uin",
        "wxuin",
        "p_skey",
        "skey",
        "euin",
        "login_type",
        "p_lskey",
        "lskey",
    ];
    for key in KEEP {
        if let Some(v) = jar.get(key) {
            if !v.is_empty() {
                pairs.insert(key.to_string(), v.clone());
            }
        }
    }

    if let Some(d) = data {
        for key in ["musickey", "musicid", "str_musicid", "wxuin", "uin"] {
            if let Some(v) = d.get(key) {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                if !s.is_empty() {
                    pairs.entry(key.to_string()).or_insert(s);
                }
            }
        }
    }

    let key = pairs
        .get("qm_keyst")
        .or_else(|| pairs.get("qqmusic_key"))
        .filter(|s| !s.is_empty())
        .cloned()
        .context("登录成功但服务端没下发 qm_keyst/qqmusic_key")?;
    let uin = pairs
        .get("wxuin")
        .or_else(|| pairs.get("uin"))
        .filter(|s| !s.is_empty())
        .cloned()
        .context("登录成功但没拿到 uin")?;

    pairs.insert("qm_keyst".into(), key.clone());
    pairs.insert("qqmusic_key".into(), key);
    pairs.insert("uin".into(), uin.clone());
    pairs.insert("wxuin".into(), uin);

    let cookie = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Auth::from_cookie(&cookie, source))
}

fn write_cookie(cookie: &str) -> Result<PathBuf> {
    let path = auth::default_cookie_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
    }
    std::fs::write(&path, cookie).with_context(|| format!("写入 Cookie 失败: {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

// ==================== curl 封装（带 Cookie 罐） ====================

/// 登录链路走 curl 子进程：ptlogin2 前面的 WAF 只放行 curl/浏览器的 TLS 指纹，
/// reqwest（rustls / native-tls）会被直接 403。curl 同时帮我们管 Cookie 罐。
struct CurlJar {
    dir: PathBuf,
    jar: PathBuf,
}

impl CurlJar {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "qqmusic-dl-login-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).with_context(|| format!("创建临时目录失败: {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let jar = dir.join("cookies.txt");
        Ok(Self { dir, jar })
    }

    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpOut> {
        self.run(url, headers, None)
    }

    fn post_form(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        form: &[(&str, &str)],
    ) -> Result<HttpOut> {
        let data = form
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let mut hs = headers.to_vec();
        hs.push(("Content-Type", "application/x-www-form-urlencoded"));
        self.run(url, &hs, Some(data))
    }

    fn post_body(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<HttpOut> {
        self.run(url, headers, Some(body.to_string()))
    }

    fn run(&self, url: &str, headers: &[(&str, &str)], data: Option<String>) -> Result<HttpOut> {
        let hdr_path = self.dir.join("h.txt");
        let body_path = self.dir.join("b.bin");
        let mut cmd = Command::new("curl");
        cmd.arg("-sS")
            .arg("--max-redirs")
            .arg("0")
            .arg("-m")
            .arg("30")
            .arg("-A")
            .arg(USER_AGENT)
            .arg("-b")
            .arg(&self.jar)
            .arg("-c")
            .arg(&self.jar)
            .arg("-D")
            .arg(&hdr_path)
            .arg("-o")
            .arg(&body_path);
        for (k, v) in headers {
            cmd.arg("-H").arg(format!("{k}: {v}"));
        }
        if let Some(d) = &data {
            cmd.arg("--data-binary").arg(d);
        }
        cmd.arg(url);

        if std::env::var_os("QQMUSIC_DEBUG").is_some() {
            eprintln!("[curl] {cmd:?}");
        }
        let out = cmd
            .output()
            .context("调用 curl 失败（登录流程需要系统里有 curl）")?;
        if !out.status.success() {
            bail!(
                "curl 请求失败（退出码 {:?}）：{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let header_text = std::fs::read_to_string(&hdr_path).unwrap_or_default();
        let body = std::fs::read(&body_path).unwrap_or_default();
        let (status, headers) = parse_response_head(&header_text);
        Ok(HttpOut {
            status,
            headers,
            body,
        })
    }

    fn cookies(&self) -> Result<BTreeMap<String, String>> {
        let text = match std::fs::read_to_string(&self.jar) {
            Ok(t) => t,
            Err(_) => return Ok(BTreeMap::new()),
        };
        let mut out = BTreeMap::new();
        for line in text.lines() {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 7 {
                out.insert(cols[5].to_string(), cols[6].to_string());
            }
        }
        Ok(out)
    }
}

impl Drop for CurlJar {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn parse_response_head(text: &str) -> (u16, Vec<(String, String)>) {
    let mut status = 0u16;
    let mut headers = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("HTTP/") {
            status = line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            headers.clear();
        } else if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    (status, headers)
}

// ==================== 二维码展示 ====================

fn show_qr(bytes: &[u8], kind: &str, opts: &LoginOpts, channel: &str) -> Result<()> {
    let ext = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else {
        "jpg"
    };
    let path = qr_path(kind, ext);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, bytes);

    println!();
    println!("  用 {channel} 扫码登录（二维码有时效，过期会提示）");
    match render_qr(bytes) {
        Ok(art) => print!("{art}"),
        Err(e) => println!("  （终端渲染失败：{e}）"),
    }
    println!("  二维码图片: {}", path.display());
    if opts.open_browser {
        open_file(&path);
    }
    println!("  等待扫码…");
    Ok(())
}

fn progress_line(tty: bool, msg: &str) {
    if tty {
        println!("\r  {msg}            ");
    } else {
        println!("  {msg}");
    }
}

fn open_file(path: &Path) {
    #[cfg(target_os = "macos")]
    let candidates = ["open"];
    #[cfg(not(target_os = "macos"))]
    let candidates = ["xdg-open", "gio"];

    for cmd in candidates {
        let mut c = Command::new(cmd);
        if cmd == "gio" {
            c.arg("open");
        }
        c.arg(path);
        if c.status().map(|s| s.success()).unwrap_or(false) {
            return;
        }
    }
}

fn qr_path(kind: &str, ext: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        });
    base.join("qqmusic-dl").join(format!("qr-{kind}.{ext}"))
}

/// 把二维码图片解码成文本，再用半块字符重新画到终端（行高按 2:1 折算，保证扫得动）。
pub fn render_qr(bytes: &[u8]) -> Result<String> {
    let img = image::load_from_memory(bytes).context("二维码图片解码失败")?;
    let gray = img.to_luma8();
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| {
        gray.get_pixel(x as u32, y as u32)[0]
    });
    let grids = prepared.detect_grids();
    let grid = grids.first().context("图片里没检测到二维码")?;
    let (_meta, text) = grid.decode().context("二维码内容解码失败")?;

    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L)
        .context("二维码内容无法重新编码")?;
    let width = code.width();
    let colors = code.to_colors();
    let dark = |x: i32, y: i32| -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < width
            && (y as usize) < width
            && colors[y as usize * width + x as usize] == qrcode::Color::Dark
    };
    let margin = 2i32;
    let size = width as i32 + margin * 2;
    let mut out = String::new();
    for y in (0..size).step_by(2) {
        out.push_str("  ");
        for x in 0..size {
            let top = dark(x - margin, y - margin);
            let bot = dark(x - margin, y + 1 - margin);
            out.push(match (top, bot) {
                (true, true) => '\u{2588}',
                (true, false) => '\u{2580}',
                (false, true) => '\u{2584}',
                (false, false) => ' ',
            });
        }
        out.push('\n');
    }
    Ok(out)
}

// ==================== 小工具 ====================

fn parse_ptuicb(text: &str) -> Option<Vec<String>> {
    let start = text.find("ptuiCB(")? + "ptuiCB(".len();
    let end = start + text[start..].find(')')?;
    let inner = &text[start..end];
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in inner.chars() {
        match ch {
            '\'' => {
                if in_quote {
                    out.push(cur.clone());
                    cur.clear();
                }
                in_quote = !in_quote;
            }
            _ if in_quote => cur.push(ch),
            _ => {}
        }
    }
    Some(out)
}

fn qs_param(url: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let mut rest = url;
    loop {
        let idx = rest.find(&needle)?;
        let after = &rest[idx + needle.len()..];
        let boundary_ok = idx == 0 || matches!(rest.as_bytes()[idx - 1], b'?' | b'&');
        if boundary_ok {
            let value = after.split(['&', '#']).next().unwrap_or("");
            if !value.is_empty() {
                return Some(pct_decode(value));
            }
        }
        rest = after;
    }
}

fn field_num(text: &str, key: &str) -> Option<i64> {
    let idx = text.find(&format!("{key}="))? + key.len() + 1;
    let digits: String = text[idx..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// g_tk 用的 hash33（种子 5381）。
fn hash33(s: &str) -> u64 {
    let mut h: u32 = 5381;
    for b in s.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u32);
    }
    (h & 0x7fff_ffff) as u64
}

/// ptlogin 二维码轮询用的 ptqrtoken（种子 0）。
fn ptqrtoken(s: &str) -> u64 {
    let mut h: u32 = 0;
    for b in s.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u32);
    }
    (h & 0x7fff_ffff) as u64
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn rand_f64() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    nanos as f64 / 1_000_000_000.0
}

fn pseudo_uuid() -> String {
    let nanos = now_ms();
    format!(
        "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
        (nanos & 0xffff_ffff) as u32,
        ((nanos >> 32) & 0xffff) as u16,
        ((nanos >> 16) & 0x0fff) as u16,
        ((nanos >> 4) & 0x0fff) as u16,
        (nanos & 0xffff_ffff_ffff) as u64
    )
}

fn mask_uin(uin: &str) -> String {
    let chars: Vec<char> = uin.chars().collect();
    if chars.len() <= 6 {
        return "*".repeat(chars.len());
    }
    format!(
        "{}***{}",
        chars[..3].iter().collect::<String>(),
        chars[chars.len() - 2..].iter().collect::<String>()
    )
}
