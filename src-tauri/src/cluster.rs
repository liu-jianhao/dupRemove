//! cluster：相似照片聚类。
//!
//! 策略（见 docs/02-评审与MVP范围.md 问题 1/2）：
//! 1. 暴力汉明比对（1–10 万张规模 <1s 并行，零漏报，替代会漏报的 LSH 分桶）
//! 2. 并查集单链接聚成初始组
//! 3. 组内 complete-link 一致性校验，拆掉链式合并产生的"不像"子组
//!
//! 相似性是**双闸门**：结构哈希（pHash）汉明距离 **且** 颜色签名 L1 距离
//! 都要在阈值内。只看结构会漏掉「同构图不同配色」，只看颜色会漏掉
//! 「同配色不同内容」，两头都得防 —— 误判会让用户误删照片，代价太高。

use crate::control::{check_cancelled, Cancelled};
use crate::finger::{color_dist, hamming, COLOR_SIG_LEN};
use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 双闸门阈值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// 结构哈希（pHash）允许的最大汉明距离
    pub phash: u32,
    /// 颜色签名允许的最大平均绝对差（每通道 0~255）
    pub color: u32,
}

/// 三档相似度阈值，与前端 `types.ts` 的 THRESHOLDS 一一对应。
///
/// 数值来自真实样本实测（19 张真实照片：系统壁纸的同构图配色变体、
/// 真实微信表情贴纸、界面截图，加 8 种变换变体，共 3500+ 对）。
///
/// | 类别 | 结构距离 | 颜色距离 |
/// |---|---|---|
/// | 真重复：缩放 / 变暗 / 变亮 / 平移 / JPEG 重压 | 0~3 | 0~5 |
/// | 真重复：不同文件但内容相同（界面截图） | 15 | 9 |
/// | 真重复：抠图小图平移 8px | 20 | 18~25 |
/// | 同构图换配色（系统壁纸 4 种色） | 3~14 | 13~61 |
/// | 同构图换配色（微信表情贴纸） | 7 | 85 |
/// | 完全不同的内容 | 30 | 89 |
///
/// 颜色阈值必须 < 13（换配色的下界），同时 ≥ 9（真重复的上界）——
/// 窗口很窄，所以**结构阈值要放宽到 16**，让「结构相近但内容其实不同」
/// 的情况由颜色通道兜住。反过来，颜色通道对「同构图换色」和「变暗」
/// 的区分靠的是亮度归一化，不能省。
///
/// 注意：抠图小图（PNG + 透明背景）在平移超过几像素后，颜色距离会涨到 25 左右，
/// 默认档会漏检。这是已知取舍 —— 宁可漏检，也不误删用户照片；
/// 需要时可切到「宽松」档。
pub const THRESHOLDS: [Thresholds; 3] = [
    // 严格
    Thresholds {
        phash: 12,
        color: 8,
    },
    // 均衡（默认）
    Thresholds {
        phash: 16,
        color: 12,
    },
    // 宽松
    Thresholds {
        phash: 22,
        color: 25,
    },
];

/// 一对指纹是否「几乎一样」：两个闸门都要过。
#[inline]
fn is_similar(
    phash_a: u64,
    csig_a: &[u8; COLOR_SIG_LEN],
    phash_b: u64,
    csig_b: &[u8; COLOR_SIG_LEN],
    th: &Thresholds,
) -> bool {
    hamming(phash_a, phash_b) <= th.phash && color_dist(csig_a, csig_b) <= th.color
}

/// 汉明距离 ≤ threshold 的点对，两两暴力比对（位打包 popcount，SIMD 友好）。
/// 返回结果按 (i < j) 约定。
pub fn similar_pairs(
    phashes: &[u64],
    csigs: &[[u8; COLOR_SIG_LEN]],
    th: Thresholds,
) -> Vec<(usize, usize)> {
    let cancel = AtomicBool::new(false);
    similar_pairs_cancellable(phashes, csigs, th, &cancel, &|_, _| {})
        .expect("固定为 false 的取消令牌不会触发")
}

pub fn similar_pairs_cancellable(
    phashes: &[u64],
    csigs: &[[u8; COLOR_SIG_LEN]],
    th: Thresholds,
    cancel: &AtomicBool,
    progress: &(dyn Fn(usize, usize) + Send + Sync),
) -> Result<Vec<(usize, usize)>, Cancelled> {
    let n = phashes.len();
    let total = n.saturating_mul(n.saturating_sub(1)) / 2;
    let compared = AtomicUsize::new(0);
    // 按块切分并行（只复制 8 字节的哈希，颜色签名留在原数组里按需取）
    let chunk = 1024.max(n / rayon::current_num_threads().max(1));
    let chunks: Vec<(usize, Vec<u64>)> = phashes
        .chunks(chunk)
        .enumerate()
        .map(|(ci, c)| (ci * chunk, c.to_vec()))
        .collect();

    let results: Result<Vec<Vec<(usize, usize)>>, Cancelled> = chunks
        .into_par_iter()
        .map(|(offset, block)| {
            let mut pairs = Vec::new();
            for (bi, &a) in block.iter().enumerate() {
                if bi % 32 == 0 {
                    check_cancelled(cancel)?;
                }
                let i = offset + bi;
                for j in (i + 1)..n {
                    // is_similar 内部先算 8 字节汉明距离快速筛掉绝大多数点对，
                    // 只有通过结构闸门的候选才去算颜色距离（48 字节比较）。
                    if is_similar(a, &csigs[i], phashes[j], &csigs[j], &th) {
                        pairs.push((i, j));
                    }
                }
                let done = compared.fetch_add(n - i - 1, Ordering::Relaxed) + n - i - 1;
                if bi % 16 == 0 || done == total {
                    progress(done.min(total), total);
                }
            }
            Ok(pairs)
        })
        .collect();

    let mut all: Vec<(usize, usize)> = results?.into_iter().flatten().collect();
    check_cancelled(cancel)?;
    all.sort_unstable();
    progress(total, total);
    Ok(all)
}

/// 并查集：返回每个点的根（组 id）。
pub fn union_find(n: usize, pairs: &[(usize, usize)]) -> Vec<usize> {
    let cancel = AtomicBool::new(false);
    union_find_cancellable(n, pairs, &cancel).expect("固定为 false 的取消令牌不会触发")
}

fn union_find_cancellable(
    n: usize,
    pairs: &[(usize, usize)],
    cancel: &AtomicBool,
) -> Result<Vec<usize>, Cancelled> {
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], x: usize) -> usize {
        let mut root = x;
        while parent[root] != root {
            root = parent[root];
        }
        // 路径压缩
        let mut cur = x;
        while parent[cur] != root {
            let next = parent[cur];
            parent[cur] = root;
            cur = next;
        }
        root
    }

    for (idx, &(i, j)) in pairs.iter().enumerate() {
        if idx % 4096 == 0 {
            check_cancelled(cancel)?;
        }
        let ri = find(&mut parent, i);
        let rj = find(&mut parent, j);
        if ri != rj {
            parent[rj] = ri;
        }
    }

    // 最终收敛：每个点直接指向根
    for x in 0..n {
        if x % 4096 == 0 {
            check_cancelled(cancel)?;
        }
        let root = find(&mut parent, x);
        parent[x] = root;
    }
    Ok(parent)
}

/// 主聚类入口：hashes → 相似组（每组 ≥2 个成员，成员为原数组下标）。
pub fn cluster_similar(
    phashes: &[u64],
    csigs: &[[u8; COLOR_SIG_LEN]],
    th: Thresholds,
) -> Vec<Vec<usize>> {
    let cancel = AtomicBool::new(false);
    cluster_similar_cancellable(phashes, csigs, th, &cancel, &|_, _, _| {})
        .expect("固定为 false 的取消令牌不会触发")
}

/// `progress(stage, done, total)`：stage 为 clustering 或 splitting。
pub fn cluster_similar_cancellable(
    phashes: &[u64],
    csigs: &[[u8; COLOR_SIG_LEN]],
    th: Thresholds,
    cancel: &AtomicBool,
    progress: &(dyn Fn(&'static str, usize, usize) + Send + Sync),
) -> Result<Vec<Vec<usize>>, Cancelled> {
    if phashes.len() < 2 {
        return Ok(vec![]);
    }
    let pairs = similar_pairs_cancellable(phashes, csigs, th, cancel, &|done, total| {
        progress("clustering", done, total)
    })?;
    if pairs.is_empty() {
        return Ok(vec![]);
    }
    check_cancelled(cancel)?;
    let parent = union_find_cancellable(phashes.len(), &pairs, cancel)?;

    // 按根分组
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut root_index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, &root) in parent.iter().enumerate() {
        match root_index.get(&root) {
            Some(&gi) => groups[gi].push(i),
            None => {
                root_index.insert(root, groups.len());
                groups.push(vec![i]);
            }
        }
    }

    // complete-link 拆分（防链式合并：A≈B、B≈C 但 A、C完全不像）
    let group_count = groups.len();
    let mut out = Vec::new();
    for (idx, group) in groups.into_iter().enumerate() {
        check_cancelled(cancel)?;
        progress("splitting", idx, group_count);
        out.extend(
            complete_link_split_cancellable(&group, phashes, csigs, th, cancel)?
                .into_iter()
                .filter(|g| g.len() >= 2),
        );
    }
    progress("splitting", group_count, group_count);
    Ok(out)
}

/// complete-link 拆分的簇大小上限。
///
/// ponytail: 拆分方阵是 O(n²) 内存（bool 矩阵 + 全部 pair），>256 张的巨簇
/// 现实中只有批量导出连拍/视频帧才会出现，不值得爆内存去精拆——
/// 超限时保留并查集原簇（接受链式误聚风险，用户在 UI 上可自查大组）。
/// 折叠精确等价类后（见 pipeline），巨簇里已不含字节副本，只剩真连拍。
const MAX_COMPLETE_LINK: usize = 256;

/// 组内 complete-link 凝聚聚类：
/// 按 pair 距离升序贪心合并，仅当合并后组内任意两点都相似才合并。
/// 输入 group 是原数组下标，输出拆分后的子组。
fn complete_link_split_cancellable(
    group: &[usize],
    phashes: &[u64],
    csigs: &[[u8; COLOR_SIG_LEN]],
    th: Thresholds,
    cancel: &AtomicBool,
) -> Result<Vec<Vec<usize>>, Cancelled> {
    let n = group.len();
    if n <= 2 {
        return Ok(vec![group.to_vec()]);
    }
    if n > MAX_COMPLETE_LINK {
        // 超大簇：跳过精拆，保留单链接原簇（原因见常量注释）
        return Ok(vec![group.to_vec()]);
    }

    // 组内两两关系：只把「双闸门都通过」的点对算作相似。
    // similar[a][b] = 组内第 a、b 个成员是否相似。
    let mut similar = vec![vec![false; n]; n];
    let mut dists: Vec<(u32, usize, usize)> = Vec::with_capacity(n * (n - 1) / 2);
    for a in 0..n {
        if a % 16 == 0 {
            check_cancelled(cancel)?;
        }
        for b in (a + 1)..n {
            let ga = group[a];
            let gb = group[b];
            let d = hamming(phashes[ga], phashes[gb]);
            if !is_similar(phashes[ga], &csigs[ga], phashes[gb], &csigs[gb], &th) {
                continue;
            }
            similar[a][b] = true;
            similar[b][a] = true;
            dists.push((d, a, b));
        }
    }
    check_cancelled(cancel)?;
    dists.sort_unstable();

    let mut leader: Vec<usize> = (0..n).collect();

    for (idx, &(_d, a, b)) in dists.iter().enumerate() {
        if idx % 64 == 0 {
            check_cancelled(cancel)?;
        }
        let (la, lb) = (leader[a], leader[b]);
        if la == lb {
            continue;
        }
        // complete-link 条件：两组间任意两点都相似
        // （组内自身成员已在此前合并时满足该条件，归纳成立）
        let members_a: Vec<usize> = (0..n).filter(|&x| leader[x] == la).collect();
        let members_b: Vec<usize> = (0..n).filter(|&x| leader[x] == lb).collect();
        let mut ok = true;
        'outer: for &x in members_a.iter() {
            for &y in &members_b {
                if !similar[x][y] {
                    ok = false;
                    break 'outer;
                }
            }
        }
        if ok {
            let target = la.min(lb);
            let source = if target == la { lb } else { la };
            for l in leader.iter_mut().take(n) {
                if *l == source {
                    *l = target;
                }
            }
        }
    }

    let mut out: Vec<Vec<usize>> = vec![];
    let mut map: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, &l) in leader.iter().enumerate() {
        match map.get(&l) {
            Some(&gi) => out[gi].push(group[i]),
            None => {
                map.insert(l, out.len());
                out.push(vec![group[i]]);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(c: u8) -> [u8; COLOR_SIG_LEN] {
        [c; COLOR_SIG_LEN]
    }

    const TH: Thresholds = Thresholds {
        phash: 8,
        color: 40,
    };

    #[test]
    fn no_pairs_when_all_unique() {
        // 8 个差异极大的哈希
        let phashes: Vec<u64> = (0..8u64).map(|i| i * 0x1111111111111111).collect();
        let csigs = vec![solid(10); 8];
        assert!(similar_pairs(&phashes, &csigs, TH).is_empty());
        assert!(cluster_similar(&phashes, &csigs, TH).is_empty());
    }

    #[test]
    fn near_duplicates_grouped() {
        // h0 为基，h1/h2/h3 各翻转少量位（距离 1-2）
        let base = 0x0000_0000_0000_0001u64;
        let phashes = vec![
            base,
            base | (1 << 10),
            base | (1 << 20),
            base | (1 << 30),
            0xFFFF_FFFF_FFFF_FFFF, // 完全不同的图
        ];
        let csigs = vec![solid(10); 5];
        let groups = cluster_similar(&phashes, &csigs, TH);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 4);
        assert_eq!(groups[0], vec![0, 1, 2, 3]);
    }

    #[test]
    fn chain_merge_is_split() {
        // 链式场景：A-B=3, B-C=3, A-C=6（阈值 3）→ complete-link 拆成 {A,B} {C}
        let a = 0u64;
        let b = 0b111; // 与 a 距离 3
        let c = 0b111_000; // 与 b 距离 3，与 a 距离 6
        let phashes = vec![a, b, c];
        let csigs = vec![solid(10); 3];
        let th = Thresholds {
            phash: 3,
            color: 40,
        };
        let groups = cluster_similar(&phashes, &csigs, th);
        // 单链接会合成 {0,1,2}，complete-link 拆分后应只有 {0,1}
        assert_eq!(groups, vec![vec![0, 1]]);
    }

    #[test]
    fn color_gate_blocks_same_structure_different_color() {
        // 结构哈希完全相同、只有颜色不同 → 结构闸门放行，但颜色闸门拦下。
        let phashes = vec![0x1234_5678_9abc_def0u64; 2];
        let csigs = vec![solid(30), solid(220)];
        assert!(similar_pairs(&phashes, &csigs, TH).is_empty());
        assert!(cluster_similar(&phashes, &csigs, TH).is_empty());
    }

    #[test]
    fn color_gate_allows_same_structure_same_color() {
        let phashes = vec![0x1234_5678_9abc_def0u64; 2];
        let csigs = vec![solid(120), solid(126)];
        let groups = cluster_similar(&phashes, &csigs, TH);
        assert_eq!(groups, vec![vec![0, 1]]);
    }

    #[test]
    fn structure_gate_blocks_different_structure_same_color() {
        // 颜色一致但结构差太远 → 结构闸门拦下
        let phashes = vec![0u64, 0xFFFF_FFFF_FFFF_FFFF];
        let csigs = vec![solid(120), solid(120)];
        assert!(cluster_similar(&phashes, &csigs, TH).is_empty());
    }

    #[test]
    fn union_find_basic() {
        let parent = union_find(5, &[(0, 1), (1, 2), (3, 4)]);
        assert_eq!(parent[0], parent[1]);
        assert_eq!(parent[1], parent[2]);
        assert_eq!(parent[3], parent[4]);
        assert_ne!(parent[0], parent[3]);
    }

    #[test]
    fn clustering_honours_cancellation() {
        let phashes = vec![0u64, 1, 3, 7];
        let csigs = vec![solid(10); 4];
        let cancel = AtomicBool::new(true);
        let result = cluster_similar_cancellable(&phashes, &csigs, TH, &cancel, &|_, _, _| {});
        assert!(matches!(result, Err(Cancelled)));
    }

    #[test]
    fn large_scale_performance_smoke() {
        // 1 万个随机哈希（≈5×10⁷ 对），验证暴力比对规模可行
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        let phashes: Vec<u64> = (0..10_000)
            .map(|i| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                seed ^ (i as u64)
            })
            .collect();
        let csigs = vec![solid(10); 10_000];
        let groups = cluster_similar(&phashes, &csigs, TH);
        // 随机哈希几乎不可能相似
        assert!(groups.is_empty());
    }

    #[test]
    fn oversized_cluster_bypasses_complete_link_not_memory() {
        // 300 张完全相似（超过 MAX_COMPLETE_LINK=256）：
        // complete-link 方阵是 O(n²) 内存 —— 超限簇必须原样保留，
        // 绝不能试图构造 300×300 的方阵（docs/04 阶段 3 防护）。
        let phashes = vec![0x1234_5678_9abc_def0u64; 300];
        let csigs = vec![solid(10); 300];
        let groups = cluster_similar(&phashes, &csigs, TH);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 300);
    }
}
