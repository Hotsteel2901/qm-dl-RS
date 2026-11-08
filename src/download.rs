use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api::{Quality, USER_AGENT};

/// 下载到 `dest`。支持断点续传（`.part` 文件）与进度条。
pub fn download(
    client: &Client,
    url: &str,
    dest: &Path,
    quality: Quality,
    expected: u64,
) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }

    if let Ok(meta) = fs::metadata(dest) {
        if meta.len() > 0 {
            println!("  已存在，跳过: {}", dest.display());
            return Ok(meta.len());
        }
    }

    let part = part_path(dest);
    let mut existing = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

    let mut req = client
        .get(url)
        .header("Referer", "https://y.qq.com/")
        .header("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(300));
    if existing > 0 {
        req = req.header("Range", format!("bytes={}-", existing));
    }

    let resp = req.send().context("下载请求失败")?;
    let status = resp.status();

    let (total, append) = if status.as_u16() == 206 {
        let len = resp.content_length().unwrap_or(0);
        (existing + len, true)
    } else if status.is_success() {
        existing = 0;
        (resp.content_length().unwrap_or(0), false)
    } else if status.as_u16() == 416 && existing > 0 {
        // .part 已经和远端一样长（或服务端对 Range 的处理有偏差）时，
        // 服务端会回 416；删掉残留分片后从头再下一次（只重试这一次）。
        let _ = fs::remove_file(&part);
        return download(client, url, dest, quality, expected);
    } else {
        bail!("下载失败: HTTP {}", status);
    };

    let pb = make_progress(total, existing);

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&part)
        .with_context(|| format!("打开文件失败: {}", part.display()))?;

    let mut reader = resp;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).context("读取下载数据失败")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("写入文件失败")?;
        pb.inc(n as u64);
    }
    file.flush().context("刷新文件失败")?;
    let _ = file.sync_all();
    pb.finish_and_clear();

    let got = fs::metadata(&part)
        .with_context(|| format!("读取临时文件失败: {}", part.display()))?
        .len();
    if got == 0 {
        let _ = fs::remove_file(&part);
        bail!("下载内容为空");
    }

    if let Err(e) = validate_audio(&part, quality) {
        let _ = fs::remove_file(&part);
        return Err(e);
    }

    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    fs::rename(&part, dest)
        .with_context(|| format!("重命名文件失败: {} -> {}", part.display(), dest.display()))?;

    if expected > 0 && got != expected {
        eprintln!(
            "  提示: 实际大小 {} 字节，与接口给的大小 {} 字节不一致（可能被服务端转码）",
            got, expected
        );
    }
    Ok(got)
}

fn part_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.part", dest.display()))
}

fn make_progress(total: u64, position: u64) -> ProgressBar {
    if total > 0 {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] \
                 {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
        );
        pb.set_position(position);
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] {bytes} ({bytes_per_sec})")
                .unwrap(),
        );
        pb
    }
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
