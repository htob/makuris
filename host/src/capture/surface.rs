//! macOS 专用：屏幕像素源——整屏采集 BGRA 像素，直接写入**调用方提供**的 `IOSurface`。
//!
//! 为什么另起一条采集路径：`CGWindowListCreateImage` 返回的 CGImage 本来就是 BGRA 布局，
//! 而 VideoToolbox 恰好吃 BGRA 的 IOSurface——按行搬运即可，省掉一整趟
//! 逐像素 BGRA→RGB→YUV 转换，那正是此前采集开销的大头。
//!
//! 为什么不自己持有 surface：本模块被设计成流水线的一环。采集线程把像素写进一块 surface
//! 后，把 surface 的**所有权**交给编码线程，编码完再归还——这样采集与编码能真正重叠执行。
//! 实测采集（约 27ms，主要是 CGWindowListCreateImage 的系统合成开销，与像素数几乎无关）
//! 与硬编（约 15ms）串行要 42ms，只有 24fps；并行后瓶颈回到 27ms，30fps 有余量。
//!
//! 用 `kCGWindowImageBestResolution` 让系统直接给出 2x 物理像素（3024x1964），
//! 与目标 surface 尺寸一致时走纯 memcpy，不做任何缩放插值——细节一个像素都不丢。

use anyhow::{anyhow, Result};
use apple_cf::iosurface::{IOSurface, IOSurfaceLockOptions};
use core_graphics::display::CGDisplay;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window::{
    kCGWindowImageBestResolution, kCGWindowImageNominalResolution, kCGWindowListOptionOnScreenOnly,
    CGWindowListCreateImage,
};
use std::os::raw::c_void;

/// BGRA 四字符码（kCVPixelFormatType_32BGRA）。
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

// ---------- 裸 FFI：直接读 CGImage 像素 ----------
// 不构造 core-graphics 的 CGImage 包装类型：它是 foreign_type，from_ptr 需要
// foreign_types trait，而那不是本项目的直接依赖。

#[repr(C)]
struct CGImageOpaque {
    _p: [u8; 0],
}
type CGImageRef = *mut CGImageOpaque;

#[repr(C)]
struct CGDataProviderOpaque {
    _p: [u8; 0],
}
type CGDataProviderRef = *mut CGDataProviderOpaque;

#[repr(C)]
struct CFDataOpaque {
    _p: [u8; 0],
}
type CFDataRef = *mut CFDataOpaque;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGImageGetWidth(img: CGImageRef) -> usize;
    fn CGImageGetHeight(img: CGImageRef) -> usize;
    fn CGImageGetBytesPerRow(img: CGImageRef) -> usize;
    fn CGImageGetDataProvider(img: CGImageRef) -> CGDataProviderRef;
    fn CGImageRelease(img: CGImageRef);
    fn CGDataProviderCopyData(provider: CGDataProviderRef) -> CFDataRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    fn CFDataGetLength(data: CFDataRef) -> isize;
    fn CFRelease(cf: *const c_void);
}

/// 分配一块 BGRA 的 `IOSurface`，作为采集/编码之间的帧载体。
pub fn create_surface(width: usize, height: usize) -> Result<IOSurface> {
    IOSurface::create(width, height, PIXEL_FORMAT_BGRA, 4)
        .ok_or_else(|| anyhow!("IOSurface 分配失败 {width}x{height}"))
}

/// 整屏像素源：把屏幕内容搬进调用方给的 `IOSurface`。
pub struct CgPixelSource {
    width: usize,
    height: usize,
    screen_w: f64,
    screen_h: f64,
    /// true 用 BestResolution（2x 物理像素），false 用 NominalResolution（1x 逻辑像素）
    use_best_resolution: bool,
}

impl CgPixelSource {
    /// `width`/`height` 为目标像素尺寸；`use_best_resolution` 决定采集倍率，
    /// 例如屏幕逻辑 1512x982 在 2x 下应传 3024x1964 + true。
    pub fn new(width: usize, height: usize, use_best_resolution: bool) -> Result<Self> {
        let bounds = CGDisplay::main().bounds();
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return Err(anyhow!("主屏 bounds 非法"));
        }
        tracing::info!(
            "[capture] 像素源就绪：主屏逻辑 {}x{} → {}x{}（{}）",
            bounds.size.width,
            bounds.size.height,
            width,
            height,
            if use_best_resolution {
                "2x 物理像素"
            } else {
                "1x 逻辑像素"
            }
        );
        Ok(Self {
            width,
            height,
            screen_w: bounds.size.width,
            screen_h: bounds.size.height,
            use_best_resolution,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// 采集一帧，像素写入 `surface`（BGRA）。
    pub fn capture_into(&mut self, surface: &IOSurface) -> Result<()> {
        // 整屏：完整显示器边界，不做任何裁切——远程桌面必须能看到边缘图标
        let rect = CGRect::new(
            &CGPoint::new(0.0, 0.0),
            &CGSize::new(self.screen_w, self.screen_h),
        );
        let flags = if self.use_best_resolution {
            kCGWindowImageBestResolution
        } else {
            kCGWindowImageNominalResolution
        };

        let img_ref = unsafe {
            CGWindowListCreateImage(rect, kCGWindowListOptionOnScreenOnly, 0, flags)
        };
        if img_ref.is_null() {
            return Err(anyhow!(
                "CGWindowListCreateImage 返回 null（可能缺屏幕录制权限）"
            ));
        }
        let img = img_ref as CGImageRef;
        let result = unsafe { self.blit_into_surface(img, surface) };
        unsafe { CGImageRelease(img) };
        result
    }

    /// 把 CGImage 的 BGRA 像素搬进 surface。
    /// 尺寸一致时是纯 memcpy（零转换、零插值）；不一致时按最近邻缩放。
    unsafe fn blit_into_surface(&mut self, img: CGImageRef, surface: &IOSurface) -> Result<()> {
        let sw = CGImageGetWidth(img);
        let sh = CGImageGetHeight(img);
        let src_bpr = CGImageGetBytesPerRow(img);
        if sw == 0 || sh == 0 || src_bpr == 0 {
            return Err(anyhow!("CGImage 尺寸为 0"));
        }

        let provider = CGImageGetDataProvider(img);
        if provider.is_null() {
            return Err(anyhow!("CGImageGetDataProvider 返回 null"));
        }
        let cf_data = CGDataProviderCopyData(provider);
        if cf_data.is_null() {
            return Err(anyhow!("CGDataProviderCopyData 返回 null"));
        }
        let src_ptr = CFDataGetBytePtr(cf_data);
        let src_len = CFDataGetLength(cf_data) as usize;
        if src_ptr.is_null() || src_len == 0 {
            CFRelease(cf_data as *const c_void);
            return Err(anyhow!("CGImage 像素数据为空"));
        }

        let src = std::slice::from_raw_parts(src_ptr, src_len);
        let (tw, th) = (self.width, self.height);
        let dst_bpr = surface.bytes_per_row();

        let outcome = (|| -> Result<()> {
            let mut guard = surface
                .lock(IOSurfaceLockOptions::NONE)
                .map_err(|c| anyhow!("IOSurface 加锁失败: {c}"))?;
            let dst = guard
                .as_slice_mut()
                .ok_or_else(|| anyhow!("IOSurface 映射为空"))?;

            let row_bytes = (tw * 4).min(src_bpr).min(dst_bpr);

            if sw == tw && sh == th {
                // 1:1 快路径：整行搬运，无任何像素运算
                for y in 0..th {
                    let s = y * src_bpr;
                    let d = y * dst_bpr;
                    if s + row_bytes > src_len || d + row_bytes > dst.len() {
                        return Err(anyhow!("像素数据越界（源 {src_len} 字节，行 {y}）"));
                    }
                    dst[d..d + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
                }
            } else {
                // 最近邻缩放路径（尺寸不符时的兜底，保证画面正确而非崩溃）
                for y in 0..th {
                    let sy = (y * sh) / th;
                    let s_row = sy * src_bpr;
                    let d_row = y * dst_bpr;
                    for x in 0..tw {
                        let sx = (x * sw) / tw;
                        let si = s_row + sx * 4;
                        let di = d_row + x * 4;
                        if si + 4 > src_len || di + 4 > dst.len() {
                            continue;
                        }
                        dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
                    }
                }
            }
            Ok(())
        })();

        CFRelease(cf_data as *const c_void);
        outcome
    }
}