//! 聚类规模基准（docs/04 阶段 3）：验证 MVP 验收规模 50,000 张的比对性能。
//!
//! 用法：cargo run --release --example bench-cluster [n]（默认 50,000）
//!
//! 合成指纹分布：1% 成真重复对（与 i-100 结构差 1 位、同色），其余随机。
//! 主导成本是 O(n²) 比对本身，真重复率只影响命中 pair 数。

use dupremove_lib::cluster::{self, Thresholds};
use dupremove_lib::finger::COLOR_SIG_LEN;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);

    let mut seed = 0x1234_5678_9ABC_DEF0u64;
    let mut phashes: Vec<u64> = Vec::with_capacity(n);
    let mut csigs: Vec<[u8; COLOR_SIG_LEN]> = Vec::with_capacity(n);
    for i in 0..n {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        if i % 100 == 0 && i > 0 {
            // 真重复对：与 i-100 结构差 1 位、同色 → 默认档必定命中
            let p = phashes[i - 100];
            let c = csigs[i - 100];
            phashes.push(p ^ 1);
            csigs.push(c);
        } else {
            phashes.push(seed);
            let c = (seed % 180 + 30) as u8;
            let mut sig = [0u8; COLOR_SIG_LEN];
            for cell in sig.chunks_exact_mut(4) {
                cell[0] = c;
                cell[1] = c / 2;
                cell[2] = 255 - c / 3;
                cell[3] = 1;
            }
            csigs.push(sig);
        }
    }

    let th: Thresholds = cluster::THRESHOLDS[1];
    let cancel = AtomicBool::new(false);

    let t0 = Instant::now();
    let pairs = cluster::similar_pairs_cancellable(&phashes, &csigs, th, &cancel, &|_, _| {})
        .expect("基准不可取消");
    let t_pairs = t0.elapsed();

    let t1 = Instant::now();
    let groups = cluster::cluster_similar_cancellable(&phashes, &csigs, th, &cancel, &|_, _, _| {})
        .expect("基准不可取消");
    let t_cluster = t1.elapsed();

    println!(
        "n={n}  两两比对 {:.2}s（命中 {} 对）  完整聚类 {:.2}s（{} 组，含拆分）",
        t_pairs.as_secs_f64(),
        pairs.len(),
        t_cluster.as_secs_f64(),
        groups.len()
    );
}
