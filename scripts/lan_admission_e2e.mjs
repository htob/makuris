// 局域网直连的准入回归：验证 lan.rs 那条路径的校验确实生效。
//
// 为什么必须单独测这条路径，不能拿「信令路径已通过」推及它：局域网直连是
// 被控端自己直接接受连接的入口（handle_viewer），与中央信令是两套代码。
// 它也恰恰是手机日常最常用的那条路——同一 Wi-Fi 下 Bonjour 一发现就点进去。
// 只测信令路径，等于把最常走的那扇门留在未验证状态。
//
// 判定与 security_e2e.mjs 同一套思路：只看「该拒的有没有拒、该放的有没有放」，
// 不看会话内部细节。证明一律用 Node crypto 独立算，避免与实现共享同一处错误。
//
// 用法：
//   MAKURIS_LAN_PORT=8544 \
//   MAKURIS_DEVICE_ID=host-test-lan \
//   MAKURIS_PAIRING_CODE=135790 \
//   MAKURIS_LOG_DIR=/tmp/makuris-lan-e2e \
//     ./target/release/makuris-host        # 另开一个终端（信令连不上无妨）
//   node scripts/lan_admission_e2e.mjs
import crypto from 'node:crypto';

const PORT = process.env.MAKURIS_LAN_PORT || 8544;
const URL = `ws://127.0.0.1:${PORT}/ws`;
const PAIRING_CODE = process.env.MAKURIS_TEST_PAIRING_CODE || '135790';
const HOST_ID = process.env.MAKURIS_TEST_HOST_ID || 'host-test-lan';
const WAIT_MS = 15000;

let passed = 0;
let failed = 0;
const ok = (n, e = '') => { passed++; console.log(`  ✅ ${n}${e ? ' —— ' + e : ''}`); };
const bad = (n, d) => { failed++; console.log(`  ❌ ${n} —— ${d}`); };

const pairProof = (nonce) => crypto.createHmac('sha256', PAIRING_CODE).update(nonce).digest('hex');
const typeIs = (t) => (m) => m.type === t;

function open(label) {
  const ws = new WebSocket(URL);
  const inbox = [];
  ws.onmessage = (e) => { try { inbox.push(JSON.parse(e.data)); } catch { /* 忽略 */ } };
  const ready = new Promise((resolve, reject) => {
    ws.onopen = () => resolve();
    ws.onerror = () => reject(new Error(`${label}: 连接失败（服务起来了吗？端口 ${PORT}）`));
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

console.log(`局域网直连端点 ${URL}`);
console.log(`被控端 device_id ${HOST_ID}   配对码 ${PAIRING_CODE}\n`);
console.log('【局域网直连准入】');

const c = open('lan-viewer');
try {
  await c.ready;
} catch (e) {
  bad('连接局域网端点', e.message);
  console.log(`\n通过 ${passed} 项，失败 ${failed} 项`);
  process.exit(1);
}
ok('可连接局域网端点（Bonjour 发现后就是连这里）');

// 1. 注册：直连模式不校验接入密钥（不过信令服务器），但流程要对齐
c.send({ type: 'register', device_id: 'lan-viewer-e2e', role: 'viewer' });
const reg = await c.waitFor(typeIs('registered'));
if (reg) ok('直连注册成功', reg.device_id);
else bad('直连注册成功', '未收到 registered');

// 2. 设备列表：直连端点只报自己
let mark = c.inbox.length;
c.send({ type: 'list_devices' });
const list = await c.waitFor(typeIs('device_list'), mark);
if (list?.devices?.some((d) => d.device_id === HOST_ID)) {
  ok('直连端点报自己的设备信息', `id=${HOST_ID} ip/port 由 Bonjour TXT 提供`);
} else {
  bad('直连端点报自己的设备信息', list ? '列表里没有本机' : '未收到 device_list');
}

// 3. 无令牌连接：必须被要求配对，且不能已经推流
mark = c.inbox.length;
c.send({ type: 'connect', from: 'lan-viewer-e2e', to: HOST_ID });
const challenge = await c.waitFor(typeIs('auth_required'), mark);
let nonce = null;
if (challenge?.nonce) { nonce = challenge.nonce; ok('无令牌连接被要求配对', `nonce ${nonce.slice(0, 12)}…`); }
else bad('无令牌连接被要求配对', challenge ? '没带 nonce' : '未收到 auth_required');

const earlyOffer = c.inbox.slice(mark).find(typeIs('sdp'));
if (!earlyOffer) ok('未配对时不推流（没有 offer）');
else bad('未配对时不推流', '收到 offer，准入没生效');

// 4. 错误配对码：应被拒
if (nonce) {
  mark = c.inbox.length;
  c.send({ type: 'auth_response', from: 'lan-viewer-e2e', to: HOST_ID, proof: 'a'.repeat(64) });
  const denied = await c.waitFor(typeIs('auth_denied'), mark);
  const granted = c.inbox.slice(mark).find(typeIs('auth_granted'));
  if (denied && !granted) ok('错误配对码被拒', denied.reason);
  else bad('错误配对码被拒', granted ? '竟然通过了' : '未收到 auth_denied');
}

// 5. 正确配对码：应放行、签发令牌，并真的开始推流
mark = c.inbox.length;
c.send({ type: 'connect', from: 'lan-viewer-e2e', to: HOST_ID });
const ch2 = await c.waitFor(typeIs('auth_required'), mark);
let token = null;
if (ch2?.nonce) {
  mark = c.inbox.length;
  c.send({ type: 'auth_response', from: 'lan-viewer-e2e', to: HOST_ID, proof: pairProof(ch2.nonce) });
  const granted = await c.waitFor(typeIs('auth_granted'), mark);
  token = granted?.token ?? null;
  if (token) ok('正确配对码通过并签发令牌', `token ${token.slice(0, 12)}…`);
  else bad('正确配对码通过并签发令牌', '未收到 auth_granted');

  const offer = await c.waitFor(typeIs('sdp'), mark);
  if (offer?.sdp?.includes('m=video')) {
    ok('配对后真正开始会话', `offer ${offer.sdp.length} 字节`);
    const hasH264 = /H264/.test(offer.sdp);
    const hasSrflx = /typ srflx/.test(offer.sdp);
    if (hasH264) ok('offer 含 H264 视频');
    else bad('offer 含 H264 视频', '没有 H264 行');
    // 代理已关闭，这里应拿到真实宽带出口；拿不到说明 STUN 又走了别的出口
    if (hasSrflx) {
      const m = offer.sdp.match(/candidate:\S+ \S+ udp \S+ (\S+) (\d+) typ srflx/);
      ok('offer 含公网候选（srflx）', m ? `映射 ${m[1]}:${m[2]}` : '');
    } else {
      bad('offer 含公网候选（srflx）', '只有内网候选，跨网络连不上');
    }
  } else {
    bad('配对后真正开始会话', offer ? 'sdp 不是视频 offer' : '未收到 offer');
  }
} else {
  bad('第二次挑战', '未收到 auth_required');
}

// 6. 持令牌重连：应免输配对码，直接进会话
if (token) {
  const c2 = open('lan-viewer2');
  await c2.ready;
  c2.send({ type: 'register', device_id: 'lan-viewer-e2e', role: 'viewer' });
  await c2.waitFor(typeIs('registered'));
  c2.send({ type: 'connect', from: 'lan-viewer-e2e', to: HOST_ID, token });
  const offer = await c2.waitFor(typeIs('sdp'));
  const challenge2 = c2.inbox.find(typeIs('auth_required'));
  if (offer && !challenge2) ok('持令牌免配对直接进会话');
  else bad('持令牌免配对直接进会话', challenge2 ? '竟然又要配对' : '未收到 offer');
  c2.close();
}

c.close();
console.log(`\n通过 ${passed} 项，失败 ${failed} 项`);
process.exit(failed === 0 ? 0 : 1);