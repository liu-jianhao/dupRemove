//! scanner：目录遍历、格式过滤、跳过云占位文件。

use crate::control::{check_cancelled, Cancelled};
use std::ffi::OsStr;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif", "heic", "heif"];

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub path: PathBuf,
    pub size: u64,
    /// 纳秒级修改时间。秒级精度挡不住「同一秒内被替换」的文件，
    /// 删除前的身份校验依赖它（见 session.rs）。
    pub mtime_ns: u64,
    /// 设备号 / inode —— 同路径换文件的强身份信号（mtime 可被刻意保留，inode 不会）。
    pub dev: u64,
    pub ino: u64,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub include_subdirs: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include_subdirs: true,
        }
    }
}

/// 一次遍历的产物：图片元信息 + 跳过/失败统计。
#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub metas: Vec<FileMeta>,
    /// 跳过的云占位文件数（iCloud/OneDrive「优化存储」未下载）。
    /// 读取占位文件会触发系统联网下载，违背「绝不联网」——必须在打开前跳过。
    pub skipped_placeholder: usize,
    /// 遍历失败的条目数（权限不足等）。不中断扫描，但必须让用户看得见。
    pub walk_errors: usize,
}

/// 遍历目录，返回图片文件元信息列表（不需要跳过统计时的便捷入口）。
///
/// 跳过：隐藏文件与隐藏**目录**（`.DS_Store`、`._*`；隐藏目录整个不递归）、
/// 符号链接、云占位文件。
pub fn scan_dir(root: &Path, opts: &ScanOptions) -> Vec<FileMeta> {
    scan_dir_with_stats(root, opts).metas
}

/// 遍历 + 统计（占位文件跳过数）。
pub fn scan_dir_with_stats(root: &Path, opts: &ScanOptions) -> ScanOutcome {
    let cancel = AtomicBool::new(false);
    scan_dir_cancellable(root, opts, &cancel, |_| {}).expect("固定为 false 的取消令牌不会触发")
}

/// 可取消的目录遍历；`on_found` 汇报已发现的图片数（总数事前未知）。
pub fn scan_dir_cancellable<F>(
    root: &Path,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    mut on_found: F,
) -> Result<ScanOutcome, Cancelled>
where
    F: FnMut(usize),
{
    let max_depth = if opts.include_subdirs { usize::MAX } else { 1 };
    let mut out = ScanOutcome::default();
    for (visited, entry) in WalkDir::new(root)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        // 隐藏目录整目录不递归；根目录除外 —— 它是用户明确选中的，
        // 即便名字以点开头（如 `.photos`）也必须扫。
        .filter_entry(|e| e.depth() == 0 || !is_hidden_name(e.file_name()))
        .enumerate()
    {
        if visited % 64 == 0 {
            check_cancelled(cancel)?;
        }
        // 遍历失败（权限不足等）不中断扫描，但计入统计让用户看得见。
        let Some(entry) = entry.ok() else {
            out.walk_errors += 1;
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if is_hidden_name(entry.file_name()) || !has_image_ext(path) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if is_cloud_placeholder(&meta) {
            out.skipped_placeholder += 1;
            continue;
        }
        let (dev, ino) = file_id(&meta);
        out.metas.push(FileMeta {
            path: path.to_path_buf(),
            size: meta.len(),
            mtime_ns: mtime_ns(&meta),
            dev,
            ino,
        });
        if out.metas.len() % 25 == 0 {
            on_found(out.metas.len());
        }
    }
    check_cancelled(cancel)?;
    on_found(out.metas.len());
    Ok(out)
}

fn has_image_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// macOS 资源叉 `._foo.jpg`、`.DS_Store`、`.git` 这类以点开头的名字。
/// 隐藏**目录**由 `WalkDir::filter_entry` 拦下，不会递归进入。
fn is_hidden_name(name: &OsStr) -> bool {
    name.to_str().map(|n| n.starts_with('.')).unwrap_or(false)
}

/// iCloud/OneDrive「优化存储」的占位文件（APFS dataless）：
/// 逻辑大小非 0，但没有任何已分配的数据块 —— 真实字节在云端，
/// 读取会触发系统联网下载，违背「绝不联网」承诺。
///
/// ponytail: blocks==0 也会把罕见的 sparse 文件当占位跳过；
/// 照片场景没人刻意造 sparse，可接受。
#[cfg(unix)]
fn is_cloud_placeholder(meta: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() == 0
}

/// ponytail: 非 unix 平台退化为「空文件」判定，Windows 版到 v2 接 reparse point。
#[cfg(not(unix))]
fn is_cloud_placeholder(meta: &Metadata) -> bool {
    meta.len() == 0
}

/// 纳秒级 mtime（缓存键 + 删除前身份校验）。
/// ponytail: as_nanos() 是 u128，截断到 u64 在 2554 年才溢出，照片场景足够。
pub fn mtime_ns(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// (设备号, inode)：同路径换文件的强身份信号。
#[cfg(unix)]
pub fn file_id(meta: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// ponytail: Windows 版到 v2 再接（NTFS FileId），当前项目仅 macOS。
#[cfg(not(unix))]
pub fn file_id(_meta: &Metadata) -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_filters_and_depths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("a.jpg"), b"x").unwrap();
        fs::write(root.join("b.TXT"), b"x").unwrap(); // 非图片
        fs::write(root.join(".DS_Store"), b"x").unwrap(); // 隐藏
        fs::write(root.join("._a.jpg"), b"x").unwrap(); // macOS 资源文件
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("c.png"), b"x").unwrap();

        // 含子目录
        let mut all = scan_dir(
            root,
            &ScanOptions {
                include_subdirs: true,
            },
        );
        all.sort_by(|a, b| a.path.file_name().unwrap().cmp(b.path.file_name().unwrap()));
        let names: Vec<String> = all
            .iter()
            .map(|m| m.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.jpg".to_string(), "c.png".to_string()]);

        // 不含子目录
        let top = scan_dir(
            root,
            &ScanOptions {
                include_subdirs: false,
            },
        );
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].path.file_name().unwrap(), "a.jpg");

        // 空文件（iCloud 占位的表现之一）跳过
        fs::write(root.join("d.jpg"), b"").unwrap();
        let again = scan_dir(
            root,
            &ScanOptions {
                include_subdirs: false,
            },
        );
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn hidden_directories_are_not_recursed() {
        // 隐藏目录必须整个不进入 —— 只过滤隐藏「文件名」会让
        // .git/objects 之类的内部文件漏进扫描。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hidden = root.join(".photos");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("v.jpg"), b"x").unwrap();
        fs::write(root.join("a.jpg"), b"x").unwrap();

        let metas = scan_dir(root, &ScanOptions::default());
        let names: Vec<String> = metas
            .iter()
            .map(|m| m.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.jpg".to_string()]);
    }

    #[test]
    fn sparse_file_is_treated_as_cloud_placeholder() {
        // APFS dataless 占位的表现 = 逻辑大小 > 0 但 blocks == 0。
        // sparse 文件呈现完全相同的元数据形态，用它验证：占位文件
        // 永远进不了扫描列表（后续任何环节都不会去读它）。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let normal = root.join("a.jpg");
        fs::write(&normal, b"real bytes").unwrap();
        let placeholder = root.join("cloud.jpg");
        fs::File::create(&placeholder)
            .unwrap()
            .set_len(1 << 20)
            .unwrap();

        let out = scan_dir_with_stats(root, &ScanOptions::default());
        assert_eq!(out.metas.len(), 1);
        assert_eq!(out.metas[0].path, normal);
        assert_eq!(out.skipped_placeholder, 1);
    }

    #[test]
    fn scan_honours_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        let cancel = AtomicBool::new(true);
        let result = scan_dir_cancellable(dir.path(), &ScanOptions::default(), &cancel, |_| {});
        assert!(matches!(result, Err(Cancelled)));
    }

    #[test]
    fn walk_errors_are_counted_not_silent() {
        // 无读权限的子目录：walkdir 报错，计入统计而不是静默装作没看见。
        // 「扫描完成」的假象（悄悄漏扫）会直接破坏「先预览后执行」的信任。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.jpg"), b"x").unwrap();
        let locked = root.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("b.jpg"), b"x").unwrap();

        #[cfg(unix)]
        fn lock(p: &Path) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o000));
        }
        #[cfg(unix)]
        fn unlock(p: &Path) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o755));
        }
        #[cfg(not(unix))]
        fn lock(_p: &Path) {}
        #[cfg(not(unix))]
        fn unlock(_p: &Path) {}

        lock(&locked);
        let out = scan_dir_with_stats(root, &ScanOptions::default());
        unlock(&locked); // 先恢复权限，tempdir 才能清理

        assert_eq!(out.metas.len(), 1);
        assert!(
            out.walk_errors >= 1,
            "无权限目录应计入 walk_errors，实际 {}",
            out.walk_errors
        );
    }
}
