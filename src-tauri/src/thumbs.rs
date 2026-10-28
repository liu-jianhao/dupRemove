//! thumbs：缩略图生成与磁盘缓存。
//! 关键工程点（docs/02 评审问题 5）：缩略图在 Rust 侧生成落盘，
//! 前端经 Tauri asset 协议直连加载，绝不经 IPC 传 base64。
//!
//! 解码走统一边界 `decode`（docs/04 阶段 2）：EXIF 方向已应用（竖拍不歪）、
//! HEIC 可出图 —— 与指纹看到的是同一份像素。

use crate::decode;
use std::path::{Path, PathBuf};

pub const THUMB_SIZE: u32 = 320;

/// 缩略图算法版本，进入缓存 key。改动缩略图生成方式必须 bump
///（v2 = 应用 EXIF 方向 + HEIC 支持 + 纳秒 mtime）。
const THUMB_ALGO_VERSION: u64 = 2;

/// 确保缩略图存在并返回其路径。key = blake3(路径 || mtime_ns || 算法版本)，
/// 文件变更（mtime_ns 变）或算法升级自动失效。
pub fn ensure_thumb(src: &Path, mtime_ns: u64, cache_dir: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(cache_dir).ok()?;

    let mut key_input = Vec::with_capacity(
        src.as_os_str().as_encoded_bytes().len() + std::mem::size_of::<u64>() * 2,
    );
    key_input.extend_from_slice(src.as_os_str().as_encoded_bytes());
    key_input.extend_from_slice(&mtime_ns.to_le_bytes());
    key_input.extend_from_slice(&THUMB_ALGO_VERSION.to_le_bytes());
    let hex = blake3::hash(&key_input).to_hex().to_string();
    let dst = cache_dir.join(format!("{}.jpg", &hex[..16]));

    if dst.exists() {
        return Some(dst);
    }

    // 统一解码边界：方向已应用、HEIC 可解；to_rgb8 去掉 alpha（JPEG 无 alpha）
    let img = decode::decode(src)?.img.to_rgb8();
    let thumb =
        image::DynamicImage::ImageRgb8(image::imageops::thumbnail(&img, THUMB_SIZE, THUMB_SIZE));
    // 显式 JPEG 编码（临时文件无 .jpg 扩展名，不能用 save 按扩展名推断格式）
    // 先写临时文件再原子改名，避免并发生成时读到半截文件
    let tmp = cache_dir.join(format!("{}.tmp", &hex[..16]));
    let file = std::fs::File::create(&tmp).ok()?;
    let writer = std::io::BufWriter::new(file);
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(writer, 78);
    thumb.write_with_encoder(encoder).ok()?;
    std::fs::rename(&tmp, &dst).ok()?;
    Some(dst)
}

/// 缩略图缓存上限（ponytail：单目录 + 按 mtime 删最旧；扫描/regroup 结束调用一次）。
pub const THUMB_CACHE_MAX_FILES: usize = 1000;
pub const THUMB_CACHE_MAX_BYTES: u64 = 150 * 1024 * 1024;

/// 超出上限时按修改时间淘汰最旧 `.jpg`，返回删除数量。
pub fn prune_cache(cache_dir: &Path) -> std::io::Result<usize> {
    prune_cache_limited(cache_dir, THUMB_CACHE_MAX_FILES, THUMB_CACHE_MAX_BYTES)
}

fn prune_cache_limited(
    cache_dir: &Path,
    max_files: usize,
    max_bytes: u64,
) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jpg") {
            continue;
        }
        let meta = ent.metadata()?;
        if !meta.is_file() {
            continue;
        }
        files.push((
            path,
            meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            meta.len(),
        ));
    }
    let mut count = files.len();
    let mut total_bytes: u64 = files.iter().map(|(_, _, s)| s).sum();
    if count <= max_files && total_bytes <= max_bytes {
        return Ok(0);
    }
    files.sort_by_key(|(_, t, _)| *t);
    let mut removed = 0usize;
    for (path, _, size) in files {
        if count <= max_files && total_bytes <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
            count -= 1;
            total_bytes = total_bytes.saturating_sub(size);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    #[test]
    fn thumb_generated_and_cached() {
        fn make_grad(w: u32, h: u32) -> image::DynamicImage {
            let mut img = RgbImage::new(w, h);
            for x in 0..w {
                for y in 0..h {
                    let v = ((x + y) % 255) as u8;
                    img.put_pixel(x, y, Rgb([v, v, v]));
                }
            }
            image::DynamicImage::ImageRgb8(img)
        }

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("photo.jpg");
        make_grad(800, 600).save(&src).unwrap();

        let cache = dir.path().join("thumbs");
        let t1 = ensure_thumb(&src, 100, &cache).unwrap();
        assert!(t1.exists());
        // 尺寸受限
        let loaded = image::open(&t1).unwrap();
        assert!(loaded.width() <= THUMB_SIZE && loaded.height() <= THUMB_SIZE);

        // 第二次命中缓存（同一 mtime）
        let t2 = ensure_thumb(&src, 100, &cache).unwrap();
        assert_eq!(t1, t2);

        // mtime 变化 → 新 key → 新文件
        let t3 = ensure_thumb(&src, 200, &cache).unwrap();
        assert_ne!(t1, t3);
    }

    #[test]
    fn prune_cache_evicts_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            let p = dir.path().join(format!("{i}.jpg"));
            std::fs::write(&p, vec![0u8; 64]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let n = prune_cache_limited(dir.path(), 3, u64::MAX).unwrap();
        assert_eq!(n, 2);
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jpg"))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 3);
        assert!(!left.contains(&"0.jpg".to_string()));
        assert!(!left.contains(&"1.jpg".to_string()));
    }
}
