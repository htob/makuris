//! 非 macOS 的采集 + 编码流水线（Windows/Linux 路径）。
//!
//! 与 macOS 版（`pipeline.rs`，VideoToolbox + IOSurface 池）的关系：
//! 两者对会话层暴露同样的接口——`frames` 接收端 + `shutdown()`——所以 `session.rs`
//! 只需按平台选一个，事件循环与 RTP 打包代码完全共用。
//!
//! 实现上比 macOS 版朴素得多，原因是硬件条件不同：
//! - 编码走 openh264 软编（`encode::H264Encoder`），不需要 GPU 表面，
//!   因此没有 surface 池、没有所有权在线程间传递的讲究；
//! - 采集产出的是 CPU 内存里的 RGB，本来就要拷一次给编码器，
//!   再拆两条线程传递只是把同样一次拷贝换个地方，收益很小。
//!   单线程「采集→编码→送出」在这个分辨率下已能跑到 30fps 附近。
//!
//! 仍保留「首帧核验 SPS 与 SDP 声明是否一致」这一步：声明与实际不符会让
//! 部分解码端直接黑屏，而这类问题在日志里毫无痕迹，只能靠主动核对。

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::encode::{parse_sps_profile_level, H264Encoder};

/// 正在运行的采集+编码流水线。接口与 macOS 版保持一致。
pub struct Pipeline {
    /// 编码后的 Annex-B 码流；接收端丢弃时流水线自动停止。
    pub frames: mpsc::Receiver<Bytes>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// 目标码率（bits/s），由会话层按链路质量改写。
    bitrate: Arc<AtomicI32>,
}

impl Pipeline {
    /// 请求把编码目标码率切到 `bps`（下一帧生效）。
    ///
    /// 与 macOS 版同一语义，但实现不同：openh264 的码率是构造期参数
    /// （EncoderConfig::set_bitrate_bps），crate 没有运行时 setter，所以这里只写入
    /// 目标值，由编码线程发现变化后重建编码器。用原子变量而不是 channel，是因为
    /// 只关心「最新值」——来不及处理的中间档位没有意义。
    pub fn set_bitrate(&self, bps: i32) {
        self.bitrate.store(bps, Ordering::Relaxed);
    }

    /// 关闭流水线并等待工作线程退出。
    pub fn shutdown(mut self) {
        // 关掉接收端，工作线程的 blocking_send 随即失败并退出
        self.frames.close();
        if let Some(h) = self.handle.take() {
            if h.join().is_err() {
                tracing::warn!("[pipeline] 采集编码线程 panic");
            }
        }
    }
}

/// 探测 openh264 在给定尺寸下实际输出的 profile-level-id。
///
/// 必须探测而不是写死：openh264 会按分辨率自动选择 level，1280x720 与 1920x1080
/// 得到的 level 不同。SDP 里声明一个比实际更低的 level，部分解码端会拒绝
/// ——这正是本项目在 macOS 上踩过的「声明与实际不符导致黑屏」。
pub fn probe_profile_level_id(width: usize, height: usize) -> Result<String> {
    let mut encoder = H264Encoder::new()?;
    // 空白帧即可：SPS 只由尺寸与编码参数决定，与画面内容无关
    let blank = vec![0u8; width * height * 3];
    for _ in 0..2 {
        let data = encoder.encode_rgb(&blank, width, height)?;
        if let Some((p, f, l)) = parse_sps_profile_level(&data) {
            return Ok(format!("{p:02x}{f:02x}{l:02x}"));
        }
    }
    anyhow::bail!("openh264 未产出 SPS")
}

/// 启动流水线：采集线程持续产出帧，编码后经 channel 送给事件循环。
pub fn spawn(width: usize, height: usize, fps: u64, bitrate_bps: i32) -> Result<Pipeline> {
    let mut capturer = crate::capture::create_capturer(width as u32, height as u32);
    let mut encoder = H264Encoder::new_with_bitrate(bitrate_bps.max(1) as u32)?;
    let bitrate = Arc::new(AtomicI32::new(bitrate_bps));
    let bitrate_target = Arc::clone(&bitrate);
    let (frame_tx, frame_rx) = mpsc::channel::<Bytes>(2);
    let frame_duration = Duration::from_millis(1000 / fps.max(1));

    let handle = std::thread::Builder::new()
        .name("makuris-encode".to_owned())
        .spawn(move || {
            let mut frames: u64 = 0;
            let mut bytes_total: u64 = 0;
            let mut fail_streak: u32 = 0;
            let mut verified = false;
            // 连续采集失败多少次后放弃：权限被回收或设备断开时，没必要永远空转
            const MAX_FAIL_STREAK: u32 = 300; // 约 10 秒

            let mut current_bitrate = bitrate_bps;
            loop {
                let frame_start = std::time::Instant::now();

                // 码率变更：openh264 只能在构造期定码率，故按目标值重建编码器。
                // 重建是毫秒级动作，且新编码器首帧是 IDR——接收端正好借它从丢包后的
                // 糊屏里恢复（与 macOS 版重建硬编会话同源）。
                let want = bitrate_target.load(Ordering::Relaxed);
                if want != current_bitrate {
                    match H264Encoder::new_with_bitrate(want.max(1) as u32) {
                        Ok(enc) => {
                            encoder = enc;
                            current_bitrate = want;
                            tracing::info!("[netadapt] 软编目标码率切到 {} bps", want);
                        }
                        Err(e) => {
                            // 不更新 current_bitrate：下一帧会再试，避免一次偶发失败
                            // 就让码率永久停在旧档
                            tracing::warn!("[encode] 重建编码器失败，稍后重试: {}", e);
                        }
                    }
                }

                let frame = match capturer.capture_frame() {
                    Ok(f) => {
                        fail_streak = 0;
                        f
                    }
                    Err(e) => {
                        fail_streak += 1;
                        if fail_streak == 1 {
                            tracing::warn!("[capture] 采集失败: {}（跳过该帧）", e);
                        }
                        if fail_streak >= MAX_FAIL_STREAK {
                            tracing::error!(
                                "[capture] 连续 {} 帧采集失败，停止流水线",
                                fail_streak
                            );
                            break;
                        }
                        std::thread::sleep(frame_duration);
                        continue;
                    }
                };

                let data = match encoder.encode_rgb(&frame.data, width, height) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("[encode] 编码失败: {}", e);
                        std::thread::sleep(frame_duration);
                        continue;
                    }
                };
                if data.is_empty() {
                    // 编码器偶尔不产出码流（等同跳帧），不算错误
                    std::thread::sleep(frame_duration);
                    continue;
                }

                // 核验一次：编码器真实 SPS 是否与 SDP 声明一致
                if !verified {
                    if let Some((p, f, l)) = parse_sps_profile_level(&data) {
                        tracing::info!(
                            "[encode] 编码器真实 profile-level-id = {:02x}{:02x}{:02x}\
                             （应与 SDP 声明一致，不一致可能黑屏）",
                            p,
                            f,
                            l
                        );
                        verified = true;
                    }
                }

                frames += 1;
                bytes_total += data.len() as u64;
                if frames % 30 == 0 {
                    tracing::info!(
                        "[perf] 已编码 {} 帧，均码 {}B/帧",
                        frames,
                        bytes_total / 30
                    );
                    bytes_total = 0;
                }

                if frame_tx.blocking_send(Bytes::from(data)).is_err() {
                    break; // 事件循环已退出
                }

                // 帧节拍：采集+编码耗时不足一帧时补齐，避免无谓占满 CPU
                let elapsed = frame_start.elapsed();
                if elapsed < frame_duration {
                    std::thread::sleep(frame_duration - elapsed);
                }
            }
        })?;

    Ok(Pipeline { frames: frame_rx, handle: Some(handle), bitrate })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探测必须在真实尺寸下给出可解析的 profile-level-id，
    /// 而不是回退到某个写死的字符串。
    /// 注意：会真实调用 openh264，故用小尺寸（测试机上快）。
    #[test]
    fn probe_returns_parseable_profile_level_id() {
        let id = probe_profile_level_id(320, 240).expect("probe 应成功");
        assert_eq!(id.len(), 6, "profile-level-id 应为 6 个十六进制字符，实际 {id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "应全为十六进制：{id}");
    }
}