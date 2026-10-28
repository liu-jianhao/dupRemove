//! trashbin：移入系统废纸篓 + 批次日志 + 应用内撤销。
//! 绝不硬删除——这是产品三原则之二。
//!
//! ## 为什么不用 `trash` crate
//!
//! `trash` crate 在 macOS 上走 AppleScript 让 Finder 执行删除（`delete_using_finder`），
//! 有两个问题：
//!
//! 1. **拿不到废纸篓里的真实路径**。Finder 删完只告诉你"删掉了"，不告诉你文件去哪了。
//!    而应用内撤销必须知道 `~/.Trash/xxx` 才能把文件搬回来。
//! 2. **依赖 Finder 自动化权限**，在无 GUI 会话 / 受限环境下会报
//!    "发生权限违例 (-10004)"，实测在后台线程直接失败。
//!
//! 这里改用 Foundation 的 `NSFileManager.trashItemAtURL:resultingItemURL:error:`：
//! 它的 `resultingItemURL` 出参直接给出文件在废纸篓里的最终路径，
//! 且不依赖 Finder、不需要任何自动化授权。
//!
//! ## 废纸篓路径的约定
//!
//! macOS 会自动处理同名冲突（`photo.jpg` → `photo 2.jpg`），
//! 所以**绝不能自己拼路径**，必须用 API 返回的 resultingItemURL。

use std::path::{Path, PathBuf};

/// 一次删除的结果：原路径 → 废纸篓里的实际路径。
///
/// `trash_path` 为 None 表示这一步失败了（原因见 `error`）。
///
/// 需要 `Deserialize`：撤销时前端把这份结构原样传回来，
/// 后端据此把文件搬回原位 —— 不重新扫描废纸篓，也不碰用户自己删的文件。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrashItem {
    /// 文件原来的绝对路径（撤销时搬回这里）
    pub original: String,
    /// 文件现在在废纸篓里的绝对路径（撤销时从这里搬回）
    pub trash_path: Option<String>,
    /// 失败原因（成功时为 None）
    pub error: Option<String>,
}

/// 移入系统废纸篓，逐个文件返回结果。
///
/// **不因为单个文件失败而中断整批** —— 用户勾了 10 张，1 张被占用不该让另外 9 张留在原地。
/// 调用方按 `trash_path.is_some()` 过滤出真正移走的文件即可。
///
/// 参数 `journal_dir` 用于写批次日志（失败不影响主流程）。
pub fn move_to_trash(paths: &[PathBuf], journal_dir: &Path) -> Vec<TrashItem> {
    let items: Vec<TrashItem> = paths
        .iter()
        .map(|p| {
            let original = p.to_string_lossy().to_string();
            match trash_one(p) {
                Ok(trash_path) => TrashItem {
                    original,
                    trash_path: Some(trash_path.to_string_lossy().to_string()),
                    error: None,
                },
                Err(e) => TrashItem {
                    original,
                    trash_path: None,
                    error: Some(e),
                },
            }
        })
        .collect();

    write_journal(&items, journal_dir);
    items
}

/// 从废纸篓恢复文件（应用内撤销）。
///
/// 按 `trash_path` → `original` 逐个搬回。目标路径已存在时不覆盖
/// （宁可恢复失败，也不能覆盖用户后来新建的同名文件）。
pub fn restore_from_trash(items: &[TrashItem]) -> Vec<TrashItem> {
    items
        .iter()
        .map(|item| {
            let Some(trash_path) = item.trash_path.as_deref() else {
                // 本来就没移走，无需恢复
                return item.clone();
            };
            let src = PathBuf::from(trash_path);
            let dst = PathBuf::from(&item.original);
            let err = if !src.exists() {
                Some("文件已不在废纸篓中（可能已被清空）".to_string())
            } else if dst.exists() {
                Some("原位置已有同名文件，未覆盖".to_string())
            } else if let Some(parent) = dst.parent() {
                match std::fs::create_dir_all(parent) {
                    Ok(()) => match std::fs::rename(&src, &dst) {
                        Ok(()) => None,
                        Err(e) => Some(format!("恢复失败：{e}")),
                    },
                    Err(e) => Some(format!("无法创建原目录：{e}")),
                }
            } else {
                Some("原路径无效".to_string())
            };
            TrashItem {
                original: item.original.clone(),
                trash_path: item.trash_path.clone(),
                error: err,
            }
        })
        .collect()
}

/// 写批次日志（失败不影响主流程，只记录）。
///
/// 日志格式：app_data_dir/trash_batches/<unix_secs>.json，记录本批完整路径。
fn write_journal(items: &[TrashItem], journal_dir: &Path) {
    let moved: Vec<&TrashItem> = items.iter().filter(|i| i.trash_path.is_some()).collect();
    if moved.is_empty() {
        return;
    }
    if std::fs::create_dir_all(journal_dir).is_err() {
        return;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let journal = serde_json::json!({
        "timestamp": secs,
        "count": moved.len(),
        "files": moved.iter().map(|i| serde_json::json!({
            "original": i.original,
            "trash_path": i.trash_path,
        })).collect::<Vec<_>>(),
    });
    let file = journal_dir.join(format!("batch-{secs}.json"));
    let _ = std::fs::write(
        file,
        serde_json::to_string_pretty(&journal).unwrap_or_default(),
    );
}

// ---------------------------------------------------------------- macOS 实现

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use objc2::rc::Retained;
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    /// 把单个文件移入废纸篓，返回它在废纸篓里的实际路径。
    pub fn trash_one(path: &Path) -> Result<PathBuf, String> {
        if !path.exists() {
            return Err(format!("文件不存在：{}", path.display()));
        }
        let raw = path.to_string_lossy();
        let ns_path = NSString::from_str(&raw);
        let url = NSURL::fileURLWithPath_isDirectory(&ns_path, path.is_dir());

        let file_mgr = NSFileManager::defaultManager();
        let mut resulting: Option<Retained<NSURL>> = None;
        file_mgr
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
            .map_err(|e| format!("移入废纸篓失败：{e}"))?;

        let resulting = resulting.ok_or_else(|| "系统未返回废纸篓路径".to_string())?;
        let path_str = resulting
            .path()
            .ok_or_else(|| "无法读取废纸篓路径".to_string())?;
        // NSString → String：NSString 实现了 Display，直接 to_string 即可。
        Ok(PathBuf::from(path_str.to_string()))
    }
}

/// 非 macOS 平台的占位实现（当前项目只支持 macOS）。
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn trash_one(path: &Path) -> Result<PathBuf, String> {
        if !path.exists() {
            return Err(format!("文件不存在：{}", path.display()));
        }
        Err("当前平台尚未实现移入废纸篓".to_string())
    }
}

use platform::trash_one;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_reported_not_panicked() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.jpg");
        let items = move_to_trash(&[missing], &dir.path().join("journal"));
        assert_eq!(items.len(), 1);
        assert!(items[0].trash_path.is_none());
        assert!(items[0].error.is_some());
    }

    #[test]
    fn empty_list_produces_no_items() {
        let dir = tempfile::tempdir().unwrap();
        let items = move_to_trash(&[], &dir.path().join("journal"));
        assert!(items.is_empty());
    }

    #[test]
    fn restore_of_never_trashed_item_is_a_noop() {
        let item = TrashItem {
            original: "/tmp/x.jpg".into(),
            trash_path: None,
            error: Some("失败".into()),
        };
        let out = restore_from_trash(&[item]);
        assert_eq!(out.len(), 1);
        assert!(out[0].trash_path.is_none());
    }

    #[test]
    fn restore_reports_missing_trash_file() {
        let dir = tempfile::tempdir().unwrap();
        let item = TrashItem {
            original: dir.path().join("a.jpg").to_string_lossy().to_string(),
            trash_path: Some(dir.path().join("gone.jpg").to_string_lossy().to_string()),
            error: None,
        };
        let out = restore_from_trash(&[item]);
        assert!(out[0].error.is_some());
        assert!(out[0].error.as_ref().unwrap().contains("已不在废纸篓"));
    }

    #[test]
    fn restore_does_not_overwrite_existing_destination() {
        // 原位置已有同名文件时必须拒绝恢复 —— 绝不能覆盖用户后来新建的文件。
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("trash_copy.jpg");
        let dst = dir.path().join("orig.jpg");
        std::fs::write(&src, b"from-trash").unwrap();
        std::fs::write(&dst, b"newer-file").unwrap();

        let item = TrashItem {
            original: dst.to_string_lossy().to_string(),
            trash_path: Some(src.to_string_lossy().to_string()),
            error: None,
        };
        let out = restore_from_trash(&[item]);
        assert!(out[0].error.is_some());
        assert!(out[0].error.as_ref().unwrap().contains("已有同名文件"));
        // 目标文件内容未被改动
        assert_eq!(std::fs::read(&dst).unwrap(), b"newer-file");
    }

    #[test]
    fn restore_moves_file_back() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("moved.jpg");
        let dst = dir.path().join("back.jpg");
        std::fs::write(&src, b"payload").unwrap();

        let item = TrashItem {
            original: dst.to_string_lossy().to_string(),
            trash_path: Some(src.to_string_lossy().to_string()),
            error: None,
        };
        let out = restore_from_trash(&[item]);
        assert!(out[0].error.is_none(), "恢复应成功：{:?}", out[0].error);
        assert!(dst.exists());
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
    }

    #[test]
    fn journal_records_only_successful_items() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        // 造一条成功 + 一条失败的记录
        let items = vec![
            TrashItem {
                original: "/tmp/a.jpg".into(),
                trash_path: Some("/tmp/.Trash/a.jpg".into()),
                error: None,
            },
            TrashItem {
                original: "/tmp/b.jpg".into(),
                trash_path: None,
                error: Some("失败".into()),
            },
        ];
        write_journal(&items, &journal);
        let files: Vec<_> = std::fs::read_dir(&journal)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1);
        let body = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(body.contains("/tmp/a.jpg"));
        assert!(!body.contains("/tmp/b.jpg"));
    }
}
