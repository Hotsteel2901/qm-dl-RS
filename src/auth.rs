use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 登录态。默认匿名；仅当显式提供 Cookie 时才带上账号。
#[derive(Clone, Debug)]
pub struct Auth {
    pub cookie: String,
    pub uin: String,
    pub g_tk: u64,
    pub logged_in: bool,
    pub source: String,
}

impl Auth {
    /// 优先级：--cookie > QQMUSIC_COOKIE > --cookie-file > 默认配置文件。
    /// 找不到任何 Cookie 时返回匿名状态（可用于免费曲目）。
    pub fn load(explicit: Option<&str>, cookie_file: Option<&Path>) -> Result<Self> {
        if let Some(c) = explicit {
            if !c.trim().is_empty() {
                return Ok(Self::from_cookie(c, "--cookie"));
            }
        }
        if let Some(env) = std::env::var_os("QQMUSIC_COOKIE") {
            let s = env.to_string_lossy().to_string();
            if !s.trim().is_empty() {
                return Ok(Self::from_cookie(&s, "QQMUSIC_COOKIE"));
            }
        }
        let path = cookie_file
            .map(Path::to_path_buf)
            .unwrap_or_else(default_cookie_path);
        if path.exists() {
            let s = std::fs::read_to_string(&path)
                .with_context(|| format!("读取 Cookie 文件失败: {}", path.display()))?;
            if !s.trim().is_empty() {
                return Ok(Self::from_cookie(&s, &path.display().to_string()));
            }
        }
        Ok(Self::anonymous())
    }

    pub fn anonymous() -> Self {
        Self {
            cookie: String::new(),
            uin: "0".to_string(),
            g_tk: 5381,
            logged_in: false,
            source: "匿名（未登录）".to_string(),
        }
    }

    pub fn from_cookie(raw: &str, source: &str) -> Self {
        let cookie = raw
            .trim()
            .replace(['\r', '\n'], "")
            .trim_end_matches(';')
            .to_string();
        let map = parse_cookie(&cookie);

        let key = map
            .get("qqmusic_key")
            .filter(|v| !v.is_empty())
            .or_else(|| map.get("qm_keyst").filter(|v| !v.is_empty()));

        let uin = map
            .get("wxuin")
            .filter(|v| !v.is_empty())
            .map(|v| normalize_uin(v))
            .or_else(|| {
                map.get("uin")
                    .filter(|v| !v.is_empty())
                    .map(|v| normalize_uin(v))
            })
            .or_else(|| {
                map.get("pt2gguin")
                    .filter(|v| !v.is_empty())
                    .map(|v| normalize_uin(v))
            })
            .unwrap_or_else(|| "0".to_string());

        let g_tk = key.map(|k| hash33(k)).unwrap_or(5381);
        let logged_in = key.is_some() && uin != "0";
        Self {
            cookie,
            uin,
            g_tk,
            logged_in,
            source: source.to_string(),
        }
    }
}

pub fn default_cookie_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("qqmusic-dl").join("cookie.txt")
}

fn parse_cookie(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for part in raw.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                map.insert(k.to_string(), v.trim().to_string());
            }
        }
    }
    map
}

fn normalize_uin(v: &str) -> String {
    let v = v.trim();
    let v = v
        .strip_prefix('o')
        .or_else(|| v.strip_prefix('O'))
        .unwrap_or(v);
    if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
        let trimmed = v.trim_start_matches('0');
        if trimmed.is_empty() {
            "0".to_string()
        } else {
            trimmed.to_string()
        }
    } else if v.is_empty() {
        "0".to_string()
    } else {
        v.to_string()
    }
}

/// 原子写入 Cookie 文件（0600），供 --cookie-file 同步和登录流程使用。
pub fn write_cookie_file(path: &Path, cookie: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("创建目录失败: {}", dir.display()))?;
    let tmp = dir.join(format!(".cookie-{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        std::fs::write(&tmp, cookie)
            .with_context(|| format!("写入 Cookie 临时文件失败: {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(path);
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("替换 Cookie 文件失败: {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// QQ 音乐 g_tk 算法（hash33）。
fn hash33(s: &str) -> u64 {
    let mut h: u32 = 5381;
    for b in s.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(*b as u32);
    }
    (h & 0x7fff_ffff) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtk_hash33_known_value() {
        // hash33("abc") = 193485963，按 u32 累加后取低 31 位。
        assert_eq!(hash33("abc"), 193_485_963);
    }

    #[test]
    fn uin_normalization() {
        assert_eq!(normalize_uin("o1186975266"), "1186975266");
        assert_eq!(normalize_uin("o01186975266"), "1186975266");
        assert_eq!(normalize_uin("1152921505192837541"), "1152921505192837541");
        assert_eq!(normalize_uin(""), "0");
    }
}
