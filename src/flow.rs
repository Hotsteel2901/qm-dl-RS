//! 公共取流 + 下载流程。
//!
//! CLI、全屏 TUI 和 WebUI 都走这里，避免三套回退顺序和校验逻辑各自漂移：
//!   QQ 原生 vkey 直链 → 会员票据桥 → 匿名镜像（咪咕/酷我/网易）→ 官方 60 秒试听。
//!
//! 调用方通过 `on_event` 接收结构化事件（阶段、来源、进度、校验结果），
//! 自己决定打印成终端文本、画进 ratatui，还是写进 Web 任务状态。

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::api::{QqClient, Quality, Song, StreamLookup, StreamUrl};
use crate::bridge;
use crate::download::{self, DownloadResult};
use crate::mirror;
use crate::probe;
use crate::util;

/// 消息级别。CLI 用不同颜色；TUI/WebUI 用不同样式。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Dim,
    Warn,
}

#[derive(Clone, Debug)]
pub enum FlowEvent {
    /// 过程提示（镜像命中、降级说明、校验跳过等）。
    Message { level: Level, text: String },
    /// 已确定音质、来源与目标文件。
    Resolved {
        quality: Quality,
        source: String,
        preview: bool,
        degraded: bool,
        expected_size: u64,
    },
    /// 目标路径与直链（dry-run 时也会发出）。
    Target { path: PathBuf, url: String },
    /// 下载进度；`total == 0` 表示服务端没给 Content-Length。
    Progress { downloaded: u64, total: u64 },
    /// 文件已落盘。
    Saved { path: PathBuf, bytes: u64 },
    /// 整曲时长校验通过。
    Verified { actual: f64, expected: u64 },
}

/// 一次下载请求。TUI / WebUI 的每个任务各持一份。
pub struct Request {
    pub quality: Quality,
    pub out: PathBuf,
    pub dry_run: bool,
    /// 只取官方 60 秒试听。
    pub preview_only: bool,
    /// 整曲不可用时，不自动回落试听。
    pub no_preview: bool,
    pub mirror: String,
    pub bridge: Option<String>,
    pub verify: bool,
    /// 由调用方持有的取消标志；下载循环每 64KB 检查一次。
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    DryRun {
        path: PathBuf,
        url: String,
    },
    Downloaded {
        path: PathBuf,
        bytes: u64,
        quality: Quality,
        source: String,
        preview: bool,
        verified: Option<f64>,
    },
}

static NEVER_CANCEL: AtomicBool = AtomicBool::new(false);

/// 执行一次完整流程。`on_event` 会在当前线程同步调用。
pub fn execute(
    client: &QqClient,
    song: &Song,
    req: &Request,
    on_event: &mut dyn FnMut(FlowEvent),
) -> Result<Outcome> {
    check_cancel(req)?;
    let (stream, source, expected, degraded) = resolve_stream(client, song, req, on_event)?;
    let path = target_path(&req.out, song, &stream);

    on_event(FlowEvent::Resolved {
        quality: stream.quality,
        source: source.clone(),
        preview: stream.preview,
        degraded,
        expected_size: expected,
    });
    on_event(FlowEvent::Target {
        path: path.clone(),
        url: stream.url.clone(),
    });

    if req.dry_run {
        return Ok(Outcome::DryRun {
            path,
            url: stream.url,
        });
    }

    let cancel = req.cancel.as_deref().unwrap_or(&NEVER_CANCEL);
    let resume_key = format!(
        "{}|{}|{}|{}",
        song.mid,
        stream.quality.slug(),
        source,
        expected
    );
    if download::existing_mismatch(&path, expected) {
        if download::existing_will_overwrite(&path, expected) {
            warn(
                on_event,
                "已存在更小的同名文件（可能是低音质），本次将下载并覆盖".to_string(),
            );
        } else {
            warn(
                on_event,
                "已存在同名文件且不小于本次目标；为免覆盖更高音质，本次保留原文件。如需强制替换请先删除它。".to_string(),
            );
        }
    }
    if download::resume_will_restart(&path, &resume_key) {
        warn(
            on_event,
            "音源或音质已变化，旧的 .part 已丢弃，从头下载".to_string(),
        );
    }

    let mut last_downloaded = 0u64;
    let result = download::download_with(
        client.client(),
        &stream.url,
        &path,
        stream.quality,
        expected,
        &resume_key,
        cancel,
        &mut |downloaded, total| {
            last_downloaded = downloaded;
            on_event(FlowEvent::Progress { downloaded, total });
        },
    )?;

    let downloaded_now = matches!(result, DownloadResult::Downloaded { .. });
    match result {
        DownloadResult::Skipped { bytes } => {
            on_event(FlowEvent::Message {
                level: Level::Warn,
                text: format!("已存在，跳过（未重新下载）: {}", path.display()),
            });
            on_event(FlowEvent::Saved {
                path: path.clone(),
                bytes,
            });
            last_downloaded = bytes;
        }
        DownloadResult::Downloaded { bytes } => {
            on_event(FlowEvent::Saved {
                path: path.clone(),
                bytes,
            });
            last_downloaded = bytes;
        }
    }

    let mut verified = None;
    if req.verify && !stream.preview && song.duration > 0 {
        match probe::duration_of(&path) {
            Ok(actual) => {
                let diff = (actual - song.duration as f64).abs();
                if diff > 2.5 {
                    if downloaded_now {
                        let _ = std::fs::remove_file(&path);
                        bail!(
                            "时长校验失败：文件 {:.1}s，歌曲 {}s —— 拿到的是片段或别的版本，已删除",
                            actual,
                            song.duration
                        );
                    }
                    // 文件是本次运行之前就存在的：宁可报错也不要误删用户文件。
                    bail!(
                        "已有文件时长校验失败：文件 {:.1}s，歌曲 {}s；为避免误删，文件已保留",
                        actual,
                        song.duration
                    );
                }
                verified = Some(actual);
                on_event(FlowEvent::Verified {
                    actual,
                    expected: song.duration,
                });
            }
            Err(e) => on_event(FlowEvent::Message {
                level: Level::Warn,
                text: format!("校验跳过: {e}"),
            }),
        }
    }

    if downloaded_now && expected > 0 && last_downloaded != expected {
        on_event(FlowEvent::Message {
            level: Level::Warn,
            text: format!(
                "实际大小 {} 字节，与接口给的大小 {} 字节不一致（可能被服务端转码）",
                last_downloaded, expected
            ),
        });
    }

    Ok(Outcome::Downloaded {
        path,
        bytes: last_downloaded,
        quality: stream.quality,
        source,
        preview: stream.preview,
        verified,
    })
}

/// 取消检查。网络请求本身不可中断，但每个阶段/降级候选之前检查一次；
/// 下载读取的停顿超时是 20s，因此取消通常能在几十秒内生效。
fn check_cancel(req: &Request) -> Result<()> {
    if req
        .cancel
        .as_deref()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        return Err(download::Cancelled.into());
    }
    Ok(())
}

fn resolve_stream(
    client: &QqClient,
    song: &Song,
    req: &Request,
    on_event: &mut dyn FnMut(FlowEvent),
) -> Result<(StreamUrl, String, u64, bool)> {
    check_cancel(req)?;

    // 1) 仅试听模式
    if req.preview_only {
        return match client.trial_url(song)? {
            StreamLookup::Found(stream) => Ok((
                stream,
                "QQ音乐 · 官方试听".to_string(),
                song.preview_size,
                false,
            )),
            StreamLookup::Unavailable { result } => {
                bail!("没有可用的官方试听片段（result={}）", result)
            }
        };
    }

    // 2) 票据桥：会员账号签发的直链，本地匿名消费
    if let Some(base) = &req.bridge {
        for q in req.quality.fallbacks() {
            check_cancel(req)?;
            match bridge::fetch_via_bridge(client.client(), base, &song.mid, q) {
                Ok(stream) => {
                    let source = format!("会员票据桥 · {}", stream.quality.label());
                    return Ok((
                        stream.clone(),
                        source,
                        song.size_for(stream.quality),
                        stream.quality != req.quality,
                    ));
                }
                Err(e) => warn(on_event, format!("票据桥 {} 不可用: {e}", q.label())),
            }
        }
    }

    // 3) QQ 原生直链
    let mut last_result = 0i64;
    for q in req.quality.fallbacks() {
        check_cancel(req)?;
        match client.stream_url(song, q)? {
            StreamLookup::Found(stream) => {
                if q != req.quality {
                    warn(
                        on_event,
                        format!(
                            "请求的 {} 不可用，已降级为 {}",
                            req.quality.label(),
                            q.label()
                        ),
                    );
                }
                let source = if client.auth().logged_in {
                    "QQ音乐 · 账号直链"
                } else {
                    "QQ音乐 · 匿名直链"
                };
                let degraded = q != req.quality;
                return Ok((stream, source.to_string(), song.size_for(q), degraded));
            }
            StreamLookup::Unavailable { result } => {
                last_result = result;
                if q == req.quality {
                    warn(
                        on_event,
                        format!("{} 被接口拒绝（result={}）", q.label(), result),
                    );
                }
            }
        }
    }

    // 4) 匿名镜像：QQ 不给，就换一家没有付费墙的曲库
    if req.mirror != "off" {
        check_cancel(req)?;
        dim(on_event, "QQ 拒绝，切换匿名镜像…".to_string());
        let cancel = req.cancel.as_deref().unwrap_or(&NEVER_CANCEL);
        match mirror::resolve_with(client.client(), song, &req.mirror, cancel, |line| {
            info(on_event, line.to_string());
        }) {
            Ok(r) => {
                if r.quality != req.quality {
                    warn(
                        on_event,
                        format!(
                            "匿名免费源上限 {}（{} 需要账号/票据桥）",
                            r.quality.label(),
                            req.quality.label()
                        ),
                    );
                }
                let stream = StreamUrl {
                    url: r.url,
                    quality: r.quality,
                    preview: false,
                };
                let expected = r.expected_size.unwrap_or(0);
                let degraded = r.quality != req.quality;
                return Ok((stream, r.source, expected, degraded));
            }
            Err(e) => {
                // 镜像层现在会原样上抛 Cancelled；这里不能再把它格式化成
                // 普通提示，否则 UI 会把取消误判成 failed。
                if e.downcast_ref::<download::Cancelled>().is_some() {
                    return Err(e);
                }
                info(on_event, format!("{e}"));
            }
        }
    }

    // 5) 官方 60 秒试听
    if !req.no_preview {
        check_cancel(req)?;
        if let StreamLookup::Found(stream) = client.trial_url(song)? {
            warn(
                on_event,
                "所有整曲通道都失败，回落到官方 60 秒试听".to_string(),
            );
            return Ok((
                stream,
                "QQ音乐 · 官方试听".to_string(),
                song.preview_size,
                true,
            ));
        }
    }

    let hint = if client.auth().logged_in {
        "当前账号没有该歌曲的播放/下载权限，或歌曲已下架。\n  可尝试：换有会员权益的账号、--bridge，或 --mirror migu。"
    } else {
        "匿名无法获取整曲。\n  可尝试：login qqmusic（QQ音乐 App 扫码）、--bridge，或 --mirror migu。"
    };
    bail!("未获取到下载地址（result={}）。\n  {}", last_result, hint)
}

fn info(on_event: &mut dyn FnMut(FlowEvent), text: String) {
    on_event(FlowEvent::Message {
        level: Level::Info,
        text,
    });
}

fn dim(on_event: &mut dyn FnMut(FlowEvent), text: String) {
    on_event(FlowEvent::Message {
        level: Level::Dim,
        text,
    });
}

fn warn(on_event: &mut dyn FnMut(FlowEvent), text: String) {
    on_event(FlowEvent::Message {
        level: Level::Warn,
        text,
    });
}

/// 按「歌手 - 歌名.扩展名」生成落盘路径，统一做非法字符清洗与字节截断。
pub fn target_path(out: &Path, song: &Song, stream: &StreamUrl) -> PathBuf {
    let artist = util::sanitize(&song.artist_str());
    let title = util::sanitize(&song.name);
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
    let base = util::truncate_bytes(&base, 180);
    out.join(format!("{}.{}", base, stream.quality.ext()))
}

/// 进度测速：每 250ms 采一次样，做指数平滑，避免 UI 数字乱跳。
pub struct SpeedMeter {
    last_time: std::time::Instant,
    last_bytes: u64,
    ema: f64,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeedMeter {
    pub fn new() -> Self {
        Self {
            last_time: std::time::Instant::now(),
            last_bytes: 0,
            ema: 0.0,
        }
    }

    /// 返回平滑后的 bytes/s；采样间隔不足时返回上一次的值。
    pub fn update(&mut self, bytes: u64) -> f64 {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_time).as_secs_f64();
        if elapsed >= 0.25 {
            let delta = bytes.saturating_sub(self.last_bytes) as f64;
            let instant = delta / elapsed;
            self.ema = if self.ema <= 0.0 {
                instant
            } else {
                self.ema * 0.65 + instant * 0.35
            };
            self.last_time = now;
            self.last_bytes = bytes;
        }
        self.ema
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song() -> Song {
        Song {
            mid: "0048Ts0Q0oi3Ti".into(),
            media_mid: "0048Ts0Q0oi3Ti".into(),
            name: "晴天".into(),
            artists: vec!["周杰伦".into()],
            album: "叶惠美".into(),
            duration: 269,
            size_128: 1,
            size_320: 2,
            size_flac: 3,
            preview_size: 4,
        }
    }

    fn stream(quality: Quality, preview: bool) -> StreamUrl {
        StreamUrl {
            url: "https://example.com/x".into(),
            quality,
            preview,
        }
    }

    #[test]
    fn target_path_is_sanitized() {
        let mut s = song();
        s.name = "a/b:c".into();
        let p = target_path(Path::new("/tmp"), &s, &stream(Quality::Mp3_320, false));
        assert_eq!(p, PathBuf::from("/tmp/周杰伦 - a_b_c.mp3"));
    }

    #[test]
    fn preview_gets_suffix_and_flac_extension() {
        let p = target_path(Path::new("."), &song(), &stream(Quality::Flac, true));
        assert!(p.to_string_lossy().contains("[试听60s]"));
        assert!(p.to_string_lossy().ends_with(".flac"));
    }

    #[test]
    fn empty_names_fall_back_to_mid() {
        let mut s = song();
        s.name = String::new();
        s.artists.clear();
        let p = target_path(Path::new("."), &s, &stream(Quality::Mp3_128, false));
        assert_eq!(p, PathBuf::from("./0048Ts0Q0oi3Ti.mp3"));
    }

    #[test]
    fn cancel_flag_short_circuits_before_network() {
        let flag = Arc::new(AtomicBool::new(true));
        let req = Request {
            quality: Quality::Mp3_320,
            out: PathBuf::from("."),
            dry_run: false,
            preview_only: false,
            no_preview: false,
            mirror: "off".to_string(),
            bridge: None,
            verify: false,
            cancel: Some(flag),
        };
        let err = check_cancel(&req).expect_err("should be cancelled");
        assert!(err.downcast_ref::<crate::download::Cancelled>().is_some());
    }

    #[test]
    fn speed_meter_smooths() {
        let mut m = SpeedMeter::new();
        // 第一次采样间隔不足 250ms，返回 0
        let first = m.update(1000);
        assert!(first >= 0.0);
        std::thread::sleep(std::time::Duration::from_millis(280));
        let second = m.update(2000);
        assert!(second > 0.0);
    }
}
