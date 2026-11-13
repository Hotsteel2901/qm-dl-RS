//! 通用小工具：歌曲 ID 解析、文件名清洗、URL 编解码、字节格式化、外部程序打开等。

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 从歌曲 ID、网页链接或带 `songmid=` 的 URL 中取出 songmid。
pub fn parse_mid(arg: &str) -> String {
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

/// 去掉文件名里不能出现的字符。保留中文与空格。
pub fn sanitize(s: &str) -> String {
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

/// 按 UTF-8 字节截断。中文歌名按字符数截到 180 会超过文件名 255 字节上限。
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end().to_string()
}

/// 展开 `~` / `~/Music`。其它路径原样返回。
pub fn expand_tilde(input: &str) -> PathBuf {
    let input = input.trim();
    if input == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(input)
}

/// 账号 uin 脱敏。
pub fn mask(s: &str) -> String {
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

/// 1024 进制的人类可读大小。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

pub fn human_speed(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "--".to_string();
    }
    format!("{}/s", human_bytes(bytes_per_sec as u64))
}

/// 解析 query string（`a=1&b=%E4%B8%AD`）。`+` 视为空格。
pub fn parse_query(q: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for kv in q.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            out.insert(percent_decode(k), percent_decode(v));
        }
    }
    out
}

/// 宽松 percent-decode：非法转义保留原样，永不报错。
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 只保留 RFC 3986 unreserved 字符，其余按 UTF-8 百分号编码。
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 去掉终端 ANSI 转义序列（登录子进程日志可能带颜色）。
pub fn strip_ansi(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\u{1b}' {
            if i + 1 < chars.len() && chars[i + 1] == '[' {
                // CSI: ESC [ ... @-~
                i += 2;
                while i < chars.len() && !('\u{40}'..='\u{7e}').contains(&chars[i]) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            if i + 1 < chars.len() && chars[i + 1] == ']' {
                // OSC: ESC ] ... BEL 或 ESC \
                i += 2;
                while i < chars.len() {
                    if chars[i] == '\u{7}' {
                        i += 1;
                        break;
                    }
                    if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 用系统默认程序打开文件/目录（不等待退出）。
pub fn open_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let candidates: &[(&str, &[&str])] = &[("open", &[])];
    #[cfg(not(target_os = "macos"))]
    let candidates: &[(&str, &[&str])] = &[("xdg-open", &[]), ("gio", &["open"])];

    for (cmd, args) in candidates {
        match Command::new(cmd)
            .args(*args)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                reap(child);
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("未找到 xdg-open / open，无法打开: {}", path.display())
}

/// 后台回收子进程，避免长驻 TUI/Web 里堆积僵尸进程。
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn player_attempts() -> [(&'static str, &'static [&'static str]); 2] {
    [
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
        ("mpv", &["--no-video", "--really-quiet"]),
    ]
}

/// 同步播放（CLI 用），直到播放器退出。
pub fn play_file(path: &Path) {
    for (cmd, args) in player_attempts() {
        let status = Command::new(cmd).args(args).arg(path).status();
        match status {
            Ok(s) if s.success() => return,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                eprintln!("  播放失败: {e}");
                return;
            }
        }
    }
    eprintln!("  未找到 ffplay/mpv，无法自动播放");
}

/// 后台播放（TUI 用），立即返回播放器名称。
pub fn spawn_player(path: &Path) -> Result<&'static str> {
    for (cmd, args) in player_attempts() {
        match Command::new(cmd)
            .args(args)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                reap(child);
                return Ok(cmd);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("未找到 ffplay/mpv，无法播放")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn songmid_from_urls() {
        assert_eq!(parse_mid("0048Ts0Q0oi3Ti"), "0048Ts0Q0oi3Ti");
        assert_eq!(
            parse_mid("https://y.qq.com/n/ryqq/songDetail/0048Ts0Q0oi3Ti"),
            "0048Ts0Q0oi3Ti"
        );
        assert_eq!(
            parse_mid("https://y.qq.com/x?songmid=0039MnYb0qxYhV&x=1"),
            "0039MnYb0qxYhV"
        );
    }

    #[test]
    fn sanitize_and_truncate() {
        assert_eq!(sanitize("a/b:c*?\"<>|"), "a_b_c______");
        assert_eq!(sanitize("  ..周杰伦.  "), "周杰伦");
        let s = "周".repeat(100);
        let t = truncate_bytes(&s, 180);
        assert!(t.len() <= 180);
        assert!(t.chars().all(|c| c == '周'));
    }

    #[test]
    fn bytes_are_human_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KB");
        assert_eq!(human_bytes(10 * 1024 * 1024), "10.0 MB");
    }

    #[test]
    fn url_roundtrip() {
        let raw = "周杰伦 & 晴天/测试";
        let enc = url_encode(raw);
        assert!(!enc.contains(' '));
        assert!(!enc.contains('/'));
        let query = format!("q={enc}&n=1");
        let map = parse_query(&query);
        assert_eq!(map.get("q").map(String::as_str), Some(raw));
        assert_eq!(map.get("n").map(String::as_str), Some("1"));
    }

    #[test]
    fn ansi_is_removed() {
        assert_eq!(strip_ansi("\u{1b}[31m红色\u{1b}[0m"), "红色");
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
    }

    #[test]
    fn uin_is_masked() {
        assert_eq!(mask("123456789"), "123***89");
        assert_eq!(mask("12345"), "*****");
    }

    #[test]
    fn tilde_expands() {
        if std::env::var_os("HOME").is_some() {
            assert!(expand_tilde("~/Music").is_absolute());
            assert_eq!(expand_tilde("/tmp/x"), PathBuf::from("/tmp/x"));
        }
    }
}
