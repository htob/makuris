//! 屏幕采集抽象层。
//!
//! 两条路径，按平台分工：
//! - **macOS（当前生产路径）**：`surface` 子模块——整屏采集直接写入 BGRA `IOSurface`，
//!   交给 VideoToolbox 硬编。见 encode/pipeline.rs。
//! - **其他平台（尚未接入）**：本文件的 `ScreenCapturer` 抽象与 `NullCapturer`，
//!   配合 encode 里的 openh264 软编路径使用。Windows/Linux 适配时从这里开始。
//!
//! 注意：绝不可按目标宽高比裁切屏幕区域——远程桌面必须呈现完整桌面，
//! 否则用户看不到边缘图标、无法操作其他窗口。

// 本文件的 RGB 采集路径当前仅 macOS 之外使用，macOS 生产链路走 surface 子模块。
#![allow(dead_code)]

/// 一帧屏幕数据（RGB，每像素 3 字节，行优先，长度 = width*height*3）。
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// macOS 专用：整屏采集直接写入 BGRA IOSurface，供 VideoToolbox 硬编零转换消费。
/// 这是 2x 物理分辨率路径的采集端——省掉 RGB 转换与缩放插值。
#[cfg(target_os = "macos")]
pub mod surface;

/// Windows 专用：GDI BitBlt 采集虚拟桌面全范围（先用 GDI 打通链路，理由见该文件）。
/// 第二个条件让本机能用 `--features windows_typecheck` 对它做类型检查。
#[cfg(any(target_os = "windows", feature = "windows_typecheck"))]
pub mod windows;

/// 屏幕采集器 trait。
pub trait ScreenCapturer: Send {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// 采集一帧，返回 RGB 数据（长度 = width*height*3）。
    fn capture_frame(&mut self) -> anyhow::Result<Frame>;
}

/// 被控端真实屏幕尺寸（仅非 macOS 需要）。
///
/// macOS 用固定 3024x1964：那是本机屏幕的 2x 物理像素尺寸，
/// 采集侧有 surface 池与之绑定。Windows 则不该写死这个值——
/// 台式机的屏幕尺寸未知，且把 1920x1080 放大到 3024x1964 毫无收益。
/// 返回 None 表示"无法探测"，调用方应回退到默认尺寸。
#[cfg(not(target_os = "macos"))]
pub fn detect_screen_size() -> Option<(usize, usize)> {
    #[cfg(target_os = "windows")]
    {
        // 必须先声明 DPI 感知再读尺寸，否则缩放显示器上会拿到虚拟坐标（只有一半像素）
        windows::ensure_dpi_aware();
        if let Some((w, h)) = windows::screen_size() {
            return Some((w as usize, h as usize));
        }
    }
    None
}

/// 平台工厂：macOS 走 CGWindowListCreateImage，Windows 走 GDI，
/// 初始化失败或未支持的平台回退测试图案。
pub fn create_capturer(width: u32, height: u32) -> Box<dyn ScreenCapturer> {
    #[cfg(target_os = "macos")]
    {
        match cg_display::CgDisplayCapturer::new(width, height) {
            Ok(c) => {
                tracing::info!("[capture] 真实屏幕采集（整屏）{}x{}", width, height);
                return Box::new(c);
            }
            Err(e) => tracing::warn!(
                "[capture] 采集器初始化失败({})，回退测试图案（检查「屏幕录制」权限）",
                e
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        match windows::GdiCapturer::new(width, height) {
            Ok(c) => {
                tracing::info!("[capture] GDI 真实屏幕采集 {}x{}", width, height);
                return Box::new(c);
            }
            Err(e) => {
                tracing::warn!("[capture] GDI 采集器初始化失败({})，回退测试图案", e)
            }
        }
    }
    Box::new(NullCapturer::new(width, height))
}

/// 测试图案采集器：移动彩条（RGB），无真实采集时验证管线用。
pub struct NullCapturer {
    width: u32,
    height: u32,
    counter: u64,
}

impl NullCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, counter: 0 }
    }
}

impl ScreenCapturer for NullCapturer {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn capture_frame(&mut self) -> anyhow::Result<Frame> {
        self.counter += 1;
        let (w, h) = (self.width as usize, self.height as usize);
        let mut rgb = vec![0u8; w * h * 3];
        let offset = (self.counter * 5) as usize;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                let bar = ((x + offset) / 80) % 6;
                let (r, g, b) = match bar {
                    0 => (255u8, 255u8, 255u8),
                    1 => (255, 255, 0),
                    2 => (0, 255, 255),
                    3 => (0, 255, 0),
                    4 => (255, 0, 255),
                    _ => (255, 0, 0),
                };
                rgb[i] = r;
                rgb[i + 1] = g;
                rgb[i + 2] = b;
            }
        }
        Ok(Frame { width: self.width, height: self.height, data: rgb })
    }
}

/// macOS 采集器：CGWindowListCreateImage 采集**整屏**（显示器完整边界，无裁切）。
///
/// 两点关键设计：
/// 1. 用 `kCGWindowImageNominalResolution` 让系统直接产出 1x 尺寸的图（如 1512x982），
///    比 BestResolution（3024x1964，约 23MB）少拷 4 倍数据，采集快得多；
///    再在 CPU 最近邻缩放到目标尺寸（如 1280x720）。
/// 2. 用裸 FFI 读像素，不构造 core-graphics 的 `CGImage` 包装类型——
///    该类型是 foreign_type，`from_ptr` 需要 `foreign_types` trait，而它并非直接依赖。
#[cfg(target_os = "macos")]
mod cg_display {
    use super::{Frame, ScreenCapturer};
    use anyhow::{anyhow, Result};
    use core_graphics::display::CGDisplay;
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};
    use core_graphics::window::{
        kCGWindowImageNominalResolution, kCGWindowListOptionOnScreenOnly, CGWindowListCreateImage,
    };
    use std::os::raw::c_void;

    // ---------- 裸 FFI：直接读 CGImage 像素 ----------

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

    /// 把 CGImage 像素读出来（BGRA），最近邻缩放到 tw×th，写成 RGB。
    /// 调用者负责释放 img。
    unsafe fn read_scaled_rgb(img: CGImageRef, tw: usize, th: usize, rgb: &mut [u8]) -> Result<()> {
        let sw = CGImageGetWidth(img);
        let sh = CGImageGetHeight(img);
        let bpr = CGImageGetBytesPerRow(img);
        if sw == 0 || sh == 0 || bpr == 0 {
            return Err(anyhow!("CGImage 尺寸为 0"));
        }

        let provider = CGImageGetDataProvider(img);
        if provider.is_null() {
            return Err(anyhow!("CGImageGetDataProvider 返回 null"));
        }
        let data = CGDataProviderCopyData(provider);
        if data.is_null() {
            return Err(anyhow!("CGDataProviderCopyData 返回 null"));
        }
        let src = CFDataGetBytePtr(data);
        let len = CFDataGetLength(data) as usize;
        if src.is_null() || len == 0 {
            CFRelease(data as *const c_void);
            return Err(anyhow!("CGImage 像素数据为空"));
        }

        // 预计算每列源偏移，避免内层循环乘法
        let mut col_off = vec![0usize; tw];
        for (dx, item) in col_off.iter_mut().enumerate() {
            *item = ((dx * sw) / tw) * 4;
        }

        let src_bytes = std::slice::from_raw_parts(src, len);

        // 快速路径：采集尺寸与目标一致（1:1，如 1512x982 → 1512x982）时直接拷贝，
        // 跳过全部缩放除法计算，只做 BGRA→RGB 通道重排。
        if sw == tw && sh == th && bpr == tw * 4 {
            for dy in 0..th {
                let s_row = &src_bytes[dy * bpr..dy * bpr + tw * 4];
                let d_row = &mut rgb[dy * tw * 3..dy * tw * 3 + tw * 3];
                for (d, s) in d_row.chunks_exact_mut(3).zip(s_row.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                }
            }
            CFRelease(data as *const c_void);
            return Ok(());
        }

        // 逐行缩放 + BGRA→RGB。
        // 目标端用 chunks_exact_mut(3) 让 3 字节内的索引在编译期确定（消除检查）；
        // 源端把边界校验提到行循环外只做一次，去掉原实现的逐像素 if 分支。
        for dy in 0..th {
            let sy = (dy * sh) / th;
            let row_base = sy * bpr;
            if row_base + col_off[tw - 1] + 2 >= len {
                break; // 源数据不完整，丢弃剩余行
            }
            let dst = &mut rgb[dy * tw * 3..dy * tw * 3 + tw * 3];
            for (dx, d) in dst.chunks_exact_mut(3).enumerate() {
                let si = row_base + col_off[dx];
                // BGRA（小端 ARGB）→ RGB
                d[0] = src_bytes[si + 2];
                d[1] = src_bytes[si + 1];
                d[2] = src_bytes[si];
            }
        }

        CFRelease(data as *const c_void);
        Ok(())
    }

    pub struct CgDisplayCapturer {
        width: u32,
        height: u32,
        /// 主屏逻辑尺寸（= 整屏采集区域）
        screen_w: f64,
        screen_h: f64,
    }

    impl CgDisplayCapturer {
        pub fn new(width: u32, height: u32) -> Result<Self> {
            let bounds = CGDisplay::main().bounds();
            if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
                return Err(anyhow!("主屏 bounds 非法"));
            }
            tracing::info!(
                "[capture] 整屏采集：主屏逻辑 {}x{}（左下原点 ({},{})）→ 缩放到 {}x{}",
                bounds.size.width,
                bounds.size.height,
                bounds.origin.x,
                bounds.origin.y,
                width,
                height
            );
            Ok(Self {
                width,
                height,
                screen_w: bounds.size.width,
                screen_h: bounds.size.height,
            })
        }
    }

    impl ScreenCapturer for CgDisplayCapturer {
        fn width(&self) -> u32 {
            self.width
        }
        fn height(&self) -> u32 {
            self.height
        }

        fn capture_frame(&mut self) -> Result<Frame> {
            let (tw, th) = (self.width as usize, self.height as usize);

            // 整屏：完整显示器边界，不做任何裁切
            let rect = CGRect::new(
                &CGPoint::new(0.0, 0.0),
                &CGSize::new(self.screen_w, self.screen_h),
            );

            let img_ref = unsafe {
                CGWindowListCreateImage(
                    rect,
                    kCGWindowListOptionOnScreenOnly,
                    0,
                    kCGWindowImageNominalResolution,
                )
            };
            if img_ref.is_null() {
                return Err(anyhow!(
                    "CGWindowListCreateImage 返回 null（可能缺屏幕录制权限）"
                ));
            }
            let img = img_ref as CGImageRef;

            let mut rgb = vec![0u8; tw * th * 3];
            let r = unsafe { read_scaled_rgb(img, tw, th, &mut rgb) };
            unsafe { CGImageRelease(img) };
            r?;

            Ok(Frame { width: self.width, height: self.height, data: rgb })
        }
    }
}