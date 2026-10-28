//! decode：统一图片解码边界（docs/04 阶段 2）。
//!
//! 指纹（finger）与缩略图（thumbs）共用同一入口，保证两路看到同一份像素：
//! - JPEG / PNG / WebP / BMP / GIF → image crate + kamadak-exif
//! - HEIC / HEIF → macOS 系统 ImageIO（零 C 依赖，iPhone 用户核心格式）
//!
//! 返回的图**已应用 EXIF 方向** —— 竖拍照片的指纹与缩略图都不会歪着。

use image::DynamicImage;
use std::path::Path;

/// 解码产物。
pub struct Decoded {
    /// 已按 EXIF 方向校正的图
    pub img: DynamicImage,
    /// 原始 EXIF orientation（1~9）
    pub orientation: u32,
    /// 是否有相机信息（Make/Model）—— keeper 排序信号
    pub camera: bool,
    /// EXIF 拍摄时间（定宽 "YYYY:MM:DD HH:MM:SS"）
    pub taken_at: Option<String>,
}

pub fn decode(path: &Path) -> Option<Decoded> {
    if is_heic(path) {
        return heic::decode(path);
    }
    let exif = read_exif(path);
    let img = image::open(path).ok()?;
    let orientation = exif.as_ref().and_then(|(o, _, _)| *o).unwrap_or(1);
    Some(Decoded {
        img: apply_orientation(img, orientation),
        orientation,
        camera: exif.as_ref().map(|(_, c, _)| *c).unwrap_or(false),
        taken_at: exif.and_then(|(_, _, t)| t),
    })
}

pub fn is_heic(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "heic" | "heif"))
        .unwrap_or(false)
}

fn apply_orientation(img: DynamicImage, orientation: u32) -> DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(), // transpose
        6 => img.rotate90(),
        7 => img.rotate270().fliph(), // transverse
        8 => img.rotate270(),
        _ => img,
    }
}

/// 读 EXIF：(orientation, 是否有相机信息, 拍摄时间)。
/// 仅 JPEG 容器（kamadak-exif 限制）；失败返回 None。HEIC 走 ImageIO 属性。
fn read_exif(path: &Path) -> Option<(Option<u32>, bool, Option<String>)> {
    let file = std::fs::File::open(path).ok()?;
    let mut buf = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut buf).ok()?;

    let orientation = exif
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0));

    let camera = exif
        .get_field(exif::Tag::Make, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::Model, exif::In::PRIMARY))
        .is_some();

    let taken_at = exif
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .map(|f| f.display_value().to_string())
        .and_then(|s| {
            // 只接受定宽合法格式，保证字典序可比
            let s = s.trim().to_string();
            if s.len() == 19 && s.as_bytes()[4] == b':' {
                Some(s)
            } else {
                None
            }
        });

    Some((orientation, camera, taken_at))
}

// ---------------------------------------------------------------- HEIC / HEIF

/// HEIC/HEIF 走 macOS 系统 ImageIO（docs/04 阶段 2）：
/// 零 C 依赖解码 iPhone 核心格式；EXIF 方向与拍摄时间也从 ImageIO 属性拿
/// （kamadak-exif 读不了 HEIF 容器）。
#[cfg(target_os = "macos")]
mod heic {
    use super::*;
    use objc2_core_foundation::{
        CFDictionary, CFNumber, CFNumberType, CFString, CFType, CGPoint, CGRect, CGSize, CFURL,
    };
    use objc2_core_graphics::{
        CGBitmapContextCreateWithData, CGBitmapContextGetData, CGColorSpace, CGContext, CGImage,
        CGImageAlphaInfo, CGImageByteOrderInfo,
    };
    use objc2_image_io::{
        kCGImagePropertyExifDateTimeOriginal, kCGImagePropertyExifDictionary,
        kCGImagePropertyOrientation, kCGImagePropertyTIFFDictionary, kCGImagePropertyTIFFMake,
        kCGImagePropertyTIFFModel, CGImageSource,
    };
    use std::ptr;

    pub fn decode(path: &Path) -> Option<Decoded> {
        // SAFETY: 所有指针的生命周期都被 CFRetained / 局部 buffer 约束。
        unsafe { decode_heic(path) }
    }

    unsafe fn decode_heic(path: &Path) -> Option<Decoded> {
        // 1. 路径 → CFURL（ImageIO 自己流式读文件，不整载内存）
        let bytes = path.as_os_str().as_encoded_bytes();
        let url = CFURL::from_file_system_representation(
            None,
            bytes.as_ptr(),
            bytes.len() as isize,
            false,
        )?;

        // 2. ImageIO 打开源
        let source = CGImageSource::with_url(&url, None)?;

        // 3. 属性：orientation / 相机信息 / 拍摄时间
        let (orientation, camera, taken_at) = source
            .properties_at_index(0, None)
            .map(|p| read_props(&p))
            .unwrap_or((1, false, None));

        // 4. 解码像素
        let cg_image = source.image_at_index(0, None)?;
        let w = CGImage::width(Some(&cg_image));
        let h = CGImage::height(Some(&cg_image));
        if w == 0 || h == 0 || w > 100_000 || h > 100_000 {
            return None; // 空图 / 异常尺寸防御
        }

        // 5. 画进 RGBA 位图上下文（kCGImageAlphaPremultipliedLast | ByteOrder32Big
        //    = 每像素 4 字节 R,G,B,A）。照片主体像素 alpha=255，预乘无影响；
        //    透明区 RGB=0 但 alpha=0 —— 合成白底/主体掩码逻辑不受干扰。
        let color_space = CGColorSpace::new_device_rgb()?;
        let mut buf = vec![0u8; w * h * 4];
        let bitmap_info =
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
        let ctx = CGBitmapContextCreateWithData(
            buf.as_mut_ptr().cast::<std::ffi::c_void>(),
            w,
            h,
            8,
            w * 4,
            Some(&color_space),
            bitmap_info,
            None, // 无释放回调：buffer 由我们持有
            ptr::null_mut(),
        )?;
        let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
        CGContext::draw_image(Some(&ctx), rect, Some(&cg_image));

        // 6. 确认上下文写入了数据。GetData 返回的指针就是步骤 5 传入的
        //    buffer 本身（ImageIO 直接画进调用方内存），无需再拷贝。
        let data = CGBitmapContextGetData(Some(&ctx));
        if data.is_null() {
            return None;
        }

        let img = image::RgbaImage::from_raw(w as u32, h as u32, buf)?;
        let img = image::DynamicImage::ImageRgba8(img);
        Some(Decoded {
            img: apply_orientation(img, orientation),
            orientation,
            camera,
            taken_at,
        })
    }

    /// 从 ImageIO 属性字典读 (orientation, 相机信息, 拍摄时间)。
    unsafe fn read_props(props: &CFDictionary) -> (u32, bool, Option<String>) {
        let orientation = value_of(props, kCGImagePropertyOrientation)
            .and_then(|v| v.downcast_ref::<CFNumber>())
            .and_then(|n| number_to_u32(n))
            .unwrap_or(1);

        let tiff = value_of(props, kCGImagePropertyTIFFDictionary)
            .and_then(|v| v.downcast_ref::<CFDictionary>());
        let camera = tiff
            .map(|d| {
                value_of(d, kCGImagePropertyTIFFMake).is_some()
                    || value_of(d, kCGImagePropertyTIFFModel).is_some()
            })
            .unwrap_or(false);

        let taken_at = value_of(props, kCGImagePropertyExifDictionary)
            .and_then(|v| v.downcast_ref::<CFDictionary>())
            .and_then(|d| value_of(d, kCGImagePropertyExifDateTimeOriginal))
            .and_then(|v| v.downcast_ref::<CFString>())
            .map(|s| s.to_string())
            .and_then(|s| {
                // 与 JPEG 路径同一套定宽校验，保证字典序可比
                let s = s.trim().to_string();
                if s.len() == 19 && s.as_bytes()[4] == b':' {
                    Some(s)
                } else {
                    None
                }
            });

        (orientation, camera, taken_at)
    }

    /// 字典取值 → CFType 引用（生命周期绑定字典本身，调用方持有字典即可）。
    unsafe fn value_of<'a>(dict: &'a CFDictionary, key: &'static CFString) -> Option<&'a CFType> {
        let ptr = dict.value(ptr::from_ref(key).cast());
        if ptr.is_null() {
            None
        } else {
            Some(&*ptr.cast::<CFType>())
        }
    }

    unsafe fn number_to_u32(n: &CFNumber) -> Option<u32> {
        let mut v: i64 = 0;
        if n.value(CFNumberType::SInt64Type, &mut v as *mut i64 as *mut _) {
            Some(v.clamp(1, 9) as u32) // EXIF orientation 合法域 1~9
        } else {
            None
        }
    }
}

/// ponytail: HEIC 仅承诺 macOS（系统 ImageIO）；其他平台的解码边界到 v2 再接。
#[cfg(not(target_os = "macos"))]
mod heic {
    use super::*;

    pub fn decode(_path: &Path) -> Option<Decoded> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientation_swap_dimensions() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            100,
            50,
            image::Rgb([200, 200, 200]),
        ));
        let rotated = apply_orientation(img, 6);
        assert_eq!((rotated.width(), rotated.height()), (50, 100));
    }

    #[test]
    fn heic_extension_is_routed() {
        assert!(is_heic(Path::new("/a/b/photo.HEIC")));
        assert!(is_heic(Path::new("/a/b/photo.heif")));
        assert!(!is_heic(Path::new("/a/b/photo.jpg")));
    }

    #[test]
    fn undecodable_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("broken.png");
        std::fs::write(&p, b"not an image").unwrap();
        assert!(decode(&p).is_none());
    }

    /// 真实 HEIC 解码验收测试（docs/04 阶段 2）：
    ///
    /// 1. ImageIO 能解出正确尺寸的像素；
    /// 2. **同一张图的 HEIC 版与 JPEG 版指纹距离必须在最严格档以内**
    ///    —— 这是「HEIC 参与相似去重」的意义所在；
    /// 3. 缩略图能生成（与指纹共用同一解码边界）。
    ///
    /// fixture 用系统 `sips` 现场把 JPEG 转成 HEIC（测试机即 macOS）；
    /// sips 不可用的极端环境跳过而非失败。
    #[test]
    fn heic_decode_matches_jpeg_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let jpg = dir.path().join("photo.jpg");
        let heic = dir.path().join("photo.heic");

        // 照片式素材：交替色块（纯渐变的 DCT 低频近乎全 0，哈希不稳定）
        let mut img = image::RgbImage::new(96, 72);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = if (x / 12 + y / 9) % 2 == 0 { 210 } else { 55 };
            *p = image::Rgb([v, v, v / 2]);
        }
        image::DynamicImage::ImageRgb8(img).save(&jpg).unwrap();

        let ok = std::process::Command::new("sips")
            .args(["-s", "format", "heic"])
            .arg(&jpg)
            .arg("--out")
            .arg(&heic)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok || !heic.exists() {
            eprintln!("[skip] sips 不可用，跳过 HEIC 真实解码测试");
            return;
        }

        let j = decode(&jpg).expect("JPEG 应可解码");
        let h = decode(&heic).expect("HEIC 应可经 ImageIO 解码");
        assert_eq!((h.img.width(), h.img.height()), (96, 72));

        // 验收核心：HEIC 版与 JPEG 版是「真重复」，pHash 距离须在严格档以内
        let dj = crate::finger::phash_64(&j.img.to_rgb8());
        let dh = crate::finger::phash_64(&h.img.to_rgb8());
        let dist = crate::finger::hamming(dj, dh);
        assert!(
            dist <= crate::cluster::THRESHOLDS[0].phash,
            "HEIC↔JPEG 指纹距离 {dist} 超过严格档 {}，去重会漏掉 HEIC",
            crate::cluster::THRESHOLDS[0].phash
        );

        // 缩略图走同一 decode 入口，也必须能出图
        let thumb = crate::thumbs::ensure_thumb(&heic, 1, &dir.path().join("thumbs"));
        assert!(thumb.is_some(), "HEIC 缩略图应可生成");
    }
}
