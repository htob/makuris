//! 输入注入抽象层。
//! P1 起提供各平台实现：CGEvent(macOS) / SendInput(Windows) / uinput+XTest(Linux)。
use makuris_proto::InputEvent;

pub trait InputInjector: Send {
    /// 注入一个输入事件（坐标为归一化 0.0-1.0，实现层负责换算为像素）
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()>;
}

/// 桩注入器：仅记录日志，用于链路验证。
pub struct NullInjector;

impl NullInjector {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NullInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl InputInjector for NullInjector {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        tracing::debug!("inject (stub): {:?}", event);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod cg_event;
#[cfg(target_os = "macos")]
pub use cg_event::CgEventInjector;

#[cfg(any(target_os = "windows", feature = "windows_typecheck"))]
mod windows;
#[cfg(any(target_os = "windows", feature = "windows_typecheck"))]
pub use windows::SendInputInjector;

/// 已注入输入事件的累计计数（进程级）。
///
/// 存在的理由：这条链路原先没有任何「事件确实到达」的证据——注入成功只打
/// debug 日志，而线上默认 info 级被过滤掉。于是「事件根本没送到」与
/// 「送到了却被系统丢弃」在日志里长得完全一样，只能靠猜。加这个计数后，
/// 任何一次真机测试都能立刻把两者分开。
static INJECTED_EVENT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 记一次成功注入，返回累计次数。
pub fn note_injected() -> u64 {
    INJECTED_EVENT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// macOS：查询本进程是否已获得「辅助功能」权限。
///
/// 必须显式查。CGEvent 在没有该权限时 post 依然返回成功、不抛任何错，
/// 事件被系统直接丢掉——用户看到的是「画面正常、鼠标完全不动」，
/// 而日志上与「事件没送到」无法区分。只有主动问 AXIsProcessTrusted
/// 才能把这两种情况分开。
///
/// 另一个坑：TCC 授权绑定 bundle id 与签名。应用从 OpenDesk 更名为
/// Makuris（bundle id 由 com.opendesk.host 变为 com.makuris.host）后，
/// 旧授权不会迁移；而且系统只对「屏幕录制」弹提示，「辅助功能」是
/// 静默拒绝——所以改名之后「什么都没动却突然不能操作」是意料之中的。
#[cfg(target_os = "macos")]
pub fn accessibility_granted() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    unsafe { AXIsProcessTrusted() }
}

/// 创建平台对应的真实注入器。
/// macOS: CGEvent（需辅助功能权限）；Windows: SendInput（无需额外授权，
/// 但目标进程若以更高完整性级别运行时会被 UIPI 拦截）；其余平台回退 NullInjector。
pub fn create_injector() -> Box<dyn InputInjector> {
    #[cfg(target_os = "macos")]
    {
        match CgEventInjector::new() {
            Ok(inj) => {
                tracing::info!("使用 CGEvent 真实注入器（需辅助功能权限）");
                return Box::new(inj);
            }
            Err(e) => {
                tracing::warn!("CGEvent 注入器初始化失败，回退到 NullInjector: {}", e);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        match SendInputInjector::new() {
            Ok(inj) => {
                tracing::info!("使用 SendInput 真实注入器");
                return Box::new(inj);
            }
            Err(e) => {
                tracing::warn!("SendInput 注入器初始化失败，回退到 NullInjector: {}", e);
            }
        }
    }
    Box::new(NullInjector::new())
}
