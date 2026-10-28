//! keeper：保留最优策略评分。
//! 优先级：分辨率最高 → 无损格式优先 → 文件最大 → 有 EXIF 相机信息 → 拍摄时间最早。
//!
//! 「无损优先」不可省略：同分辨率下 JPEG 可能比 PNG 更大（合成图案/截图类内容，
//! PNG 压缩后极小，而 JPEG 在硬边振铃上开销大）。若只比文件大小，
//! 会把有损的 JPEG 留下、删掉无损的 PNG——画质上更差。

use crate::finger::Fingerprint;
use std::path::Path;

/// 对一组指纹排序：最优在前，其余为建议删除项。
pub fn rank_group(fps: &[Fingerprint], group: &[usize]) -> Vec<usize> {
    let mut v = group.to_vec();
    v.sort_by(|&a, &b| {
        let (fa, fb) = (&fps[a], &fps[b]);
        let res_a = pixel_count(fa);
        let res_b = pixel_count(fb);
        // 分辨率降序
        res_b
            .cmp(&res_a)
            // 无损格式优先（仅在分辨率相同时生效）
            .then(is_lossless(&fb.path).cmp(&is_lossless(&fa.path)))
            // 文件大小降序
            .then(fb.size.cmp(&fa.size))
            // 有相机信息优先
            .then(fb.has_exif_camera.cmp(&fa.has_exif_camera))
            // 拍摄时间早的优先（None 排最后）
            .then(taken_at_key(fa).cmp(&taken_at_key(fb)))
    });
    v
}

/// 无损格式判定：PNG / BMP / TIFF 保真度高于 JPEG / WebP / HEIC。
/// GIF 虽是索引色（最多 256 色），画质低于 RGB 无损图，故不算。
fn is_lossless(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png") | Some("bmp") | Some("tif") | Some("tiff")
    )
}

fn pixel_count(fp: &Fingerprint) -> u64 {
    fp.width.unwrap_or(0) as u64 * fp.height.unwrap_or(0) as u64
}

/// Some 定宽时间字符串 → (0, s)；None → (1, "")，保证 None 恒排最后。
fn taken_at_key(fp: &Fingerprint) -> (u8, &str) {
    match fp.taken_at.as_deref() {
        Some(s) => (0, s),
        None => (1, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fp(path: &str, w: u32, h: u32, size: u64, camera: bool, taken: Option<&str>) -> Fingerprint {
        Fingerprint {
            path: PathBuf::from(path),
            size,
            mtime_ns: 0,
            dev: None,
            ino: None,
            phash: Some(0),
            csig: Some([0u8; crate::finger::COLOR_SIG_LEN]),
            width: Some(w),
            height: Some(h),
            has_exif_camera: camera,
            taken_at: taken.map(|s| s.to_string()),
        }
    }

    #[test]
    fn resolution_wins_first() {
        let fps = vec![
            fp(
                "small.jpg",
                100,
                100,
                5000,
                true,
                Some("2020:01:01 00:00:00"),
            ),
            fp("big.jpg", 4000, 3000, 4000, false, None),
        ];
        let ranked = rank_group(&fps, &[0, 1]);
        assert_eq!(ranked[0], 1); // 4000x3000 胜，尽管更小且无 EXIF
    }

    #[test]
    fn lossless_format_wins_over_bigger_lossy() {
        // 同分辨率：JPEG 文件反而更大（硬边振铃开销），但无损 PNG 画质更高 → 应保留 PNG
        let fps = vec![
            fp("shot.jpg", 4000, 3000, 3_000_000, false, None),
            fp("shot.png", 4000, 3000, 1_200_000, false, None),
        ];
        assert_eq!(rank_group(&fps, &[0, 1])[0], 1, "无损格式应优先保留");
    }

    #[test]
    fn resolution_still_beats_lossless() {
        // 分辨率更高的有损图，仍胜过低分辨率的无损图
        let fps = vec![
            fp("small.png", 800, 600, 2_000_000, false, None),
            fp("big.jpg", 4000, 3000, 500_000, false, None),
        ];
        assert_eq!(rank_group(&fps, &[0, 1])[0], 1);
    }

    #[test]
    fn then_size_then_camera_then_time() {
        // 同分辨率：大文件优先
        let fps = vec![
            fp("a.jpg", 100, 100, 100, false, None),
            fp("b.jpg", 100, 100, 200, false, None),
        ];
        assert_eq!(rank_group(&fps, &[0, 1])[0], 1);

        // 同分辨率同大小：有相机信息优先
        let fps = vec![
            fp("a.jpg", 100, 100, 100, false, None),
            fp("b.jpg", 100, 100, 100, true, None),
        ];
        assert_eq!(rank_group(&fps, &[0, 1])[0], 1);

        // 全同级：时间早的优先
        let fps = vec![
            fp("a.jpg", 100, 100, 100, true, Some("2021:05:01 00:00:00")),
            fp("b.jpg", 100, 100, 100, true, Some("2020:01:01 00:00:00")),
        ];
        assert_eq!(rank_group(&fps, &[0, 1])[0], 1);

        // 有时间的优先于无时间
        let fps = vec![
            fp("a.jpg", 100, 100, 100, true, None),
            fp("b.jpg", 100, 100, 100, true, Some("2019:01:01 00:00:00")),
        ];
        assert_eq!(rank_group(&fps, &[0, 1])[0], 1);
    }
}
