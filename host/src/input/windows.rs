//! Windows SendInput 输入注入器。
//!
//! 与 macOS（CGEvent）的两点结构差异，决定了这里的写法：
//!
//! 1. **修饰键在 Windows 上是系统级持续状态**。CGEvent 把 flags 附在单个事件上，
//!    而 Windows 的 Ctrl/Alt/Shift/Win 一旦按下就持续生效，直到收到对应的 key-up。
//!    协议里每个按键事件都自带 modifiers 位掩码，于是这里用「按下修饰键 → 按主键 →
//!    立刻释放修饰键」的组合键染色写法：主键按下时系统确实看到修饰键处于按下状态
//!    （因此 ⌘C 被识别为快捷键，而不是输入字符 c），同时修饰键不会残留卡住——
//!    因为 release 事件未必还带着修饰键位。
//!
//! 2. **坐标要归一化到整个虚拟桌面**。多显示器下用主屏坐标会漏掉副屏（甚至负坐标
//!    区域），所以取 SM_{X,Y,CX,CY}VIRTUALSCREEN 的范围，再按 SendInput 的约定
//!    映射到 0..65535，并带上 MOUSEEVENTF_VIRTUALDESK。
//!
//! 不引入 windows / windows-sys crate：这里总共只需要十来个符号，手写 FFI 能让
//! 台机上 `cargo build` 不新增任何依赖，断网也能编译。

use anyhow::{anyhow, Result};
use makuris_proto::{modifier, InputEvent};
use std::mem::size_of;
use std::os::raw::{c_int, c_void};

use super::InputInjector;

// ---------------- Win32 结构体（布局必须与 C 逐字节一致） ----------------

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeybdInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HardwareInput {
    msg: u32,
    param_l: u16,
    param_h: u16,
}

#[repr(C)]
union InputUnion {
    mi: MouseInput,
    ki: KeybdInput,
    hi: HardwareInput,
}

#[repr(C)]
struct Input {
    type_: u32,
    u: InputUnion,
}

/// GetCursorPos 读回光标位置用的坐标对。
#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;

const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;

const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;

const SM_XVIRTUALSCREEN: c_int = 76;
const SM_YVIRTUALSCREEN: c_int = 77;
const SM_CXVIRTUALSCREEN: c_int = 78;
const SM_CYVIRTUALSCREEN: c_int = 79;

// 桌面查询：自检时确认进程能否访问「用户正在看的那个」输入桌面。
// 一个会话里可以有多个桌面（Default / Winlogon / 锁屏），注入进非输入桌面
// 时 SendInput 照样返回成功，但用户屏幕上什么都不会发生。
const DESKTOP_READOBJECTS: u32 = 0x0001;
const UOI_NAME: c_int = 2;

// 虚拟键码
const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_CAPITAL: u16 = 0x14;
const VK_ESCAPE: u16 = 0x1B;
const VK_SPACE: u16 = 0x20;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_INSERT: u16 = 0x2D;
const VK_DELETE: u16 = 0x2E;
const VK_LWIN: u16 = 0x5B;

#[link(name = "user32")]
extern "system" {
    fn SendInput(ninputs: u32, pinputs: *const Input, cb_size: c_int) -> u32;
    fn GetSystemMetrics(index: c_int) -> c_int;
    fn SetProcessDPIAware() -> i32;
    fn GetCursorPos(lp_point: *mut Point) -> i32;
    fn OpenInputDesktop(dw_flags: u32, f_inherit: i32, dw_desired_access: u32) -> *mut c_void;
    fn GetUserObjectInformationW(
        h_obj: *mut c_void,
        n_index: c_int,
        pv_info: *mut u16,
        n_length: u32,
        lpn_length_needed: *mut u32,
    ) -> i32;
    fn CloseDesktop(h_desktop: *mut c_void) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetLastError() -> u32;
}

// ---------------- 注入器 ----------------

pub struct SendInputInjector {
    vw: i32,
    vh: i32,
}

impl SendInputInjector {
    pub fn new() -> Result<Self> {
        // 进程必须 DPI 感知：否则在 150% 缩放的显示器上 GetSystemMetrics 返回逻辑尺寸，
        // 按它算出的归一化坐标会整体偏小，表现为「右半边屏幕点不到」。
        unsafe { SetProcessDPIAware() };

        let (vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if vw <= 0 || vh <= 0 {
            return Err(anyhow!("虚拟桌面尺寸非法: {}x{}", vw, vh));
        }
        let (vx, vy) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            )
        };
        tracing::info!(
            "SendInput 注入器初始化：虚拟桌面 {}x{}（原点 {},{}）",
            vw,
            vh,
            vx,
            vy
        );
        let this = Self { vw, vh };
        this.self_check();
        Ok(this)
    }

    /// 启动自检：确认注入真的能到达「用户正在看的那个桌面」。
    ///
    /// SendInput 返回 1 只代表事件已入队，不代表它到了用户眼前。进程若跑在
    /// 已断开的远程会话、或非输入桌面（Winlogon / 锁屏 / 服务会话）上，注入
    /// 照样「成功」，日志一切正常，用户屏幕上却什么都不会发生——现象就是
    /// 「操作完全没反应」，且从日志里看不出任何异常，排查成本极高。
    ///
    /// 所以这里做闭环验证：注入一次绝对移动，再读回真实光标位置比对。
    /// 读回值命中目标才说明注入确实生效；同时打印输入桌面名，
    /// Default 才是用户桌面，其它名字都意味着注入进了看不见的桌面。
    fn self_check(&self) {
        // 1. 当前输入桌面名
        let desktop = unsafe {
            let h = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
            if h.is_null() {
                format!("<OpenInputDesktop 失败 err={}>", GetLastError())
            } else {
                let mut buf = [0u16; 256];
                let mut needed = 0u32;
                let ok = GetUserObjectInformationW(
                    h,
                    UOI_NAME,
                    buf.as_mut_ptr(),
                    (buf.len() * 2) as u32,
                    &mut needed,
                );
                let name = if ok != 0 {
                    // needed 是含结尾 null 的字节数，宽字符要除 2 再去掉 null
                    let n = (needed as usize / 2).saturating_sub(1);
                    String::from_utf16_lossy(&buf[..n.min(buf.len())])
                } else {
                    format!("<取桌面名失败 err={}>", GetLastError())
                };
                CloseDesktop(h);
                name
            }
        };

        // 2. 注入一次绝对移动，读回比对
        let mut before = Point { x: 0, y: 0 };
        let got_before = unsafe { GetCursorPos(&mut before) } != 0;

        let target_x = (self.vw / 3).max(1);
        let target_y = (self.vh / 3).max(1);
        let abs = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        let (dx, dy) = self.to_absolute(1.0 / 3.0, 1.0 / 3.0);
        let inject = Self::mouse(abs, dx, dy, 0);

        std::thread::sleep(std::time::Duration::from_millis(150));

        let mut after = Point { x: 0, y: 0 };
        let got_after = unsafe { GetCursorPos(&mut after) } != 0;

        let moved = got_before && got_after && (after.x != before.x || after.y != before.y);
        let hit = got_after && (after.x - target_x).abs() <= 3 && (after.y - target_y).abs() <= 3;
        let verdict = if hit {
            "✅ 注入生效（光标已到达目标）"
        } else if moved {
            "⚠️ 光标动了但没到目标：坐标映射有偏差"
        } else {
            "❌ 注入被吞：SendInput 报成功但光标纹丝不动"
        };

        tracing::info!(
            "[input-selfcheck] 输入桌面={:?} 注入={} 光标 {}({},{}) -> {}({},{}) 目标=({},{}) 判定={}",
            desktop,
            if inject.is_ok() { "成功" } else { "失败" },
            if got_before { "读到" } else { "读不到" },
            before.x,
            before.y,
            if got_after { "读到" } else { "读不到" },
            after.x,
            after.y,
            target_x,
            target_y,
            verdict
        );

        // 3. 把光标挪回原处，别让自检改变用户的鼠标位置。
        //    这里按「虚拟桌面原点为 (0,0)」换算——单显示器必然成立，
        //    多显示器且原点非零时恢复位置会有偏差，但不影响自检结论。
        if got_before {
            let (rx, ry) = self.to_absolute(
                before.x as f32 / self.vw as f32,
                before.y as f32 / self.vh as f32,
            );
            let _ = Self::mouse(abs, rx, ry, 0);
        }
    }

    /// 归一化坐标 → SendInput 绝对坐标（0..65535，覆盖整个虚拟桌面）。
    fn to_absolute(&self, x: f32, y: f32) -> (i32, i32) {
        let nx = x.clamp(0.0, 1.0) as f64 * 65535.0;
        let ny = y.clamp(0.0, 1.0) as f64 * 65535.0;
        (nx.round() as i32, ny.round() as i32)
    }

    fn post(input: Input) -> Result<()> {
        let sent = unsafe { SendInput(1, &input, size_of::<Input>() as c_int) };
        if sent != 1 {
            return Err(anyhow!(
                "SendInput 返回 {}（期望 1；通常是被 UIPI 拦截，即目标以更高完整性级别运行）",
                sent
            ));
        }
        Ok(())
    }

    fn mouse(flags: u32, dx: i32, dy: i32, data: u32) -> Result<()> {
        Self::post(Input {
            type_: INPUT_MOUSE,
            u: InputUnion {
                mi: MouseInput {
                    dx,
                    dy,
                    mouse_data: data,
                    flags,
                    time: 0,
                    extra_info: 0,
                },
            },
        })
    }

    fn key(vk: u16, up: bool) -> Result<()> {
        Self::post(Input {
            type_: INPUT_KEYBOARD,
            u: InputUnion {
                ki: KeybdInput {
                    vk,
                    scan: 0,
                    flags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    extra_info: 0,
                },
            },
        })
    }

    /// 按 Unicode 码元注入字符：绕开键盘布局，中文/emoji/需 Shift 的符号都能原样输入。
    fn unicode(unit: u16, up: bool) -> Result<()> {
        Self::post(Input {
            type_: INPUT_KEYBOARD,
            u: InputUnion {
                ki: KeybdInput {
                    vk: 0,
                    scan: unit,
                    flags: KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    extra_info: 0,
                },
            },
        })
    }
}

/// 协议里的修饰键位掩码 → 需要「染色」的虚拟键码。
/// 顺序即按下顺序（Ctrl、Alt、Shift、Win），释放时反序。
fn modifier_vks(mods: u8) -> Vec<u16> {
    let mut v = Vec::new();
    if mods & modifier::CONTROL != 0 {
        v.push(VK_CONTROL);
    }
    if mods & modifier::ALT != 0 {
        v.push(VK_MENU);
    }
    if mods & modifier::SHIFT != 0 {
        v.push(VK_SHIFT);
    }
    if mods & modifier::COMMAND != 0 {
        // Windows 键：协议里 COMMAND 的含义是「本平台的命令修饰键」，
        // macOS 是 ⌘、Windows 是 Win，同一个位掩码两端各自解释。
        v.push(VK_LWIN);
    }
    v
}

/// 协议里的滚动量是**像素**（macOS 侧直接按像素注入 CGEvent）。
/// Windows 的最小单位是「格」(mouseData=120)，一格约合 40~60 像素，
/// 这里按 40 像素/格换算，并保证非零输入至少滚一格——
/// 否则轻微的滑动手势会被四舍五入成 0，表现成「完全滚不动」。
fn scroll_units(px: f32) -> i32 {
    if px == 0.0 {
        return 0;
    }
    let units = (px / 40.0).round() as i32;
    if units == 0 {
        px.signum() as i32
    } else {
        units
    }
}

/// USB HID Usage ID（Keyboard/Keypad page 0x07）→ Windows 虚拟键码。
///
/// 与控制端（iOS）的约定是：协议里一律传**平台无关**的 HID Usage ID，
/// 由被控端翻译成本平台的键码。这与 macOS 侧的 hid_to_macos_vk 是同一张表的
/// Windows 版本——注意 VK 码和 macOS 虚拟键码毫无关系（A 在 macOS 是 0，
/// 在 Windows 是 0x41，即 'A' 的 ASCII），照抄另一边必然错位。
fn hid_to_windows_vk(hid: u32) -> Option<u16> {
    Some(match hid {
        // 字母 a-z：VK 恰好等于大写 ASCII
        0x04 => 0x41, // A
        0x05 => 0x42, // B
        0x06 => 0x43, // C
        0x07 => 0x44, // D
        0x08 => 0x45, // E
        0x09 => 0x46, // F
        0x0A => 0x47, // G
        0x0B => 0x48, // H
        0x0C => 0x49, // I
        0x0D => 0x4A, // J
        0x0E => 0x4B, // K
        0x0F => 0x4C, // L
        0x10 => 0x4D, // M
        0x11 => 0x4E, // N
        0x12 => 0x4F, // O
        0x13 => 0x50, // P
        0x14 => 0x51, // Q
        0x15 => 0x52, // R
        0x16 => 0x53, // S
        0x17 => 0x54, // T
        0x18 => 0x55, // U
        0x19 => 0x56, // V
        0x1A => 0x57, // W
        0x1B => 0x58, // X
        0x1C => 0x59, // Y
        0x1D => 0x5A, // Z
        // 数字行 1-9、0
        0x1E => 0x31,
        0x1F => 0x32,
        0x20 => 0x33,
        0x21 => 0x34,
        0x22 => 0x35,
        0x23 => 0x36,
        0x24 => 0x37,
        0x25 => 0x38,
        0x26 => 0x39,
        0x27 => 0x30,
        // 编辑与空白键
        0x28 => VK_RETURN,
        0x29 => VK_ESCAPE,
        0x2A => VK_BACK,
        0x2B => VK_TAB,
        0x2C => VK_SPACE,
        // 符号键：Windows 用 OEM 系列虚拟键码，必须按物理位置对应，
        // 不能按字符的 ASCII（'-' 的 VK 是 0xBD 而不是 0x2D，后者是 Insert）
        0x2D => 0xBD, // -
        0x2E => 0xBB, // =
        0x2F => 0xDB, // [
        0x30 => 0xDD, // ]
        0x31 => 0xDC, // \
        0x33 => 0xBA, // ;
        0x34 => 0xDE, // '
        0x35 => 0xC0, // `
        0x36 => 0xBC, // ,
        0x37 => 0xBE, // .
        0x38 => 0xBF, // /
        0x39 => VK_CAPITAL,
        // 功能键 F1-F12
        0x3A => 0x70,
        0x3B => 0x71,
        0x3C => 0x72,
        0x3D => 0x73,
        0x3E => 0x74,
        0x3F => 0x75,
        0x40 => 0x76,
        0x41 => 0x77,
        0x42 => 0x78,
        0x43 => 0x79,
        0x44 => 0x7A,
        0x45 => 0x7B,
        // 导航键
        0x49 => VK_INSERT,
        0x4A => VK_HOME,
        0x4B => VK_PRIOR,
        0x4C => VK_DELETE,
        0x4D => VK_END,
        0x4E => VK_NEXT,
        0x4F => VK_RIGHT,
        0x50 => VK_LEFT,
        0x51 => VK_DOWN,
        0x52 => VK_UP,
        _ => return None,
    })
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        let abs = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        match event {
            InputEvent::MouseMove { x, y } => {
                let (dx, dy) = self.to_absolute(*x, *y);
                Self::mouse(abs, dx, dy, 0)?;
                tracing::debug!("SendInput MouseMove -> ({},{})", dx, dy);
            }
            InputEvent::MouseButton { x, y, button, down } => {
                let (dx, dy) = self.to_absolute(*x, *y);
                // 先移动再按键：否则会在上一次的位置按下，点击偏到别处
                Self::mouse(abs, dx, dy, 0)?;
                let flag = match (*button, *down) {
                    (0, true) => MOUSEEVENTF_LEFTDOWN,
                    (0, false) => MOUSEEVENTF_LEFTUP,
                    (1, true) => MOUSEEVENTF_RIGHTDOWN,
                    (1, false) => MOUSEEVENTF_RIGHTUP,
                    (2, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (2, false) => MOUSEEVENTF_MIDDLEUP,
                    (b, _) => return Err(anyhow!("不支持的鼠标按键 {}", b)),
                };
                Self::mouse(flag, dx, dy, 0)?;
                tracing::debug!("SendInput MouseButton {} down={}", button, down);
            }
            InputEvent::Scroll { dx, dy } => {
                // mouseData 是 u32 装载的 i32，负值靠位转换传递，不能先取绝对值
                let v = -scroll_units(*dy) * 120;
                if v != 0 {
                    Self::mouse(MOUSEEVENTF_WHEEL, 0, 0, v as u32)?;
                }
                let h = scroll_units(*dx) * 120;
                if h != 0 {
                    Self::mouse(MOUSEEVENTF_HWHEEL, 0, 0, h as u32)?;
                }
                tracing::debug!("SendInput Scroll dx={} dy={}", dx, dy);
            }
            InputEvent::Key { keycode, down, modifiers } => {
                let vk = hid_to_windows_vk(*keycode)
                    .ok_or_else(|| anyhow!("未知的 HID 键码 0x{:02x}", keycode))?;
                if *down {
                    let mods = modifier_vks(*modifiers);
                    for m in &mods {
                        Self::key(*m, false)?;
                    }
                    Self::key(vk, false)?;
                    // 立刻释放修饰键，避免残留成「Ctrl 一直按着」的状态
                    for m in mods.iter().rev() {
                        Self::key(*m, true)?;
                    }
                } else {
                    Self::key(vk, true)?;
                }
                tracing::debug!(
                    "SendInput Key hid=0x{:02x} vk=0x{:02x} down={} modifiers=0x{:02x}",
                    keycode,
                    vk,
                    down,
                    modifiers
                );
            }
            InputEvent::Text { content } => {
                for unit in content.encode_utf16() {
                    Self::unicode(unit, false)?;
                    Self::unicode(unit, true)?;
                }
                tracing::debug!("SendInput Text len={}", content.len());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 结构体布局错一个字节，SendInput 就会静默失败（返回 0），
    /// 用户看到的现象是「点了完全没反应」，排查成本极高。这里把 ABI 尺寸锁死。
    #[test]
    fn input_struct_matches_windows_abi() {
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<Input>(), 40, "x64 上 sizeof(INPUT) 必须为 40");
        #[cfg(target_pointer_width = "32")]
        assert_eq!(size_of::<Input>(), 28, "x86 上 sizeof(INPUT) 必须为 28");
    }

    /// HID→VK 映射是手写的，最危险的错误是「照抄 macOS 表」或「用 ASCII 当 VK」。
    /// 挑几个两者差异大的键锁死。
    #[test]
    fn hid_maps_to_windows_vk_not_macos_or_ascii() {
        // A：HID 0x04 → VK 'A'(0x41)；macOS 侧是 0，ASCII 是 0x61
        assert_eq!(hid_to_windows_vk(0x04), Some(0x41));
        // C：VK 0x43
        assert_eq!(hid_to_windows_vk(0x06), Some(0x43));
        // Return：HID 0x28 → VK_RETURN 0x0D
        assert_eq!(hid_to_windows_vk(0x28), Some(VK_RETURN));
        // Escape：HID 0x29 → VK_ESCAPE 0x1B
        assert_eq!(hid_to_windows_vk(0x29), Some(VK_ESCAPE));
        // 减号：HID 0x2D → VK_OEM_MINUS 0xBD。若误写成 ASCII 0x2D，
        // 那就成了 VK_INSERT，按减号会变成插字符
        assert_eq!(hid_to_windows_vk(0x2D), Some(0xBD));
        assert_ne!(hid_to_windows_vk(0x2D), Some(VK_INSERT));
        // 方向键
        assert_eq!(hid_to_windows_vk(0x50), Some(VK_LEFT));
        assert_eq!(hid_to_windows_vk(0x52), Some(VK_UP));
    }

    #[test]
    fn unknown_hid_key_is_rejected() {
        assert_eq!(hid_to_windows_vk(0xFF), None);
        assert_eq!(hid_to_windows_vk(0x00), None);
    }

    /// ⌘ 与 ⌃ 必须是不同的键：位掩码混用会让 ⌘C（复制）变成 ⌃C（终端中断）
    #[test]
    fn command_and_control_map_to_different_keys() {
        let cmd = modifier_vks(modifier::COMMAND);
        let ctrl = modifier_vks(modifier::CONTROL);
        assert_eq!(cmd, vec![VK_LWIN]);
        assert_eq!(ctrl, vec![VK_CONTROL]);
        assert_ne!(cmd, ctrl);
        assert_eq!(modifier_vks(0), Vec::<u16>::new());
    }

    /// 多修饰键按固定顺序展开，保证「按下顺序」与「释放反序」成对
    #[test]
    fn combined_modifiers_expand_in_order() {
        let both = modifier_vks(modifier::CONTROL | modifier::SHIFT);
        assert_eq!(both, vec![VK_CONTROL, VK_SHIFT]);
    }

    /// 轻微滑动也必须产生滚动：四舍五入成 0 的话手势会「完全没反应」
    #[test]
    fn small_scroll_still_moves_one_unit() {
        assert_eq!(scroll_units(0.0), 0, "零位移不该滚动");
        assert_ne!(scroll_units(3.0), 0, "轻微上滑必须至少滚一格");
        assert_eq!(scroll_units(3.0), 1);
        assert_eq!(scroll_units(-3.0), -1, "反向同理");
        assert_eq!(scroll_units(400.0), 10, "400 像素应约合 10 格");
    }
}