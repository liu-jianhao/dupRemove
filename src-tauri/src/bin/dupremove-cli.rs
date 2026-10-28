//! dupremove-cli：命令行冒烟入口。
//!
//! 用途：在没有 GUI 的环境（CI、批量验证、脚本化体检）对真实目录跑完整管线，
//! 输出统计与分组明细。GUI 走的正是同一套 pipeline/scanner/store 代码，
//! 因此这里跑通即证明核心链路可用。
//!
//! 用法：
//!   dupremove-cli <目录> [--threshold N] [--color-threshold N] [--subdirs] [--json]
//!
//! 示例：
//!   dupremove-cli ~/Pictures                       # 默认含子目录，阈值 8
//!   dupremove-cli ~/Pictures --threshold 5 --json  # 机器可读输出

use dupremove_lib::cluster;
use dupremove_lib::pipeline::{self, PipelineProgress, ScanStats};
use dupremove_lib::scanner::{self, ScanOptions};
use dupremove_lib::store::Store;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    // 解析参数
    let mut dir: Option<PathBuf> = None;
    // 默认 = 均衡档（与 cluster::THRESHOLDS / 前端 THRESHOLDS 一致）
    let mut threshold: u32 = cluster::THRESHOLDS[1].phash;
    let mut color_threshold: u32 = cluster::THRESHOLDS[1].color;
    let mut include_subdirs = true;
    let mut as_json = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--threshold" | "-t" => {
                i += 1;
                threshold = args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                    eprintln!("错误：--threshold 需要一个数字");
                    std::process::exit(2);
                });
            }
            "--color-threshold" | "-c" => {
                i += 1;
                color_threshold = args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                    eprintln!("错误：--color-threshold 需要一个数字");
                    std::process::exit(2);
                });
            }
            "--no-subdirs" => include_subdirs = false,
            "--json" => as_json = true,
            other if !other.starts_with('-') && dir.is_none() => {
                dir = Some(PathBuf::from(other));
            }
            other => {
                eprintln!("错误：未知参数 {other}");
                print_usage();
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let dir = match dir {
        Some(d) => d,
        None => {
            eprintln!("错误：缺少目录参数");
            print_usage();
            return ExitCode::from(2);
        }
    };

    if !dir.is_dir() {
        eprintln!("错误：目录不存在或不是目录：{}", dir.display());
        return ExitCode::from(2);
    }

    // 缓存与缩略图放在临时目录，不污染被扫描目录
    let work = std::env::temp_dir().join("dupremove-cli");
    let _ = std::fs::create_dir_all(&work);
    let thumb_dir = work.join("thumbs");
    let db_path = work.join("cache.db");

    let outcome = scanner::scan_dir_with_stats(&dir, &ScanOptions { include_subdirs });
    if outcome.metas.is_empty() {
        if as_json {
            println!(
                "{}",
                serde_json::json!({
                    "dir": dir.display().to_string(),
                    "stats": empty_stats(),
                    "groups": []
                })
            );
        } else {
            println!("目录中没有可识别的图片文件：{}", dir.display());
        }
        return ExitCode::SUCCESS;
    }

    let store = match Store::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：缓存数据库打开失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    let started = std::time::Instant::now();
    let progress = |p: PipelineProgress| {
        if !as_json && p.stage == "fingerprints" {
            eprint!("\r指纹计算 {}/{}", p.done, p.total);
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
    };
    let cancel = AtomicBool::new(false);

    let th = cluster::Thresholds {
        phash: threshold,
        color: color_threshold,
    };
    let (_fps, groups, stats) =
        match pipeline::run_pipeline(outcome, &dir, &store, &thumb_dir, th, &progress, &cancel) {
            Ok(result) => result,
            Err(e) => {
                eprintln!("错误：{e}");
                return ExitCode::FAILURE;
            }
        };

    if !as_json {
        eprintln!("\r{:width$}\r", "", width = 30);
    }
    let elapsed = started.elapsed();

    if as_json {
        let out = serde_json::json!({
            "dir": dir.display().to_string(),
            "threshold": threshold,
            "color_threshold": color_threshold,
            "elapsed_ms": elapsed.as_millis() as u64,
            "stats": stats,
            "groups": groups,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        print_human(&dir, threshold, color_threshold, elapsed, &stats, &groups);
    }

    ExitCode::SUCCESS
}

fn empty_stats() -> ScanStats {
    ScanStats::default()
}

fn print_human(
    dir: &Path,
    threshold: u32,
    color_threshold: u32,
    elapsed: std::time::Duration,
    stats: &ScanStats,
    groups: &[pipeline::GroupJson],
) {
    println!("目录        {}", dir.display());
    println!("相似度阈值  结构 ≤ {threshold} / 颜色 ≤ {color_threshold}");
    println!("耗时        {:.2}s", elapsed.as_secs_f64());
    println!("---");
    println!("扫描文件    {}", stats.scanned);
    println!("  解码成功  {}", stats.decoded);
    println!("  解码失败  {}", stats.skipped_decode);
    println!("  缓存命中  {}", stats.cache_hits);
    println!("  新计算    {}", stats.new_fingerprints);
    println!("---");
    println!("精确重复组  {}", stats.exact_groups);
    println!("相似重复组  {}", stats.similar_groups);
    println!(
        "建议删除    {} 张，可释放 {}",
        stats.duplicate_files,
        human_bytes(stats.reclaimable_bytes)
    );

    if !groups.is_empty() {
        println!("---");
        println!("分组明细（前 10 组）:");
        for (i, g) in groups.iter().take(10).enumerate() {
            println!("  [{}] {} · {} 个文件", i + 1, g.kind, g.files.len());
            for f in &g.files {
                let name = PathBuf::from(&f.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let mark = if f.is_best { "★保留" } else { "  删除" };
                println!(
                    "      {mark}  {name}  {}x{}  {}",
                    f.width,
                    f.height,
                    human_bytes(f.size)
                );
            }
        }
        if groups.len() > 10 {
            println!("  … 其余 {} 组省略", groups.len() - 10);
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn print_usage() {
    println!("dupremove-cli — 完全离线的照片去重工具（命令行冒烟入口）");
    println!();
    println!("用法：");
    println!("  dupremove-cli <目录> [选项]");
    println!();
    println!("选项：");
    println!(
        "  -t, --threshold N       结构相似阈值（汉明距离 ≤ N），默认 {}",
        cluster::THRESHOLDS[1].phash
    );
    println!(
        "  -c, --color-threshold N 颜色相似阈值（平均差 ≤ N），默认 {}",
        cluster::THRESHOLDS[1].color
    );
    println!("      --no-subdirs    不扫描子目录");
    println!("      --json          输出 JSON（机器可读）");
    println!("  -h, --help          显示帮助");
    println!();
    println!("示例：");
    println!("  dupremove-cli ~/Pictures");
    println!("  dupremove-cli ~/Pictures --threshold 5 --color-threshold 30 --json");
}
