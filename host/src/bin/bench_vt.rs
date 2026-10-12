//! VideoToolbox 硬编真实链路基准。
//!
//! 与 /tmp/vt_probe 的合成探针不同，这里跑的是**生产模块本身**：
//! `capture::surface::CgPixelSource`（真实整屏采集 → BGRA IOSurface）
//! + `encode::vt::VtEncoder`（硬编 → Annex-B）。
//! 因此它同时验证四件事：
//! 1. 2x 物理分辨率采集是否真能拿到 3024x1964；
//! 2. AVCC→Annex-B 转换是否产出带起始码与 SPS 的合法码流；
//! 3. 编码器实际输出的 SPS profile/level（用于同步 SDP 的 profile-level-id，
//!    历史上多次因二者不一致导致解码端黑屏）；
//! 4. 采集+编码耗时能否进 30fps 预算（33.3ms），以及流水线并行后的余量。
//!
//! 运行前请先播放一段视频或拖动窗口制造动态画面，静态桌面测出的耗时偏低。
//!
//! 平台门禁：本基准依赖 `capture::surface` 与 `encode::vt` 两个 macOS 专有模块，
//! 在非 macOS 目标下这些模块不存在。若不加门禁，交叉编译 Windows 被控端时
//! 会报 `unresolved import crate::capture::surface` 而让整包构建失败——而它
//! 本来就与 Windows 平台无关。故非 macOS 下只保留一个提示用的空 main。

// host 是 bin-only crate（无 lib.rs），bin 之间无法互相引用模块，
// 故用 #[path] 直接引入生产源码——保证基准测的就是线上那条代码路径。
#[cfg(target_os = "macos")]
#[path = "../capture/mod.rs"]
mod capture;
#[cfg(target_os = "macos")]
#[path = "../encode/mod.rs"]
mod encode;

#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

/// 各分辨率的码率（bps）：像素越多给得越宽，保证文字边缘不被压糊。
#[cfg(target_os = "macos")]
fn bitrate_for(w: usize, h: usize) -> i32 {
    let px = (w * h) as f64;
    // 约 2.2 bits/像素/秒，3024x1964 → 约 26 Mbps
    let bps = px * 2.2;
    bps.clamp(4_000_000.0, 40_000_000.0) as i32
}

#[cfg(target_os = "macos")]
fn bench(width: usize, height: usize, frames: usize) {
    use crate::capture::surface::{create_surface, CgPixelSource};
    use crate::encode::vt::VtEncoder;
    use crate::encode::{parse_sps_profile_level, profile_level_id_hex};

    let mut source = match CgPixelSource::new(width, height, true) {
        Ok(s) => s,
        Err(e) => {
            println!("{width:>5}x{height:<5} → 采集源创建失败: {e}");
            return;
        }
    };
    let surface = match create_surface(width, height) {
        Ok(s) => s,
        Err(e) => {
            println!("{width:>5}x{height:<5} → IOSurface 分配失败: {e}");
            return;
        }
    };
    let bitrate = bitrate_for(width, height);
    let mut encoder = match VtEncoder::new(width, height, 30, bitrate) {
        Ok(e) => e,
        Err(e) => {
            println!("{width:>5}x{height:<5} → 硬编创建失败: {e}");
            return;
        }
    };

    let mut cap_total = Duration::ZERO;
    let mut enc_total = Duration::ZERO;
    let mut cap_peak = Duration::ZERO;
    let mut enc_peak = Duration::ZERO;
    let mut last_len = 0usize;
    let mut last_bytes = 0usize;
    let mut sps_seen: Option<(u8, u8, u8)> = None;
    let mut real_size = (0usize, 0usize);

    for i in 0..frames {
        let t0 = Instant::now();
        if let Err(e) = source.capture_into(&surface) {
            println!("采集失败: {e}");
            return;
        }
        let cap_el = t0.elapsed();
        if i == 0 {
            real_size = (surface.width(), surface.height());
        }

        let t1 = Instant::now();
        let data = match encoder.encode(&surface) {
            Ok(d) => d,
            Err(e) => {
                println!("编码失败: {e}");
                return;
            }
        };
        let enc_el = t1.elapsed();

        cap_total += cap_el;
        enc_total += enc_el;
        cap_peak = cap_peak.max(cap_el);
        enc_peak = enc_peak.max(enc_el);
        last_len = data.len();
        last_bytes += data.len();
        if sps_seen.is_none() {
            sps_seen = parse_sps_profile_level(&data);
        }
    }

    let cap_avg = cap_total.as_secs_f64() * 1000.0 / frames as f64;
    let enc_avg = enc_total.as_secs_f64() * 1000.0 / frames as f64;
    let total_avg = cap_avg + enc_avg;
    let fps_cap = if total_avg > 0.0 { 1000.0 / total_avg } else { 0.0 };
    let fps_par = if enc_avg > 0.0 && cap_avg > 0.0 {
        1000.0 / cap_avg.max(enc_avg)
    } else {
        0.0
    };
    let sps_txt = match sps_seen {
        Some((p, f, l)) => format!(
            "SPS profile=0x{p:02x} flags=0x{f:02x} level=0x{l:02x} → profile-level-id={}",
            profile_level_id_hex(p, f, l)
        ),
        None => "未解析到 SPS（异常！）".to_owned(),
    };

    println!(
        "{width:>5}x{height:<5} 采集源 {:>5}x{:<5} 码率{:>2}Mbps\n      \
         采集 均值{:>6.1}ms 峰值{:>6.1}ms | 编码 均值{:>6.1}ms 峰值{:>6.1}ms\n      \
         串行合计{:>6.1}ms → 上限 {fps_cap:.1}fps | 流水线并行后上限 {fps_par:.1}fps\n     \
         码流 {last_len:>6}B/帧 均码 {:>7.0}B | {sps_txt}\n",
        real_size.0,
        real_size.1,
        bitrate / 1_000_000,
        cap_avg,
        cap_peak.as_secs_f64() * 1000.0,
        enc_avg,
        enc_peak.as_secs_f64() * 1000.0,
        total_avg,
        last_bytes as f64 / frames as f64,
    );
}

#[cfg(target_os = "macos")]
fn main() {
    println!("=== VideoToolbox 硬编真实链路基准（30fps 预算 33.3ms）===");
    println!("提示：请让屏幕上出现动态内容（播放视频 / 拖动窗口）再运行，静态桌面耗时偏低\n");
    let frames = 60;
    bench(1512, 982, frames);
    bench(3024, 1964, frames);
    println!("=== 结束 ===");
}

/// 非 macOS 目标（如交叉编译 Windows 被控端）下本基准无意义，保留空 main 以满足
/// bin 必须含 main 的要求。
#[cfg(not(target_os = "macos"))]
fn main() {
    println!("bench_vt 仅适用于 macOS 的 VideoToolbox 硬编链路，当前目标平台不支持该基准。");
}