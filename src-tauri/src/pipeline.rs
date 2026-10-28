//! pipeline：组合扫描 → 缓存 → 指纹 → 分组 → 缩略图的完整管线。

use crate::cluster;
use crate::control::{check_cancelled, Cancelled};
use crate::finger::{self, Fingerprint};
use crate::keeper;
use crate::scanner::{FileMeta, ScanOutcome};
use crate::store::Store;
use crate::thumbs;
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug, Clone, Serialize)]
pub struct FileJson {
    pub path: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub thumb_path: String,
    pub is_best: bool,
    /// 与本簇保留项是同一份文件（内容逐字节一致）。
    /// UI 用它在「几乎一样」的簇里区分「内容一致」（删了零损失）与「仅相似」（删前需确认）；
    /// 「完全相同」的簇里每个成员都必然为 true，故无需逐张标注。
    pub identical: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupJson {
    /// "exact"（内容完全相同）| "similar"（几乎一样）
    pub kind: String,
    pub files: Vec<FileJson>,
}

#[derive(Debug, Clone, Copy)]
pub struct PipelineProgress {
    pub stage: &'static str,
    pub done: usize,
    pub total: usize,
}

#[derive(Debug)]
pub enum PipelineError {
    Cancelled,
    Store(rusqlite::Error),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Cancelled => write!(f, "扫描已取消"),
            PipelineError::Store(e) => write!(f, "缓存数据库写入失败：{e}"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<Cancelled> for PipelineError {
    fn from(_: Cancelled) -> Self {
        PipelineError::Cancelled
    }
}

impl From<rusqlite::Error> for PipelineError {
    fn from(e: rusqlite::Error) -> Self {
        PipelineError::Store(e)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanStats {
    pub scanned: usize,
    pub decoded: usize,
    pub skipped_decode: usize,
    /// 跳过的云占位文件数（iCloud/OneDrive 未下载 —— 读取会触发联网，绝不打开）
    pub skipped_placeholder: usize,
    /// 遍历失败的条目数（权限不足等）
    pub walk_errors: usize,
    pub cache_hits: usize,
    pub new_fingerprints: usize,
    pub exact_groups: usize,
    pub similar_groups: usize,
    /// 建议删除的文件数
    pub duplicate_files: usize,
    /// 建议删除可释放的字节数
    pub reclaimable_bytes: u64,
}

/// 扫描 + 指纹 + 分组全流程。返回 (指纹全集, 分组结果, 统计)。
/// 指纹全集供后续 regroup（切换相似度档位）复用，无需重扫。
pub fn run_pipeline(
    outcome: ScanOutcome,
    scan_root: &Path,
    store: &Store,
    thumb_dir: &Path,
    th: cluster::Thresholds,
    progress: &(dyn Fn(PipelineProgress) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<(Vec<Fingerprint>, Vec<GroupJson>, ScanStats), PipelineError> {
    fn emit(
        progress: &(dyn Fn(PipelineProgress) + Send + Sync),
        stage: &'static str,
        done: usize,
        total: usize,
    ) {
        progress(PipelineProgress { stage, done, total });
    }

    // 1. 缓存分流
    let skipped_placeholder = outcome.skipped_placeholder;
    let walk_errors = outcome.walk_errors;
    let metas = outcome.metas;
    let total_files = metas.len();
    let mut hits: Vec<Fingerprint> = Vec::new();
    let mut misses: Vec<FileMeta> = Vec::new();
    for (idx, meta) in metas.into_iter().enumerate() {
        if idx % 64 == 0 {
            check_cancelled(cancel)?;
        }
        // 防御：跳过缩略图缓存目录内的文件（避免把缓存当成照片重复扫）
        if meta.path.starts_with(thumb_dir) {
            continue;
        }
        match store.lookup(&meta.path, meta.size, meta.mtime_ns) {
            Some(fp) => hits.push(fp),
            None => misses.push(meta),
        }
        if idx % 250 == 0 || idx + 1 == total_files {
            emit(progress, "cache", idx + 1, total_files);
        }
    }
    let cache_hits = hits.len();

    // 2. 未命中并行计算指纹（rayon），每 50 张汇报进度。
    // Result 的并行 collect 会在任一 worker 返回 Cancelled 后停止接收新工作。
    let total = misses.len();
    let counter = AtomicUsize::new(0);
    let new_fps: Vec<Fingerprint> = misses
        .into_par_iter()
        .map(|meta| {
            check_cancelled(cancel)?;
            let fp = finger::fingerprint(&meta);
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(50) || n == total {
                emit(progress, "fingerprints", n, total);
            }
            check_cancelled(cancel)?;
            Ok(fp)
        })
        .collect::<Result<Vec<_>, Cancelled>>()?;
    emit(progress, "fingerprints", total, total);

    // 3. 单事务回写缓存，并只清理本次扫描目录的失效缓存。
    check_cancelled(cancel)?;
    store.upsert_all(&new_fps)?;
    check_cancelled(cancel)?;
    store.prune_missing(scan_root)?;

    // 4. 合并指纹全集
    let mut fps = hits;
    let new_count = new_fps.len();
    fps.extend(new_fps);

    let (groups, mut stats) = group_all_cancellable(&fps, th, thumb_dir, progress, cancel)?;
    stats.cache_hits = cache_hits;
    stats.new_fingerprints = new_count;
    stats.skipped_placeholder = skipped_placeholder;
    stats.walk_errors = walk_errors;

    Ok((fps, groups, stats))
}

/// 分组：统一聚类（精确等价 + 感知相似合并为同一簇），每簇保留一个最优项。
///
/// 关键设计（修正早期"精确组 / 相似组各自独立选最优"的缺陷）：
/// 一张照片的"字节副本"与"缩放版"若分属两组，两组各自选最优会删掉真正最好的那张
/// （原图落进精确组被删，缩放版落进相似组被保留——恰好反了）。
/// 统一成簇后，全簇只保留一个最优项，其余全部计入可释放。
///
/// 合并安全性：字节完全相同的文件其 pHash / 颜色签名必然相同，
/// 故 dist(A,C) == dist(B,C)，精确等价并入相似簇不会引入超出阈值的误合并。
///
/// 簇类型：全簇字节相同 → "exact"；否则 → "similar"。
/// 独立函数：regroup 切换阈值时直接复用（指纹已缓存）。
pub fn group_all(
    fps: &[Fingerprint],
    th: cluster::Thresholds,
    thumb_dir: &Path,
) -> (Vec<GroupJson>, ScanStats) {
    let cancel = AtomicBool::new(false);
    group_all_cancellable(fps, th, thumb_dir, &|_| {}, &cancel)
        .expect("固定为 false 的取消令牌不会触发")
}

/// 剔除指定路径后重新分组 —— 删除照片后用。
///
/// 为什么必须重新分组而不是前端直接删掉那一项：
/// 1. **保留项可能变化**。删掉的若是系统建议保留的那张，剩下的候选要重新评选
///    （分辨率/格式/大小/EXIF 优先级），否则会出现"这一组没有保留项"的错误状态。
/// 2. **组可能消失**。一组只剩 1 张时它不再构成"重复"，整组应该消失，
///    否则用户会看到"1 张的重复组"这种自相矛盾的东西。
/// 3. **统计要一致**。可释放字节数、建议删除文件数都依赖分组结果。
///
/// 指纹已缓存，不重新解码图片，只重算聚类 —— 耗时与切换阈值档位同级。
pub fn group_excluding(
    fps: &[Fingerprint],
    excluded: &std::collections::HashSet<PathBuf>,
    th: cluster::Thresholds,
    thumb_dir: &Path,
) -> (Vec<GroupJson>, ScanStats) {
    let kept: Vec<Fingerprint> = fps
        .iter()
        .filter(|fp| !excluded.contains(&fp.path))
        .cloned()
        .collect();
    group_all(&kept, th, thumb_dir)
}

pub fn group_all_cancellable(
    fps: &[Fingerprint],
    th: cluster::Thresholds,
    thumb_dir: &Path,
    progress: &(dyn Fn(PipelineProgress) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<(Vec<GroupJson>, ScanStats), PipelineError> {
    let mut stats = ScanStats {
        scanned: fps.len(),
        ..ScanStats::default()
    };
    for fp in fps {
        if fp.phash.is_some() {
            stats.decoded += 1;
        } else {
            stats.skipped_decode += 1;
        }
    }

    // ---- 1. 精确哈希：只对同 size 的候选计算 blake3（大小不同必不是同一文件）----
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, fp) in fps.iter().enumerate() {
        by_size.entry(fp.size).or_default().push(i);
    }
    let candidates: Vec<usize> = by_size
        .values()
        .filter(|v| v.len() >= 2)
        .flatten()
        .copied()
        .collect();
    let hash_total = candidates.len();
    let hash_done = AtomicUsize::new(0);
    let computed: Vec<(usize, [u8; 32])> = candidates
        .par_iter()
        .map(|&i| {
            check_cancelled(cancel)?;
            let result = finger::file_blake3_cancellable(&fps[i].path, cancel)?.map(|h| (i, h));
            let done = hash_done.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(50) || done == hash_total {
                progress(PipelineProgress {
                    stage: "exact_hash",
                    done,
                    total: hash_total,
                });
            }
            check_cancelled(cancel)?;
            Ok(result)
        })
        .collect::<Result<Vec<Option<_>>, Cancelled>>()?
        .into_iter()
        .flatten()
        .collect();
    progress(PipelineProgress {
        stage: "exact_hash",
        done: hash_total,
        total: hash_total,
    });
    let mut hash_of: HashMap<usize, [u8; 32]> = HashMap::new();
    for (i, h) in computed {
        hash_of.insert(i, h);
    }

    // ---- 2. 相似聚类：先折叠精确等价类，代表参与 O(n²) 感知比对 ----
    // 感知相似是双闸门（结构 pHash + 颜色签名），两路信号都必须齐全才能参与聚类。
    //
    // 折叠（docs/04 阶段 3）：字节完全相同的副本必然逐对相似，让全部副本
    // 参与两两比对会把 C(n,²) 的对数（数万张相同照片 → 数十亿对）烧在
    // 重复判定上。同 (size, blake3) 的文件只派代表比对，聚类完成后副本
    // 展开回代表所在簇。合并安全性：字节相同 → 感知指纹相同 →
    // dist(副本, X) == dist(代表, X)，展开不会引入超出阈值的误合并。
    let decodable: Vec<usize> = (0..fps.len())
        .filter(|&i| fps[i].phash.is_some() && fps[i].csig.is_some())
        .collect();
    let mut rep_by_hash: HashMap<(u64, [u8; 32]), usize> = HashMap::new();
    let mut members_of: HashMap<usize, Vec<usize>> = HashMap::new(); // 代表 → 等价类成员
    let mut reps: Vec<usize> = Vec::with_capacity(decodable.len());
    for &i in &decodable {
        let rep = match hash_of.get(&i).map(|h| (fps[i].size, *h)) {
            Some(key) => match rep_by_hash.get(&key) {
                Some(&r) => r,
                None => {
                    rep_by_hash.insert(key, i);
                    i
                }
            },
            // 唯一大小（无精确哈希）或哈希失败的文件自成一类
            None => i,
        };
        if rep == i {
            reps.push(i);
        }
        members_of.entry(rep).or_default().push(i);
    }
    let phashes: Vec<u64> = reps.iter().map(|&i| fps[i].phash.unwrap()).collect();
    let csigs: Vec<[u8; finger::COLOR_SIG_LEN]> =
        reps.iter().map(|&i| fps[i].csig.unwrap()).collect();
    let mut clusters: Vec<Vec<usize>> = cluster::cluster_similar_cancellable(
        &phashes,
        &csigs,
        th,
        cancel,
        &|stage, done, total| progress(PipelineProgress { stage, done, total }),
    )?
    .into_iter()
    // 展开：代表的簇 + 该等价类其余成员（成员的感知指纹与代表一致）
    .map(|g| {
        g.into_iter()
            .flat_map(|k| members_of[&reps[k]].clone())
            .collect()
    })
    .collect();

    // ---- 3. 不可解码文件的精确组独立成簇（HEIC / 损坏文件只能做精确去重）----
    let mut assigned = vec![false; fps.len()];
    for c in &clusters {
        for &i in c {
            assigned[i] = true;
        }
    }
    let mut by_hash: HashMap<(u64, [u8; 32]), Vec<usize>> = HashMap::new();
    for (&i, &h) in hash_of.iter() {
        by_hash.entry((fps[i].size, h)).or_default().push(i);
    }
    let mut extras: Vec<Vec<usize>> = Vec::new();
    for idxs in by_hash.values() {
        let unassigned: Vec<usize> = idxs.iter().copied().filter(|&i| !assigned[i]).collect();
        if unassigned.len() >= 2 {
            for &i in &unassigned {
                assigned[i] = true;
            }
            extras.push(unassigned);
        }
    }
    clusters.extend(extras);

    // ---- 4. 簇类型判定 ----
    let kinds: Vec<String> = clusters
        .iter()
        .map(|c| {
            let same_size = c.iter().all(|&i| fps[i].size == fps[c[0]].size);
            let all_same_hash = same_size
                && c.iter()
                    .all(|&i| hash_of.contains_key(&i) && hash_of.get(&i) == hash_of.get(&c[0]));
            if all_same_hash {
                "exact".to_string()
            } else {
                "similar".to_string()
            }
        })
        .collect();
    stats.exact_groups = kinds.iter().filter(|k| k.as_str() == "exact").count();
    stats.similar_groups = kinds.iter().filter(|k| k.as_str() == "similar").count();

    check_cancelled(cancel)?;
    let groups = build_group_json(
        fps, &clusters, &kinds, &hash_of, thumb_dir, &mut stats, progress, cancel,
    )?;
    let _ = thumbs::prune_cache(thumb_dir);
    Ok((groups, stats))
}

/// 缩略图生成进度与缩略图目录的传递都是阶段 3 的产物，参数确实多；
/// 收敛成一个 struct 只是搬家，等真正再加第 9 个参数时再收（ponytail）。
#[allow(clippy::too_many_arguments)]
fn build_group_json(
    fps: &[Fingerprint],
    clusters: &[Vec<usize>],
    kinds: &[String],
    hash_of: &HashMap<usize, [u8; 32]>,
    thumb_dir: &Path,
    stats: &mut ScanStats,
    progress: &(dyn Fn(PipelineProgress) + Send + Sync),
    cancel: &AtomicBool,
) -> Result<Vec<GroupJson>, Cancelled> {
    // 缩略图并行生成（幂等缓存：同 path+mtime 只生成一次）
    let all_files: Vec<(usize, &Fingerprint)> = clusters
        .iter()
        .flat_map(|idxs| idxs.iter().map(|&i| (i, &fps[i])))
        .collect();
    let thumb_total = all_files.len();
    let thumb_done = AtomicUsize::new(0);
    let thumb_map: HashMap<usize, String> = all_files
        .par_iter()
        .map(|(i, fp)| {
            check_cancelled(cancel)?;
            let thumb = thumbs::ensure_thumb(&fp.path, fp.mtime_ns, thumb_dir)
                .map(|t| (*i, t.to_string_lossy().to_string()));
            let done = thumb_done.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(20) || done == thumb_total {
                progress(PipelineProgress {
                    stage: "thumbnails",
                    done,
                    total: thumb_total,
                });
            }
            check_cancelled(cancel)?;
            Ok(thumb)
        })
        .collect::<Result<Vec<Option<_>>, Cancelled>>()?
        .into_iter()
        .flatten()
        .collect();
    progress(PipelineProgress {
        stage: "thumbnails",
        done: thumb_total,
        total: thumb_total,
    });

    let mut out: Vec<GroupJson> = Vec::with_capacity(clusters.len());
    for (ci, idxs) in clusters.iter().enumerate() {
        let ranked = keeper::rank_group(fps, idxs);
        let keeper_idx = ranked[0];
        let keeper_hash = hash_of.get(&keeper_idx).copied();
        let mut files = Vec::with_capacity(ranked.len());
        for (rank, &i) in ranked.iter().enumerate() {
            let fp = &fps[i];
            if rank > 0 {
                stats.duplicate_files += 1;
                stats.reclaimable_bytes += fp.size;
            }
            // 与保留项是同一份文件（内容逐字节一致）。
            // 在「几乎一样」的簇里，UI 用它区分真副本与仅相似的不同版本；
            // 在「完全相同」的簇里该字段恒为 true，无需逐张标注。
            let identical = keeper_hash.is_some()
                && fp.size == fps[keeper_idx].size
                && hash_of.get(&i).copied() == keeper_hash;
            files.push(FileJson {
                path: fp.path.to_string_lossy().to_string(),
                size: fp.size,
                width: fp.width.unwrap_or(0),
                height: fp.height.unwrap_or(0),
                thumb_path: thumb_map.get(&i).cloned().unwrap_or_default(),
                is_best: rank == 0,
                identical,
            });
        }
        out.push(GroupJson {
            kind: kinds[ci].clone(),
            files,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finger::Fingerprint;
    use image::{Rgb, RgbImage};

    /// 默认测试阈值 = 均衡档（与前端 THRESHOLDS.balanced 一致）
    const TH: cluster::Thresholds = cluster::THRESHOLDS[1];

    fn run_for_test(
        metas: Vec<FileMeta>,
        root: &Path,
        store: &Store,
        thumb_dir: &Path,
    ) -> (Vec<Fingerprint>, Vec<GroupJson>, ScanStats) {
        let cancel = AtomicBool::new(false);
        run_pipeline(
            ScanOutcome {
                metas,
                ..ScanOutcome::default()
            },
            root,
            store,
            thumb_dir,
            TH,
            &|_| {},
            &cancel,
        )
        .unwrap()
    }

    /// 真实感测试素材：明暗交替的粗色块。
    ///
    /// 不能用纯线性渐变：渐变的 DCT 低频系数几乎全为 0，中位数阈值会把
    /// 数值噪声放大成完全不同的哈希，缩放后无法稳定复现。
    /// 交替色块的 pHash 反映画面粗结构，缩放/重压缩后依然稳定（与真实照片一致）。
    ///
    /// `axis = false` → 竖向色块（组 A）；`axis = true` → 横向色块（组 B）。
    fn scene_pixel(x: u32, y: u32, w: u32, h: u32, axis: bool) -> u8 {
        let band = if axis {
            // 横向色块：值只随 y 变化
            (y * 3 / h.max(1)) % 2
        } else {
            // 竖向色块：值只随 x 变化
            (x * 3 / w.max(1)) % 2
        };
        if band == 0 {
            210
        } else {
            55
        }
    }

    fn write_scene(path: &std::path::Path, w: u32, h: u32, axis: bool) {
        let mut img = RgbImage::new(w, h);
        for x in 0..w {
            for y in 0..h {
                let v = scene_pixel(x, y, w, h, axis);
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        image::DynamicImage::ImageRgb8(img).save(path).unwrap();
    }

    /// 给场景图加配色：保持灰度结构不变（各通道按同比例缩放），
    /// 只改颜色。两张同结构、不同配色的图正是灰度哈希的盲区。
    fn write_tinted_scene(path: &std::path::Path, w: u32, h: u32, tint: [u8; 3]) {
        let mut img = RgbImage::new(w, h);
        for x in 0..w {
            for y in 0..h {
                let v = scene_pixel(x, y, w, h, false);
                img.put_pixel(
                    x,
                    y,
                    Rgb([
                        (v as u32 * tint[0] as u32 / 255) as u8,
                        (v as u32 * tint[1] as u32 / 255) as u8,
                        (v as u32 * tint[2] as u32 / 255) as u8,
                    ]),
                );
            }
        }
        image::DynamicImage::ImageRgb8(img).save(path).unwrap();
    }

    /// 真实感素材存成 JPEG（quality 决定压缩强度）
    fn write_scene_jpeg(path: &std::path::Path, w: u32, h: u32, quality: u8, axis: bool) {
        let mut img = RgbImage::new(w, h);
        for x in 0..w {
            for y in 0..h {
                let v = scene_pixel(x, y, w, h, axis);
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        let file = std::fs::File::create(path).unwrap();
        let writer = std::io::BufWriter::new(file);
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(writer, quality);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(encoder)
            .unwrap();
    }

    /// 噪声图：与场景图 dHash 差异极大（对照组）
    fn write_noise(path: &std::path::Path, w: u32, h: u32) {
        let mut img = RgbImage::new(w, h);
        let mut s = 0x1234_5678_9ABC_DEF0u64;
        for x in 0..w {
            for y in 0..h {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let v = ((s >> 33) & 0xFF) as u8;
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        image::DynamicImage::ImageRgb8(img).save(path).unwrap();
    }

    #[test]
    fn end_to_end_groups_exact_and_similar() {
        let dir = tempfile::tempdir().unwrap();
        // 扫描目录与缩略图缓存目录分离（真实产品中缓存位于 AppData 目录）
        let root = dir.path().join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let thumb_dir = dir.path().join("thumbs");

        // 精确组：a.png + 字节完全相同的拷贝（竖向色块）
        write_scene(&root.join("a.png"), 400, 300, false);
        std::fs::copy(root.join("a.png"), root.join("a_copy.png")).unwrap();
        // 相似组：b.png + 同画面的低质量 JPEG（横向色块，dHash 与 A 组差异大）
        write_scene(&root.join("b.png"), 380, 280, true);
        write_scene_jpeg(&root.join("b_jpg.jpg"), 380, 280, 70, true);
        // 无关图片
        write_noise(&root.join("other.png"), 640, 480);

        let store = Store::open_in_memory().unwrap();

        let metas = crate::scanner::scan_dir(&root, &crate::scanner::ScanOptions::default());
        assert_eq!(metas.len(), 5);

        let (fps, groups, stats) = run_for_test(metas, &root, &store, &thumb_dir);

        // 精确组：a.png + a_copy.png
        assert_eq!(stats.exact_groups, 1);
        // 相似组：b.png + b_jpg.jpg（精确组成员不重复计入相似聚类）
        assert_eq!(stats.similar_groups, 1);
        assert_eq!(groups.len(), 2);

        let exact = groups.iter().find(|g| g.kind == "exact").unwrap();
        assert_eq!(exact.files.len(), 2);
        assert!(exact.files.iter().any(|f| f.path.ends_with("a.png")));
        assert!(exact.files.iter().any(|f| f.path.ends_with("a_copy.png")));

        let similar = groups.iter().find(|g| g.kind == "similar").unwrap();
        assert_eq!(similar.files.len(), 2);
        assert!(similar.files.iter().any(|f| f.path.ends_with("b.png")));
        assert!(similar.files.iter().any(|f| f.path.ends_with("b_jpg.jpg")));

        // 每组最优项保留，其余计入可释放
        assert_eq!(stats.duplicate_files, 2);
        assert!(stats.reclaimable_bytes > 0);

        // 缩略图已生成
        for g in &groups {
            for f in &g.files {
                assert!(!f.thumb_path.is_empty(), "缺少缩略图: {}", f.path);
                assert!(std::path::Path::new(&f.thumb_path).exists());
            }
        }

        // 第二次扫描走缓存，命中数 = 4，且分组结果一致
        let metas2 = crate::scanner::scan_dir(&root, &crate::scanner::ScanOptions::default());
        let (_fps2, groups2, stats2) = run_for_test(metas2, &root, &store, &thumb_dir);
        assert_eq!(stats2.cache_hits, 5);
        assert_eq!(stats2.new_fingerprints, 0);
        assert_eq!(groups2.len(), groups.len());

        // 重新聚类（切换阈值）复用指纹
        let (groups3, _) = group_all(&fps, cluster::THRESHOLDS[0], &thumb_dir);
        assert!(groups3.len() <= groups.len());
    }

    #[test]
    fn keeper_stays_within_unified_cluster_across_exact_and_similar() {
        // 回归测试：早期实现把"精确组"和"相似组"分开选 keeper，
        // 导致同一张图的字节副本与缩放版分属两组，最优的那张反而被标记删除。
        // 统一聚类后必须同簇，且保留项为分辨率最高/文件最大者。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // orig = 400x300 原图；copy = 字节完全相同；small = 缩放版；recomp = 重压缩版
        write_scene(&root.join("orig.png"), 400, 300, false);
        std::fs::copy(root.join("orig.png"), root.join("copy.png")).unwrap();
        write_scene(&root.join("small.png"), 200, 150, false);
        write_scene_jpeg(&root.join("recomp.jpg"), 400, 300, 60, false);

        let store = Store::open_in_memory().unwrap();
        let metas = crate::scanner::scan_dir(root, &crate::scanner::ScanOptions::default());
        assert_eq!(metas.len(), 4);

        let (_, groups, stats) = run_for_test(metas, root, &store, &dir.path().join("thumbs"));

        // 四个文件必须落在同一簇
        assert_eq!(groups.len(), 1, "同一张图的不同版本应合并为一簇");
        let g = &groups[0];
        assert_eq!(g.files.len(), 4);

        // 保留项必须是 400x300 的原图类（分辨率最高），而非缩放版
        let best = g.files.iter().find(|f| f.is_best).unwrap();
        assert_eq!(
            (best.width, best.height),
            (400, 300),
            "保留项应为分辨率最高的 400x300 版本，实际为 {}x{}",
            best.width,
            best.height
        );
        assert!(
            best.path.ends_with("orig.png") || best.path.ends_with("copy.png"),
            "保留项应是原始 PNG，实际为 {}",
            best.path
        );

        // 其余 3 个全部计入可释放
        assert_eq!(stats.duplicate_files, 3);
        // 组内既有字节相同（copy）也有相似版本（small/recomp）→ 整簇标记为 similar
        assert_eq!(g.kind, "similar");
        // copy.png 与 orig.png 字节一致 → identical 标记为真
        let copy = g
            .files
            .iter()
            .find(|f| f.path.ends_with("copy.png"))
            .unwrap();
        assert!(copy.identical, "字节相同的成员应标记 identical");
    }

    #[test]
    fn undecodable_files_are_skipped_not_crashed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 伪图片：扩展名对但内容不是图片
        std::fs::write(root.join("broken.png"), b"not an image").unwrap();
        write_scene(&root.join("good.png"), 100, 80, false);

        let store = Store::open_in_memory().unwrap();
        let metas = crate::scanner::scan_dir(root, &crate::scanner::ScanOptions::default());
        let (fps, groups, stats) = run_for_test(metas, root, &store, &dir.path().join("thumbs"));

        assert_eq!(fps.len(), 2);
        assert_eq!(stats.skipped_decode, 1);
        assert_eq!(stats.decoded, 1);
        assert!(groups.is_empty());

        // 损坏文件的 phash 为 None
        let broken = fps.iter().find(|f| f.path.ends_with("broken.png")).unwrap();
        assert!(broken.phash.is_none());
    }

    #[test]
    fn pipeline_honours_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_scene(&root.join("a.png"), 100, 80, false);
        let metas = crate::scanner::scan_dir(root, &crate::scanner::ScanOptions::default());
        let store = Store::open_in_memory().unwrap();
        let cancel = AtomicBool::new(true);
        let result = run_pipeline(
            ScanOutcome {
                metas,
                ..ScanOutcome::default()
            },
            root,
            &store,
            &root.join("thumbs"),
            TH,
            &|_| {},
            &cancel,
        );
        assert!(matches!(result, Err(PipelineError::Cancelled)));
    }

    #[test]
    fn keeper_marks_best_file_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_scene(&root.join("small.png"), 100, 100, false);
        write_scene(&root.join("large.png"), 900, 700, true);
        std::fs::copy(root.join("small.png"), root.join("small2.png")).unwrap();

        let store = Store::open_in_memory().unwrap();
        let metas = crate::scanner::scan_dir(root, &crate::scanner::ScanOptions::default());
        let (_, groups, _) = run_for_test(metas, root, &store, &dir.path().join("thumbs"));

        // small / small2 完全相同 → 精确组；large 是另一套画面（横 vs 竖色块），不在同一组
        assert!(!groups.is_empty());
        let g = &groups[0];
        assert!(g.files[0].is_best);
        assert!(!g.files[1].is_best);
    }

    #[test]
    fn many_identical_copies_collapse_into_one_cluster() {
        // 8 份字节相同副本 + 1 份缩放版 → 必须同一簇（折叠路径回归，docs/04 阶段 3）。
        // 折叠只让等价类的「代表」参与 O(n²) 感知比对，展开后所有成员回到代表的簇：
        // 大量相同照片不再制造平方级对数。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_scene(&root.join("orig.png"), 400, 300, false);
        for i in 0..8 {
            std::fs::copy(root.join("orig.png"), root.join(format!("copy{i}.png"))).unwrap();
        }
        write_scene(&root.join("small.png"), 200, 150, false);

        let store = Store::open_in_memory().unwrap();
        let metas = crate::scanner::scan_dir(root, &crate::scanner::ScanOptions::default());
        assert_eq!(metas.len(), 10);
        let (_, groups, stats) = run_for_test(metas, root, &store, &dir.path().join("thumbs"));

        assert_eq!(
            groups.len(),
            1,
            "10 个同一张图的版本应合并为一簇: {:?}",
            groups
                .iter()
                .map(|g| g.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
        assert_eq!(groups[0].files.len(), 10);
        assert_eq!(stats.duplicate_files, 9);
    }

    /// 回归测试：同构图、不同配色，绝不能被归进同一组。
    ///
    /// 早期实现只用灰度 dHash，两张图的灰度结构一致 → 哈希完全相同（距离 0），
    /// 于是「粉色 20」和「蓝色 31」被当成几乎一样。现在靠颜色闸门拦下。
    #[test]
    fn same_layout_different_color_is_not_similar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 两个配色的亮度几乎相同（98.6 vs 100.3）→ 结构哈希看不出区别
        write_tinted_scene(&root.join("pink.png"), 300, 200, [233, 30, 99]);
        write_tinted_scene(&root.join("blue.png"), 300, 200, [30, 110, 235]);

        let store = Store::open_in_memory().unwrap();
        let metas = crate::scanner::scan_dir(root, &crate::scanner::ScanOptions::default());
        let (fps, groups, _) = run_for_test(metas, root, &store, &dir.path().join("thumbs"));

        // 前置校验：结构闸门确实放行（否则这个测试就不是在测颜色闸门）
        let a = fps.iter().find(|f| f.path.ends_with("pink.png")).unwrap();
        let b = fps.iter().find(|f| f.path.ends_with("blue.png")).unwrap();
        let struct_dist = finger::hamming(a.phash.unwrap(), b.phash.unwrap());
        assert!(
            struct_dist <= cluster::THRESHOLDS[2].phash,
            "前提不成立：结构距离 {struct_dist} 已超过宽松阈值，颜色闸门就无从谈起"
        );
        let color_dist = finger::color_dist(a.csig.as_ref().unwrap(), b.csig.as_ref().unwrap());
        assert!(
            color_dist > cluster::THRESHOLDS[2].color,
            "前提不成立：颜色距离 {color_dist} 未超过最宽松阈值"
        );

        // 但最终不能归为一组
        assert!(
            groups.is_empty(),
            "同构图不同配色的两张图不应被归为「几乎一样」，实际分组: {:?}",
            groups
                .iter()
                .map(|g| g.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
    }

    /// 回归测试：同配色、不同画面，同样不能归为一组（结构闸门负责）。
    #[test]
    fn same_color_different_structure_is_not_similar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tinted_scene(&root.join("a.png"), 300, 200, [233, 30, 99]);
        // 横向色块 + 同样配色 → 灰度结构不同
        let mut img = RgbImage::new(300, 200);
        for x in 0..300u32 {
            for y in 0..200u32 {
                let v = scene_pixel(x, y, 300, 200, true);
                img.put_pixel(
                    x,
                    y,
                    Rgb([
                        (v as u32 * 233 / 255) as u8,
                        (v as u32 * 30 / 255) as u8,
                        (v as u32 * 99 / 255) as u8,
                    ]),
                );
            }
        }
        image::DynamicImage::ImageRgb8(img)
            .save(root.join("b.png"))
            .unwrap();

        let store = Store::open_in_memory().unwrap();
        let metas = crate::scanner::scan_dir(root, &crate::scanner::ScanOptions::default());
        let (_fps, groups, _) = run_for_test(metas, root, &store, &dir.path().join("thumbs"));
        assert!(groups.is_empty(), "不同画面的两张图不应归为一组");
    }
}
