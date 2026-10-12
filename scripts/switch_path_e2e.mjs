// 切换连接线路的回归：验证 host 全局只允许一个会话存活。
//
// 为什么必须专门测这条路径：用户最常做的操作是「先经公网连上，回来再点
// 局域网那台」或反过来。修之前，host 的两条会话入口（中央信令、局域网直连）
// 各持一个句柄互不可见，切线路时旧会话根本不会结束——它继续采集屏幕、
// 继续编码推流，与新会话抢同一块屏幕和同一个 VideoToolbox 编码器。
//
// 判定依据不看画面，只看 host 日志：每次经另一条路径发起 Connect 时，
// 都必须出现一次「已有会话在跑，先停掉它再建新会话」。这个计数是决定性的——
// 少一次就说明旧会话又被留在后台了。
//
// 用法（前置服务见文件末尾注释）：
//   node scripts/switch_path_e2e.mjs
import crypto from 'node:crypto';
import fs from 'node:fs';

const SIG_PORT = process.env.MAKURIS_TEST_PORT || 8445;
const LAN_PORT = process.env.MAKURIS_TEST_LAN_PORT || 8544;
const SIG_URL = `ws://127.0.0.1:${SIG_PORT}/ws`;
const LAN_URL = `ws://127.0.0.1:${LAN_PORT}/ws`;
const PAIRING_CODE = process.env.MAKURIS_TEST_PAIRING_CODE || '135790';
const HOST_ID = process.env.MAKURIS_TEST_HOST_ID || 'host-switch-01';
const LOG = process.env.MAKURIS_TEST_LOG || '/tmp/makuris-switch/host.log';
const VIEWER = 'switch-viewer-01';
const WAIT_MS = 15000;

let passed = 0;
let failed = 0;
const ok = (n, e = '') => { passed++; console.log(`  ✅ ${n}${e ? ' —— ' + e : ''}`); };
const bad = (n, d) => { failed++; console.log(`  ❌ ${n} —— ${d}`); };
const typeIs = (t) => (m) => m.type === t;

const pairProof = (nonce) => crypto.createHmac('sha256', PAIRING_CODE).update(nonce).digest('hex');

/// 信令接入证明。信令服务器启用鉴权时，viewer 的注册也必须带上它，
/// 否则会被直接驳回——而症状只是「设备列表一直不来」，与密钥没有
/// 任何字面关联。算法与 Rust 侧 access::register_proof 一致。
const ACCESS_KEY = process.env.MAKURIS_TEST_ACCESS_KEY || 'test-access-key-abc123';
const registerProof = (id) => crypto.createHmac('sha256', ACCESS_KEY).update('register|' + id).digest('hex');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// 读取 host 日志里「停掉旧会话」的出现次数。
/// 这是本脚本唯一的判定信号，直接反映会话互斥有没有生效。
function stopCount() {
  try {
    const text = fs.readFileSync(LOG, 'utf8');
    return (text.match(/已有会话在跑，先停掉它再建新会话/g) || []).length;
  } catch {
    return -1; // 读不到日志：宁可报错，也不要误判成「没有停过」
  }
}

function open(url, label) {
  const ws = new WebSocket(url);
  const inbox = [];
  ws.onmessage = (e) => { try { inbox.push(JSON.parse(e.data)); } catch { /* 忽略 */ } };
  const ready = new Promise((resolve, reject) => {
    ws.onopen = () => resolve();
    ws.onerror = () => reject(new Error(`${label}: 连接失败（${url}）`));
    setTimeout(() => reject(new Error(`${label}: 连接超时`)), WAIT_MS);
  });
  const waitFor = (pred, from = 0, ms = WAIT_MS) => new Promise((resolve) => {
    const t0 = Date.now();
    const tick = setInterval(() => {
      for (let i = from; i < inbox.length; i++) {
        if (pred(inbox[i])) { clearInterval(tick); return resolve(inbox[i]); }
      }
      if (Date.now() - t0 > ms) { clearInterval(tick); resolve(null); }
    }, 40);
  });
  return { ws, inbox, ready, waitFor, send: (o) => ws.send(JSON.stringify(o)), close: () => ws.close() };
}

/// 在某个端点上完成「注册 → （必要时配对）→ 发起 Connect → 等到 offer」。
/// 配对码只输一次，之后复用 token——这与 App 的行为一致。
async function connectAt(url, label, token) {
  const c = open(url, label);
  await c.ready;
  c.send({ type: 'register', device_id: VIEWER, role: 'viewer', key_proof: registerProof(VIEWER) });
  await c.waitFor(typeIs('registered'));

  let mark = c.inbox.length;
  c.send({ type: 'connect', from: VIEWER, to: HOST_ID, token });

  const granted = c.inbox[mark - 1]?.type === 'auth_granted' ? c.inbox[mark - 1] : null;
  let useToken = token ?? granted?.token ?? null;

  // 没有可用令牌：走一次配对挑战
  const challenge = await c.waitFor(typeIs('auth_required'), mark, 3000);
  if (challenge?.nonce) {
    mark = c.inbox.length;
    c.send({ type: 'auth_response', from: VIEWER, to: HOST_ID, proof: pairProof(challenge.nonce) });
    const g = await c.waitFor(typeIs('auth_granted'), mark);
    if (!g?.token) throw new Error(`${label}: 配对未通过`);
    useToken = g.token;
    mark = c.inbox.length;
  }

  const offer = await c.waitFor(typeIs('sdp'), mark);
  return { conn: c, offer, token: useToken };
}

console.log(`信令 ${SIG_URL}`);
console.log(`局域网 ${LAN_URL}`);
console.log(`被控端日志 ${LOG}\n`);
console.log('【切换线路：host 是否保持单会话】');

if (stopCount() < 0) {
  bad('可读取被控端日志', `读不到 ${LOG}（服务起来了吗？）`);
  process.exit(1);
}
// 基线在「线路一建立之后」才取。
// 不这样做会误判：上一次运行若留下过会话残留，线路一启动时也会打印一次
// 「已有会话在跑」，把它算进切换增量就会得出「切换时多停了一次」的假结论。
let base = 0;

let token = null;
let first;
try {
  // 第一条线路：中央信令（公网走的就是这条）
  first = await connectAt(SIG_URL, '信令线路', null);
  token = first.token;
  base = stopCount();
  if (first.offer?.sdp?.includes('m=video')) ok('线路一（信令）建会话成功', `offer ${first.offer.sdp.length} 字节`);
  else bad('线路一（信令）建会话成功', '未收到视频 offer');
} catch (e) {
  bad('线路一（信令）建会话成功', e.message);
  console.log(`\n通过 ${passed} 项，失败 ${failed} 项`);
  process.exit(1);
}

// 【关键断言】先不关掉线路一，直接经线路二发起 Connect。
// 这正是用户的操作：公网那条还连着，就去点局域网那台。
await sleep(500);
let second;
try {
  second = await connectAt(LAN_URL, '局域网线路', token);
  const afterFirstSwitch = stopCount();
  if (afterFirstSwitch === base + 1) {
    ok('切到局域网时旧会话被停掉', `「先停掉它再建新会话」计数 ${base} → ${afterFirstSwitch}`);
  } else {
    bad('切到局域网时旧会话被停掉',
      `计数未按预期递增（${base} → ${afterFirstSwitch}）：旧会话可能仍在后台推流`);
  }
  if (second.offer?.sdp?.includes('m=video')) ok('线路二（局域网）建会话成功');
  else bad('线路二（局域网）建会话成功', '未收到视频 offer');
} catch (e) {
  bad('线路二（局域网）建会话成功', e.message);
}

// 再切回信令：验证反向切换同样只留一个会话
await sleep(500);
try {
  const third = await connectAt(SIG_URL, '信令线路（切回）', token);
  const afterSecondSwitch = stopCount();
  if (afterSecondSwitch === base + 2) {
    ok('切回信令时旧会话被停掉', `计数 ${base + 1} → ${afterSecondSwitch}`);
  } else {
    bad('切回信令时旧会话被停掉',
      `计数未按预期递增（${base + 1} → ${afterSecondSwitch}）`);
  }
  if (third.offer?.sdp?.includes('m=video')) ok('线路三（切回信令）建会话成功');
  else bad('线路三（切回信令）建会话成功', '未收到视频 offer');
  third.conn.close();
} catch (e) {
  bad('线路三（切回信令）建会话成功', e.message);
}

second?.conn.close();
first.conn.close();

console.log(`\n通过 ${passed} 项，失败 ${failed} 项`);
process.exit(failed === 0 ? 0 : 1);

// ─────────────────────────────────────────────────────────────────
// 前置服务（两条路径都要在，且共用同一 STATE_DIR 使配对一次即通）：
//
//   # 1) 信令服务器（8445）
//   MAKURIS_ACCESS_KEY=test-access-key-abc123 PORT=8445 \
//     MAKURIS_STATE_DIR=/tmp/makuris-switch \
//     ./target/release/makuris-signaling
//
//   # 2) 被控端（信令走 8445，局域网听 8544，独立日志目录）
//   MAKURIS_SIGNALING=ws://127.0.0.1:8445/ws \
//   MAKURIS_ACCESS_KEY=test-access-key-abc123 \
//   MAKURIS_DEVICE_ID=host-switch-01 \
//   MAKURIS_PAIRING_CODE=135790 \
//   MAKURIS_LAN_PORT=8544 \
//   MAKURIS_STATE_DIR=/tmp/makuris-switch \
//   MAKURIS_LOG_DIR=/tmp/makuris-switch \
//     ./target/release/makuris-host
// ─────────────────────────────────────────────────────────────────