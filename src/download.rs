//! 断点续传与文件校验。进度通过回调上报，由 CLI / TUI / WebUI 各自渲染。

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::api::{Quality, USER_AGENT};

/// 用户主动取消。调用方可以 downcast 出来，区分「取消」和真正的失败。
#[derive(Debug, Clone, Copy)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("下载已取消")
    }
}

impl std::error::Error for Cancelled {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadResult {
    /// 目标文件已存在且非空，没有发起网络请求。
    Skipped { bytes: u64 },
    /// 本次下载完成。
    Downloaded { bytes: u64 },
}

/// 下载到 `dest`。支持断点续传（`.part` 文件）、取消与进度回调。
///
/// `resume_key` 标识「本次音源 + 音质 + 时长」，记录在 `.part.meta`；
/// 音源变化时旧的 `.part` 会被丢弃，避免把咪咕 128k 与 QQ 320k 拼在一起。
///
/// 同名文件处理：已有文件不小于目标（或目标大小未知）时跳过，避免拿 128k
/// 降级版覆盖已有的 320k；只有已有文件明显更小（expected 更大）时才继续下载
/// 覆盖，实现低音质到高音质的升级。
///
/// `on_progress(downloaded, total)`：`total == 0` 表示服务端未给 Content-Length。
#[allow(clippy::too_many_arguments)]
pub fn download_with(
    client: &Client,
    url: &str,
    dest: &Path,
    quality: Quality,
    expected: u64,
    resume_key: &str,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<DownloadResult> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }

    if let Ok(meta) = fs::metadata(dest) {
        if meta.is_dir() {
            bail!("目标路径是目录，无法写入文件: {}", dest.display());
        }
        if !meta.file_type().is_file() {
            bail!("目标路径不是普通文件: {}", dest.display());
        }
        if meta.len() > 0 {
            // expected 未知，或已有文件不小于目标：一律保留，避免拿降级版
            // 覆盖更高音质。只有明确知道新文件更大时才继续下载覆盖。
            if expected == 0 || meta.len() >= expected {
                // 已完成文件旁边若还残留旧分片，顺手清掉，避免下次误导续传。
                let _ = fs::remove_file(part_path(dest));
                let _ = fs::remove_file(part_meta_path(dest));
                return Ok(DownloadResult::Skipped { bytes: meta.len() });
            }
        }
    }

    let part = part_path(dest);
    let part_meta = part_meta_path(dest);
    let mut existing = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if existing > 0 {
        let old = fs::read_to_string(&part_meta).unwrap_or_default();
        if old.trim() != resume_key {
            let _ = fs::remove_file(&part);
            let _ = fs::remove_file(&part_meta);
            existing = 0;
        }
    }
    if existing == 0 {
        fs::write(&part_meta, resume_key)
            .with_context(|| format!("写入续传标记失败: {}", part_meta.display()))?;
    }

    // blocking Response 的 Read 会把该 timeout 应用到**每一次读取**上，
    // 所以它是“停顿超时”而不是整个下载的总时限：只要数据持续到达，大文件
    // 可以下载任意久；一旦 CDN 卡住，最多等 20s 就能检查取消标志。
    let mut req = client
        .get(url)
        .header("Referer", "https://y.qq.com/")
        .header("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(20));
    if existing > 0 {
        req = req.header("Range", format!("bytes={}-", existing));
    }

    let resp = match req.send() {
        Ok(resp) => resp,
        Err(e) => {
            if cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            return Err(e).context("下载请求失败");
        }
    };
    let status = resp.status();

    let (total, append) = if status.as_u16() == 206 {
        let len = resp.content_length().unwrap_or(0);
        (if len > 0 { existing + len } else { 0 }, true)
    } else if status.is_success() {
        existing = 0;
        (resp.content_length().unwrap_or(0), false)
    } else if status.as_u16() == 416 && existing > 0 {
        // .part 已经和远端一样长（或服务端对 Range 的处理有偏差）时，
        // 服务端会回 416；删掉残留分片后从头再下一次（只重试这一次）。
        let _ = fs::remove_file(&part);
        let _ = fs::remove_file(&part_meta);
        return download_with(
            client,
            url,
            dest,
            quality,
            expected,
            resume_key,
            cancel,
            on_progress,
        );
    } else {
        bail!("下载失败: HTTP {}", status);
    };

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&part)
        .with_context(|| format!("打开文件失败: {}", part.display()))?;

    let mut position = existing;
    on_progress(position, total);

    let mut reader = resp;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            let _ = file.sync_all();
            return Err(Cancelled.into());
        }
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = file.flush();
                    let _ = file.sync_all();
                    return Err(Cancelled.into());
                }
                return Err(e).context("读取下载数据失败");
            }
        };
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("写入文件失败")?;
        position += n as u64;
        on_progress(position, total);
    }
    file.flush().context("刷新文件失败")?;
    let _ = file.sync_all();
    drop(file);

    let got = fs::metadata(&part)
        .with_context(|| format!("读取临时文件失败: {}", part.display()))?
        .len();
    if got == 0 {
        let _ = fs::remove_file(&part);
        let _ = fs::remove_file(&part_meta);
        bail!("下载内容为空");
    }

    if let Err(e) = validate_audio(&part, quality) {
        let _ = fs::remove_file(&part);
        let _ = fs::remove_file(&part_meta);
        return Err(e);
    }

    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    fs::rename(&part, dest)
        .with_context(|| format!("重命名文件失败: {} -> {}", part.display(), dest.display()))?;
    let _ = fs::remove_file(&part_meta);

    Ok(DownloadResult::Downloaded { bytes: got })
}

fn part_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", dest.display()))
}

fn part_meta_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part.meta", dest.display()))
}

/// 已有同名文件、但大小与本次接口给的大小不同（常见于 128k → 320k 换音质）。
pub fn existing_mismatch(dest: &Path, expected: u64) -> bool {
    if expected == 0 {
        return false;
    }
    fs::metadata(dest)
        .map(|m| m.is_file() && m.len() > 0 && m.len() != expected)
        .unwrap_or(false)
}

/// 已有同名文件比目标小，本次会下载并覆盖（通常是低音质升级）。
pub fn existing_will_overwrite(dest: &Path, expected: u64) -> bool {
    expected > 0
        && fs::metadata(dest)
            .map(|m| m.is_file() && m.len() > 0 && m.len() < expected)
            .unwrap_or(false)
}

/// 存在旧 `.part`，且其音源/音质标记与本次不同；下载时会丢弃并从头开始。
pub fn resume_will_restart(dest: &Path, resume_key: &str) -> bool {
    let part = part_path(dest);
    let has_part = fs::metadata(&part).map(|m| m.len() > 0).unwrap_or(false);
    if !has_part {
        return false;
    }
    let old = fs::read_to_string(part_meta_path(dest)).unwrap_or_default();
    old.trim() != resume_key
}

/// 轻量文件头校验，防止把错误页/JSON 当成音频保存。
fn validate_audio(path: &Path, quality: Quality) -> Result<()> {
    let mut f = File::open(path).with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut head = [0u8; 8];
    let n = f.read(&mut head).unwrap_or(0);
    let is_mp3 = n >= 3 && (&head[..3] == b"ID3" || head[0] == 0xFF);
    let is_flac = n >= 4 && &head[..4] == b"fLaC";
    let is_mp4 = n >= 8 && &head[4..8] == b"ftyp";
    let ok = match quality {
        Quality::Flac => is_flac,
        _ => is_mp3 || is_mp4,
    };
    if !ok {
        bail!("下载到的内容不是有效音频（链接可能失效、需要登录/会员，或服务端返回了错误页）");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_is_distinguishable() {
        let e: anyhow::Error = Cancelled.into();
        assert!(e.downcast_ref::<Cancelled>().is_some());
    }

    #[test]
    fn part_path_appends_suffix() {
        assert_eq!(
            part_path(Path::new("/tmp/a.mp3")),
            PathBuf::from("/tmp/a.mp3.part")
        );
        assert_eq!(
            part_meta_path(Path::new("/tmp/a.mp3")),
            PathBuf::from("/tmp/a.mp3.part.meta")
        );
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("qqmusic-dl-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn completed_file_size_conflict_is_detected() {
        let dir = temp_dir("size-conflict");
        let dest = dir.join("song.mp3");
        fs::write(&dest, vec![0u8; 100]).expect("write");
        assert!(existing_mismatch(&dest, 200));
        assert!(!existing_mismatch(&dest, 100));
        assert!(!existing_mismatch(&dest, 0));
        assert!(!existing_mismatch(&dir.join("missing.mp3"), 200));
        // 现有文件更小 -> 升级覆盖；现有文件更大 -> 保留。
        assert!(existing_will_overwrite(&dest, 200));
        fs::write(&dest, vec![0u8; 300]).expect("write");
        assert!(!existing_will_overwrite(&dest, 200));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_part_with_other_source_is_detected() {
        let dir = temp_dir("part-meta");
        let dest = dir.join("song.mp3");
        fs::write(part_path(&dest), vec![0u8; 10]).expect("part");
        fs::write(part_meta_path(&dest), "qq|320|10").expect("meta");
        assert!(!resume_will_restart(&dest, "qq|320|10"));
        assert!(resume_will_restart(&dest, "migu|128|10"));
        let _ = fs::remove_dir_all(&dir);
    }
}
