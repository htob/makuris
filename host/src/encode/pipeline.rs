//! 采集 + 硬编流水线（macOS / VideoToolbox 路径）。
//!
//! 为什么需要流水线：2x 物理分辨率下，采集约 27ms（主要是 `CGWindowListCreateImage`
//! 的系统合成开销，与像素数几乎无关）与硬编约 15ms 串行要 42ms，只能到 24fps；
//! 把两者放到两条线程重叠执行后，帧节拍由较慢的一环决定（约 27ms），30fps 才有余量。
//!
//! 数据流（surface 池传递**所有权**，不拷贝像素）：
//!
//! ```text
//!   idle 池 ──surface──▶ 采集线程（填像素）──surface──▶ 编码线程（编码）
//!      ▲                                                   │
//!      └───────────────── 归还 surface ◀────────────────────┘
//!                                    └── Annex-B 码流 ──▶ 事件循环（RTP 打包）
//! ```
//!
//! 一帧 3024x1964 BGRA 是 23MB，池深 3 约 71MB——够流水线转动（一块在采集、
//! 一块在编码、一块在途），又不至于把内存吃爆。
//!
//! 非 macOS 平台：本模块只提供报错的占位实现。不给"假画面"兜底——
//! 推测试图案会让远端看着正常、实则什么都控制不了，掩盖真实状态。

use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
use anyhow::anyhow;
#[cfg(target_os = "macos")]
use apple_cf::iosurface::IOSurface;
#[cfg(target_os = "macos")]
use std::time::Instant;

#[cfg(target_os = "macos")]
use crate::capture::surface::{create_surface, CgPixelSource};
#[cfg(target_os = "macos")]
use crate::encode::parse_sps_profile_level;
#[cfg(target_os = "macos")]
use crate::encode::vt::VtEncoder;

/// surface 池深：采集、编码、在途各一块。
#[cfg(target_os = "macos")]
const POOL_DEPTH: usize = 3;
/// 连续多少帧无码流产出后强制关键帧（防御编码器异常静默）。
#[cfg(target_os = "macos")]
const EMPTY_FRAMES_BEFORE_KEYFRAME: u32 = 10;

/// 正在运行的采集+编码流水线。
pub struct Pipeline {
    /// 编码后的 Annex-B 码流；接收端丢弃时流水线自动停止。
    pub frames: mpsc::Receiver<Bytes>,
    /// 运行中切换编码目标码率的指令口（判据与步长见 `crate::netadapt`）。
    ///
    /// 为什么走「重建硬编会话」而不是 `VTSessionSetProperty`：后者要自己用 apple_cf
    /// 拼 CFDictionary 绕过 crate 的封装，且改属性后编码器的码控状态不会立刻收敛；
    /// 而码率调整是低频动作（最快 1.5 秒一次），重建一次硬编会话只要几十毫秒，代价
    /// 可以忽略，还附带一个好处——新会话的首帧天然是 IDR，接收端正好借此立刻从丢包
    /// 后遗症里恢复（不必再等 1 秒的关键帧周期）。
    bitrate_tx: mpsc::Sender<i32>,
    #[cfg(target_os = "macos")]
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Pipeline {
    /// 请求把编码目标码率切到 `bps`（下一帧生效）。流水线已退出时静默忽略。
    pub fn set_bitrate(&self, bps: i32) {
        // 容量 1：来不及处理的中间档位没有意义，丢掉比排队更符合「尽快贴合当前链路」
        let _ = self.bitrate_tx.try_send(bps);
    }

    /// 关闭流水线并等待工作线程退出。
    /// 关闭顺序：先断输出通道 → 编码线程写失败退出 → 采集线程随之退出。
    pub fn shutdown(mut self) {
        self.frames.close();
        #[cfg(target_os = "macos")]
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// 探测编码器在给定尺寸下实际输出的 profile-level-id（SDP 声明必须与之一致）。
///
/// 用一个空白 surface 编码一帧即可：SPS 的 profile/level 由尺寸与编码参数决定，
/// 与画面内容无关。比起事后修正已建立的连接，这里多花约 30ms 换取零不一致风险。
#[cfg(target_os = "macos")]
pub fn probe_profile_level_id(
    width: usize,
    height: usize,
    fps: u32,
    bitrate_bps: i32,
) -> Result<String> {
    let surface = create_surface(width, height)?;
    let mut encoder = VtEncoder::new(width, height, fps, bitrate_bps)?;
    // 编两帧：确保参数集与 SPS 都已产出
    let mut profile = None;
    for _ in 0..2 {
        let data = encoder.encode(&surface)?;
        if let Some(p) = parse_sps_profile_level(&data) {
            profile = Some(p);
            break;
        }
    }
    let (pidc, flags, lidc) = profile.ok_or_else(|| anyhow!("编码器未产出 SPS"))?;
    Ok(format!("{pidc:02x}{flags:02x}{lidc:02x}"))
}

#[cfg(not(target_os = "macos"))]
pub fn probe_profile_level_id(
    _width: usize,
    _height: usize,
    _fps: u32,
    _bitrate_bps: i32,
) -> Result<String> {
    anyhow::bail!("profile-level-id 探测依赖 VideoToolbox，仅 macOS 可用")
}

/// 启动流水线。`expected_profile_level_id` 传入 SDP 里声明的值，
/// 首帧编码后用于核对——不一致时打警告，避免又出现"声明与实际不符导致黑屏"。
#[cfg(target_os = "macos")]
pub fn spawn(
    width: usize,
    height: usize,
    fps: u64,
    bitrate_bps: i32,
    expected_profile_level_id: Option<String>,
) -> Result<Pipeline> {
    // 预分配 surface 池
    let mut pool = Vec::with_capacity(POOL_DEPTH);
    for _ in 0..POOL_DEPTH {
        pool.push(create_surface(width, height)?);
    }

    let source = CgPixelSource::new(width, height, true)?;
    let encoder = VtEncoder::new(width, height, fps as u32, bitrate_bps)?;

    // idle：可填像素的空 surface；ready：已填好待编码的 surface
    let (idle_tx, idle_rx) = std::sync::mpsc::sync_channel::<IOSurface>(POOL_DEPTH);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<IOSurface>(1);
    for s in pool {
        idle_tx.send(s).map_err(|_| anyhow!("surface 池预填充失败"))?;
    }

    let (frame_tx, frame_rx) = mpsc::channel::<Bytes>(2);
    // 码率调整指令口：容量 1，只保留最新档位（中间态没有意义，堆积只会让调整滞后）
    let (bitrate_tx, mut bitrate_rx) = mpsc::channel::<i32>(1);
    let frame_duration = Duration::from_millis(1000 / fps);

    // 采集线程：较慢的一环，负责帧节拍
    // 编码线程也要归还 surface，故克隆一份发送端（两条线程各持一份）
    let encode_idle_tx = idle_tx.clone();
    let cap_handle = std::thread::Builder::new()
        .name("makuris-capture".to_owned())
        .spawn({
            let mut source = source;
            move || {
                let mut frames: u64 = 0;
                let mut total = Duration::ZERO;
                let mut fail_logged = false;
                loop {
                    let frame_start = Instant::now();

                    // 取一块空 surface；idle 通道关闭说明编码线程已退出
                    let Ok(surface) = idle_rx.recv() else { break };

                    let t = Instant::now();
                    match source.capture_into(&surface) {
                        Ok(()) => {
                            total += t.elapsed();
                            frames += 1;
                            // 交给编码线程；编码线程已退出则结束
                            if ready_tx.send(surface).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            // 采集失败（如权限被回收）：归还 surface 并跳过本帧，
                            // 避免把陈旧画面当作新帧推给编码器
                            if !fail_logged {
                                tracing::warn!("[capture] 采集失败: {}（跳过该帧）", e);
                                fail_logged = true;
                            }
                            if idle_tx.send(surface).is_err() {
                                break;
                            }
                        }
                    }

                    if frames > 0 && frames % 30 == 0 {
                        let avg = total.as_secs_f64() * 1000.0 / 30.0;
                        tracing::info!("[perf] 采集 30 帧均值 {:.1}ms（上限 {:.0}fps）", avg, 1000.0 / avg);
                        total = Duration::ZERO;
                    }

                    // 节拍：补足到下一帧起点（编码已在另一线程重叠执行）
                    let elapsed = frame_start.elapsed();
                    if elapsed < frame_duration {
                        std::thread::sleep(frame_duration - elapsed);
                    }
                }
            }
        })?;

    // 编码线程
    let enc_handle = std::thread::Builder::new()
        .name("makuris-encode".to_owned())
        .spawn({
            let mut encoder = encoder;
            // 当前生效的编码目标码率：自适应调整时用它判断要不要重建会话
            let mut cur_bitrate = bitrate_bps;
            move || {
                let mut frames: u64 = 0;
                let mut total = Duration::ZERO;
                let mut bytes_total: u64 = 0;
                let mut empty_streak: u32 = 0;
                let mut verified = false;
                loop {
                    // 自适应码率：只取最新档位（旧档位作废），换档要重建硬编会话——
                    // 几十毫秒的代价，换来新会话首帧即 IDR，接收端正好借这一帧从
                    // 「丢包后等关键帧」的僵住状态里立刻恢复。
                    while let Ok(new_bps) = bitrate_rx.try_recv() {
                        if new_bps == cur_bitrate {
                            continue;
                        }
                        match VtEncoder::new(width, height, fps as u32, new_bps) {
                            Ok(fresh) => {
                                encoder = fresh;
                                cur_bitrate = new_bps;
                                tracing::info!(
                                    "[encode] 目标码率自适应调整为 {} bps（已重建硬编会话）",
                                    new_bps
                                );
                            }
                            Err(e) => {
                                tracing::warn!("[encode] 切换码率到 {} bps 失败: {}", new_bps, e);
                            }
                        }
                    }

                    // 等待已填像素的 surface；通道关闭说明采集线程已退出
                    let Ok(surface) = ready_rx.recv() else { break };

                    let t = Instant::now();
                    let force_key = empty_streak >= EMPTY_FRAMES_BEFORE_KEYFRAME;
                    let data = match if force_key {
                        encoder.encode_force_keyframe(&surface)
                    } else {
                        encoder.encode(&surface)
                    } {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!("[encode] 编码失败: {}", e);
                            if encode_idle_tx.send(surface).is_err() {
                                break;
                            }
                            continue;
                        }
                    };
                    total += t.elapsed();

                    // 关键：先把 surface 归还池，再往下游送码流。
                    // 否则下游（事件循环按帧率取包）一旦变慢，采集线程会因无空 surface 停摆。
                    if encode_idle_tx.send(surface).is_err() {
                        break;
                    }

                    if data.is_empty() {
                        empty_streak += 1;
                        if empty_streak == EMPTY_FRAMES_BEFORE_KEYFRAME {
                            tracing::warn!("[encode] 连续 {} 帧无码流，下一次强制关键帧", empty_streak);
                        }
                        continue;
                    }
                    empty_streak = 0;

                    // 核验：编码器真实 SPS 是否与 SDP 声明一致（不一致会黑屏）
                    if !verified {
                        if let (Some((p, f, l)), Some(expected)) =
                            (parse_sps_profile_level(&data), expected_profile_level_id.as_deref())
                        {
                            let actual = format!("{p:02x}{f:02x}{l:02x}");
                            if actual == expected {
                                tracing::info!(
                                    "[encode] SPS 核验通过：profile-level-id={} 与 SDP 声明一致",
                                    actual
                                );
                            } else {
                                tracing::warn!(
                                    "[encode] ⚠️ SPS 与 SDP 不一致：编码器输出 {}，SDP 声明 {}（可能导致黑屏）",
                                    actual,
                                    expected
                                );
                            }
                            verified = true;
                        }
                    }

                    frames += 1;
                    bytes_total += data.len() as u64;
                    if frames % 30 == 0 {
                        let avg = total.as_secs_f64() * 1000.0 / 30.0;
                        tracing::info!(
                            "[perf] 编码 30 帧均值 {:.1}ms（上限 {:.0}fps），均码 {}B/帧",
                            avg,
                            1000.0 / avg,
                            bytes_total / 30
                        );
                        total = Duration::ZERO;
                        bytes_total = 0;
                    }

                    if frame_tx.blocking_send(Bytes::from(data)).is_err() {
                        break; // 事件循环已退出
                    }
                }
            }
        })?;

    Ok(Pipeline { frames: frame_rx, bitrate_tx, handles: vec![cap_handle, enc_handle] })
}

#[cfg(not(target_os = "macos"))]
pub fn spawn(
    _width: usize,
    _height: usize,
    _fps: u64,
    _bitrate_bps: i32,
    _expected_profile_level_id: Option<String>,
) -> Result<Pipeline> {
    anyhow::bail!("采集+硬编流水线依赖 VideoToolbox，仅 macOS 可用")
}