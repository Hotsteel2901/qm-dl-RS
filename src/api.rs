use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::auth::Auth;

pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

const SEARCH_URL: &str = "https://c.y.qq.com/soso/fcgi-bin/client_search_cp";
const DETAIL_URL: &str = "https://c.y.qq.com/v8/fcg-bin/fcg_play_single_song.fcg";
const MUSICU_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Quality {
    #[value(name = "128", alias = "mp3-128", alias = "low")]
    Mp3_128,
    #[value(name = "320", alias = "mp3", alias = "high")]
    Mp3_320,
    #[value(name = "flac", alias = "lossless", alias = "sq")]
    Flac,
}

impl Quality {
    /// QQ 音乐文件前缀：
    ///   M800 = 320kbps MP3
    ///   M500 = 128kbps MP3
    ///   F000 = FLAC 无损
    /// 已通过真实下载 + ffprobe 验证。
    pub fn prefix(self) -> (&'static str, &'static str) {
        match self {
            Quality::Mp3_128 => ("M500", "mp3"),
            Quality::Mp3_320 => ("M800", "mp3"),
            Quality::Flac => ("F000", "flac"),
        }
    }

    pub fn ext(self) -> &'static str {
        self.prefix().1
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::Mp3_128 => "128k MP3",
            Quality::Mp3_320 => "320k MP3",
            Quality::Flac => "FLAC 无损",
        }
    }

    /// 高音质不可用时依次降级。
    pub fn fallbacks(self) -> Vec<Quality> {
        match self {
            Quality::Flac => vec![Quality::Flac, Quality::Mp3_320, Quality::Mp3_128],
            Quality::Mp3_320 => vec![Quality::Mp3_320, Quality::Mp3_128],
            Quality::Mp3_128 => vec![Quality::Mp3_128],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Song {
    pub mid: String,
    pub media_mid: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration: u64,
    pub size_128: u64,
    pub size_320: u64,
    pub size_flac: u64,
    pub preview_size: u64,
}

impl Song {
    pub fn artist_str(&self) -> String {
        self.artists.join("/")
    }

    pub fn size_for(&self, q: Quality) -> u64 {
        match q {
            Quality::Mp3_128 => self.size_128,
            Quality::Mp3_320 => self.size_320,
            Quality::Flac => self.size_flac,
        }
    }

    pub fn duration_str(&self) -> String {
        format!("{:02}:{:02}", self.duration / 60, self.duration % 60)
    }
}

#[derive(Clone, Debug)]
pub struct StreamUrl {
    pub url: String,
    pub quality: Quality,
    /// true 表示这是官方 60 秒试听片段，而非整曲。
    pub preview: bool,
}

pub enum StreamLookup {
    Found(StreamUrl),
    /// 没有可用的 purl，附带服务端 result 码（0 表示未知情况）。
    Unavailable { result: i64 },
}

pub struct QqClient {
    http: reqwest::blocking::Client,
    auth: Auth,
    guid: String,
}

impl QqClient {
    pub fn new(auth: Auth, proxy: Option<&str>, insecure: bool) -> Result<Self> {
        let mut builder = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30));
        if let Some(p) = proxy {
            builder = builder
                .proxy(reqwest::Proxy::all(p).with_context(|| format!("无效代理: {p}"))?);
        }
        if insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let http = builder.build().context("创建 HTTP 客户端失败")?;

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut guid = (nanos % 10_000_000_000) as u64;
        if guid < 1_000_000_000 {
            guid += 1_000_000_000;
        }

        Ok(Self {
            http,
            auth,
            guid: guid.to_string(),
        })
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    pub fn client(&self) -> &reqwest::blocking::Client {
        &self.http
    }

    /// 搜索歌曲。该接口匿名可用。
    pub fn search(&self, keyword: &str, limit: usize) -> Result<Vec<Song>> {
        let n = limit.clamp(1, 50).to_string();
        let resp = self
            .http
            .get(SEARCH_URL)
            .query(&[
                ("p", "1"),
                ("n", n.as_str()),
                ("w", keyword),
                ("format", "json"),
                ("cr", "1"),
            ])
            .header("Referer", "https://y.qq.com/")
            .send()
            .context("搜索请求失败")?;
        let status = resp.status();
        let text = resp.text().context("读取搜索响应失败")?;
        if !status.is_success() {
            bail!("搜索接口返回 HTTP {}", status);
        }
        let v: Value = serde_json::from_str(&text).context("解析搜索响应失败")?;
        let list = v
            .pointer("/data/song/list")
            .and_then(Value::as_array)
            .context("搜索结果为空或接口格式已变化")?;
        Ok(list.iter().filter_map(parse_song).collect())
    }

    /// 通过 songmid 获取歌曲详情（含 media_mid 与各音质大小）。匿名可用。
    pub fn detail(&self, mid: &str) -> Result<Song> {
        let resp = self
            .http
            .get(DETAIL_URL)
            .query(&[("songmid", mid), ("format", "json"), ("platform", "yqq")])
            .header("Referer", "https://y.qq.com/")
            .send()
            .context("获取歌曲详情失败")?;
        let status = resp.status();
        let text = resp.text().context("读取歌曲详情失败")?;
        if !status.is_success() {
            bail!("歌曲详情接口返回 HTTP {}", status);
        }
        let v: Value = serde_json::from_str(&text).context("解析歌曲详情失败")?;
        let item = v
            .get("data")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .context("歌曲不存在或已下架")?;
        let mut song = parse_song(item).context("无法解析歌曲信息")?;
        if let Some(file) = item.get("file") {
            if let Some(m) = file.get("media_mid").and_then(Value::as_str) {
                if !m.is_empty() {
                    song.media_mid = m.to_string();
                }
            }
            song.size_128 = file
                .get("size_128mp3")
                .and_then(Value::as_u64)
                .unwrap_or(song.size_128);
            song.size_320 = file
                .get("size_320mp3")
                .and_then(Value::as_u64)
                .unwrap_or(song.size_320);
            song.size_flac = file
                .get("size_flac")
                .and_then(Value::as_u64)
                .unwrap_or(song.size_flac);
            if song.preview_size == 0 {
                song.preview_size = file
                    .get("size_try")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
        }
        Ok(song)
    }

    /// 获取整曲下载地址。返回 Unavailable 时调用方可尝试降级音质或试听。
    pub fn stream_url(&self, song: &Song, quality: Quality) -> Result<StreamLookup> {
        let (prefix, ext) = quality.prefix();
        let filename = format!("{}{}.{}", prefix, song.media_mid, ext);
        self.lookup(song, &filename, quality, false)
    }

    /// 获取官方 60 秒试听片段（匿名可用）。不发往付费内容，只是服务端公开的试听流。
    pub fn trial_url(&self, song: &Song) -> Result<StreamLookup> {
        let filename = format!("RS02{}.mp3", song.media_mid);
        self.lookup(song, &filename, Quality::Mp3_128, true)
    }

    fn lookup(
        &self,
        song: &Song,
        filename: &str,
        quality: Quality,
        preview: bool,
    ) -> Result<StreamLookup> {
        let body = json!({
            "comm": {
                "cv": 4747474,
                "ct": 24,
                "format": "json",
                "inCharset": "utf-8",
                "outCharset": "utf-8",
                "notice": 0,
                "platform": "yqq.json",
                "needNewCode": 1,
                "uin": self.auth.uin,
                "g_tk_new_20200303": self.auth.g_tk,
                "g_tk": self.auth.g_tk
            },
            "req_0": {
                "module": "vkey.GetVkeyServer",
                "method": "CgiGetVkey",
                "param": {
                    "guid": self.guid,
                    "songmid": [song.mid],
                    "songtype": [0],
                    "uin": self.auth.uin,
                    "loginflag": 1,
                    "platform": "20",
                    "filename": [filename]
                }
            }
        });

        let mut req = self
            .http
            .post(MUSICU_URL)
            .header("Referer", "https://y.qq.com/")
            .header("Origin", "https://y.qq.com");
        // 只有确认是有效登录态时才带 Cookie；残缺 Cookie 不应影响匿名访问。
        if self.auth.logged_in && !self.auth.cookie.is_empty() {
            req = req.header("Cookie", &self.auth.cookie);
        }
        let resp = req.json(&body).send().context("获取播放地址失败")?;
        let status = resp.status();
        let text = resp.text().context("读取播放地址响应失败")?;
        if !status.is_success() {
            bail!("取播放地址接口返回 HTTP {}", status);
        }
        let v: Value = serde_json::from_str(&text).context("解析播放地址响应失败")?;

        if let Some(code) = v.get("code").and_then(Value::as_i64) {
            if code != 0 {
                return Ok(StreamLookup::Unavailable { result: code });
            }
        }
        if let Some(code) = v.pointer("/req_0/code").and_then(Value::as_i64) {
            if code != 0 {
                return Ok(StreamLookup::Unavailable { result: code });
            }
        }

        let result = v
            .pointer("/req_0/data/midurlinfo/0/result")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let purl = v
            .pointer("/req_0/data/midurlinfo/0/purl")
            .and_then(Value::as_str)
            .unwrap_or("");
        if purl.is_empty() {
            return Ok(StreamLookup::Unavailable { result });
        }
        let sip = v
            .pointer("/req_0/data/sip/0")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("http://dl.stream.qqmusic.qq.com/");
        let url = format!(
            "{}/{}",
            sip.trim_end_matches('/'),
            purl.trim_start_matches('/')
        );
        Ok(StreamLookup::Found(StreamUrl {
            url,
            quality,
            preview,
        }))
    }
}

fn parse_song(v: &Value) -> Option<Song> {
    let mid = get_str(v, &["songmid", "mid", "songMid", "song_mid"])?;
    let media_mid = get_str(v, &["strMediaMid", "media_mid", "mediaMid"])
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| mid.clone());
    let name = get_str(v, &["songname", "name", "title"]).unwrap_or_else(|| mid.clone());
    let album = get_str(v, &["albumname", "album_name"])
        .or_else(|| {
            v.pointer("/album/name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let artists = v
        .get("singer")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let duration = v.get("interval").and_then(Value::as_u64).unwrap_or(0);
    let size_128 = v.get("size128").and_then(Value::as_u64).unwrap_or(0);
    let size_320 = v.get("size320").and_then(Value::as_u64).unwrap_or(0);
    let size_flac = v
        .get("sizeflac")
        .or_else(|| v.get("size_flac"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let preview_size = v
        .pointer("/preview/trysize")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    Some(Song {
        mid,
        media_mid,
        name,
        artists,
        album,
        duration,
        size_128,
        size_320,
        size_flac,
        preview_size,
    })
}

fn get_str(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::to_string)
}
