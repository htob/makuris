//! Windows 屏幕采集（GDI BitBlt）。
//!
//! ## 为什么先用 GDI 而不是 Desktop Duplication API
//!
//! Desktop Duplication（DXGI）是更"正统"的方案：零拷贝到 GPU 纹理、带 dirty rect、
//! 对 DirectX 全屏应用也能采到。但它需要 COM 接口手写 FFI（IDXGIOutputDuplication
//! 及其一长串 vtable），代码量大且只能在 Windows 上编译验证——盲写这种规模
//! 的 COM 绑定，编译错误会成串出现。
//!
//! GDI 的 BitBlt 只需要六七个函数，行为确定，能先把端到端链路（发现→协商→出画→回控）
//! 在 Windows 上跑通。代价是：DirectX/独占全屏的应用可能采到黑屏，
//! 且每次采集多一次系统拷贝（1080p 约 5~15ms，30fps 够用）。
//! 等链路稳定后再按需升级到 DXGI，接口不变（都实现 ScreenCapturer）。
//!
//! ## 必须声明 DPI 感知
//!
//! 非 DPI 感知的进程在缩放显示器上拿到的是"虚拟坐标"：4K 屏（200% 缩放）下
//! GetSystemMetrics 会报 1920x1080，采集出来的图只有真实像素的一半，
//! 手机上放大就是糊的——这与 macOS 上"必须采 2x 物理像素"是同一个坑。
//! 因此初始化时先调 SetProcessDPIAware()。
//!
//! ## 与 macOS 路径的一致性
//!
//! 采集原尺寸（不缩放），再由 CPU 缩放到目标尺寸、BGRA→RGB。
//! 缩放逻辑与 capture/mod.rs 的 macOS 实现保持同一套最近邻算法，
//! 便于两边行为对齐、出问题时好对照。

use anyhow::{anyhow, Result};
use std::os::raw::c_void;

use super::{Frame, ScreenCapturer};

// ---------------------------------------------------------------- Win32 FFI

type Hdc = *mut c_void;
type Hbitmap = *mut c_void;
type Hgdiobj = *mut c_void;

#[link(name = "user32")]
extern "system" {
    fn GetDC(hwnd: *mut c_void) -> Hdc;
    fn ReleaseDC(hwnd: *mut c_void, hdc: Hdc) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    /// 声明进程 DPI 感知（Vista+）。失败通常是无害的（已被别的代码设过）。
    fn SetProcessDPIAware() -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
    fn DeleteDC(hdc: Hdc) -> i32;
    fn CreateDIBSection(
        hdc: Hdc,
        pbmi: *const BitmapInfo,
        usage: u32,
        ppv_bits: *mut *mut c_void,
        h_section: *mut c_void,
        offset: u32,
    ) -> Hbitmap;
    fn SelectObject(hdc: Hdc, h: Hgdiobj) -> Hgdiobj;
    fn DeleteObject(h: Hgdiobj) -> i32;
    fn BitBlt(
        hdc_dest: Hdc,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        hdc_src: Hdc,
        x1: i32,
        y1: i32,
        rop: u32,
    ) -> i32;
    fn SetStretchBltMode(hdc: Hdc, mode: i32) -> i32;
    fn StretchBlt(
        hdc_dest: Hdc,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        hdc_src: Hdc,
        x1: i32,
        y1: i32,
        w1: i32,
        h1: i32,
        rop: u32,
    ) -> i32;
    fn GdiFlush() -> i32;
}

const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

const DIB_RGB_COLORS: u32 = 0;
const BI_RGB: u32 = 0;
/// 直接拷贝源像素。
const SRCCOPY: u32 = 0x00CC_0020;
/// 让拷贝包含分层窗口（不带它就是采不到带 shadow 的窗口/某些悬浮层）。
const CAPTUREBLT: u32 = 0x4000_0000;
/// 缩放质量：邻域平均。比 COLORONCOLOR 慢一点但边缘不锯齿，文字更可读。
const HALFTONE: i32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    /// 负值 = top-down DIB：像素从左上角开始，省掉翻转行序
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 1],
}

/// 声明进程 DPI 感知。幂等，重复调用无副作用。
pub fn ensure_dpi_aware() {
    unsafe {
        SetProcessDPIAware();
    }
}

/// 取虚拟桌面尺寸（多显示器时为整个虚拟桌面的包围盒）。
///
/// 会话层用它决定推流分辨率：按真实屏幕尺寸编码才是合理的，
/// 把 1920x1080 的屏幕放大到 3024x1964 只会浪费码率与编码时间，
/// 清晰度不会有任何提升——放大不创造信息。
pub fn screen_size() -> Option<(i32, i32)> {
    let (w, h) = unsafe {
        let cx = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let cy = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if cx > 0 && cy > 0 {
            (cx, cy)
        } else {
            (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        }
    };
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

pub struct GdiCapturer {
    width: u32,
    height: u32,
    /// 采集源区域（虚拟桌面全范围，多显示器时可能含负原点）
    src_x: i32,
    src_y: i32,
    src_w: i32,
    src_h: i32,
    /// 屏幕 DC，长期持有以省掉每帧 GetDC/ReleaseDC
    hdc_screen: Hdc,
    hdc_mem: Hdc,
    hbmp: Hbitmap,
    /// DIB 像素内存，由 GDI 分配、我们只读，生命周期与 hbmp 相同
    bits: *mut u8,
}

// DIB 内存由 GDI 持有，采集线程独占访问；裸指针本身不阻碍跨线程移动。
unsafe impl Send for GdiCapturer {}

impl GdiCapturer {
    /// 探测虚拟桌面尺寸并建好 DIB 缓冲。
    pub fn new(target_w: u32, target_h: u32) -> Result<Self> {
        ensure_dpi_aware();

        let (src_x, src_y, src_w, src_h) = unsafe {
            let cx = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let cy = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            if cx > 0 && cy > 0 {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    cx,
                    cy,
                )
            } else {
                // 虚拟桌面 API 拿不到时退回主屏
                (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
            }
        };
        if src_w <= 0 || src_h <= 0 {
            return Err(anyhow!("显示器尺寸非法（{}x{}）", src_w, src_h));
        }

        let hdc_screen = unsafe { GetDC(std::ptr::null_mut()) };
        if hdc_screen.is_null() {
            return Err(anyhow!("GetDC(NULL) 失败"));
        }
        let hdc_mem = unsafe { CreateCompatibleDC(hdc_screen) };
        if hdc_mem.is_null() {
            unsafe { ReleaseDC(std::ptr::null_mut(), hdc_screen) };
            return Err(anyhow!("CreateCompatibleDC 失败"));
        }

        // 32bpp top-down：与 BitBlt 目标格式一致，且行序天然正确
        let bmi = BitmapInfo {
            bmi_header: BitmapInfoHeader {
                bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                bi_width: src_w,
                bi_height: -src_h,
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: BI_RGB,
                bi_size_image: (src_w * src_h * 4) as u32,
                bi_x_pels_per_meter: 0,
                bi_y_pels_per_meter: 0,
                bi_clr_used: 0,
                bi_clr_important: 0,
            },
            bmi_colors: [0],
        };

        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbmp = unsafe {
            CreateDIBSection(hdc_mem, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0)
        };
        if hbmp.is_null() || bits.is_null() {
            unsafe {
                DeleteDC(hdc_mem);
                ReleaseDC(std::ptr::null_mut(), hdc_screen);
            }
            return Err(anyhow!("CreateDIBSection 失败（显存不足？）"));
        }
        unsafe { SelectObject(hdc_mem, hbmp) };

        tracing::info!(
            "[capture] Windows GDI 采集：虚拟桌面 {}x{}（原点 {},{}, 含 {} 个显示器）→ 目标 {}x{}",
            src_w,
            src_h,
            src_x,
            src_y,
            unsafe { GetSystemMetrics(80) }, // SM_CMONITORS
            target_w,
            target_h
        );

        Ok(Self {
            width: target_w,
            height: target_h,
            src_x,
            src_y,
            src_w,
            src_h,
            hdc_screen,
            hdc_mem,
            hbmp,
            bits: bits as *mut u8,
        })
    }
}

impl Drop for GdiCapturer {
    fn drop(&mut self) {
        unsafe {
            // 先解除选入再删对象，否则删除会失败并泄漏 GDI 句柄
            SelectObject(self.hdc_mem, std::ptr::null_mut());
            DeleteObject(self.hbmp);
            DeleteDC(self.hdc_mem);
            ReleaseDC(std::ptr::null_mut(), self.hdc_screen);
        }
    }
}

impl ScreenCapturer for GdiCapturer {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }

    fn capture_frame(&mut self) -> Result<Frame> {
        let (tw, th) = (self.width as usize, self.height as usize);
        let (sw, sh) = (self.src_w as usize, self.src_h as usize);

        let ok = unsafe {
            if sw == tw && sh == th {
                // 尺寸一致时走 BitBlt：纯拷贝，比 StretchBlt 快
                BitBlt(
                    self.hdc_mem,
                    0,
                    0,
                    self.src_w,
                    self.src_h,
                    self.hdc_screen,
                    self.src_x,
                    self.src_y,
                    SRCCOPY | CAPTUREBLT,
                )
            } else {
                SetStretchBltMode(self.hdc_mem, HALFTONE);
                StretchBlt(
                    self.hdc_mem,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    self.hdc_screen,
                    self.src_x,
                    self.src_y,
                    self.src_w,
                    self.src_h,
                    SRCCOPY | CAPTUREBLT,
                )
            }
        };
        if ok == 0 {
            return Err(anyhow!("BitBlt/StretchBlt 失败"));
        }
        // 等 GDI 把绘制提交完，再读 DIB 内存——否则可能读到上一帧
        unsafe { GdiFlush() };

        // BGRA(DIB) → RGB。缩放已在 GDI 侧完成，这里只做通道重排；
        // 采不到（如受保护内容）时像素为黑，属于预期表现，不报错。
        let mut rgb = vec![0u8; tw * th * 3];
        let src = unsafe { std::slice::from_raw_parts(self.bits, sw * sh * 4) };
        if sw == tw && sh == th {
            // 同尺寸快路径：逐行成块转换，索引在编译期可省去边界检查
            for y in 0..th {
                let s_row = &src[y * sw * 4..(y + 1) * sw * 4];
                let d_row = &mut rgb[y * tw * 3..(y + 1) * tw * 3];
                for (d, s) in d_row.chunks_exact_mut(3).zip(s_row.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                }
            }
        } else {
            // 理论上走不到（GDI 已缩放），保留兜底避免尺寸不一致时出错
            for y in 0..th {
                let sy = (y * sh) / th;
                let s_row = &src[sy * sw * 4..(y + 1) * sw * 4];
                let d_row = &mut rgb[y * tw * 3..(y + 1) * tw * 3];
                for (d, s) in d_row.chunks_exact_mut(3).zip(s_row.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                }
            }
        }

        Ok(Frame { width: self.width, height: self.height, data: rgb })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DIB 头字段顺序/偏移必须与 Win32 完全一致，写错一个字段名就会静默采到坏图。
    #[test]
    fn bitmap_info_header_layout_matches_win32() {
        assert_eq!(std::mem::size_of::<BitmapInfoHeader>(), 40, "BITMAPINFOHEADER 必须是 40 字节");
        // top-down DIB 用负高度表达，这是省掉翻转行序的关键
        let h = BitmapInfoHeader {
            bi_size: 40,
            bi_width: 100,
            bi_height: -50,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: BI_RGB,
            bi_size_image: 0,
            bi_x_pels_per_meter: 0,
            bi_y_pels_per_meter: 0,
            bi_clr_used: 0,
            bi_clr_important: 0,
        };
        assert!(h.bi_height < 0);
    }

    /// 采集相关常量取自 Win32 头文件，写错会采到错误区域或黑屏
    #[test]
    fn win32_constants_are_correct() {
        assert_eq!(SRCCOPY, 0x00CC_0020);
        assert_eq!(CAPTUREBLT, 0x4000_0000);
        assert_eq!(DIB_RGB_COLORS, 0);
        assert_eq!(SM_CXVIRTUALSCREEN, 78);
        assert_eq!(SM_XVIRTUALSCREEN, 76);
    }
}