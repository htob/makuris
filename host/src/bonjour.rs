//! macOS 系统 Bonjour 广播（FFI 调用 libdns_sd）。
//!
//! 为什么不用纯 Rust 的 mdns-sd？
//! 本机实测对照：
//!   - mdns-sd 自注册 + 自浏览 → resolved = 0（连自己都发现不了）
//!   - 系统 dns-sd 自注册     → 立刻被浏览到
//!   - 本模块 FFI 注册        → 立刻被浏览到（探针 ODProbe 已验证）
//! 原因是 macOS 的 mDNSResponder 长期占用 5353 端口，独立的纯 Rust daemon
//! 既发不出也收不到多播。所以必须把广播交给系统守护进程，
//! 让它替我们发通告、答查询。
//!
//! 注意：`host` 参数刻意传 NULL，由系统注册本机所有接口地址。
//! 自己算 IP 反而危险——本机存在代理的 utun 虚拟网卡（198.18.0.1），
//! 选错网卡会让手机连到一个根本不存在的地址。

use std::ffi::CString;
use std::os::raw::{c_char, c_void};

type DNSServiceRef = *mut c_void;

/// 注册结果回调。注册是异步的，真正成功与否由这个回调告知。
type RegisterReply = extern "C" fn(
    sd_ref: DNSServiceRef,
    flags: u32,
    error_code: i32,
    name: *const c_char,
    regtype: *const c_char,
    domain: *const c_char,
    context: *mut c_void,
);

// 注意这里没有 `#[link(name = "dns_sd")]`：macOS 上 Bonjour 的符号
// （DNSServiceRegister 等）已包含在 libSystem 中，显式 -ldns_sd 反而会
// 报 "library 'dns_sd' not found"（该库不在默认搜索路径里）。
// 探针实测：直接声明 extern 即可正常解析并运行。
extern "C" {
    fn DNSServiceRegister(
        sd_ref: *mut DNSServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt_record: *const c_void,
        call_back: Option<RegisterReply>,
        context: *mut c_void,
    ) -> i32;

    /// 阻塞式驱动事件，必须持续调用，注册才会真正生效并保活。
    fn DNSServiceProcessResult(sd_ref: DNSServiceRef) -> i32;
}

/// TXT 记录编码：DNS-SD 要求每条为「1 字节长度 + "key=value"」，依次拼接。
fn build_txt(items: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in items {
        let entry = format!("{}={}", k, v);
        if entry.len() > 255 {
            continue;
        }
        out.push(entry.len() as u8);
        out.extend_from_slice(entry.as_bytes());
    }
    out
}

/// 注册并长期广播一个服务。
///
/// `regtype` 形如 `_makuris._tcp.`（结尾带点、不含 `.local.`）。
/// 返回后广播即已交给系统；内部会起一个线程持续驱动事件循环，
/// 该线程随进程退出而结束（进程没了，广播自然也就没了，无需显式注销）。
pub fn register(
    name: &str,
    regtype: &str,
    port: u16,
    txt: &[(&str, &str)],
) -> anyhow::Result<()> {
    let name_c = CString::new(name)?;
    let regtype_c = CString::new(regtype)?;
    let txt_bytes = build_txt(txt);

    let mut sd: DNSServiceRef = std::ptr::null_mut();
    let err = unsafe {
        DNSServiceRegister(
            &mut sd,
            0,                // flags：默认
            0,                // 所有网络接口
            name_c.as_ptr(),
            regtype_c.as_ptr(),
            std::ptr::null(), // domain：默认 local.
            std::ptr::null(), // host：NULL = 系统注册本机所有接口地址
            port.to_be(),     // 端口必须是网络字节序
            txt_bytes.len() as u16,
            if txt_bytes.is_empty() {
                std::ptr::null()
            } else {
                txt_bytes.as_ptr() as *const c_void
            },
            Some(on_reply),
            std::ptr::null_mut(),
        )
    };
    if err != 0 {
        anyhow::bail!("DNSServiceRegister 返回错误码 {}", err);
    }

    // 事件循环必须持续驱动，否则注册不会生效。原始指针非 Send，
    // 转成 usize 传递——sd 只由这一个线程使用，是安全的。
    let raw = sd as usize;
    std::thread::spawn(move || {
        let sd = raw as DNSServiceRef;
        loop {
            unsafe {
                DNSServiceProcessResult(sd);
            }
        }
    });

    Ok(())
}

extern "C" fn on_reply(
    _sd: DNSServiceRef,
    _flags: u32,
    error_code: i32,
    name: *const c_char,
    regtype: *const c_char,
    _domain: *const c_char,
    _context: *mut c_void,
) {
    if error_code != 0 {
        tracing::warn!("[lan] Bonjour 注册回调报错，错误码 {}", error_code);
        return;
    }
    let instance = unsafe { cstr_to_string(name) };
    let kind = unsafe { cstr_to_string(regtype) };
    tracing::info!("[lan] Bonjour 广播生效：{} {}", instance, kind);
}

unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
}