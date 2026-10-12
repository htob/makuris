//! 编码性能 + 质量基准。
//! 性能：实测各分辨率单帧耗时，确定 30fps（33.3ms 预算）下能推到多高。
//! 质量：对比不同码控模式/码率下的单帧码流大小（越大 = 保留细节越多 = 文字越清晰）。
use openh264::encoder::{Encoder, EncoderConfig, RateControlMode, UsageType};
use openh264::formats::{RgbSliceU8, YUVBuffer};
use openh264::OpenH264API;
use std::time::Instant;

/// 生成细节丰富的画面：2px 棋盘格 ≈ 屏幕文字笔画的高频特征。
fn detail_frame(w: usize, h: usize, phase: usize) -> Vec<u8> {
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            let v = if ((x / 2) + (y / 2) + phase / 8) % 2 == 0 { 235u8 } else { 20 };
            rgb[i] = v;
            rgb[i + 1] = v;
            rgb[i + 2] = v;
        }
    }
    rgb
}

fn bench(w: usize, h: usize, threads: u16, bitrate: u32, frames: usize) {
    let config = EncoderConfig::new()
        .set_bitrate_bps(bitrate)
        .enable_skip_frame(false)
        .usage_type(UsageType::ScreenContentRealTime)
        .max_frame_rate(30.0)
        .set_multiple_thread_idc(threads);
    let mut enc = match Encoder::with_api_config(OpenH264API::from_source(), config) {
        Ok(e) => e,
        Err(e) => {
            println!("{:>5}x{:<5} 线程{:<2} → 初始化失败: {}", w, h, threads, e);
            return;
        }
    };

    let rgb = detail_frame(w, h, 0);
    let mut yuv = YUVBuffer::new(w, h);
    yuv.read_rgb(RgbSliceU8::new(&rgb, (w, h)));
    let _ = enc.encode(&yuv);

    let mut total_ms = 0.0f64;
    let mut peak = 0.0f64;
    for _ in 0..frames {
        let t = Instant::now();
        yuv.read_rgb(RgbSliceU8::new(&rgb, (w, h)));
        let _ = enc.encode(&yuv).expect("encode");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        total_ms += ms;
        if ms > peak {
            peak = ms;
        }
    }
    let avg = total_ms / frames as f64;
    let macroblocks = w.div_ceil(16) * h.div_ceil(16);
    println!(
        "{:>5}x{:<5} 线程{:<2} → 均值 {:>6.1}ms  峰值 {:>6.1}ms  ({}块)",
        w, h, threads, avg, peak, macroblocks
    );
}

/// 质量对比：同一细节画面下，测各码控配置的稳态单帧码流大小。
/// 码流越大 = 量化越细 = 文字边缘保留越好。
fn quality_compare(w: usize, h: usize, mode: RateControlMode, bitrate: u32, frames: usize) {
    let config = EncoderConfig::new()
        .set_bitrate_bps(bitrate)
        .enable_skip_frame(false)
        .usage_type(UsageType::ScreenContentRealTime)
        .max_frame_rate(30.0)
        .rate_control_mode(mode)
        .set_multiple_thread_idc(4);
    let mut enc = match Encoder::with_api_config(OpenH264API::from_source(), config) {
        Ok(e) => e,
        Err(_) => {
            println!("  模式{:?} → 初始化失败", mode);
            return;
        }
    };

    let mut yuv = YUVBuffer::new(w, h);
    let mut sizes: Vec<usize> = Vec::new();
    for f in 0..frames {
        let rgb = detail_frame(w, h, f);
        yuv.read_rgb(RgbSliceU8::new(&rgb, (w, h)));
        if let Ok(out) = enc.encode(&yuv) {
            sizes.push(out.to_vec().len());
        }
    }
    let n = sizes.len();
    let avg_tail = if n > 4 {
        sizes[n / 2..].iter().sum::<usize>() / (n - n / 2)
    } else {
        0
    };
    println!(
        "  模式 {:?} 码率{:>3}M → 稳态单帧 {:>8}B",
        mode,
        bitrate / 1_000_000,
        avg_tail
    );
}

fn main() {
    let frames = 40;

    println!("=== openh264 编码耗时基准（含 RGB→YUV），30fps 预算 33.3ms ===");
    bench(1280, 832, 1, 3_000_000, frames);
    bench(1512, 982, 1, 8_000_000, frames);
    bench(1512, 982, 4, 8_000_000, frames);
    bench(1920, 1248, 4, 12_000_000, frames);
    bench(3024, 1964, 4, 20_000_000, frames);

    println!();
    println!("=== 码控质量对比（1512x982，稳态单帧码流越大 = 文字越清晰）===");
    quality_compare(1512, 982, RateControlMode::Quality, 3_000_000, 30);
    quality_compare(1512, 982, RateControlMode::Bitrate, 8_000_000, 30);
    quality_compare(1512, 982, RateControlMode::Bitrate, 20_000_000, 30);
}