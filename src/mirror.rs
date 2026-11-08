use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::Value;

use crate::api::{Quality, Song, USER_AGENT};

const MIGU_SEARCH: &str = "https://app.c.nf.migu.cn/MIGUM2.0/v1.0/content/search_all.do";
const MIGU_LISTEN: &str = "https://app.c.nf.migu.cn/MIGUM2.0/v1.0/content/sub/listen.do";
const MIGU_UA: &str = "Mozilla/5.0 (Linux; Android 10) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Mobile Safari/537.36";

const KUWO_SEARCH: &str = "http://search.kuwo.cn/r.s";
const KUWO_RESOLVE: &str = "http://antiserver.kuwo.cn/anti.s";

const NETEASE_SEARCH: &str = "https://music.163.com/api/search/get/web";
const NETEASE_OUTER: &str = "https://music.163.com/song/media/outer/url";

pub const PROVIDERS: [&str; 3] = ["migu", "kuwo", "netease"];

#[derive(Clone, Debug)]
pub struct Candidate {
    pub provider: &'static str,
    pub id: String,
    pub copyright_id: String,
    pub title: String,
    pub artist: String,
    pub duration: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Resolved {
    pub url: String,
    pub quality: Quality,
    pub source: String,
    pub expected_size: Option<u64>,
}

/// 依次尝试匿名镜像源，返回可直接下载的整曲地址。
pub fn resolve(client: &Client, song: &Song, policy: &str) -> Result<Resolved> {
    let providers: Vec<&str> = match policy {
        "auto" => PROVIDERS.to_vec(),
        "off" => bail!("已禁用匿名镜像"),
        other if PROVIDERS.contains(&other) => vec![other],
        other => bail!("未知镜像源: {other}（可选 auto/migu/kuwo/netease/off）"),
    };

    let mut errors = Vec::new();
    for name in providers {
        match try_provider(client, name, song) {
            Ok(Some(r)) => return Ok(r),
            Ok(None) => errors.push(format!("{name}: 曲库中无匹配曲目")),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    bail!("匿名镜像未命中 —— {}", errors.join("；"))
}

/// 同一个镜像源最多尝试几个候选。第一个候选经常是会员专享/已下架，
/// 后面的同名普通版本反而能匿名播放，所以不能只试“最高分”那一个。
const MAX_CANDIDATE_TRIES: usize = 5;

fn try_provider(client: &Client, name: &str, song: &Song) -> Result<Option<Resolved>> {
    let queries = search_queries(song);
    let debug = std::env::var_os("QQMUSIC_DEBUG").is_some();
    let mut scored: Vec<(i32, Candidate)> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for q in &queries {
        let cands = match name {
            "migu" => search_migu(client, q)?,
            "kuwo" => search_kuwo(client, q)?,
            "netease" => search_netease(client, q)?,
            _ => bail!("未知镜像源"),
        };
        for c in cands {
            let sc = score(song, &c);
            if debug {
                println!(
                    "    [{}] {} - {} {:?} => {:?}",
                    c.provider, c.artist, c.title, c.duration, sc
                );
            }
            if let Some(s) = sc {
                scored.push((s, c));
            }
        }
        if !scored.is_empty() {
            break;
        }
    }

    if scored.is_empty() {
        return Ok(None);
    }

    // 分数高的优先；同分时保持搜索结果原有顺序。
    scored.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut last_err: Option<anyhow::Error> = None;
    let mut attempts = 0usize;
    for (sc, cand) in scored {
        if !seen_ids.insert(cand.id.clone()) {
            continue;
        }
        if attempts >= MAX_CANDIDATE_TRIES {
            if debug {
                eprintln!("    已尝试 {MAX_CANDIDATE_TRIES} 个候选，放弃剩余");
            }
            break;
        }
        attempts += 1;
        if attempts == 1 {
            println!(
                "  镜像命中: {} - {}（{}，score {}）",
                cand.artist, cand.title, cand.provider, sc
            );
        } else {
            println!(
                "  改试候选: {} - {}（{}，score {}）",
                cand.artist, cand.title, cand.provider, sc
            );
        }
        let result = match cand.provider {
            "migu" => resolve_migu(client, &cand),
            "kuwo" => resolve_kuwo(client, &cand),
            "netease" => resolve_netease(client, &cand),
            _ => unreachable!(),
        };
        match result {
            Ok(r) => return Ok(Some(r)),
            Err(e) => {
                if debug {
                    eprintln!("    该候选不可用: {e:#}");
                }
                last_err = Some(e);
            }
        }
    }

    match last_err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// 会改变作品本体的版本标记：命中且原曲没有 → 直接淘汰。
const VERSION_MARKERS: [&str; 20] = [
    "live", "伴奏", "ktv", "dj", "remix", "翻唱", "cover", "纯音乐", "铃声", "女声版",
    "童声", "片段", "慢摇", "广场舞", "钢琴版", "吉他版", "八音盒", "纯享", "现场",
    "演唱会",
];

fn search_queries(song: &Song) -> Vec<String> {
    let core = core_title(&song.name);
    let mut qs = Vec::new();
    if let Some(a) = song.artists.first() {
        qs.push(format!("{} {}", core, a));
    }
    qs.push(core.clone());
    qs
}

// ---------- 咪咕音乐 ----------

fn search_migu(client: &Client, q: &str) -> Result<Vec<Candidate>> {
    let v = get_json(
        client.get(MIGU_SEARCH)
            .query(&[
                ("text", q),
                ("pageNo", "1"),
                ("pageSize", "30"),
                ("isCopyright", "1"),
                ("searchSwitch", r#"{"song":1}"#),
            ])
            .header("User-Agent", MIGU_UA)
            .header("Referer", "https://music.migu.cn/"),
    )?;
    let arr = v
        .pointer("/songResultData/result")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for it in arr {
        if it.get("resourceType").and_then(Value::as_str) != Some("2")
            && it.get("resourceType").and_then(Value::as_i64) != Some(2)
        {
            continue;
        }
        let content_id = str_of(&it, &["contentId"]);
        let copyright_id = str_of(&it, &["copyrightId"]);
        let title = str_of(&it, &["name"]);
        if content_id.is_empty() || copyright_id.is_empty() || title.is_empty() {
            continue;
        }
        let artist = it
            .get("singers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default();
        out.push(Candidate {
            provider: "migu",
            id: content_id,
            copyright_id,
            title,
            artist,
            duration: None,
        });
    }
    Ok(out)
}

fn resolve_migu(client: &Client, c: &Candidate) -> Result<Resolved> {
    let url = MIGU_LISTEN.to_string();
    let req = client
        .get(&url)
        .query(&[
            ("contentId", c.id.as_str()),
            ("copyrightId", c.copyright_id.as_str()),
            ("toneFlag", "PQ"),
            ("resourceType", "2"),
            ("channel", "0146951"),
            ("userId", ""),
            ("ua", "Android_migu"),
            ("version", "6.0"),
            ("netType", "01"),
        ])
        .header("User-Agent", MIGU_UA)
        .header("Referer", "https://music.migu.cn/");
    let v = get_json(req)?;
    let code = v.get("code").and_then(Value::as_str).unwrap_or("");
    if code != "000000" {
        let info = v.get("info").and_then(Value::as_str).unwrap_or("未知错误");
        bail!("咪咕返回 {code}: {info}");
    }
    let item = v
        .pointer("/songListens/0")
        .context("咪咕响应缺少 songListens")?;
    let purl = item.get("url").and_then(Value::as_str).unwrap_or("");
    if !purl.starts_with("http") {
        bail!("咪咕未给出可播放地址（该曲可能无版权/需会员）");
    }
    let size = item
        .get("size")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok());
    Ok(Resolved {
        url: purl.to_string(),
        quality: Quality::Mp3_128,
        source: "咪咕音乐 · 匿名 128k 整曲".to_string(),
        expected_size: size,
    })
}

// ---------- 酷我音乐 ----------

fn search_kuwo(client: &Client, q: &str) -> Result<Vec<Candidate>> {
    let body = client
        .get(KUWO_SEARCH)
        .query(&[
            ("all", q),
            ("ft", "music"),
            ("itemset", "web_2013"),
            ("client", "kt"),
            ("pn", "0"),
            ("rn", "30"),
            ("rformat", "json"),
            ("encoding", "utf8"),
        ])
        .header("User-Agent", USER_AGENT)
        .send()
        .context("酷我搜索请求失败")?
        .text()
        .context("读取酷我搜索响应失败")?;

    let fixed = body.replace('\'', "\"");
    let v: Value = match serde_json::from_str(&fixed) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    let arr = v.get("abslist").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for it in arr {
        let rid = str_of(&it, &["DC_TARGETID", "MUSICRID"]);
        let title = clean(&str_of(&it, &["SONGNAME"]));
        let artist = clean(&str_of(&it, &["ARTIST"]));
        if rid.is_empty() || title.is_empty() {
            continue;
        }
        let rid = rid.trim_start_matches("MUSIC_").to_string();
        let duration = str_of(&it, &["DURATION"])
            .parse::<u64>()
            .ok()
            .filter(|d| *d > 0);
        out.push(Candidate {
            provider: "kuwo",
            id: rid,
            copyright_id: String::new(),
            title,
            artist,
            duration,
        });
    }
    Ok(out)
}

fn resolve_kuwo(client: &Client, c: &Candidate) -> Result<Resolved> {
    let body = client
        .get(KUWO_RESOLVE)
        .query(&[
            ("type", "convert_url"),
            ("format", "mp3"),
            ("response", "url"),
            ("rid", &format!("MUSIC_{}", c.id)),
        ])
        .header("User-Agent", USER_AGENT)
        .send()
        .context("酷我取流请求失败")?
        .text()
        .context("读取酷我取流响应失败")?;
    let purl = body.trim();
    if !purl.starts_with("http") {
        bail!("酷我未给出可播放地址（可能已下架/需会员）");
    }
    Ok(Resolved {
        url: purl.to_string(),
        quality: Quality::Mp3_128,
        source: "酷我音乐 · 匿名 128k 整曲".to_string(),
        expected_size: None,
    })
}

// ---------- 网易云音乐 ----------

fn search_netease(client: &Client, q: &str) -> Result<Vec<Candidate>> {
    let v = get_json(
        client
            .get(NETEASE_SEARCH)
            .query(&[("s", q), ("type", "1"), ("offset", "0"), ("limit", "30")])
            .header("User-Agent", USER_AGENT)
            .header("Referer", "https://music.163.com/"),
    )?;
    let arr = v
        .pointer("/result/songs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for it in arr {
        let fee = it.get("fee").and_then(Value::as_i64).unwrap_or(0);
        // fee=0 免费；fee=8 可免费试听低音质（不保证整曲，交给下载后校验兜底）
        if fee != 0 && fee != 8 {
            continue;
        }
        let id = it.get("id").and_then(Value::as_i64).map(|x| x.to_string()).unwrap_or_default();
        let title = str_of(&it, &["name"]);
        if id.is_empty() || title.is_empty() {
            continue;
        }
        let artist = it
            .get("artists")
            .or_else(|| it.get("ar"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default();
        let duration = it
            .get("duration")
            .or_else(|| it.get("dt"))
            .and_then(Value::as_u64)
            .map(|ms| ms / 1000);
        out.push(Candidate {
            provider: "netease",
            id,
            copyright_id: String::new(),
            title,
            artist,
            duration,
        });
    }
    Ok(out)
}

fn resolve_netease(_client: &Client, c: &Candidate) -> Result<Resolved> {
    Ok(Resolved {
        url: format!("{}?id={}.mp3", NETEASE_OUTER, c.id),
        quality: Quality::Mp3_128,
        source: "网易云音乐 · 匿名 128k 整曲".to_string(),
        expected_size: None,
    })
}

// ---------- 匹配打分 ----------

fn score(song: &Song, c: &Candidate) -> Option<i32> {
    let want = normalize(&core_title(&song.name));
    let got = normalize(&core_title(&c.title));
    if want.is_empty() || got.is_empty() {
        return None;
    }
    if want != got && !got.contains(&want) && !want.contains(&got) {
        return None;
    }

    let cand_artist = normalize(&c.artist);
    if cand_artist.is_empty() {
        // 空歌手名会让 contains("") 恒真，绝不能当成匹配成功。
        return None;
    }
    let artists: Vec<String> = song
        .artists
        .iter()
        .map(|a| normalize(a))
        .filter(|a| !a.is_empty())
        .collect();
    if artists.is_empty() {
        return None;
    }
    let artist_hit = artists
        .iter()
        .any(|a| cand_artist.contains(a.as_str()) || a.contains(&cand_artist));
    if !artist_hit {
        return None;
    }

    let mut s = 0;
    if want == got {
        s += 100;
    }
    s += 60;

    if let Some(d) = c.duration {
        let diff = d as i64 - song.duration as i64;
        if diff.abs() > 5 {
            return None;
        }
        s += 80 - diff.abs() as i32 * 10;
    }

    let song_lower = song.name.to_lowercase();
    let cand_lower = c.title.to_lowercase();
    for bad in VERSION_MARKERS {
        let in_song = has_marker(&song_lower, bad);
        let in_cand = has_marker(&cand_lower, bad);
        // 原曲不是伴奏/翻唱/Live 时，命中标记的候选直接淘汰，绝不拿替代品冒充。
        // 注意不能用裸子串匹配：那样 "Alive" 会被当成 "live"，
        // 结果是 Live 版反而能通过、正版原曲被误杀。
        if in_cand && !in_song {
            return None;
        }
        if in_song {
            s += 10;
        }
    }

    Some(s)
}

/// 版本标记匹配。ASCII 标记（live / dj / remix / cover / ktv）按单词边界匹配，
/// 避免 "Alive" 命中 "live"；中文标记仍按子串匹配。
fn has_marker(title_lower: &str, marker: &str) -> bool {
    if marker.is_ascii() {
        title_lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| word == marker)
    } else {
        title_lower.contains(marker)
    }
}

pub fn core_title(s: &str) -> String {
    let mut end = s.len();
    for pat in ["(", "（", "[", "【", "（", " - ", "–", "—", "《"] {
        if let Some(i) = s.find(pat) {
            if i < end {
                end = i;
            }
        }
    }
    s[..end].trim().to_string()
}

pub fn normalize(s: &str) -> String {
    let s = s.replace("&nbsp;", " ").replace('\u{3000}', " ");
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        }
    }
    out
}

fn clean(s: &str) -> String {
    let mut out = s.replace("&nbsp;", " ").replace("&amp;", "&");
    // 酷我返回的是单引号伪 JSON，值里的 & 可能带一层或多层反斜杠转义，
    // 例如 4 个反斜杠 + u0026。先折叠重复反斜杠，再还原实体。
    while out.contains("\\\\") {
        out = out.replace("\\\\", "\\");
    }
    out.replace("\\u0026", "&")
        .replace("\\u003c", "<")
        .replace("\\u003e", ">")
        .replace("\\u0027", "'")
        .replace("\\&", "&")
        .trim()
        .to_string()
}

fn str_of(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        match v.get(*k) {
            Some(Value::String(s)) => return s.clone(),
            Some(Value::Number(n)) => return n.to_string(),
            _ => {}
        }
    }
    String::new()
}

fn get_json(req: reqwest::blocking::RequestBuilder) -> Result<Value> {
    let resp = req.send().context("镜像请求失败")?;
    let status = resp.status();
    let text = resp.text().context("读取镜像响应失败")?;
    if !status.is_success() {
        bail!("镜像返回 HTTP {status}");
    }
    serde_json::from_str(&text).context("解析镜像响应失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(name: &str, artist: &str, duration: u64) -> Song {
        Song {
            mid: "m".into(),
            media_mid: "mm".into(),
            name: name.into(),
            artists: vec![artist.into()],
            album: String::new(),
            duration,
            size_128: 0,
            size_320: 0,
            size_flac: 0,
            preview_size: 0,
        }
    }

    fn cand(title: &str, artist: &str, duration: Option<u64>) -> Candidate {
        Candidate {
            provider: "migu",
            id: "id".into(),
            copyright_id: "c".into(),
            title: title.into(),
            artist: artist.into(),
            duration,
        }
    }

    #[test]
    fn alive_is_not_treated_as_live() {
        let s = song("Alive", "Pearl Jam", 294);
        assert!(score(&s, &cand("Alive", "Pearl Jam", Some(294))).is_some());
        assert!(score(&s, &cand("Alive (Live)", "Pearl Jam", Some(294))).is_none());
        assert!(score(&s, &cand("Alive(Live)", "Pearl Jam", None)).is_none());
    }

    #[test]
    fn ascii_markers_match_whole_tokens() {
        assert!(has_marker("晴天 (live)", "live"));
        assert!(has_marker("晴天live", "live"));
        assert!(!has_marker("alive", "live"));
        assert!(has_marker("song (dj版)", "dj"));
        assert!(has_marker("song（ktv版）", "ktv"));
    }

    #[test]
    fn original_with_marker_allows_same_marker() {
        let s = song("晴天 (Live)", "周杰伦", 269);
        assert!(score(&s, &cand("晴天 (Live)", "周杰伦", Some(269))).is_some());
    }

    #[test]
    fn version_marker_candidate_is_rejected() {
        let s = song("晴天", "周杰伦", 269);
        assert!(score(&s, &cand("晴天 (KTV版伴奏)", "周杰伦", Some(269))).is_none());
        assert!(score(&s, &cand("晴天 (现场版)", "周杰伦", Some(269))).is_none());
    }

    #[test]
    fn empty_candidate_artist_never_matches() {
        let s = song("晴天", "周杰伦", 269);
        assert!(score(&s, &cand("晴天", "", Some(269))).is_none());
    }

    #[test]
    fn kuwo_double_escaped_ampersand() {
        // serde 解析后剩下两个反斜杠 + u0026
        assert_eq!(clean("李宗盛\\\\u0026陈淑桦"), "李宗盛&陈淑桦");
    }
}
