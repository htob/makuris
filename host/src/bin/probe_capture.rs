//! 屏幕采集探针：抓一帧真实屏幕，报告尺寸与像素统计量。
//!
//! 为什么需要它：远程桌面最致命的失败模式是「连上了但画面全黑」。而日志里
//! 那句「GDI 真实屏幕采集 2560x1440」只说明采集器初始化成功，并不代表像素
//! 里有内容——进程若跑在 session 0（服务/SSH 会话），GDI 拿到的就是全黑。
//! 所以这里直接读像素算统计量来区分两种情况：
//!   全为 0 / 方差接近 0 → 黑屏（采集失败或跑错会话）
//!   亮度正常且有方差   → 真实画面
//!
//! 用法（Windows 被控端部署后自检）：
//!   probe_capture.exe
//! 注意：必须在**用户桌面会话**里运行才有真实画面；经 SSH 直接跑会得到黑屏
//! （SSH 落在 session 0），此时应通过计划任务（InteractiveToken）触发。

use std::time::Instant;

// host 是 bin-only crate（无 lib.rs），bin 之间无法互相引用模块，
// 故用 #[path] 直接引入生产源码——保证探针测的就是线上那条采集路径。
#[path = "../capture/mod.rs"]
mod capture;

/// macOS 的 detect_screen_size 是非 macOS 专有实现（macOS 侧用固定 2x 物理尺寸），
/// 这里各给一个：macOS 取主屏逻辑尺寸，其他平台走生产探测函数。
#[cfg(target_os = "macos")]
fn target_size() -> (usize, usize) {
    (1512, 982)
}

#[cfg(not(target_os = "macos"))]
fn target_size() -> (usize, usize) {
    capture::detect_screen_size().unwrap_or((1280, 720))
}

fn main() -> anyhow::Result<()> {
    let (w, h) = target_size();
    println!("探测到屏幕尺寸: {w}x{h}");

    let mut cap = capture::create_capturer(w as u32, h as u32);
    println!("采集器尺寸: {}x{}", cap.width(), cap.height());

    // 连采 3 帧：单帧可能撞上 BitBlt 与显示刷新不同步，多帧更能说明稳定行为
    for i in 1..=3 {
        let t0 = Instant::now();
        let frame = cap.capture_frame()?;
        let el = t0.elapsed();

        let n = frame.data.len();
        if n == 0 {
            println!("第 {i} 帧：❌ 数据为空");
            continue;
        }
        let sum: u64 = frame.data.iter().map(|&b| b as u64).sum();
        let mean = sum as f64 / n as f64;
        let var: f64 = frame
            .data
            .iter()
            .map(|&b| {
                let d = b as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / n as f64;
        let min = frame.data.iter().copied().min().unwrap_or(0);
        let max = frame.data.iter().copied().max().unwrap_or(0);

        println!(
            "第 {i} 帧：{}x{} 耗时 {:.1}ms | 平均亮度 {:.1}/255 | 方差 {:.1} | min/max {}/{}",
            frame.width,
            frame.height,
            el.as_secs_f64() * 1000.0,
            mean,
            var,
            min,
            max
        );

        if i == 3 {
            println!();
            if max == 0 {
                println!("❌ 判定：全黑（所有像素为 0）——采集失败，或进程不在用户桌面会话");
            } else if var < 1.0 {
                println!("⚠ 判定：几乎纯色（方差 < 1）——可能是全白/全黑桌面，或采集异常");
            } else {
                println!("✅ 判定：采集到真实画面（像素有正常亮度与层次）");
            }
        }
    }
    Ok(())
}