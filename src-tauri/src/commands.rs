//! commands：Tauri IPC 命令层。

use crate::cluster;
use crate::finger::Fingerprint;
use crate::pipeline::{self, GroupJson, PipelineProgress, ScanStats};
use crate::scanner::{self, ScanOptions};
use crate::session;
use crate::store::Store;
use crate::trashbin;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};

pub struct ActiveScan {
    pub id: u64,
    pub cancel: Arc<AtomicBool>,
}

/// 全局应用状态：指纹全集（供 regroup 复用）+ 当前扫描控制 + 路径。
pub struct AppState {
    pub fps: Mutex<Vec<Fingerprint>>,
    /// 已移入废纸篓但可撤销的文件指纹，key 是原路径。
    ///
    /// 为什么不直接扔掉：撤销时必须把指纹塞回 `fps`，照片才会重新出现在分组里；
    /// 重新计算指纹又要解码一遍图片，浪费。暂存到会话结束（或用户主动重扫）即可。
    pub stashed_fps: Mutex<HashMap<PathBuf, Fingerprint>>,
    /// 待撤销批次：batch_id → 本批已移入废纸篓的条目。
    /// 后端是撤销路径的唯一事实来源 —— 前端只持有不透明 batch_id，
    /// 伪造不了「从哪搬回哪」（docs/04 阶段 1）。
    pub undo_batches: Mutex<HashMap<u64, Vec<trashbin::TrashItem>>>,
    pub next_batch_id: AtomicU64,
    pub active_scan: Mutex<Option<ActiveScan>>,
    pub next_scan_id: AtomicU64,
    pub thumb_dir: PathBuf,
    pub db_path: PathBuf,
    pub journal_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanProgress {
    pub stage: String,
    pub done: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub groups: Vec<GroupJson>,
    pub stats: ScanStats,
}

/// `trash_files` 的返回值：逐条删除结果 + 撤销批次 ID + 删除后重新分组的快照。
#[derive(Debug, Clone, Serialize)]
pub struct TrashOutcome {
    /// 每个文件的移入结果（含废纸篓中的实际路径，仅供日志/展示；撤销靠 batch_id）
    pub items: Vec<trashbin::TrashItem>,
    /// 本次成功移入的撤销批次 ID。全部失败时为 None。
    pub batch_id: Option<u64>,
    /// 删除后重新分组的结果 —— 保留项可能已变化，组数可能已减少
    pub groups: Vec<GroupJson>,
    pub stats: ScanStats,
}

/// `restore_files` 的返回值：逐条恢复结果 + 恢复后重新分组的快照。
#[derive(Debug, Clone, Serialize)]
pub struct RestoreOutcome {
    pub items: Vec<trashbin::TrashItem>,
    pub groups: Vec<GroupJson>,
    pub stats: ScanStats,
}

#[tauri::command]
pub async fn start_scan(
    app: AppHandle,
    state: State<'_, AppState>,
    dir: String,
    include_subdirs: bool,
    level: String,
) -> Result<ScanResult, String> {
    let root = PathBuf::from(&dir);
    if !root.is_dir() {
        return Err(format!("目录不存在：{dir}"));
    }
    // 授权边界：档位是后端枚举，前端不再传数值阈值
    let Some(level) = session::parse_level(&level) else {
        return Err(format!("未知相似度档位：{level}"));
    };
    let thresholds = session::level_thresholds(level);

    // 每次扫描持有独立令牌；旧任务完全退出前不允许启动新任务，避免结果互相覆盖。
    let scan_id = state.next_scan_id.fetch_add(1, Ordering::Relaxed) + 1;
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut active = state.active_scan.lock().unwrap();
        if active.is_some() {
            return Err("已有扫描正在进行，请先取消或等待完成".into());
        }
        *active = Some(ActiveScan {
            id: scan_id,
            cancel: cancel.clone(),
        });
    }

    let thumb_dir = state.thumb_dir.clone();
    let db_path = state.db_path.clone();
    let app_handle = app.clone();
    let worker_cancel = cancel.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        // 目录遍历也放进阻塞线程，避免大目录卡住 UI。
        let scan_app = app_handle.clone();
        let outcome = scanner::scan_dir_cancellable(
            &root,
            &ScanOptions { include_subdirs },
            &worker_cancel,
            |done| {
                let _ = scan_app.emit(
                    "scan:progress",
                    ScanProgress {
                        stage: "scanning".into(),
                        done,
                        total: 0,
                    },
                );
            },
        )
        .map_err(|_| "扫描已取消".to_string())?;
        if outcome.metas.is_empty() {
            return Ok::<_, String>((
                vec![],
                vec![],
                ScanStats {
                    skipped_placeholder: outcome.skipped_placeholder,
                    walk_errors: outcome.walk_errors,
                    ..ScanStats::default()
                },
            ));
        }

        let store = Store::open(&db_path).map_err(|e| format!("缓存数据库打开失败：{e}"))?;
        let progress = move |p: PipelineProgress| {
            let _ = app_handle.emit(
                "scan:progress",
                ScanProgress {
                    stage: p.stage.into(),
                    done: p.done,
                    total: p.total,
                },
            );
        };
        pipeline::run_pipeline(
            outcome,
            &root,
            &store,
            &thumb_dir,
            thresholds,
            &progress,
            &worker_cancel,
        )
        .map_err(|e| e.to_string())
    })
    .await;

    // 无论成功、取消还是 panic，都释放当前任务；只有任务 ID 匹配时才清理。
    {
        let mut active = state.active_scan.lock().unwrap();
        if active.as_ref().map(|s| s.id) == Some(scan_id) {
            *active = None;
        }
    }

    let (fps, groups, stats) = joined.map_err(|e| format!("扫描任务失败：{e}"))??;
    *state.fps.lock().unwrap() = fps;
    // 新一轮扫描开始，上一轮的「可撤销」窗口过期。
    state.stashed_fps.lock().unwrap().clear();
    state.undo_batches.lock().unwrap().clear();
    Ok(ScanResult { groups, stats })
}

/// 请求当前扫描协作式退出；没有运行中的扫描时返回 false。
#[tauri::command]
pub fn cancel_scan(state: State<'_, AppState>) -> bool {
    let active = state.active_scan.lock().unwrap();
    if let Some(scan) = active.as_ref() {
        scan.cancel.store(true, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// 切换相似度档位后重聚类（指纹已缓存，秒级响应）。
#[tauri::command]
pub fn regroup(state: State<'_, AppState>, level: String) -> Result<ScanResult, String> {
    let Some(level) = session::parse_level(&level) else {
        return Err(format!("未知相似度档位：{level}"));
    };
    let fps = state.fps.lock().unwrap().clone();
    if fps.is_empty() {
        return Err("尚无扫描结果，请先扫描".into());
    }
    let (groups, stats) =
        pipeline::group_all(&fps, session::level_thresholds(level), &state.thumb_dir);
    Ok(ScanResult { groups, stats })
}

/// 移入系统废纸篓（绝不硬删除）。路径列表由前端确认勾选后传入。
///
/// **授权边界（docs/04 阶段 1）**：路径必须属于当前扫描会话，且文件身份
/// （dev + ino + size + mtime_ns）与扫描时一致 —— 会话外路径、扫描后被
/// 替换/修改的文件一律拒绝，拒绝原因逐条返回给用户。
///
/// 同时返回**重新分组后的结果**：删除会改变保留项（删掉建议保留的那张，
/// 另一张要顶上）、也会让只剩 1 张的组整组消失。
#[tauri::command]
pub fn trash_files(
    state: State<'_, AppState>,
    paths: Vec<String>,
    level: String,
) -> Result<TrashOutcome, String> {
    let Some(level) = session::parse_level(&level) else {
        return Err(format!("未知相似度档位：{level}"));
    };
    let thresholds = session::level_thresholds(level);

    // 双重校验：会话成员 + 文件身份。校验不通过的直接作为失败项返回。
    let verdicts = {
        let fps = state.fps.lock().unwrap();
        session::validate_paths(&fps, &paths)
    };
    let mut allowed: Vec<PathBuf> = Vec::new();
    let mut items: Vec<trashbin::TrashItem> = Vec::new();
    for verdict in verdicts {
        match verdict {
            session::Verdict::Allow(path) => allowed.push(path),
            session::Verdict::Reject(original, reason) => items.push(trashbin::TrashItem {
                original,
                trash_path: None,
                error: Some(reason),
            }),
        }
    }
    items.extend(trashbin::move_to_trash(&allowed, &state.journal_dir));

    // 只有真正移走的文件才从列表中剔除；失败的要留在原地让用户重试。
    let moved: Vec<trashbin::TrashItem> = items
        .iter()
        .filter(|i| i.trash_path.is_some())
        .cloned()
        .collect();
    let removed: HashSet<PathBuf> = moved.iter().map(|i| PathBuf::from(&i.original)).collect();

    // 撤销批次由后端签发：前端只拿不透明 batch_id，不持有可信路径。
    let batch_id = (!moved.is_empty()).then(|| {
        let id = state.next_batch_id.fetch_add(1, Ordering::Relaxed) + 1;
        state.undo_batches.lock().unwrap().insert(id, moved);
        id
    });

    // 把被删文件的指纹搬到暂存区（撤销时塞回去，避免重新解码图片）。
    {
        let mut fps = state.fps.lock().unwrap();
        let mut stash = state.stashed_fps.lock().unwrap();
        fps.retain(|fp| {
            if removed.contains(&fp.path) {
                stash.insert(fp.path.clone(), fp.clone());
                false
            } else {
                true
            }
        });
    }

    let (groups, stats) = regroup_current(&state, thresholds);
    Ok(TrashOutcome {
        items,
        batch_id,
        groups,
        stats,
    })
}

/// 从废纸篓恢复文件（应用内撤销）。
///
/// 前端只传 `batch_id` —— 撤销路径的映射保存在后端批次注册表里，
/// 伪造的批次 ID 只会得到「批次不存在」。只恢复本次会话里由本应用
/// 移入的文件，不碰用户自己删的文件。
#[tauri::command]
pub fn restore_files(
    state: State<'_, AppState>,
    batch_id: u64,
    level: String,
) -> Result<RestoreOutcome, String> {
    let Some(level) = session::parse_level(&level) else {
        return Err(format!("未知相似度档位：{level}"));
    };
    let Some(items) = state.undo_batches.lock().unwrap().remove(&batch_id) else {
        return Err("撤销批次不存在或已过期".into());
    };
    let restored = trashbin::restore_from_trash(&items);

    // 恢复成功的文件要把指纹从暂存区塞回 fps，它才会重新参与分组。
    // 部分失败时批次已消费不再回滚 ——
    // ponytail: 失败项的指纹留在暂存区、批次日志在磁盘上，重试入口靠系统废纸篓。
    {
        let mut fps = state.fps.lock().unwrap();
        let mut stash = state.stashed_fps.lock().unwrap();
        for item in &restored {
            // trash_path.is_some() && error.is_none() = 本次真的搬回原位了
            if item.trash_path.is_some() && item.error.is_none() {
                let key = PathBuf::from(&item.original);
                if let Some(fp) = stash.remove(&key) {
                    fps.push(fp);
                }
            }
        }
    }

    let (groups, stats) = regroup_current(&state, session::level_thresholds(level));
    Ok(RestoreOutcome {
        items: restored,
        groups,
        stats,
    })
}

/// 用当前 `state.fps` 直接重算分组 —— 删除/撤销后调这个，
/// 因为增删已经在调用点处理过了，这里只负责重新聚类。
fn regroup_current(
    state: &State<'_, AppState>,
    thresholds: cluster::Thresholds,
) -> (Vec<GroupJson>, ScanStats) {
    let fps = state.fps.lock().unwrap().clone();
    pipeline::group_all(&fps, thresholds, &state.thumb_dir)
}

#[cfg(test)]
mod tests {
    //! 命令层集成测试（docs/04 阶段 4）：不走 IPC 序列化层，但覆盖
    //! 「档位解析 → 会话授权 → 批次签发 → 废纸篓 → 撤销 → 重聚类」整条链路。

    use super::*;
    use tauri::Manager;

    fn fingerprint_of(path: &std::path::Path) -> Fingerprint {
        let meta = std::fs::metadata(path).unwrap();
        let (dev, ino) = crate::scanner::file_id(&meta);
        Fingerprint {
            path: path.to_path_buf(),
            size: meta.len(),
            mtime_ns: crate::scanner::mtime_ns(&meta),
            dev: Some(dev),
            ino: Some(ino),
            phash: None,
            csig: None,
            width: None,
            height: None,
            has_exif_camera: false,
            taken_at: None,
        }
    }

    fn make_app(
        fps: Vec<Fingerprint>,
    ) -> (tauri::App<tauri::test::MockRuntime>, tempfile::TempDir) {
        let app = tauri::test::mock_app();
        let dir = tempfile::tempdir().unwrap();
        app.manage(AppState {
            fps: Mutex::new(fps),
            stashed_fps: Mutex::new(HashMap::new()),
            undo_batches: Mutex::new(HashMap::new()),
            next_batch_id: AtomicU64::new(0),
            active_scan: Mutex::new(None),
            next_scan_id: AtomicU64::new(0),
            thumb_dir: dir.path().join("thumbs"),
            db_path: dir.path().join("cache.db"),
            journal_dir: dir.path().join("journal"),
        });
        (app, dir)
    }

    #[test]
    fn invalid_level_is_rejected_everywhere() {
        let (app, _dir) = make_app(vec![]);
        assert!(trash_files(app.state(), vec![], "ultra".into()).is_err());
        assert!(restore_files(app.state(), 1, "16".into()).is_err());
        // 合法档位 + 空批次：报批次不存在（档位解析已过）
        assert!(restore_files(app.state(), 1, "balanced".into()).is_err());
    }

    #[test]
    fn trash_rejects_paths_outside_session() {
        // 会话外文件（真实存在于磁盘）——越权路径必须被拦在会话校验
        let (app, dir) = make_app(vec![]);
        let outside = dir.path().join("outside.jpg");
        std::fs::write(&outside, b"x").unwrap();

        let raw = outside.to_string_lossy().to_string();
        let out = trash_files(app.state(), vec![raw], "balanced".into()).unwrap();
        assert!(out.batch_id.is_none(), "全部失败不应签发批次");
        let item = &out.items[0];
        assert!(item.trash_path.is_none());
        assert!(item
            .error
            .as_deref()
            .unwrap()
            .contains("不在本次扫描结果中"));
        assert!(outside.exists(), "会话外文件必须留在原地");
    }

    #[test]
    fn trash_and_restore_roundtrip_via_backend_batch() {
        // 端到端授权链路：会话内文件 → 真实移入系统废纸篓 →
        // 伪造批次被拒 → 后端批次撤销 → 文件回到原位、指纹回到会话。
        let (app, dir) = make_app(vec![]);
        let photos = dir.path().join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let photo = photos.join("a.png");
        std::fs::write(&photo, b"payload").unwrap();

        // 把指纹塞进会话（模拟扫描完成）
        app.state::<AppState>()
            .fps
            .lock()
            .unwrap()
            .push(fingerprint_of(&photo));
        let raw = photo.to_string_lossy().to_string();

        // 删除：真实经过 session 校验 + NSFileManager
        let out = trash_files(app.state(), vec![raw.clone()], "balanced".into()).unwrap();
        let batch_id = out.batch_id.expect("成功删除应签发批次");
        assert!(!photo.exists(), "文件应已移入系统废纸篓");
        assert!(
            app.state::<AppState>().fps.lock().unwrap().is_empty(),
            "指纹应进暂存区"
        );

        // 伪造批次 ID：撤销路径的唯一事实来源在后端
        assert!(restore_files(app.state(), batch_id + 999, "balanced".into()).is_err());

        // 撤销：真实从系统废纸篓搬回
        let back = restore_files(app.state(), batch_id, "balanced".into()).unwrap();
        assert!(
            back.items[0].error.is_none(),
            "恢复应成功：{:?}",
            back.items[0].error
        );
        assert!(photo.exists(), "文件应已回到原位");
        assert_eq!(
            app.state::<AppState>().fps.lock().unwrap().len(),
            1,
            "指纹应回到会话"
        );
        // 批次已消费
        assert!(app
            .state::<AppState>()
            .undo_batches
            .lock()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn trash_rejects_replaced_file() {
        // TOCTOU：扫描后文件被替换（inode 变了）→ 拒绝删除
        let (app, dir) = make_app(vec![]);
        let photos = dir.path().join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let photo = photos.join("a.png");
        std::fs::write(&photo, b"original").unwrap();

        {
            let state = app.state::<AppState>();
            state.fps.lock().unwrap().push(fingerprint_of(&photo));
        }
        // 模拟替换：删掉原文件写一个同路径新文件（inode/mtime 全变）
        std::fs::remove_file(&photo).unwrap();
        std::fs::write(&photo, b"replacement").unwrap();

        let raw = photo.to_string_lossy().to_string();
        let out = trash_files(app.state(), vec![raw], "balanced".into()).unwrap();
        let item = &out.items[0];
        assert!(item.trash_path.is_none(), "被替换的文件必须被拒绝");
        assert!(item.error.is_some());
        assert!(photo.exists(), "新文件必须留在原地");
    }
}
