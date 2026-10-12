//! 设备准入控制：配对码 + 信任令牌。
//!
//! 解决的问题：此前被控端收到 `Connect` 就直接建会话。同一 Wi-Fi 下勉强可以
//! 把「在同一个网段」当作信任边界，但只要把端点暴露到公网，这就等于任何人
//! 都能看你的屏幕、动你的鼠标。所以准入必须显式。
//!
//! 方案选择上避开了一个常见做法——让控制端直接把配对码发过来比对。那样码
//! 会出现在链路上：局域网是明文 ws://，公网未配 TLS 时同样是明文，嗅探到
//! 就等于绕过全部准入。改成挑战应答后，链路上只走 nonce 和它的 HMAC 结果，
//! 配对码始终留在两端本地（算法见 `makuris_proto::pairing`）。
//!
//! 配对成功后签发信任令牌，写盘留存。此后同一台手机会带令牌连接，不再需要
//! 输码——否则每连一次都要去电脑上抄六位数字，这功能没人会用。
//!
//! 一个必须有的配套是**失败限速**：配对码只有 6 位（10^6 空间），若允许无限
//! 尝试，程序化暴破几分钟就能穷举完。锁定状态放在全局单例里而不是每个连接
//! 各存一份，否则攻击者只要不断重连就能把计数清零。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use makuris_proto::pairing;

/// 连续失败多少次后开始锁定。
const MAX_FAILURES: u32 = 5;
/// 首次锁定时长，之后按失败次数翻倍。
const LOCK_BASE: Duration = Duration::from_secs(30);
/// 锁定时长上限，避免一旦被反复试探就永久锁死自己的机器。
const LOCK_MAX: Duration = Duration::from_secs(3600);
/// nonce 有效期。给足用户「看了一眼手机、去电脑上抄码、再回来输」的时间。
const NONCE_TTL: Duration = Duration::from_secs(180);
/// 配对码轮换周期：5 分钟。
///
/// 为什么要轮换：码是永久值 + 公网信令此前是明文 ws，攻击者只要嗅探到
/// 一次配对流量，就能离线暴破 6 位码（10^6 次 HMAC 毫秒级算完），而码
/// 不变意味着**一次泄露 = 永久失守**。限时轮换把泄露码的可用窗口压到
/// 最多 5 分钟；配合「配对成功立即换码」（一次一用），窗口进一步压到
/// 单次配对。
const CODE_TTL: Duration = Duration::from_secs(300);

/// 状态目录，按各平台惯例放用户数据，不混进日志目录。
///
/// `pub` 是给 main.rs 用的：接入密钥的回落文件放在同一个目录（见 `access_key`）。
/// 两处各自拼一遍路径迟早会漂移成两个目录，那时「写了密钥却不生效」无从排查。
pub fn state_dir() -> PathBuf {
    // MAKURIS_STATE_DIR 优先，用于测试隔离。
    //
    // 这不是可选项：测试实例会往状态目录写信任令牌，而默认目录就是用户日常
    // 那一个。写进去的令牌虽然无人持有（随机生成、只存在于测试进程里），
    // 但会留在用户的授权列表里，让人分不清「这台设备是我配过对的吗」。
    // 有了这个开关，回归测试可以完全在自己的目录里跑，不碰用户数据。
    if let Ok(d) = std::env::var("MAKURIS_STATE_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d.trim());
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join("Library/Application Support/Makuris")
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        PathBuf::from(base).join("Makuris")
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let base = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{home}/.local/state")
        });
        PathBuf::from(base).join("makuris")
    }
}

pub struct Pairing {
    /// 当前配对码。可通过 MAKURIS_PAIRING_CODE 固定，便于自动化测试与反复部署。
    code: String,
    /// 当前码的生成/轮换时刻，TTL 判定用。
    code_issued: Instant,
    /// 码是否由环境变量固定。固定模式**不轮换**：部署者显式要一个稳定码
    /// （自动化、反复演示），就该得到稳定码；安全轮换只对随机码生效。
    code_pinned: bool,
    /// 已授权的令牌（持久化，重启后仍然免输码）。
    trusted: HashSet<String>,
    /// 各 viewer 待验证的 nonce：`viewer_id -> (nonce, 签发时刻)`。
    pending: HashMap<String, (String, Instant)>,
    failures: u32,
    locked_until: Option<Instant>,
    dir: PathBuf,
}

impl Pairing {
    fn load() -> Self {
        let dir = state_dir();
        let _ = std::fs::create_dir_all(&dir);

        // 配对码优先级：环境变量 > 已存盘的码 > 新生成。
        // 存盘是为了「重启后码不变」——否则每次重启都要重新抄一遍，
        // 而且用户手机上可能正开着配对界面。注意这只管重启：运行期
        // 随机码仍按 CODE_TTL 轮换（见 maintenance），env 固定码不轮换。
        let (code, code_pinned) = match std::env::var("MAKURIS_PAIRING_CODE") {
            Ok(v) if v.trim().len() >= 4 => (v.trim().to_owned(), true),
            _ => {
                let path = dir.join("pairing-code.txt");
                match std::fs::read_to_string(&path) {
                    Ok(v) if v.trim().len() >= 4 => (v.trim().to_owned(), false),
                    _ => {
                        let fresh = pairing::generate_code();
                        let _ = std::fs::write(&path, &fresh);
                        (fresh, false)
                    }
                }
            }
        };

        let trusted = std::fs::read_to_string(dir.join("trusted-tokens.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();

        Self {
            code,
            code_issued: Instant::now(),
            code_pinned,
            trusted,
            pending: HashMap::new(),
            failures: 0,
            locked_until: None,
            dir,
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    /// 通往配对码文件的可读路径，用于在日志里告知用户去哪里看码。
    pub fn code_path(&self) -> PathBuf {
        self.dir.join("pairing-code.txt")
    }

    /// 轮换配对码：生成新码并写盘。菜单栏助手每 2 秒重读码文件，
    /// 图标下拉里的码会自动跟随，无需任何通知机制。
    /// env 固定模式（MAKURIS_PAIRING_CODE）永不轮换——部署者显式要
    /// 稳定码就该得到稳定码。
    fn rotate(&mut self) {
        if self.code_pinned {
            return;
        }
        let fresh = pairing::generate_code();
        let _ = std::fs::write(self.dir.join("pairing-code.txt"), &fresh);
        tracing::info!("[pairing] 配对码已轮换为 {}（旧码即刻作废）", fresh);
        self.code = fresh;
        self.code_issued = Instant::now();
    }

    /// 周期维护主体（全局函数 `maintain` 加锁后调用；抽成实例方法便于单测）。
    ///
    /// 两条规则：
    /// 1. 先清过期 nonce——被放弃的 pending 条目如果不清，会让下面的
    ///    「有人正在配对」判断永远为真，码就永远不轮换（变相 DoS 自家安全）。
    /// 2. 有配对进行中（pending 非空）时不打断：用户正抄着码呢，码变了
    ///    会莫名其妙；等这次配对结束（成功即换码，失败/过期后下个周期换）。
    fn maintain_locked(&mut self) {
        self.pending.retain(|_, (_, issued)| issued.elapsed() <= NONCE_TTL);
        if self.code_pinned || !self.pending.is_empty() {
            return;
        }
        if self.code_issued.elapsed() >= CODE_TTL {
            self.rotate();
        }
    }

    /// 已配对的设备数。为 0 说明还没任何设备通过配对。
    pub fn trusted_count(&self) -> usize {
        self.trusted.len()
    }

    /// 令牌是否被信任。
    pub fn is_trusted(&self, token: &str) -> bool {
        // 空令牌一律不认：否则控制端漏发字段时会因「空 == 空」直接放行。
        !token.is_empty() && self.trusted.contains(token)
    }

    /// 剩余锁定时间；未锁定返回 None。
    pub fn lock_remaining(&self) -> Option<Duration> {
        let until = self.locked_until?;
        let now = Instant::now();
        if until > now {
            Some(until - now)
        } else {
            None
        }
    }

    /// 为该 viewer 签发一个一次性 nonce 并返回。
    ///
    /// 每次都新建而不是复用：nonce 一旦可预测或可重放，挑战应答就退化成
    /// 明文口令——攻击者录下一次成功的 proof 再重放即可。
    pub fn issue_nonce(&mut self, viewer_id: &str) -> String {
        let nonce = pairing::random_hex(16);
        self.pending.insert(viewer_id.to_owned(), (nonce.clone(), Instant::now()));
        nonce
    }

    /// 校验配对应答。成功返回新签发的信任令牌。
    pub fn submit(&mut self, viewer_id: &str, got_proof: &str) -> Result<String, String> {
        if let Some(left) = self.lock_remaining() {
            return Err(format!("尝试次数过多，请 {} 秒后再试", left.as_secs().max(1)));
        }

        // 无论成败都取走 nonce（一次性）。取不到说明没走过 AuthRequired，
        // 或者这个 nonce 已经用过——两种情况都不该接受。
        let Some((nonce, issued)) = self.pending.remove(viewer_id) else {
            return Err("配对会话已失效，请重新发起连接".to_owned());
        };
        if issued.elapsed() > NONCE_TTL {
            return Err("配对超时，请重新发起连接".to_owned());
        }

        let expected = pairing::proof(&self.code, &nonce);
        if !pairing::verify(&expected, got_proof) {
            self.failures += 1;
            if self.failures >= MAX_FAILURES {
                let extra = self.failures - MAX_FAILURES;
                let dur = LOCK_BASE
                    .checked_mul(1u32 << extra.min(16))
                    .unwrap_or(LOCK_MAX)
                    .min(LOCK_MAX);
                self.locked_until = Some(Instant::now() + dur);
                tracing::warn!("[pairing] 连续失败 {} 次，锁定 {} 秒", self.failures, dur.as_secs());
            }
            // 回话只说「不正确」，绝不透露偏差程度——那会把 6 位码拆成
            // 可逐位试探的问题，等于大幅缩小搜索空间。
            return Err("配对码不正确".to_owned());
        }

        self.failures = 0;
        self.locked_until = None;

        let token = pairing::random_hex(32);
        self.trusted.insert(token.clone());
        if let Err(e) = self.save_trusted() {
            // 存盘失败不影响本次放行：令牌在内存里已生效，只是重启后需要重新配对。
            tracing::warn!("[pairing] 信任令牌写入失败（重启后需重新配对）: {}", e);
        }
        tracing::info!("[pairing] {} 配对成功，已授权", viewer_id);
        // 一次一用：配对成功后立即换码。这个码已经出现在链路上被对方
        // 使用过（哪怕只是本次），作废它是最便宜的防重放/防泄露手段；
        // 新设备配对用菜单栏里的新码即可。
        self.rotate();
        Ok(token)
    }

    fn save_trusted(&self) -> std::io::Result<()> {
        let list: Vec<&String> = self.trusted.iter().collect();
        let json = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
        std::fs::write(self.dir.join("trusted-tokens.json"), json)
    }

    /// 撤销全部信任（用户怀疑令牌泄露时用；也可直接删文件后重启）。
    pub fn revoke_all(&mut self) {
        self.trusted.clear();
        let _ = self.save_trusted();
    }
}

/// 连接请求的处置结果。
pub enum Admission {
    /// 令牌有效，直接建会话。
    Granted,
    /// 需要配对：附本次的挑战随机数。
    Challenge(String),
    /// 拒绝（如正处于限速锁定中）：附给用户看的原因。
    Denied(String),
}

/// 收到 `Connect` 时的判定：带有效令牌放行，否则发挑战。
///
/// 抽成函数而不是让两条信令路径（中央信令、局域网直连）各写一遍：准入是
/// 安全边界，任何一处漏判都等于开了后门，重复实现迟早会漂移。
pub fn admit(viewer_id: &str, token: Option<&str>) -> Admission {
    let mut guard = global().lock().expect("pairing mutex");

    if let Some(t) = token {
        if guard.is_trusted(t) {
            tracing::info!("[pairing] {} 持有效令牌，直接放行", viewer_id);
            return Admission::Granted;
        }
        // 带令牌但没被认可：常见于用户删过信任文件、或令牌来自另一台机器。
        // 这里不报错，照常走配对流程，让用户重新输一次码即可恢复。
        tracing::info!("[pairing] {} 的令牌不被认可，转入配对流程", viewer_id);
    }

    if let Some(left) = guard.lock_remaining() {
        return Admission::Denied(format!("尝试次数过多，请 {} 秒后再试", left.as_secs().max(1)));
    }

    Admission::Challenge(guard.issue_nonce(viewer_id))
}

/// 处理配对应答，成功返回信任令牌。
pub fn respond(viewer_id: &str, proof: &str) -> Result<String, String> {
    global().lock().expect("pairing mutex").submit(viewer_id, proof)
}

/// 周期维护（main.rs 后台任务调用）：清理过期挑战、到点轮换配对码。
pub fn maintain() {
    global().lock().expect("pairing mutex").maintain_locked();
}

/// 全局单例：限速状态与信任列表必须跨连接共享，否则重连即可绕过。
static INSTANCE: OnceLock<Mutex<Pairing>> = OnceLock::new();

pub fn global() -> &'static Mutex<Pairing> {
    INSTANCE.get_or_init(|| Mutex::new(Pairing::load()))
}

/// 启动时打印一次准入状态：配对码、查看路径、已授权设备数。
///
/// 之所以要把码打进日志：被控端常常是双击启动的无界面 .app，
/// 用户没有别的途径知道该输什么。
pub fn announce() {
    let guard = global().lock().expect("pairing mutex");
    tracing::info!(
        "[pairing] 本机配对码 {}（查看 {:?}）；已授权设备 {} 台",
        guard.code(),
        guard.code_path(),
        guard.trusted_count()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立的临时状态目录：rotate 会往目录写码文件，
    /// 绝不能碰用户真实的 Application Support（那会改掉用户的码）。
    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("makuris-pairing-test-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn new_pairing(dir: PathBuf) -> Pairing {
        Pairing {
            code: pairing::generate_code(),
            code_issued: Instant::now(),
            code_pinned: false,
            trusted: HashSet::new(),
            pending: HashMap::new(),
            failures: 0,
            locked_until: None,
            dir,
        }
    }

    #[test]
    fn rotate_changes_code_and_persists() {
        let dir = temp_dir("rotate");
        let mut p = new_pairing(dir.clone());
        let old = p.code.clone();
        p.rotate();
        assert_ne!(p.code, old, "轮换后码必须变化");
        assert_eq!(p.code.len(), pairing::CODE_LEN);
        let on_disk = std::fs::read_to_string(dir.join("pairing-code.txt")).unwrap();
        assert_eq!(on_disk.trim(), p.code, "盘上必须与内存一致（菜单栏助手读的是盘）");
    }

    #[test]
    fn pinned_code_never_rotates() {
        let dir = temp_dir("pinned");
        let mut p = new_pairing(dir.clone());
        p.code = "123456".into();
        p.code_pinned = true;
        p.rotate();
        assert_eq!(p.code, "123456", "env 固定模式就该得到稳定码");
        assert!(!dir.join("pairing-code.txt").exists(), "固定码不落盘");
    }

    #[test]
    fn successful_pairing_rotates_immediately() {
        let dir = temp_dir("onetime");
        let mut p = new_pairing(dir.clone());
        let old = p.code.clone();
        let nonce = p.issue_nonce("viewer-a");
        let proof = pairing::proof(&old, &nonce);
        let token = p.submit("viewer-a", &proof).expect("正确 proof 应通过");
        assert!(!token.is_empty());
        assert_ne!(p.code, old, "一次一用：配对成功后码必须已换");
    }

    #[test]
    fn failed_pairing_keeps_code() {
        let dir = temp_dir("fail");
        let mut p = new_pairing(dir.clone());
        let old = p.code.clone();
        let _nonce = p.issue_nonce("viewer-b");
        assert!(p.submit("viewer-b", "deadbeef").is_err());
        assert_eq!(p.code, old, "失败不换码——否则攻击者可用失败请求诱导换码做探测");
    }

    #[test]
    fn maintain_rotates_expired_but_respects_pending_and_pin() {
        let expired = Instant::now()
            .checked_sub(CODE_TTL + Duration::from_secs(1))
            .expect("测试要求系统开机超过 CODE_TTL");
        // 过期且无人配对 → 轮换
        let dir = temp_dir("maintain-expired");
        let mut p = new_pairing(dir.clone());
        let old = p.code.clone();
        p.code_issued = expired;
        p.maintain_locked();
        assert_ne!(p.code, old, "码过期即轮换");
        // 过期但有人正在配对 → 不打断
        let dir = temp_dir("maintain-pending");
        let mut p = new_pairing(dir.clone());
        let old = p.code.clone();
        p.code_issued = expired;
        p.pending.insert("v".into(), ("n".into(), Instant::now()));
        p.maintain_locked();
        assert_eq!(p.code, old, "用户正抄码时不能变");
        // 过期但 env 固定 → 不轮换
        let dir = temp_dir("maintain-pinned");
        let mut p = new_pairing(dir.clone());
        p.code_pinned = true;
        p.code_issued = expired;
        let old = p.code.clone();
        p.maintain_locked();
        assert_eq!(p.code, old, "固定码永不轮换");
    }
}
