//! 诊断探针：打印一批图片两两之间的结构哈希距离与颜色距离。
//!
//! 用法：cargo run --example diag -- <目录> [phash阈值] [color阈值]
//!
//! 自动为每张原图生成变体（缩小 50% / 变暗 15%），用于观察真重复的距离上界。

use dupremove_lib::finger::{color_dist, color_sig, hamming, phash_64};
use image::{DynamicImage, RgbaImage};

fn composite_on_white(rgba: &RgbaImage) -> image::RgbImage {
    let mut out = image::RgbImage::new(rgba.width(), rgba.height());
    for (dst, src) in out.pixels_mut().zip(rgba.pixels()) {
        let a = src.0[3] as u32;
        for c in 0..3 {
            dst.0[c] = ((src.0[c] as u32 * a + 255 * (255 - a)) / 255) as u8;
        }
    }
    out
}

fn darken(img: &DynamicImage, pct: f32) -> DynamicImage {
    let mut rgba = img.to_rgba8();
    for p in rgba.pixels_mut() {
        for c in 0..3 {
            p.0[c] = (p.0[c] as f32 * (1.0 - pct)) as u8;
        }
    }
    DynamicImage::ImageRgba8(rgba)
}

fn brighten(img: &DynamicImage, pct: f32) -> DynamicImage {
    let mut rgba = img.to_rgba8();
    for p in rgba.pixels_mut() {
        for c in 0..3 {
            p.0[c] = (p.0[c] as f32 + (255.0 - p.0[c] as f32) * pct) as u8;
        }
    }
    DynamicImage::ImageRgba8(rgba)
}

/// 整体平移（模拟重新截图时的轻微错位）
fn shift(img: &DynamicImage, dx: i32, dy: i32) -> DynamicImage {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = image::RgbaImage::new(w, h);
    for (x, y, p) in rgba.enumerate_pixels() {
        let nx = x as i32 + dx;
        let ny = y as i32 + dy;
        if nx >= 0 && ny >= 0 && (nx as u32) < w && (ny as u32) < h {
            out.put_pixel(nx as u32, ny as u32, *p);
        }
    }
    DynamicImage::ImageRgba8(out)
}

/// JPEG 重压（转成有损格式）
fn recompress(img: &DynamicImage) -> Option<DynamicImage> {
    let mut buf = Vec::new();
    let dyn_img = DynamicImage::ImageRgba8(img.to_rgba8());
    if dyn_img
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .is_err()
    {
        return None;
    }
    // image crate 的 Jpeg 编码器默认质量 75，无法直接指定；这里用解码-再编码近似
    image::load_from_memory_with_format(&buf, image::ImageFormat::Jpeg).ok()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("用法: diag <目录>");
    let th: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4);
    let ct: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(40);

    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("读取目录失败")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map(|x| matches!(x, "png" | "jpg" | "jpeg" | "webp"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    let mut items: Vec<(String, DynamicImage)> = Vec::new();
    for f in &files {
        let Ok(img) = image::open(f) else { continue };
        let name = f
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let stem = name
            .rsplit_once('.')
            .map(|(s, _)| s.to_string())
            .unwrap_or_else(|| name.clone());
        items.push((name, img.clone()));
        if std::env::var("DIAG_VARS").is_err() {
            continue; // 默认不生成变体（全量两两比对时用）
        }
        let (w, h) = (img.width() / 2, img.height() / 2);
        if w > 0 && h > 0 {
            items.push((
                format!("{stem}·缩50%"),
                img.resize(w, h, image::imageops::FilterType::Lanczos3),
            ));
        }
        let (w9, h9) = (img.width() * 9 / 10, img.height() * 9 / 10);
        if w9 > 0 && h9 > 0 {
            items.push((
                format!("{stem}·缩90%"),
                img.resize(w9, h9, image::imageops::FilterType::Lanczos3),
            ));
        }
        items.push((format!("{stem}·暗15%"), darken(&img, 0.15)));
        items.push((format!("{stem}·亮10%"), brighten(&img, 0.10)));
        for dx in [2i32, 4, 8] {
            items.push((format!("{stem}·右移{dx}px"), shift(&img, dx, 0)));
        }
        if let Some(r) = recompress(&img) {
            items.push((format!("{stem}·JPEG重压"), r));
        }
    }

    let mut rows: Vec<(String, u64, [u8; dupremove_lib::finger::COLOR_SIG_LEN])> = Vec::new();
    for (name, img) in &items {
        let rgba = img.to_rgba8();
        rows.push((
            name.clone(),
            phash_64(&composite_on_white(&rgba)),
            color_sig(&rgba),
        ));
    }

    println!("\n目录: {dir}   阈值: 结构<={th}  颜色<={ct}\n");
    // 只列出结构距离 <= 展示上限 的对，避免刷屏
    const SHOW_MAX: u32 = 20;
    let mut same = 0usize;
    let mut diff = 0usize;
    for i in 0..rows.len() {
        for j in (i + 1)..rows.len() {
            let hd = hamming(rows[i].1, rows[j].1);
            let cd = color_dist(&rows[i].2, &rows[j].2);
            if hd > SHOW_MAX {
                diff += 1;
                continue;
            }
            same += 1;
            println!(
                "{:<34} vs {:<34} 结构 {:>3}  颜色 {:>3}{}",
                rows[i].0,
                rows[j].0,
                hd,
                cd,
                if hd <= th && cd <= ct {
                    "  << 同组"
                } else {
                    ""
                }
            );
        }
    }
    println!("\n结构距离 <= {SHOW_MAX} 的对: {same}    > {SHOW_MAX} 的对: {diff}");
}
