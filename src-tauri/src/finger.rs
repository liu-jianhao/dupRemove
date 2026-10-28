//! finger：指纹计算——精确哈希（blake3）+ 感知指纹（结构 pHash + 颜色签名）。
//!
//! 为什么要两路感知指纹：
//!
//! 早期实现只用了 64 位灰度 dHash（9×8 相邻像素比较）。实测发现它对
//! 「大面积纯色 + 少量文字」的图片几乎没有区分力：
//!
//! | 图片对 | dHash 距离 | pHash 距离 | 颜色差 |
//! |---|---|---|---|
//! | 粉色"20" ↔ 蓝色"31" | 11 | 19 | 81 |
//! | 蓝色"31" ↔ 粉色"31" | 0 | 0 | 76 |
//!
//! 第二行是致命的：两张图结构完全一致、只有底色不同，灰度哈希**完全看不出
//! 区别**（距离 0）。因为任何灰度哈希都把颜色信息丢掉了。
//!
//! 所以现在用两路信号做「双闸门」：
//! 1. **结构**：pHash（32×32 DCT 取低频 8×8，去 DC 后按中位数二值化）。
//!    比 dHash 抗压缩、抗缩放，对内容差异的区分度也更高。
//!    透明像素先合成到白底 —— 抠图贴纸的直接 RGB 会把透明区当黑色。
//! 2. **颜色**：6×6 网格的 RGB 均值（144 字节），**只统计主体像素**。
//!    补上灰度哈希的颜色盲区，且不被大面积背景稀释。
//!
//! 两路都通过才算「几乎一样」—— 宁可漏检，不可误判（误判会让用户误删照片）。
//!
//! 真实案例（微信表情贴纸 141×89，实测只有 36.9% 像素不透明）：
//!
//! | 图片对 | 结构距离 | 颜色距离 | 判定 |
//! |---|---|---|---|
//! | 粉色「20₽」↔ 蓝色「31₽」 | 7 | 103 | 不同组 ✅ |
//! | 同贴纸缩小 50% | 1 | 12 | 同一组 ✅ |
//! | 同贴纸变暗 15% | 2 | 22 | 同一组 ✅ |

use crate::control::{check_cancelled, Cancelled};
use crate::scanner::FileMeta;
use image::imageops::FilterType;
use image::{RgbImage, RgbaImage};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

/// 颜色签名网格边长（6×6 = 36 格）
pub const COLOR_GRID: u32 = 6;
/// 颜色网格格数
pub const COLOR_CELLS: usize = (COLOR_GRID * COLOR_GRID) as usize;
/// 颜色签名字节数：每格 3 字节 RGB + 1 字节有效标志（该格是否存在主体像素）
pub const COLOR_SIG_LEN: usize = COLOR_CELLS * 4;

/// 指纹算法版本，写入缓存（store 的 algo_version 列）。
/// 改动指纹算法 —— 哪怕签名长度不变 —— 必须 bump，让旧缓存自动失效重算。
/// 1 = 早期 dHash；2 = pHash + 颜色签名双闸门。
pub const ALGO_VERSION: i64 = 2;

/// 单个文件的完整指纹。
///
/// `phash == None` 表示无法解码（损坏 / HEIC 暂未支持）；
/// `csig == None` 同理。两路信号缺一不可，双闸门才有意义。
#[derive(Debug, Clone)]
pub struct Fingerprint {
    pub path: PathBuf,
    pub size: u64,
    /// 纳秒级修改时间（缓存键 + 删除前身份校验）
    pub mtime_ns: u64,
    /// 设备号 / inode（删除前身份校验；旧缓存行可能缺失 → None，此时退化为 size+mtime 校验）
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    /// 结构感知哈希（pHash，64 位）
    pub phash: Option<u64>,
    /// 颜色签名（6×6 RGB 主体网格）
    pub csig: Option<[u8; COLOR_SIG_LEN]>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub has_exif_camera: bool,
    /// EXIF 拍摄时间，格式 "YYYY:MM:DD HH:MM:SS"（定宽，字典序 = 时间序）。
    pub taken_at: Option<String>,
}

/// 文件内容精确哈希（流式读取，不整载内存）。
/// `Ok(None)` = 读取失败（文件消失/权限），调用方跳过该文件；`Err(Cancelled)` = 用户取消。
/// 每读 4MB 检查一次取消令牌 —— 几百 MB 的超大文件不该把「取消」卡死。
pub fn file_blake3_cancellable(
    path: &Path,
    cancel: &AtomicBool,
) -> Result<Option<[u8; 32]>, Cancelled> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut reads = 0u32;
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return Ok(None),
        };
        hasher.update(&buf[..n]);
        reads += 1;
        if reads.is_multiple_of(64) {
            check_cancelled(cancel)?;
        }
    }
    Ok(Some(*hasher.finalize().as_bytes()))
}

/// 不可取消的便捷入口（测试与 CLI 诊断用）。
pub fn file_blake3(path: &Path) -> std::io::Result<[u8; 32]> {
    let cancel = AtomicBool::new(false);
    match file_blake3_cancellable(path, &cancel) {
        // 固定 false 的令牌不会触发取消；这里 unreachable 只是类型收敛
        Err(Cancelled) => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "取消")),
        Ok(Some(h)) => Ok(h),
        Ok(None) => Err(std::io::Error::other("文件读取失败")),
    }
}

/// 计算单个文件的指纹：统一解码（EXIF 方向已校正，HEIC 支持）→ pHash + 颜色签名。
/// 解码失败的文件返回 phash=None 的指纹（缓存后避免反复重试解码）。
///
/// 注意：EXIF 相机信息在解码成功后才填 —— 损坏文件丢掉的只是 keeper
/// 排序的一个次要信号，不影响去重正确性。
pub fn fingerprint(meta: &FileMeta) -> Fingerprint {
    let mut fp = Fingerprint {
        path: meta.path.clone(),
        size: meta.size,
        mtime_ns: meta.mtime_ns,
        dev: Some(meta.dev),
        ino: Some(meta.ino),
        phash: None,
        csig: None,
        width: None,
        height: None,
        has_exif_camera: false,
        taken_at: None,
    };

    if let Some(d) = crate::decode::decode(&meta.path) {
        fp.width = Some(d.img.width());
        fp.height = Some(d.img.height());
        // 一次解码、一次转 RGBA，两路指纹共用，避免对大图重复转换。
        let rgba = d.img.to_rgba8();
        // 结构通道看「合成到白底」的图（透明区不当黑色）；
        // 颜色通道看原始 alpha 掩码（只统计主体像素，不被背景稀释）。
        fp.phash = Some(phash_64(&composite_on_white(&rgba)));
        fp.csig = Some(color_sig(&rgba));
        fp.has_exif_camera = d.camera;
        fp.taken_at = d.taken_at;
    }

    fp
}

/// DCT-II 的余弦表（只算一次）。
fn cos_table() -> &'static [[f32; 32]; 32] {
    static TABLE: OnceLock<[[f32; 32]; 32]> = OnceLock::new();
    TABLE.get_or_init(|| {
        const N: usize = 32;
        let mut t = [[0f32; N]; N];
        for (u, row) in t.iter_mut().enumerate() {
            for (x, v) in row.iter_mut().enumerate() {
                *v = (((2 * x + 1) as f32) * (u as f32) * std::f32::consts::PI / (2.0 * N as f32))
                    .cos();
            }
        }
        t
    })
}

/// pHash：32×32 灰度 → DCT-II → 低频 8×8（去掉 DC）→ 相对量化 + 中位数阈值 → 64 位。
///
/// 对缩放、转存、轻压缩稳定（实测距离 0），对内容差异敏感
/// （实测「粉色 20」与「蓝色 31」距离 19，而 dHash 只有 11）。
///
/// 两个关键细节，缺一不可：
///
/// 1. **必须用亮度，不能取 RGB 的单个通道**。取红通道会让粉色图（红 233）
///    与蓝色图（红 30）被判成完全不同，而它们的实际亮度只差 24/255。
/// 2. **阈值必须用「相对量化后的中位数」**。标准中位数阈值在一维结构图
///    （纯渐变、条纹）上极不稳定：64 个系数里大半本该是 0，数值噪声会让它们
///    相对中位数随机翻转，同一张图缩放前后能差出 15~17 位。
///    先把系数按最大绝对值归一化并取整，噪声就被压成一致的 0，
///    缩放前后距离回落到 0~1。
pub fn phash_64(rgb: &RgbImage) -> u64 {
    const N: usize = 32;
    let small = image::imageops::resize(rgb, N as u32, N as u32, FilterType::Triangle);

    // 行变换。用 f32 亮度而非 to_luma8()：整数截断会引入 ±0.5 的量化噪声。
    let cos = cos_table();
    let mut tmp = [[0f32; N]; N];
    for (y, row) in tmp.iter_mut().enumerate() {
        for (u, out) in row.iter_mut().enumerate() {
            let mut s = 0f32;
            for (x, cos_x) in cos[u].iter().enumerate() {
                let p = small.get_pixel(x as u32, y as u32);
                let luma = 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32;
                s += luma * cos_x;
            }
            *out = s;
        }
    }
    // 列变换（只保留低频 8×8）
    let mut coef = [[0f32; 8]; 8];
    for v in 0..8 {
        for u in 0..8 {
            let mut s = 0f32;
            for y in 0..N {
                s += tmp[y][u] * cos[v][y];
            }
            coef[v][u] = s;
        }
    }

    // 相对量化：按最大绝对值归一化后取整，数值噪声被压成一致的 0。
    const LEVELS: f32 = 256.0;
    let mut max_abs = 0f32;
    for (v, row) in coef.iter().enumerate() {
        for (u, c) in row.iter().enumerate() {
            if v == 0 && u == 0 {
                continue;
            }
            max_abs = max_abs.max(c.abs());
        }
    }
    if max_abs < 1e-9 {
        return 0;
    }
    let q = |c: f32| -> i32 { (c / max_abs * LEVELS).round() as i32 };

    // 中位数阈值（排除直流分量）
    let mut vals: Vec<i32> = Vec::with_capacity(63);
    for (v, row) in coef.iter().enumerate() {
        for (u, c) in row.iter().enumerate() {
            if v == 0 && u == 0 {
                continue;
            }
            vals.push(q(*c));
        }
    }
    vals.sort_unstable();
    let median = vals[vals.len() / 2];

    let mut hash = 0u64;
    let mut idx = 0usize;
    for (v, row) in coef.iter().enumerate() {
        for (u, c) in row.iter().enumerate() {
            if v == 0 && u == 0 {
                continue;
            }
            if q(*c) > median {
                hash |= 1 << idx;
            }
            idx += 1;
        }
    }
    hash
}

/// 透明像素合成到白底。
///
/// 抠图贴纸（表情包）大量像素是透明的，直接丢掉 alpha 取 RGB 会把透明区
/// 当成黑色像素参与灰度化，pHash 看到的是「黑底上的图案」而不是「白底上的图案」。
/// 合成到白底符合视觉直觉，也和图片在相册里的实际观感一致。
fn composite_on_white(rgba: &RgbaImage) -> RgbImage {
    let mut out = RgbImage::new(rgba.width(), rgba.height());
    for (dst, src) in out.pixels_mut().zip(rgba.pixels()) {
        let a = src.0[3] as u32;
        for c in 0..3 {
            dst.0[c] = ((src.0[c] as u32 * a + 255 * (255 - a)) / 255) as u8;
        }
    }
    out
}

/// 颜色签名的预缩放边长。
///
/// 取色前先把图缩到 36×36，等价于一道低通滤波：
/// JPEG 的块效应和色度噪点会被平均掉，而「换配色」是整体色度偏移，
/// 滤波不会削弱它。实测没有这一步时，JPEG 重压过的真重复颜色差高达 67，
/// 比真正的换色还大 —— 噪点被当成了配色差异。
const COLOR_PRESCALE: u32 = 36;

/// 颜色签名：6×6 网格，**每格只统计主体像素**（alpha ≥ 128），
/// 且存的是**全图统一亮度基准归一化后的色度**。
///
/// 为什么必须只看主体：真实表情贴纸（141×89，实测）只有 **36.9% 的像素不透明**，
/// 其余是透明背景。若把背景一起算进 4×4 全图均值，粉色贴纸与蓝色贴纸的
/// 颜色差会被 13 个「两图相同」的背景格稀释到 **40** —— 恰好卡在阈值上，
/// 换个阈值就漏判或误判。
///
/// 为什么必须归一化亮度：原始 RGB 距离分不清「同一张图变暗了」和「同一张图换配色」。
/// 实测（系统壁纸同一张的配色变体 + 真实贴纸）：
///
/// | 图片对 | 类别 | 原始 RGB 距离 | 归一化色度距离 |
/// |---|---|---|---|
/// | 同贴纸变暗 15% | 真重复 | 22 | **0** |
/// | 同构图换配色（Mac 蓝↔紫） | 不同 | 23 | **22** |
/// | 同构图换配色（iMac 蓝↔橙） | 不同 | 78 | **88** |
///
/// 归一化后，「变暗」几乎不产生色差，「换色」被拉开 —— 两类彻底分开。
///
/// 关键细节：基准必须是**全图统一的一个 scale**，不能每格各自除以自己的最大通道。
/// 每格独立归一化会把深色格子的压缩噪点放大好几倍。
///
/// 布局：每格 4 字节 = 色度 RGB(3) + 有效标志(1)。无主体像素的格子标志为 0。
pub fn color_sig(rgba: &RgbaImage) -> [u8; COLOR_SIG_LEN] {
    // 低通预缩放：平滑压缩噪点。alpha 一起缩放，掩码与颜色天然对齐。
    let small = if rgba.width() <= COLOR_PRESCALE && rgba.height() <= COLOR_PRESCALE {
        rgba.clone()
    } else {
        image::imageops::resize(
            rgba,
            COLOR_PRESCALE,
            COLOR_PRESCALE,
            image::imageops::FilterType::Triangle,
        )
    };
    let (w, h) = small.dimensions();
    let mut acc = [[0u32; 4]; COLOR_CELLS]; // R, G, B, 计数
    let mut global = [0u64; 3]; // 全图主体像素的通道总和
    let mut total = 0u64;
    for (x, y, p) in small.enumerate_pixels() {
        if p.0[3] < 128 {
            continue; // 透明像素不是主体
        }
        let cx = ((x as u64 * COLOR_GRID as u64) / w as u64) as usize;
        let cy = ((y as u64 * COLOR_GRID as u64) / h as u64) as usize;
        let idx = cy * COLOR_GRID as usize + cx;
        acc[idx][0] += p.0[0] as u32;
        acc[idx][1] += p.0[1] as u32;
        acc[idx][2] += p.0[2] as u32;
        acc[idx][3] += 1;
        global[0] += p.0[0] as u64;
        global[1] += p.0[1] as u64;
        global[2] += p.0[2] as u64;
        total += 1;
    }
    // 全图统一亮度基准：主体像素的平均亮度
    let scale = if total == 0 {
        0.0f32
    } else {
        (global[0] + global[1] + global[2]) as f32 / (3 * total) as u32 as f32
    };

    let mut out = [0u8; COLOR_SIG_LEN];
    if scale < 1.0 {
        return out; // 全黑 / 无主体
    }
    for i in 0..COLOR_CELLS {
        let n = acc[i][3];
        if n == 0 {
            continue; // 该格没有主体像素，有效标志保持 0
        }
        let (r, g, b) = (
            acc[i][0] as f32 / n as f32,
            acc[i][1] as f32 / n as f32,
            acc[i][2] as f32 / n as f32,
        );
        // 除以全图基准：等比变暗/变亮完全抵消，配色差异原样保留
        out[i * 4] = ((r * 255.0 / scale).round() as i32).clamp(0, 255) as u8;
        out[i * 4 + 1] = ((g * 255.0 / scale).round() as i32).clamp(0, 255) as u8;
        out[i * 4 + 2] = ((b * 255.0 / scale).round() as i32).clamp(0, 255) as u8;
        out[i * 4 + 3] = 1;
    }
    out
}

/// 「一方有主体、另一方没有」时的每通道惩罚值。
///
/// 说明两张图的图案位置不同 —— 即使颜色恰好接近，也不该判为「几乎一样」。
/// 取 90（而非 255）：足以拉高平均差，又不至于让任何一对图直接顶到满值。
const COLOR_MISMATCH_PENALTY: u32 = 25;

/// 两个颜色签名的平均绝对差（每通道 0~255）。
///
/// 只在**双方都有主体像素**的格子上取平均：
/// - 两边都有 → 累加 RGB 差
/// - 只有一边有 → 记惩罚（图案位置不同）
/// - 两边都没有 → 跳过，不进分母（否则纯背景图会被算成"完全不同"）
pub fn color_dist(a: &[u8; COLOR_SIG_LEN], b: &[u8; COLOR_SIG_LEN]) -> u32 {
    let mut sum = 0u32;
    let mut n = 0u32;
    for i in 0..COLOR_CELLS {
        let va = a[i * 4 + 3] != 0;
        let vb = b[i * 4 + 3] != 0;
        match (va, vb) {
            (true, true) => {
                sum += (a[i * 4] as i32 - b[i * 4] as i32).unsigned_abs();
                sum += (a[i * 4 + 1] as i32 - b[i * 4 + 1] as i32).unsigned_abs();
                sum += (a[i * 4 + 2] as i32 - b[i * 4 + 2] as i32).unsigned_abs();
                n += 1;
            }
            (true, false) | (false, true) => {
                sum += COLOR_MISMATCH_PENALTY * 3;
                n += 1;
            }
            (false, false) => {}
        }
    }
    if n == 0 {
        return 0; // 两张都是空白
    }
    sum / (n * 3)
}

/// 64 位汉明距离。
#[inline]
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage, Rgba};

    fn grad_img(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for x in 0..w {
            for y in 0..h {
                let v = ((x * 255) / w.max(1)) as u8;
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    fn noise_img(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for x in 0..w {
            for y in 0..h {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let v = ((s >> 33) & 0xFF) as u8;
                img.put_pixel(x, y, Rgb([v, v, v]));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    /// 照片式素材：渐变背景 + 两个明暗不同的方块。
    /// 有二维结构（真实照片的典型特征），pHash 在缩放/压缩后保持稳定。
    fn photo_like(w: u32, h: u32) -> RgbImage {
        let mut img = RgbImage::new(w, h);
        for x in 0..w {
            for y in 0..h {
                let mut v = (x * 120 / w.max(1)) as i32 + (y * 80 / h.max(1)) as i32;
                if x > w / 4 && x < w / 2 && y > h / 5 && y < h / 3 {
                    v += 70;
                }
                if x > w / 3 && x < w * 3 / 5 && y > h / 2 && y < h * 4 / 5 {
                    v -= 55;
                }
                let v = v.clamp(0, 255) as u8;
                img.put_pixel(x, y, Rgb([v, v, v / 2]));
            }
        }
        img
    }

    /// 5×7 点阵数字（0/1/2/3），用来造「色块 + 文字」这类平坦图形。
    const FONT: &[(char, [&str; 7])] = &[
        (
            '0',
            [
                "#####", "#   #", "#  ##", "# # #", "##  #", "#   #", "#####",
            ],
        ),
        (
            '1',
            ["   #", "  ##", "   #", "   #", "   #", "   #", "  ###"],
        ),
        (
            '2',
            ["#####", "#   #", "    #", "   #", "  #", " #", "#####"],
        ),
        (
            '3',
            [
                "#####", "#   #", "    #", "  ###", "    #", "#   #", "#####",
            ],
        ),
    ];

    /// 圆角色块 + 白色文字，模拟表情包 / 图标这类平坦图形。
    fn sticker(w: u32, h: u32, bg: [u8; 3], digits: &str) -> DynamicImage {
        let mut img = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
        for y in 2..h - 2 {
            for x in 2..w - 2 {
                img.put_pixel(x, y, Rgb(bg));
            }
        }
        let scale = (h / 4).max(1);
        let cw = 6u32 * scale;
        let ch = 7u32 * scale;
        let total = digits.chars().count() as u32 * cw;
        let ox = (w.saturating_sub(total)) / 2;
        let oy = (h.saturating_sub(ch)) / 2;
        for (i, c) in digits.chars().enumerate() {
            let glyph = FONT.iter().find(|(ch, _)| *ch == c).map(|(_, g)| g);
            if let Some(glyph) = glyph {
                for (gy, row) in glyph.iter().enumerate() {
                    for (gx, cell) in row.chars().enumerate() {
                        if cell == '#' {
                            for dy in 0..scale {
                                for dx in 0..scale {
                                    let x = ox + i as u32 * cw + gx as u32 * scale + dx;
                                    let y = oy + gy as u32 * scale + dy;
                                    if x < w && y < h {
                                        img.put_pixel(x, y, Rgb([255, 255, 255]));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    /// RgbImage → RgbaImage（不透明像素 alpha=255）
    fn as_rgba(rgb: &RgbImage) -> RgbaImage {
        let mut out = RgbaImage::new(rgb.width(), rgb.height());
        for (dst, src) in out.pixels_mut().zip(rgb.pixels()) {
            dst.0 = [src.0[0], src.0[1], src.0[2], 255];
        }
        out
    }

    /// 抠图贴纸：透明背景 + 居中的实心色块 + 白色文字。
    ///
    /// 模拟真实微信表情包 —— 实测这类图只有 **36.9% 的像素不透明**，
    /// 其余是透明背景。颜色签名必须只看主体，否则配色差会被背景稀释。
    fn cutout_sticker(w: u32, h: u32, bg: [u8; 3], digits: &str) -> RgbaImage {
        let mut img = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
        // 主体占画面中部（左右各留 40% 空白，模拟真实抠图的留白）
        let bw = (w as f32 * 0.6) as u32;
        let bh = (h as f32 * 0.9) as u32;
        let x0 = (w - bw) / 2;
        let y0 = (h - bh) / 2;
        for y in y0..y0 + bh {
            for x in x0..x0 + bw {
                img.put_pixel(x, y, Rgba([bg[0], bg[1], bg[2], 255]));
            }
        }
        let scale = (bh / 4).max(1);
        let cw = 6u32 * scale;
        let ch = 7u32 * scale;
        let total = digits.chars().count() as u32 * cw;
        let ox = (w.saturating_sub(total)) / 2;
        let oy = (h.saturating_sub(ch)) / 2;
        for (i, c) in digits.chars().enumerate() {
            let glyph = FONT.iter().find(|(ch, _)| *ch == c).map(|(_, g)| g);
            if let Some(glyph) = glyph {
                for (gy, row) in glyph.iter().enumerate() {
                    for (gx, cell) in row.chars().enumerate() {
                        if cell == '#' {
                            for dy in 0..scale {
                                for dx in 0..scale {
                                    let x = ox + i as u32 * cw + gx as u32 * scale + dx;
                                    let y = oy + gy as u32 * scale + dy;
                                    if x < w && y < h {
                                        img.put_pixel(x, y, Rgba([255, 255, 255, 255]));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        img
    }

    fn reencode_jpeg(rgb: &RgbImage, quality: u8) -> RgbImage {
        let mut buf = Vec::new();
        {
            use image::ImageEncoder;
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
            enc.write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        }
        image::load_from_memory_with_format(&buf, image::ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8()
    }

    #[test]
    fn phash_identical_images_match() {
        let a = phash_64(&grad_img(400, 300).to_rgb8());
        // 重缩放的同图 → 汉明距离应为 0
        let b = phash_64(&grad_img(200, 150).to_rgb8());
        // 允许 1 位抖动：系数正好落在中位数边界上时，缩放可能让它翻转
        assert!(hamming(a, b) <= 1, "缩放后哈希抖动过大: {}", hamming(a, b));
    }

    #[test]
    fn phash_stable_under_resize() {
        let src = photo_like(900, 600);
        for (w, h) in [(450u32, 300u32), (300, 200), (180, 120)] {
            let small = image::imageops::resize(&src, w, h, FilterType::Triangle);
            assert_eq!(
                hamming(phash_64(&src), phash_64(&small)),
                0,
                "缩放到 {w}x{h} 后哈希变了"
            );
        }
    }

    #[test]
    fn phash_stable_under_jpeg_recompression() {
        // 转存（重压 JPEG）是重复照片最常见的形态，必须认得出
        let src = photo_like(900, 600);
        let base = phash_64(&src);
        for q in [85u8, 70, 55] {
            let re = reencode_jpeg(&src, q);
            let d = hamming(base, phash_64(&re));
            assert!(d <= 4, "重压 JPEG(q={q}) 后距离过大: {d}");
        }
    }

    #[test]
    fn phash_different_images_differ() {
        let a = phash_64(&photo_like(400, 300));
        let b = phash_64(&noise_img(400, 300).to_rgb8());
        assert!(hamming(a, b) > 10, "distance = {}", hamming(a, b));
    }

    #[test]
    fn phash_separates_different_digits_same_color() {
        let a = sticker(141, 89, [233, 30, 99], "20");
        let b = sticker(141, 89, [233, 30, 99], "31");
        let d = hamming(phash_64(&a.to_rgb8()), phash_64(&b.to_rgb8()));
        assert!(d > 12, "同色不同数字的距离应超过宽松阈值，实际 {d}");
    }

    #[test]
    fn color_gate_separates_same_layout_different_color() {
        // 这是旧 dHash 漏掉的场景：结构完全一致、只有底色不同。
        let pink = sticker(141, 89, [233, 30, 99], "31");
        let blue = sticker(141, 89, [30, 110, 235], "31");
        let dp = phash_64(&pink.to_rgb8());
        let db = phash_64(&blue.to_rgb8());
        // 结构上二者确实一样（灰度信息一致），pHash 也分不开 ——
        // 靠的是颜色闸门。此断言把「结构距离为 0」这个前提固定下来。
        assert_eq!(hamming(dp, db), 0, "前提变了需要重新评估颜色闸门");

        let cp = color_sig(&as_rgba(&pink.to_rgb8()));
        let cb = color_sig(&as_rgba(&blue.to_rgb8()));
        assert!(
            color_dist(&cp, &cb) > 60,
            "颜色闸门应能区分不同底色，实际 {}",
            color_dist(&cp, &cb)
        );
    }

    #[test]
    fn color_sig_same_for_same_image() {
        let img = photo_like(300, 200);
        assert_eq!(
            color_dist(&color_sig(&as_rgba(&img)), &color_sig(&as_rgba(&img))),
            0
        );
    }

    #[test]
    fn color_sig_survives_resize() {
        let img = photo_like(300, 200);
        let small = image::imageops::resize(&img, 150, 100, FilterType::Triangle);
        let d = color_dist(&color_sig(&as_rgba(&img)), &color_sig(&as_rgba(&small)));
        assert!(d <= 15, "缩放后颜色偏差过大: {d}");
    }

    #[test]
    fn color_sig_ignores_brightness_shift() {
        // 整体调暗 15% 是同图的常见变体，必须**几乎不产生色差**。
        //
        // 这是亮度归一化的直接收益：颜色签名除以全图统一亮度基准，
        // 等比变暗被完全抵消。旧实现（原始 RGB 均值）这里差 22，
        // 与「同构图换配色」的 20 混在一起，分不开。
        let img = photo_like(300, 200);
        let mut dark = img.clone();
        for p in dark.pixels_mut() {
            for k in 0..3 {
                p[k] = (p[k] as u32 * 85 / 100) as u8;
            }
        }
        let d = color_dist(&color_sig(&as_rgba(&img)), &color_sig(&as_rgba(&dark)));
        assert!(d <= 3, "调暗 15% 后颜色偏差应几乎为 0，实际 {d}");
    }

    #[test]
    fn color_sig_sees_through_transparent_background() {
        // 真实微信表情贴纸实测只有 36.9% 像素不透明。
        // 若把透明背景算进均值，配色差会被稀释到刚好卡在阈值上 ——
        // 主体掩码就是为解决这个而存在的。
        let pink = cutout_sticker(141, 89, [233, 30, 99], "31");
        let blue = cutout_sticker(141, 89, [30, 110, 235], "31");
        let d = color_dist(&color_sig(&pink), &color_sig(&blue));
        // 必须远高于默认档的颜色阈值（12），否则抠图贴纸的配色差会漏过去
        assert!(d > 40, "抠图贴纸的配色差应被完整保留，实际 {d}");
    }

    #[test]
    fn color_sig_ignores_transparent_canvas_size() {
        // 同一主体放在不同大小的透明画布上 —— 主体占比变了，
        // 但配色没变，颜色距离应保持很小（背景不参与比较）。
        let small = cutout_sticker(141, 89, [233, 30, 99], "31");
        let big = cutout_sticker(282, 178, [233, 30, 99], "31");
        let d = color_dist(&color_sig(&small), &color_sig(&big));
        assert!(d <= 20, "透明留白多少不应影响配色判定，实际 {d}");
    }

    #[test]
    fn color_sig_survives_transparent_shift() {
        // 抠图贴纸平移几像素后主体在网格里换了位置，
        // 颜色距离应保持在可用范围内（不能把真重复判成换色）。
        let a = cutout_sticker(141, 89, [233, 30, 99], "31");
        let mut b = RgbaImage::from_pixel(141, 89, Rgba([0, 0, 0, 0]));
        for y in 0..89 {
            for x in 0..141 {
                if (4..137).contains(&x) {
                    b.put_pixel(x, y, *a.get_pixel(x - 4, y));
                }
            }
        }
        let d = color_dist(&color_sig(&a), &color_sig(&b));
        assert!(d <= 40, "抠图贴纸平移后颜色偏差过大: {d}");
    }

    #[test]
    fn blake3_stable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        std::fs::write(&p, b"hello").unwrap();
        let h1 = file_blake3(&p).unwrap();
        let h2 = file_blake3(&p).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn blake3_honours_cancellation() {
        // 5MB 文件：读到第 64 块（4MB）触发取消检查。
        // 大文件哈希不该把「取消扫描」卡死几十秒。
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.bin");
        std::fs::write(&p, vec![0u8; 5 << 20]).unwrap();
        let cancel = AtomicBool::new(true);
        assert!(file_blake3_cancellable(&p, &cancel).is_err());
        // 未取消时正常完成
        let cancel = AtomicBool::new(false);
        assert!(file_blake3_cancellable(&p, &cancel).unwrap().is_some());
    }
}
