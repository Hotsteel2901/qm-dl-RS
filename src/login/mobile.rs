//! QQ音乐 App 扫码登录。
//!
//! 与 QQ / 微信二维码不同，这条链路是：
//!   1. musicu `CreateQRCode` 拿二维码（PNG data URL）和 qrcodeID；
//!   2. 通过 MQTT 5 over WebSocket（wss://mu.y.qq.com/ws/handshake）订阅
//!      `management.qrcode_login/<qrcodeID>`，接收 scanned / canceled / cookies 等事件；
//!   3. 手机确认后，用事件里的 qqmusic_uin + qqmusic_key 调 LoginServer/Login
//!      （tmeLoginType=6）换 musickey / musicid，再走统一的 Cookie 落盘。

use super::{
    build_auth, musicu_post_json, now_ms, show_qr, CurlJar, HttpOut, LoginOpts,
};
use crate::auth::Auth;
use anyhow::{bail, Context, Result};
use base64::Engine;
use rumqttc::v5::mqttbytes::v5::{
    Packet, Publish, SubscribeProperties, SubscribeReasonCode,
};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{Client, Event, MqttOptions};
use rumqttc::{Proxy, ProxyAuth, ProxyType, TlsConfiguration, Transport};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MQTT_HOST: &str = "mu.y.qq.com";
const MQTT_PATH: &str = "/ws/handshake";
const MQTT_TOPIC_PREFIX: &str = "management.qrcode_login";

// 手机客户端链路走 Android 版 musicu 公参（与 QQMusicApi 已验证可用的实现一致）。
// 用 Web 版 comm（yqq / cv=0）换票会被服务端判为登录态失效（code=1000）。
const ANDROID_CT: u64 = 11;
const ANDROID_CV: u64 = 13_020_508;
const ANDROID_UID: &str = "3931641530";
const FALLBACK_QIMEI36: &str = "6c9d3cd110abca9b16311cee10001e717614";
const LOGIN_MODULE: &str = "music.login.LoginServer";

/// 扫码成功后由 MQTT 事件带回来的临时票据。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScanCreds {
    uin: String,
    key: String,
}

/// 一次扫码事件。未知事件保持 Ignore，继续等待。
#[derive(Clone, Debug, PartialEq, Eq)]
enum ScanEvent {
    Scanned,
    Credentials(ScanCreds),
    Ignore,
}

pub(super) fn login(http: &CurlJar, opts: &LoginOpts) -> Result<(Auth, String)> {
    let (qr_png, qrcode_id, expires_in) = create_qr(http)?;

    let mut steps: Vec<String> = vec![
        "打开 QQ音乐 App → 我的 → 扫一扫".to_string(),
        "也可以用手机 QQ / 微信扫同一个码".to_string(),
        "在手机上点击“确认登录”，Cookie 会自动保存".to_string(),
    ];
    if let Some(secs) = expires_in {
        steps.push(format!(
            "二维码约 {} 分钟有效；本次最多等待 {} 秒（--timeout 可调）",
            (secs / 60).max(1),
            opts.timeout.as_secs()
        ));
    }
    let step_refs: Vec<&str> = steps.iter().map(String::as_str).collect();
    show_qr(&qr_png, "qqmusic", opts, "QQ音乐 App", &step_refs)?;

    // 关键：换票必须发生在 MQTT 会话仍然存活时。扫码 token 可能绑定当前
    // MQTT 连接，先断开再换票会被服务端判为登录态失效（code=1000）。
    let (nick, data) = wait_for_scan(&qrcode_id, opts, |creds| {
        exchange_creds(http, &qrcode_id, creds)
    })?;
    let auth = build_auth(&http.cookies()?, Some(&data), "QQ音乐 App 扫码登录")?;
    Ok((auth, nick.unwrap_or_default()))
}

fn exchange_creds(
    http: &CurlJar,
    qrcode_id: &str,
    creds: ScanCreds,
) -> Result<(Option<String>, Value)> {
    println!("  正在用手机端票据换取登录态…");
    let musicid: u64 = creds
        .uin
        .parse()
        .with_context(|| format!("手机端返回的 uin 不是数字: {}", creds.uin))?;
    let param = json!({
        "musicid": musicid,
        "qrCodeID": qrcode_id,
        "token": creds.key,
    });
    let body = mobile_request_body("Login", param, Some("6")).to_string();
    if std::env::var_os("QQMUSIC_DEBUG").is_some() {
        // 只打印结构，token 已脱敏。
        eprintln!("[login] Login 请求: {}", redact_token(&body));
    }
    let resp = musicu_post_json(http, &body)?;
    parse_module_response(&resp, LOGIN_MODULE, "Login")
}

/// 手机客户端请求：Android 公参（ct=11 / cv=13020508 / QIMEI36 / uid），
/// 外层 key 用 `module.method`，body 用 JSON。这是 QQMusicApi 已验证的形状。
fn mobile_request_body(method: &str, param: Value, tme_login_type: Option<&str>) -> Value {
    let mut comm = json!({
        "ct": ANDROID_CT.to_string(),
        "cv": ANDROID_CV.to_string(),
        "v": ANDROID_CV.to_string(),
        "QIMEI36": FALLBACK_QIMEI36,
        "tmeAppID": "qqmusic",
        "format": "json",
        "inCharset": "utf-8",
        "outCharset": "utf-8",
        "uid": ANDROID_UID,
    });
    if let Some(t) = tme_login_type {
        comm["tmeLoginType"] = Value::String(t.to_string());
    }
    let mut payload = serde_json::Map::new();
    payload.insert("comm".to_string(), comm);
    payload.insert(
        format!("{LOGIN_MODULE}.{method}"),
        json!({"module": LOGIN_MODULE, "method": method, "param": param}),
    );
    Value::Object(payload)
}

/// 手机客户端接口的响应按 `module.method` 回包；兼容 `req` / `req_0`
/// 以及老版本可能的其它外层 key。
fn parse_module_response(resp: &HttpOut, module: &str, method: &str) -> Result<(Option<String>, Value)> {
    let value: Value = serde_json::from_slice(&resp.body).with_context(|| {
        format!(
            "登录响应不是 JSON（HTTP {}）：{}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
                .chars()
                .take(160)
                .collect::<String>()
        )
    })?;
    let node = value
        .get(format!("{module}.{method}"))
        .or_else(|| value.get("req"))
        .or_else(|| value.get("req_0"))
        .or_else(|| {
            value.as_object().and_then(|obj| {
                obj.values()
                    .find(|v| v.is_object() && v.get("code").is_some())
            })
        })
        .context("登录响应缺少模块数据")?;
    let code = node.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        if std::env::var_os("QQMUSIC_DEBUG").is_some() {
            eprintln!("[login] 失败响应: {node}");
        }
        let data = node.get("data");
        let hint = data
            .and_then(|d| {
                d.get("errtip")
                    .or_else(|| d.get("errMsg"))
                    .or_else(|| d.get("errtip2"))
            })
            .and_then(Value::as_str)
            .unwrap_or("");
        if hint.is_empty() {
            bail!("登录换票失败，code={code}");
        }
        bail!("登录换票失败，code={code}（{hint}）");
    }
    let data = node.get("data").cloned().unwrap_or(Value::Null);
    let nick = data
        .get("nick")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok((nick, data))
}

fn redact_token(body: &str) -> String {
    // 把 "token":"..." 的值替换掉，避免 debug 日志泄露登录票据。
    match body.find("\"token\":\"") {
        Some(idx) => {
            let start = idx + "\"token\":\"".len();
            let end = body[start..].find('"').map(|i| start + i).unwrap_or(body.len());
            format!(
                "{}<redacted>{}",
                &body[..start],
                &body[end..]
            )
        }
        None => body.to_string(),
    }
}

/// 申请二维码。返回 (PNG 字节, qrcodeID, 有效期秒数)。
fn create_qr(http: &CurlJar) -> Result<(Vec<u8>, String, Option<u64>)> {
    let param = json!({
        "tmeAppID": "qqmusic",
        "ct": ANDROID_CT,
        "cv": ANDROID_CV,
    });
    let body = mobile_request_body("CreateQRCode", param, None);
    let resp = musicu_post_json(http, &body.to_string())?;
    let (_, data) = parse_module_response(&resp, LOGIN_MODULE, "CreateQRCode")
        .context("申请 QQ音乐 登录二维码失败")?;
    let qrcode = data
        .get("qrcode")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("二维码响应缺少 qrcode")?;
    let qrcode_id = data
        .get("qrcodeID")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("二维码响应缺少 qrcodeID")?
        .to_string();
    let png = decode_qr_data_url(qrcode)?;
    let expires_in = data.get("expiresIn").and_then(Value::as_u64);
    Ok((png, qrcode_id, expires_in))
}

/// 二维码是 `data:image/png;base64,...` 形式。只接受图片 data URL，
/// 避免把任意 base64 当图片写盘。
fn decode_qr_data_url(raw: &str) -> Result<Vec<u8>> {
    let (meta, b64) = raw
        .split_once(',')
        .context("二维码不是 data URL（缺少逗号）")?;
    if !meta.to_ascii_lowercase().contains(";base64") || !meta.starts_with("data:image/") {
        bail!("二维码 data URL 格式不支持: {}", meta);
    }
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("二维码 base64 解码失败")
}

fn wait_for_scan<T>(
    qrcode_id: &str,
    opts: &LoginOpts,
    exchange: impl FnOnce(ScanCreds) -> Result<T>,
) -> Result<T> {
    let mqtt = build_mqtt_options(qrcode_id, opts)?;
    let (client, mut connection) = Client::new(mqtt, 10);
    let topic = format!("{MQTT_TOPIC_PREFIX}/{qrcode_id}");
    let subscribe_props = SubscribeProperties {
        id: None,
        user_properties: vec![
            ("authorization".to_string(), "tmelogin".to_string()),
            ("pubsub".to_string(), "unicast".to_string()),
        ],
    };

    let deadline = Instant::now() + opts.timeout;
    let mut last_error: Option<String> = None;
    let mut connect_errors = 0usize;
    loop {
        let now = Instant::now();
        if now >= deadline {
            let detail = last_error
                .as_deref()
                .map(|e| format!("（最近一次连接错误: {e}）"))
                .unwrap_or_default();
            bail!("等待扫码超时（{}s）{}", opts.timeout.as_secs(), detail);
        }
        let wait = (deadline - now).min(Duration::from_millis(500));
        match connection.recv_timeout(wait) {
            Ok(Ok(Event::Incoming(Packet::ConnAck(ack)))) => {
                if std::env::var_os("QQMUSIC_DEBUG").is_some() {
                    eprintln!("[mqtt] CONNACK: {:?}", ack.code);
                }
                if matches!(ack.code, rumqttc::v5::mqttbytes::v5::ConnectReturnCode::Success) {
                    connect_errors = 0;
                    // clean_start=true：服务端不保留会话，断线重连后必须重新订阅。
                    client
                        .subscribe_with_properties(
                            topic.clone(),
                            QoS::AtMostOnce,
                            subscribe_props.clone(),
                        )
                        .context("MQTT 订阅二维码状态失败")?;
                    println!(
                        "  {}",
                        crate::ui::ok("已连接二维码状态服务，等待扫码…")
                    );
                }
            }
            Ok(Ok(Event::Incoming(Packet::SubAck(ack)))) => {
                if std::env::var_os("QQMUSIC_DEBUG").is_some() {
                    eprintln!("[mqtt] SUBACK: {:?}", ack.return_codes);
                }
                if let Some(code) = ack
                    .return_codes
                    .iter()
                    .find(|c| !matches!(c, SubscribeReasonCode::Success(_)))
                {
                    bail!("二维码状态订阅被服务端拒绝: {code:?}");
                }
            }
            Ok(Ok(Event::Incoming(Packet::Publish(p)))) => {
                match parse_scan_message(&user_properties(&p), &p.payload)? {
                    ScanEvent::Scanned => {
                        println!(
                            "  {}",
                            crate::ui::warn("已扫码，请在手机上确认登录…")
                        );
                    }
                    // 在 MQTT 会话仍然存活时完成换票。
                    ScanEvent::Credentials(c) => return exchange(c),
                    ScanEvent::Ignore => {}
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                // rumqttc 会自动重连；这里只记录最近错误，避免网络抖动直接中断扫码。
                last_error = Some(format!("{e:?}"));
                connect_errors += 1;
                if std::env::var_os("QQMUSIC_DEBUG").is_some() {
                    eprintln!("[mqtt] 连接错误（第 {connect_errors} 次）: {e:?}");
                }
                if connect_errors == 3 {
                    println!(
                        "  {}",
                        crate::ui::warn(
                            "连接二维码状态服务失败，正在自动重试（不影响继续扫码）…"
                        )
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(rumqttc::v5::RecvTimeoutError::Timeout) => {}
            Err(rumqttc::v5::RecvTimeoutError::Disconnected) => {
                bail!("MQTT 连接已断开，请重试 login qqmusic")
            }
        }
    }
}

fn build_mqtt_options(qrcode_id: &str, opts: &LoginOpts) -> Result<MqttOptions> {
    let client_id = format!("{}{:04}", now_ms(), now_ms() % 10_000);
    let mut mqtt = MqttOptions::new(
        client_id,
        format!("wss://{MQTT_HOST}:443{MQTT_PATH}"),
        443,
    );
    mqtt.set_keep_alive(Duration::from_secs(45));
    mqtt.set_clean_start(true);
    mqtt.set_transport(Transport::wss_with_config(TlsConfiguration::Rustls(
        Arc::new(tls_config(opts.insecure)?),
    )));
    mqtt.set_authentication_method(Some("pass".to_string()));
    mqtt.set_user_properties(vec![
        ("tmeAppID".to_string(), "qqmusic".to_string()),
        ("business".to_string(), "management".to_string()),
        ("hashTag".to_string(), qrcode_id.to_string()),
        ("clientTag".to_string(), "management.user".to_string()),
        ("userID".to_string(), qrcode_id.to_string()),
    ]);
    mqtt.set_request_modifier(|mut req| {
        req.headers_mut()
            .insert("Origin", "https://y.qq.com".parse().unwrap());
        req.headers_mut()
            .insert("Referer", "https://y.qq.com/".parse().unwrap());
        async move { req }
    });
    if let Some(proxy) = opts.proxy.as_deref() {
        mqtt.set_proxy(parse_proxy(proxy, opts.insecure)?);
    }
    Ok(mqtt)
}

fn tls_config(insecure: bool) -> Result<rustls::ClientConfig> {
    if insecure {
        return Ok(rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth());
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// `--insecure` 时接受任何证书（配合 Reqable 等 MITM 代理）。风险由用户显式承担。
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

/// 支持 http:// 代理（Reqable 默认就是 http 代理），也支持 user:pass@。
fn parse_proxy(raw: &str, insecure: bool) -> Result<Proxy> {
    let (scheme, rest) = match raw.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("http", raw),
    };
    let (auth, host_port) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let host_port = host_port.trim_end_matches('/');
    let (host, port) = host_port
        .rsplit_once(':')
        .with_context(|| format!("代理地址缺少端口: {raw}（示例 http://127.0.0.1:9000）"))?;
    let port: u16 = port.parse().context("代理端口不是数字")?;
    if host.is_empty() {
        bail!("代理地址缺少主机名: {raw}");
    }
    let auth = match auth {
        None => ProxyAuth::None,
        Some(a) => {
            let (username, password) = a
                .split_once(':')
                .with_context(|| format!("代理认证格式应为 user:pass: {raw}"))?;
            ProxyAuth::Basic {
                username: username.to_string(),
                password: password.to_string(),
            }
        }
    };
    let ty = if scheme.eq_ignore_ascii_case("https") {
        ProxyType::Https(TlsConfiguration::Rustls(Arc::new(tls_config(insecure)?)))
    } else if scheme.eq_ignore_ascii_case("http") {
        ProxyType::Http
    } else {
        bail!("不支持 {scheme}:// 代理，目前只支持 http(s)");
    };
    Ok(Proxy {
        ty,
        auth,
        addr: host.to_string(),
        port,
    })
}

fn user_properties(p: &Publish) -> Vec<(String, String)> {
    p.properties
        .as_ref()
        .map(|props| props.user_properties.clone())
        .unwrap_or_default()
}

/// 解析 MQTT 推送。消息类型放在 UserProperty 的 `type` 里，payload 是 JSON。
fn parse_scan_message(props: &[(String, String)], payload: &[u8]) -> Result<ScanEvent> {
    let kind = props
        .iter()
        .find(|(k, _)| k == "type")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let value: Option<Value> = serde_json::from_slice(payload).ok();
    match kind {
        "scanned" => Ok(ScanEvent::Scanned),
        "canceled" => bail!("已在手机上取消登录"),
        "timeout" => bail!("二维码已过期，请重新执行 login qqmusic"),
        "loginFailed" => bail!(
            "手机端登录失败：{}",
            value
                .as_ref()
                .map(compact_json)
                .unwrap_or_else(|| String::from_utf8_lossy(payload).to_string())
        ),
        "cookies" => {
            let value = value.context("登录消息不是 JSON")?;
            let uin = json_field(&value, &["cookies", "qqmusic_uin", "value"])
                .context("登录消息缺少 qqmusic_uin")?;
            let key = json_field(&value, &["cookies", "qqmusic_key", "value"])
                .context("登录消息缺少 qqmusic_key")?;
            if std::env::var_os("QQMUSIC_DEBUG").is_some() {
                eprintln!(
                    "[mqtt] cookies 事件: uin={uin} key_len={} key_prefix={}…",
                    key.len(),
                    key.chars().take(4).collect::<String>()
                );
                eprintln!("[mqtt] cookies 结构: {}", redact_cookies_json(&value));
            }
            Ok(ScanEvent::Credentials(ScanCreds { uin, key }))
        }
        other => {
            if std::env::var_os("QQMUSIC_DEBUG").is_some() && !other.is_empty() {
                let body = value
                    .as_ref()
                    .map(compact_json)
                    .unwrap_or_else(|| String::from_utf8_lossy(payload).to_string());
                eprintln!("[mqtt] 忽略未知消息类型 {other}: {body}");
            }
            Ok(ScanEvent::Ignore)
        }
    }
}

fn json_field(value: &Value, path: &[&str]) -> Option<String> {
    let mut cur = value;
    for key in path {
        cur = cur.get(*key)?;
    }
    match cur {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn redact_cookies_json(value: &Value) -> String {
    let mut clone = value.clone();
    if let Some(cookies) = clone.get_mut("cookies").and_then(Value::as_object_mut) {
        for (name, entry) in cookies.iter_mut() {
            let lower = name.to_lowercase();
            if lower.contains("key") || lower.contains("token") || lower.contains("skey") {
                if let Some(obj) = entry.as_object_mut() {
                    if obj.contains_key("value") {
                        obj.insert("value".to_string(), Value::String("<redacted>".to_string()));
                    }
                } else {
                    *entry = Value::String("<redacted>".to_string());
                }
            }
        }
    }
    clone.to_string()
}

fn compact_json(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 160 {
        format!("{}…", text.chars().take(160).collect::<String>())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_png_data_url() {
        // "PNG" 的 base64
        let bytes = decode_qr_data_url("data:image/png;base64,UE5H").unwrap();
        assert_eq!(bytes, b"PNG");
        assert!(decode_qr_data_url("data:text/plain;base64,UE5H").is_err());
        assert!(decode_qr_data_url("UE5H").is_err());
    }

    #[test]
    fn parses_scan_events() {
        let scanned = parse_scan_message(
            &[("type".to_string(), "scanned".to_string())],
            br#"{"foo":1}"#,
        )
        .unwrap();
        assert_eq!(scanned, ScanEvent::Scanned);

        let payload = br#"{"cookies":{"qqmusic_uin":{"value":"12345"},"qqmusic_key":{"value":"W_Xkey"}}}"#;
        let event = parse_scan_message(
            &[("type".to_string(), "cookies".to_string())],
            payload,
        )
        .unwrap();
        assert_eq!(
            event,
            ScanEvent::Credentials(ScanCreds {
                uin: "12345".to_string(),
                key: "W_Xkey".to_string(),
            })
        );

        assert!(parse_scan_message(
            &[("type".to_string(), "canceled".to_string())],
            b""
        )
        .is_err());
        assert_eq!(
            parse_scan_message(&[("type".to_string(), "something".to_string())], b"{}").unwrap(),
            ScanEvent::Ignore
        );
    }

    #[test]
    fn mobile_login_body_uses_android_comm_and_module_key() {
        let body = mobile_request_body(
            "Login",
            json!({"musicid": 1, "qrCodeID": "q", "token": "secret"}),
            Some("6"),
        );
        assert_eq!(body["comm"]["ct"], "11");
        assert_eq!(body["comm"]["cv"], "13020508");
        assert_eq!(body["comm"]["tmeAppID"], "qqmusic");
        assert_eq!(body["comm"]["tmeLoginType"], "6");
        assert!(body.get("music.login.LoginServer.Login").is_some());
        assert!(body.get("req").is_none());
    }

    #[test]
    fn parses_module_keyed_response() {
        let out = HttpOut {
            status: 200,
            headers: Vec::new(),
            body: br#"{"code":0,"music.login.LoginServer.Login":{"code":0,"data":{"musickey":"k","musicid":1,"nick":"n"}}}"#.to_vec(),
        };
        let (nick, data) = parse_module_response(&out, LOGIN_MODULE, "Login").unwrap();
        assert_eq!(nick.as_deref(), Some("n"));
        assert_eq!(data["musickey"], "k");
    }

    #[test]
    fn redacts_token_in_debug_body() {
        let body = mobile_request_body(
            "Login",
            json!({"musicid": 1, "qrCodeID": "q", "token": "super-secret"}),
            Some("6"),
        )
        .to_string();
        let redacted = redact_token(&body);
        assert!(!redacted.contains("super-secret"));
        assert!(redacted.contains("<redacted>"));
    }

    #[test]
    fn parses_proxy_urls() {
        let p = parse_proxy("http://127.0.0.1:9000", false).unwrap();
        assert_eq!(p.addr, "127.0.0.1");
        assert_eq!(p.port, 9000);
        assert!(matches!(p.ty, ProxyType::Http));
        assert!(parse_proxy("http://127.0.0.1:9000/", false).is_ok());

        let p = parse_proxy("user:pass@proxy.local:8080", false).unwrap();
        assert_eq!(p.addr, "proxy.local");
        match p.auth {
            ProxyAuth::Basic { username, password } => {
                assert_eq!(username, "user");
                assert_eq!(password, "pass");
            }
            _ => panic!("expected basic auth"),
        }

        assert!(parse_proxy("socks5://127.0.0.1:1080", false).is_err());
        assert!(parse_proxy("http://127.0.0.1", false).is_err());
    }
}
