//! macOS CGEvent 输入注入器。
//! 将归一化坐标(0.0-1.0)换算为屏幕逻辑点，通过 CGEvent API 注入系统。
//! 运行需要「辅助功能」权限（系统设置 > 隐私与安全性 > 辅助功能）。
//!
//! 注意：CGEventSource 不是 Send，无法存进跨线程的注入器结构体，
//! 因此每次注入时新建一个 source（开销极小，macOS 内部会复用 HID 状态）。

use anyhow::{anyhow, Result};
use core_graphics::display::{CGDisplay, CGMainDisplayID};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use makuris_proto::{modifier, InputEvent};

use super::InputInjector;

pub struct CgEventInjector {
    screen_w: f64,
    screen_h: f64,
}

impl CgEventInjector {
    pub fn new() -> Result<Self> {
        let display = CGDisplay::new(unsafe { CGMainDisplayID() });
        let bounds = display.bounds();
        tracing::info!(
            "CGEvent 注入器初始化：屏幕 {:.0}x{:.0}",
            bounds.size.width,
            bounds.size.height
        );
        Ok(Self {
            screen_w: bounds.size.width,
            screen_h: bounds.size.height,
        })
    }

    /// 每次注入新建事件源（CGEventSource 非 Send，不能缓存到结构体）。
    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("CGEventSource::new failed"))
    }

    fn to_pixel(&self, x: f32, y: f32) -> CGPoint {
        CGPoint::new(
            (x.clamp(0.0, 1.0) as f64) * self.screen_w,
            (y.clamp(0.0, 1.0) as f64) * self.screen_h,
        )
    }

    /// 协议里的修饰键位掩码 → CGEvent flags。
    /// Control 与 Command 必须分开映射：macOS 上 ⌃ 和 ⌘ 是两个不同的键，
    /// 混为一谈会让 ⌘C（复制）变成 ⌃C（某些终端里的中断）。
    fn flags_from(modifiers: u8) -> CGEventFlags {
        let mut flags = CGEventFlags::CGEventFlagNull;
        if modifiers & modifier::SHIFT != 0 {
            flags |= CGEventFlags::CGEventFlagShift;
        }
        if modifiers & modifier::CONTROL != 0 {
            flags |= CGEventFlags::CGEventFlagControl;
        }
        if modifiers & modifier::ALT != 0 {
            flags |= CGEventFlags::CGEventFlagAlternate;
        }
        if modifiers & modifier::COMMAND != 0 {
            flags |= CGEventFlags::CGEventFlagCommand;
        }
        flags
    }

    fn post(&self, ev: CGEvent) {
        ev.post(CGEventTapLocation::HID);
    }
}

/// USB HID Usage ID（Keyboard/Keypad page 0x07）→ macOS 虚拟键码。
///
/// 为什么需要这层映射：控制端（iOS）无法知道被控端是 macOS 还是 Windows，
/// 所以协议里传的必须是**平台无关**的 HID Usage ID（WebRTC、VNC、微软 RDP
/// 都采用同一套），由被控端按自己的平台翻译。此前直接把控制端传来的数字
/// 当作 macOS 虚拟键码使用，而控制端发的其实是 ASCII 码（如 27=Esc、65='A'），
/// 两套编码完全错位——例如 ASCII 27 在 macOS 虚拟键码里对应的是减号键，
/// 于是 Esc 按钮实际打出 "-"。这是快捷键面板一直不工作的真正原因。
///
/// 表里只列远程桌面用得到的键：字母、数字、符号、编辑键、方向键、功能键。
/// 可用 `Option` 表达"本平台无此键"，比返回错误码更能让调用方决定如何降级。
fn hid_to_macos_vk(hid: u32) -> Option<u16> {
    Some(match hid {
        // 字母 a-z
        0x04 => 0,  // A
        0x05 => 11, // B
        0x06 => 8,  // C
        0x07 => 2,  // D
        0x08 => 14, // E
        0x09 => 3,  // F
        0x0A => 5,  // G
        0x0B => 4,  // H
        0x0C => 34, // I
        0x0D => 38, // J
        0x0E => 40, // K
        0x0F => 37, // L
        0x10 => 46, // M
        0x11 => 45, // N
        0x12 => 31, // O
        0x13 => 35, // P
        0x14 => 12, // Q
        0x15 => 15, // R
        0x16 => 1,  // S
        0x17 => 17, // T
        0x18 => 32, // U
        0x19 => 9,  // V
        0x1A => 13, // W
        0x1B => 7,  // X
        0x1C => 16, // Y
        0x1D => 6,  // Z
        // 数字 1-9、0
        0x1E => 18,
        0x1F => 19,
        0x20 => 20,
        0x21 => 21,
        0x22 => 23,
        0x23 => 22,
        0x24 => 26,
        0x25 => 28,
        0x26 => 25,
        0x27 => 29,
        // 编辑与空白键
        0x28 => 36, // Return
        0x29 => 53, // Escape
        0x2A => 51, // Backspace
        0x2B => 48, // Tab
        0x2C => 49, // Space
        0x2D => 27, // -
        0x2E => 24, // =
        0x2F => 33, // [
        0x30 => 30, // ]
        0x31 => 42, // \
        0x33 => 41, // ;
        0x34 => 39, // '
        0x35 => 50, // `
        0x36 => 43, // ,
        0x37 => 47, // .
        0x38 => 44, // /
        0x39 => 57, // Caps Lock
        // 功能键 F1-F12
        0x3A => 122,
        0x3B => 120,
        0x3C => 99,
        0x3D => 118,
        0x3E => 96,
        0x3F => 97,
        0x40 => 98,
        0x41 => 100,
        0x42 => 101,
        0x43 => 109,
        0x44 => 103,
        0x45 => 111,
        // 导航键
        0x49 => 114, // Insert / Help
        0x4A => 115, // Home
        0x4B => 116, // Page Up
        0x4C => 117, // Forward Delete
        0x4D => 119, // End
        0x4E => 121, // Page Down
        0x4F => 124, // Right
        0x50 => 123, // Left
        0x51 => 125, // Down
        0x52 => 126, // Up
        _ => return None,
    })
}

impl InputInjector for CgEventInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMove { x, y } => {
                let point = self.to_pixel(*x, *y);
                let ev = CGEvent::new_mouse_event(
                    Self::source()?,
                    CGEventType::MouseMoved,
                    point,
                    CGMouseButton::Left,
                )
                .map_err(|_| anyhow!("new_mouse_event(MouseMoved) failed"))?;
                self.post(ev);
                tracing::debug!("CGEvent MouseMove -> ({:.0},{:.0})", point.x, point.y);
            }
            InputEvent::MouseButton { x, y, button, down } => {
                let point = self.to_pixel(*x, *y);
                let (down_type, up_type, mb) = match button {
                    0 => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGMouseButton::Left),
                    1 => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGMouseButton::Right),
                    2 => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center),
                    _ => return Err(anyhow!("unsupported button {}", button)),
                };
                let event_type = if *down { down_type } else { up_type };
                let ev = CGEvent::new_mouse_event(Self::source()?, event_type, point, mb)
                    .map_err(|_| anyhow!("new_mouse_event failed"))?;
                self.post(ev);
                tracing::debug!("CGEvent MouseButton {} down={}", button, down);
            }
            InputEvent::Scroll { dx, dy } => {
                let ev = CGEvent::new_scroll_event(
                    Self::source()?,
                    ScrollEventUnit::PIXEL,
                    2,
                    *dy as i32,
                    *dx as i32,
                    0,
                )
                .map_err(|_| anyhow!("new_scroll_event failed"))?;
                self.post(ev);
                tracing::debug!("CGEvent Scroll dx={} dy={}", dx, dy);
            }
            InputEvent::Key { keycode, down, modifiers } => {
                let vk = hid_to_macos_vk(*keycode)
                    .ok_or_else(|| anyhow!("未知的 HID 键码 0x{:02x}", keycode))?;
                let ev = CGEvent::new_keyboard_event(Self::source()?, vk, *down)
                    .map_err(|_| anyhow!("new_keyboard_event failed"))?;
                // 修饰键必须附着在事件上，否则 ⌘C 只会输入字符 "c"
                let flags = Self::flags_from(*modifiers);
                if !flags.is_empty() {
                    ev.set_flags(flags);
                }
                self.post(ev);
                tracing::debug!(
                    "CGEvent Key hid=0x{:02x} vk={} down={} modifiers=0x{:02x}",
                    keycode,
                    vk,
                    down,
                    modifiers
                );
            }
            InputEvent::Text { content } => {
                let ev = CGEvent::new_keyboard_event(Self::source()?, 0, true)
                    .map_err(|_| anyhow!("new_keyboard_event(Text) failed"))?;
                ev.set_string(content);
                self.post(ev);
                tracing::debug!("CGEvent Text len={}", content.len());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HID→macOS 虚拟键码映射是手写的，最容易错的是「把 HID 码当成虚拟键码直接用」。
    /// 这里挑几个两者差异明显的键锁死，防止以后被"简化"回去。
    #[test]
    fn hid_to_vk_maps_edit_keys_correctly() {
        // 字母 A：HID 0x04，macOS kVK_ANSI_A = 0
        assert_eq!(hid_to_macos_vk(0x04), Some(0), "A 应为虚拟键码 0");
        // C：HID 0x06，macOS kVK_ANSI_C = 8
        assert_eq!(hid_to_macos_vk(0x06), Some(8), "C 应为虚拟键码 8");
        // Return：HID 0x28，macOS kVK_Return = 36
        assert_eq!(hid_to_macos_vk(0x28), Some(36), "Return 应为 36");
        // Escape：HID 0x29，macOS kVK_Escape = 53
        assert_eq!(hid_to_macos_vk(0x29), Some(53), "Escape 应为 53");
        // Backspace：HID 0x2A，macOS kVK_Delete = 51
        assert_eq!(hid_to_macos_vk(0x2A), Some(51), "Backspace 应为 51（少 3 就撞上其它键）");
        // Tab：HID 0x2B，macOS kVK_Tab = 48
        assert_eq!(hid_to_macos_vk(0x2B), Some(48), "Tab 应为 48");
        // 方向键：HID 0x50←，macOS kVK_LeftArrow = 123
        assert_eq!(hid_to_macos_vk(0x50), Some(123), "左方向键应为 123");
        assert_eq!(hid_to_macos_vk(0x52), Some(126), "上方向键应为 126");
    }

    /// 未知键码必须返回 None 而不是静默当成键码 0（那会变成敲 A）。
    #[test]
    fn unknown_hid_key_is_rejected() {
        assert_eq!(hid_to_macos_vk(0xFF), None);
        assert_eq!(hid_to_macos_vk(0x00), None);
    }

    /// 修饰键映射：⌃ 与 ⌘ 必须是不同的 flag。
    /// 混为一谈会让 ⌘C（复制）变成 ⌃C（中断），在终端里后果完全不同。
    #[test]
    fn modifier_flags_are_distinct() {
        let cmd = CgEventInjector::flags_from(modifier::COMMAND);
        let ctrl = CgEventInjector::flags_from(modifier::CONTROL);
        assert!(cmd.contains(CGEventFlags::CGEventFlagCommand), "⌘ 应映射为 Command flag");
        assert!(!cmd.contains(CGEventFlags::CGEventFlagControl), "⌘ 不应带上 Control flag");
        assert!(ctrl.contains(CGEventFlags::CGEventFlagControl), "⌃ 应映射为 Control flag");
        assert_ne!(cmd, ctrl, "⌘ 与 ⌃ 必须不同");
    }

    /// 多修饰键按位或，无修饰键时不应产生任何 flag。
    #[test]
    fn modifier_flags_combine_and_default_to_empty() {
        let both = CgEventInjector::flags_from(modifier::COMMAND | modifier::SHIFT);
        assert!(both.contains(CGEventFlags::CGEventFlagCommand));
        assert!(both.contains(CGEventFlags::CGEventFlagShift));
        let none = CgEventInjector::flags_from(0);
        assert!(none.is_empty(), "无修饰键时不应带任何 flag");
    }
}