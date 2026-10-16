//! 视频码率自适应控制器。
//!
//! 为什么需要它：蜂窝实测（2026-10-16）把延迟的根因钉死了——不是网络差，而是
//! 「发送码率超过了这条链路的实际容量」。同一台手机、同一条路径（家宽上行 →
//! 成都中继 → 蜂窝），**只把码率从 10 Mbps 降到 3 Mbps**，接收端抖动缓冲就从
//! 1400ms 回落到几百 ms、操作立刻跟手；而 10 Mbps 下实测丢包 15.9 万、NACK 4338，
//! 缓冲被重传等待一路顶到 1400ms，体感就是「画面慢好几秒」。
//!
//! 但固定一个低码率也不行：网络好时白白牺牲画质。局域网直连走的是另一条 8444
//! 通道、不受这里影响，可同为「外网」，WiFi 与蜂窝的可用容量能差好几倍。所以由
//! 控制端把实时链路质量上报给被控端，被控端据此动态调整编码器目标码率。
//!
//! 设计取舍（都不是随手定的）：
//!
//! 1. **判据只用丢包率**，不看 RTT 绝对值。丢包率直接说明「发得比链路能吃下的多」；
//!    RTT 里混着排队延迟，在只有一路视频流时它更慢也更抖，做阈值判据容易误判。
//! 2. **乘性调整、升降不对称（AIMD 风格）**：升要慢（×1.15 且需连续干净），降要快
//!    （×0.6）。对拥塞的惩罚必须比试探更激进，否则会长期滞留在过载区来回震荡，
//!    而每震荡一次用户就卡一次。
//! 3. **升降各带静默期**，且降档等得短、升档等得长。编码器改目标码率后，链路层
//!    的排队要几百毫秒才排空，太密的连续调整只是在噪声上做决策。
//! 4. **丢包门槛不对称**：>5% 才降（轻微丢包由 NACK 重传补上，不值得为它牺牲画质，
//!    代价是缓冲会涨一点），<1% 才升（要确凿干净才敢加码）。
//!
//! 边界：本模块只做决策，不碰编码器与网络。调用方负责把 `Some(bps)` 应用到编码器，
//! 并保证 `on_report` 由同一线程串行调用（无内部锁）。

use std::time::{Duration, Instant};

/// 目标码率下界：0.6 Mbps。
///
/// 再低画面会碎到不可读，宁可让用户看到卡顿也不该糊成一片——分辨率是本项目的
/// 硬约束（用户明确要求不许降分辨率），所以低码率是最后的兜底手段。
pub const MIN_BITRATE_BPS: i32 = 600_000;

/// 目标码率上界：8 Mbps。
///
/// 参考实测：10 Mbps 在「家宽上行 → 成都中继 → 蜂窝」上已经稳定过载，8 Mbps 留一点
/// 余量；局域网走独立通道，不受这里限制，所以上限不必往 26 Mbps 的老值靠。
pub const MAX_BITRATE_BPS: i32 = 8_000_000;

/// 起始档：3 Mbps。蜂窝实测能立刻把抖动缓冲从 1400ms 压到几百 ms 的值。
///
/// 为什么从低起步而不是从高试探：起步过高会在连接最初几秒制造一次丢包风暴与缓冲
/// 膨胀，而「刚进画面就卡住」是最伤第一印象的；从低往上探的代价只是开头几秒略糊。
pub const INITIAL_BITRATE_BPS: i32 = 3_000_000;

/// 丢包率超过它就降档。
const LOSS_DOWN: f32 = 0.05;
/// 丢包率低于它才允许升档。
const LOSS_UP: f32 = 0.01;
/// 降档系数（乘性，快降）。
const DOWN_FACTOR: f64 = 0.6;
/// 升档系数（乘性，慢升）。
const UP_FACTOR: f64 = 1.15;
/// 降档后的静默期。
const SETTLE_DOWN: Duration = Duration::from_millis(1500);
/// 升档后的静默期。
const SETTLE_UP: Duration = Duration::from_secs(5);
/// 升档前需要的连续干净报告数（控制端每秒报一次，3 次约 3 秒）。
const UP_STREAK: u32 = 3;

/// 码率自适应控制器。
pub struct BitrateController {
    /// 当前目标码率（bps）。
    current: i32,
    /// 上次调整的时刻，用于静默期判定。
    last_change: Instant,
    /// 连续「干净」报告计数（丢包率低于升档门槛的轮数）。
    clean_streak: u32,
}

impl BitrateController {
    /// 以 `initial` 为起点建立控制器（自动钳到上下界内）。
    pub fn new(initial: i32) -> Self {
        Self {
            current: initial.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS),
            last_change: Instant::now(),
            clean_streak: 0,
        }
    }

    /// 当前目标码率。
    pub fn current(&self) -> i32 {
        self.current
    }

    /// 收到一次链路质量报告，返回需要下发的新码率；`None` 表示维持现状。
    ///
    /// `loss` 是这一秒的丢包率（0.0~1.0）。非有限值（NaN/Inf）一律忽略——统计里
    /// 取不到值时不该被当成「零丢包」而触发升档。
    pub fn on_report(&mut self, loss: f32, now: Instant) -> Option<i32> {
        if !loss.is_finite() {
            return None;
        }
        let loss = loss.clamp(0.0, 1.0);

        // —— 过载：快降 ——
        if loss > LOSS_DOWN {
            self.clean_streak = 0;
            if now.saturating_duration_since(self.last_change) < SETTLE_DOWN {
                return None;
            }
            let next = (f64::from(self.current) * DOWN_FACTOR) as i32;
            let next = next.max(MIN_BITRATE_BPS);
            if next >= self.current {
                return None; // 已在下界，无处可降
            }
            self.current = next;
            self.last_change = now;
            return Some(next);
        }

        // —— 干净：慢升 ——
        if loss < LOSS_UP {
            self.clean_streak = self.clean_streak.saturating_add(1);
            if self.clean_streak < UP_STREAK {
                return None;
            }
            if now.saturating_duration_since(self.last_change) < SETTLE_UP {
                return None;
            }
            let next = (f64::from(self.current) * UP_FACTOR) as i32;
            let next = next.min(MAX_BITRATE_BPS);
            if next <= self.current {
                return None; // 已在上界
            }
            self.current = next;
            self.last_change = now;
            self.clean_streak = 0;
            return Some(next);
        }

        // —— 中间地带：维持，且不累积升档资格 ——
        // 「勉强的干净」（1%~5% 丢包）不算干净：它说明链路已经有点紧张，
        // 这时再往上加码，下一步多半就是过载。
        self.clean_streak = 0;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 起点会被钳进上下界。
    #[test]
    fn initial_is_clamped() {
        assert_eq!(BitrateController::new(100).current(), MIN_BITRATE_BPS);
        assert_eq!(
            BitrateController::new(999_000_000).current(),
            MAX_BITRATE_BPS
        );
        assert_eq!(
            BitrateController::new(INITIAL_BITRATE_BPS).current(),
            INITIAL_BITRATE_BPS
        );
    }

    /// 过载立刻降档，且降幅是乘性的 0.6。
    #[test]
    fn loss_triggers_downward_step() {
        let mut c = BitrateController::new(3_000_000);
        let t0 = Instant::now();
        assert_eq!(c.on_report(0.20, t0), Some(1_800_000));
        assert_eq!(c.current(), 1_800_000);
    }

    /// 降档后有静默期：紧接着的高丢包报告不会再降第二次。
    #[test]
    fn down_step_respects_settle_window() {
        let mut c = BitrateController::new(3_000_000);
        let t0 = Instant::now();
        assert_eq!(c.on_report(0.20, t0), Some(1_800_000));
        let t1 = t0 + SETTLE_DOWN / 2;
        assert_eq!(c.on_report(0.20, t1), None);
        let t2 = t0 + SETTLE_DOWN;
        assert_eq!(c.on_report(0.20, t2), Some(1_080_000));
    }

    /// 干净报告要连续若干轮、且过了升档静默期才升。
    #[test]
    fn clean_streak_required_before_upward_step() {
        let mut c = BitrateController::new(3_000_000);
        let t0 = Instant::now();
        // 前 UP_STREAK-1 轮只是攒计数
        for i in 0..(UP_STREAK - 1) {
            assert_eq!(c.on_report(0.0, t0 + Duration::from_secs(u64::from(i))), None);
        }
        // 第 UP_STREAK 轮满足计数，但静默期已过（3 秒 < 5 秒）→ 仍不升
        assert_eq!(c.on_report(0.0, t0 + Duration::from_secs(10)), Some(3_450_000));
    }

    /// 中间地带（1%~5%）不升也不算干净。
    #[test]
    fn middling_loss_never_climbs() {
        let mut c = BitrateController::new(3_000_000);
        let t0 = Instant::now();
        for i in 0..20u64 {
            assert_eq!(c.on_report(0.03, t0 + Duration::from_secs(i * 10)), None);
        }
        assert_eq!(c.current(), 3_000_000);
    }

    /// 连续干净足够久，最终会被上界钳住，不会无限上涨。
    #[test]
    fn upward_step_is_capped() {
        let mut c = BitrateController::new(7_900_000);
        let mut t = Instant::now();
        for _ in 0..40 {
            t += Duration::from_secs(10);
            for _ in 0..UP_STREAK {
                c.on_report(0.0, t);
            }
        }
        assert_eq!(c.current(), MAX_BITRATE_BPS);
    }

    /// 非有限值被忽略，不会被当成零丢包而触发升档。
    #[test]
    fn non_finite_loss_is_ignored() {
        let mut c = BitrateController::new(3_000_000);
        let t0 = Instant::now();
        for _ in 0..10 {
            assert_eq!(c.on_report(f32::NAN, t0), None);
            assert_eq!(c.on_report(f32::INFINITY, t0), None);
        }
        assert_eq!(c.current(), 3_000_000);
    }

    /// 降到下界后不再继续降。
    #[test]
    fn downward_step_stops_at_floor() {
        let mut c = BitrateController::new(700_000);
        let mut t = Instant::now();
        for _ in 0..10 {
            t += Duration::from_secs(2);
            c.on_report(0.9, t);
        }
        assert_eq!(c.current(), MIN_BITRATE_BPS);
    }
}
