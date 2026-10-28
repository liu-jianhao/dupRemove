//! session：破坏性操作的授权边界（docs/04 阶段 1）。
//!
//! 三条铁律：
//! 1. **只能删当前扫描会话里的文件** —— 会话集合之外的一律拒绝，
//!    前端被攻破（XSS / 依赖污染 / 调试注入）也带不进越界路径。
//! 2. **删除前校验文件身份**（dev + ino + size + mtime_ns）——
//!    扫描后被替换的同路径文件必须拒绝，用户预览的必须是即将删除的那一份。
//! 3. **相似度档位是后端枚举** —— 前端传任意阈值数字不再被接受。
//!
//! 本模块是纯函数（不依赖 Tauri State），伪造路径 / 文件替换 / 非法档位
//! 都能直接写回归测试。

use crate::cluster::{self, Thresholds};
use crate::finger::Fingerprint;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 相似度档位。与前端 `types.ts` 的 `ThresholdLevel` 一一对应，
/// 但**阈值数值只存在于后端**（`cluster::THRESHOLDS`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Strict,
    Balanced,
    Loose,
}

pub fn parse_level(s: &str) -> Option<Level> {
    match s {
        "strict" => Some(Level::Strict),
        "balanced" => Some(Level::Balanced),
        "loose" => Some(Level::Loose),
        _ => None,
    }
}

pub fn level_thresholds(level: Level) -> Thresholds {
    cluster::THRESHOLDS[match level {
        Level::Strict => 0,
        Level::Balanced => 1,
        Level::Loose => 2,
    }]
}

/// 单个请求路径的校验结论。
pub enum Verdict {
    /// 会话成员且身份一致，允许移入废纸篓
    Allow(PathBuf),
    /// (原始路径, 用户可读的拒绝原因)
    Reject(String, String),
}

/// 校验待删除的路径列表：会话成员 + 文件身份双检查。
///
/// 返回按请求顺序的结论。调用方把 Allow 的交给 `move_to_trash`，
/// Reject 的作为失败项进返回值（用户能看到原因，文件留在原地）。
///
/// ponytail: 校验与真正移动之间仍有毫秒级 TOCTOU 窗口
/// （NSFileManager 按路径操作）。已从「扫描→删除」缩小到「校验→删除」；
/// 彻底消除需要基于文件描述符的删除，收益不抵复杂度。
pub fn validate_paths(fps: &[Fingerprint], requested: &[String]) -> Vec<Verdict> {
    let by_path: HashMap<&Path, &Fingerprint> =
        fps.iter().map(|fp| (fp.path.as_path(), fp)).collect();

    requested
        .iter()
        .map(|raw| {
            let path = PathBuf::from(raw);
            let Some(&fp) = by_path.get(path.as_path()) else {
                return Verdict::Reject(raw.clone(), "不在本次扫描结果中，已拒绝".into());
            };
            match std::fs::metadata(&path) {
                Err(_) => Verdict::Reject(raw.clone(), "文件已不存在，可能已被移动".into()),
                Ok(meta) => {
                    if meta.len() != fp.size {
                        return Verdict::Reject(
                            raw.clone(),
                            "文件大小与扫描时不一致，请重新扫描后再删除".into(),
                        );
                    }
                    if crate::scanner::mtime_ns(&meta) != fp.mtime_ns {
                        return Verdict::Reject(
                            raw.clone(),
                            "文件在扫描后被修改过，请重新扫描后再删除".into(),
                        );
                    }
                    // dev/ino 来自阶段 1 新增的缓存列；老库行是 NULL → 退化为
                    // size + mtime_ns 校验（首次重扫后即有完整身份）。
                    if let (Some(dev), Some(ino)) = (fp.dev, fp.ino) {
                        let (cur_dev, cur_ino) = crate::scanner::file_id(&meta);
                        if cur_dev != dev || cur_ino != ino {
                            return Verdict::Reject(
                                raw.clone(),
                                "文件已被替换成另一份，请重新扫描后再删除".into(),
                            );
                        }
                    }
                    Verdict::Allow(path)
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finger::COLOR_SIG_LEN;
    use std::path::PathBuf;

    fn fingerprint_of(path: &Path) -> Fingerprint {
        let meta = std::fs::metadata(path).unwrap();
        let (dev, ino) = crate::scanner::file_id(&meta);
        Fingerprint {
            path: path.to_path_buf(),
            size: meta.len(),
            mtime_ns: crate::scanner::mtime_ns(&meta),
            dev: Some(dev),
            ino: Some(ino),
            phash: Some(42),
            csig: Some([7u8; COLOR_SIG_LEN]),
            width: Some(100),
            height: Some(80),
            has_exif_camera: false,
            taken_at: None,
        }
    }

    #[test]
    fn parse_level_accepts_known_rejects_arbitrary() {
        assert_eq!(parse_level("strict"), Some(Level::Strict));
        assert_eq!(parse_level("balanced"), Some(Level::Balanced));
        assert_eq!(parse_level("loose"), Some(Level::Loose));
        // 伪造档位（含旧版数字阈值）一律拒绝
        assert_eq!(parse_level("ultra"), None);
        assert_eq!(parse_level("16"), None);
        assert_eq!(parse_level(""), None);
    }

    #[test]
    fn level_thresholds_match_cluster_presets() {
        assert_eq!(level_thresholds(Level::Strict), cluster::THRESHOLDS[0]);
        assert_eq!(level_thresholds(Level::Balanced), cluster::THRESHOLDS[1]);
        assert_eq!(level_thresholds(Level::Loose), cluster::THRESHOLDS[2]);
    }

    #[test]
    fn path_outside_session_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("a.jpg");
        std::fs::write(&inside, b"x").unwrap();
        let fps = vec![fingerprint_of(&inside)];

        // 会话为空：任何路径都拒绝
        let out = validate_paths(&[], &[inside.to_string_lossy().to_string()]);
        assert!(matches!(out[0], Verdict::Reject(_, _)));

        // 会话外的真实文件：拒绝（这是修复的核心越权场景）
        let outside = dir.path().join("b.jpg");
        std::fs::write(&outside, b"x").unwrap();
        let out = validate_paths(&fps, &[outside.to_string_lossy().to_string()]);
        match &out[0] {
            Verdict::Reject(raw, reason) => {
                assert!(
                    reason.contains("不在本次扫描结果中"),
                    "原因应指向会话校验: {reason}"
                );
                assert_eq!(raw, &outside.to_string_lossy().to_string());
            }
            _ => panic!("会话外文件必须被拒绝"),
        }
    }

    #[test]
    fn unchanged_session_file_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.jpg");
        std::fs::write(&f, b"hello").unwrap();
        let fps = vec![fingerprint_of(&f)];

        let out = validate_paths(&fps, &[f.to_string_lossy().to_string()]);
        assert!(matches!(out[0], Verdict::Allow(_)));
    }

    #[test]
    fn missing_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("gone.jpg");
        std::fs::write(&f, b"x").unwrap();
        let fps = vec![fingerprint_of(&f)];
        std::fs::remove_file(&f).unwrap();

        let out = validate_paths(&fps, &[f.to_string_lossy().to_string()]);
        match &out[0] {
            Verdict::Reject(_, reason) => assert!(reason.contains("不存在")),
            _ => panic!("已消失的文件必须被拒绝"),
        }
    }

    #[test]
    fn size_change_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.jpg");
        std::fs::write(&f, b"short").unwrap();
        let mut fp = fingerprint_of(&f);
        std::fs::write(&f, b"a much longer content").unwrap();
        // mtime 也变了，但先让 size 不匹配触发 size 分支
        fp.mtime_ns = crate::scanner::mtime_ns(&std::fs::metadata(&f).unwrap());

        let out = validate_paths(&[fp], &[f.to_string_lossy().to_string()]);
        match &out[0] {
            Verdict::Reject(_, reason) => assert!(reason.contains("大小")),
            _ => panic!("size 变化的文件必须被拒绝"),
        }
    }

    #[test]
    fn mtime_change_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.jpg");
        std::fs::write(&f, b"same-size").unwrap();
        let mut fp = fingerprint_of(&f);
        fp.mtime_ns = fp.mtime_ns.wrapping_add(1_000_000_000);

        let out = validate_paths(&[fp], &[f.to_string_lossy().to_string()]);
        match &out[0] {
            Verdict::Reject(_, reason) => assert!(reason.contains("修改过")),
            _ => panic!("mtime 变化的文件必须被拒绝"),
        }
    }

    #[test]
    fn replaced_file_with_kept_identity_is_rejected_by_inode() {
        // 攻击/竞态模型：同路径、同 size、mtime 刻意保留，但底层已是另一份文件。
        // 只有 dev/ino 校验能拦下这种替换。
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.jpg");
        std::fs::write(&f, b"payload").unwrap();
        let mut fp = fingerprint_of(&f);
        // 模拟「扫描后文件被替换」：inode 与现场不一致
        fp.ino = fp.ino.map(|v| v.wrapping_add(1));

        let out = validate_paths(&[fp], &[f.to_string_lossy().to_string()]);
        match &out[0] {
            Verdict::Reject(_, reason) => assert!(reason.contains("替换"), "原因: {reason}"),
            _ => panic!("同 size+mtime 但 inode 不同的文件必须被拒绝"),
        }
    }

    #[test]
    fn legacy_fingerprint_without_inode_degrades_gracefully() {
        // 老缓存行没有 dev/ino：退化为 size + mtime_ns 校验，不误拒正常文件
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.jpg");
        std::fs::write(&f, b"x").unwrap();
        let mut fp = fingerprint_of(&f);
        fp.dev = None;
        fp.ino = None;

        let out = validate_paths(&[fp], &[f.to_string_lossy().to_string()]);
        assert!(matches!(out[0], Verdict::Allow(_)));
    }

    #[test]
    fn verdicts_keep_request_order_and_mixed_results() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.jpg");
        std::fs::write(&good, b"x").unwrap();
        let fps = vec![fingerprint_of(&good)];

        let reqs = vec![
            dir.path().join("unknown.jpg").to_string_lossy().to_string(),
            good.to_string_lossy().to_string(),
        ];
        let out = validate_paths(&fps, &reqs);
        assert!(matches!(out[0], Verdict::Reject(_, _)));
        assert!(matches!(out[1], Verdict::Allow(ref p) if *p == good));

        // PathBuf 比较确认拿到的是规范化后的会话路径
        let _ = PathBuf::from(&reqs[1]);
    }
}
