// 端到端安全流程验证：信令接入鉴权 + 设备配对挑战应答。
//
// 为什么要专门写这个脚本，而不是靠手动点 App 验证：这两套机制各自都有一个
// 「静默拒绝」的失败模式——密钥或配对码算错时，两端都不报错，只表现为
// 「连不上」「一直找不到电脑」「黑屏」。人工测试遇到这种表现，很容易误判为
// 网络问题而放过一个真的算法不一致。脚本会逐项断言「该拒的拒了、该放的放了」。
//
// 另外，证明是用 Node 的 crypto 独立算的，等于对 Rust 与 Swift 两侧的实现做了
// 一次交叉验证：脚本能通过，说明链路上跑的确实是标准 HMAC-SHA256，而不是
// 某种只在单侧成立的巧合约定。
//
// 用法（前置服务见文件末尾注释）：
//   node scripts/security_e2e.mjs
import crypto from 'node:crypto';

const PORT = process.env.MAKURIS_TEST_PORT || 8445;
const SIGNALING = `ws://127.0.0.1:${PORT}/ws`;
const ACCESS_KEY = process.env.MAKURIS_TEST_ACCESS_KEY || 'test-access-key-abc123';
const PAIRING_CODE = process.env.MAKURIS_TEST_PAIRING_CODE || '135790';
const HOST_ID = process.env.MAKURIS_TEST_HOST_ID || 'host-test-01';
const WAIT_MS = 12000;

let passed = 0;
let failed = 0;

function ok(name, extra = '') {
  passed++;
  console.log(`  ✅ ${name}${extra ? ' —— ' + extra : ''}`);
}
function bad(name, detail) {
  failed++;
  console.log(`  ❌ ${name} —— ${detail}`);
}

const hmac = (key, msg) => crypto.createHmac('sha256', key).update(msg).digest('hex');
// 必须与 Rust `makuris_proto::access::register_proof` 完全一致
const registerProof = (id) => hmac(ACCESS_KEY, 'register|' + id);
// 必须与 Rust `makuris_proto::pairing::proof` 完全一致
const pairProof = (nonce) => hmac(PAIRING_CODE, nonce);

/// 打开一条信令连接，带回「等新消息」的能力。
/// 等消息只在新到达的消息里找（用 from 下标切分），否则上一次的同类型消息
/// 会被重复匹配——比如两次 auth_required 会分不清是不是新的。
function open(label) {
  const ws = new WebSocket(SIGNALING);
  const inbox = [];
  ws.onmessage = (e) => {
    try { inbox.push(JSON.parse(e.data)); } catch { /* 忽略非 JSON */ }
  };
  const ready = new Promise((resolve, reject) => {
    ws.onopen = () => resolve();
    ws.onerror = () => reject(new Error(`${label}: 连接失败`));
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

  return {
    ws, inbox, ready, waitFor,
    send: (o) => ws.send(JSON.stringify(o)),
    close: () => ws.close(),
  };
}

const typeIs = (t) => (m) => m.type === t;

console.log(`信令端点 ${SIGNALING}`);
console.log(`接入密钥 ${ACCESS_KEY.slice(0, 6)}…   配对码 ${PAIRING_CODE}\n`);

// ── 一、信令接入鉴权 ───────────────────────────────────────────────
console.log('【一】信令接入鉴权');

// 1. 不带接入证明注册：应被拒
{
  const c = open('无证明');
  await c.ready;
  c.send({ type: 'register', device_id: 'intruder-1', role: 'viewer' });
  const err = await c.waitFor(typeIs('error'));
  const reg = c.inbox.find(typeIs('registered'));
  if (err && !reg) ok('未带接入证明的注册被拒', err.message);
  else bad('未带接入证明的注册被拒', reg ? '竟然注册成功了' : '没有拒绝，也没回 error');
  c.close();
}

// 2. 带错误证明注册：应被拒
{
  const c = open('错证明');
  await c.ready;
  c.send({ type: 'register', device_id: 'intruder-2', role: 'viewer', key_proof: 'deadbeef' });
  const err = await c.waitFor(typeIs('error'));
  const reg = c.inbox.find(typeIs('registered'));
  if (err && !reg) ok('接入证明错误的注册被拒');
  else bad('接入证明错误的注册被拒', reg ? '竟然注册成功了' : '没有拒绝');
  c.close();
}

// 3. 证明与 device_id 绑定：拿别人 id 的证明去注册自己，应被拒
{
  const c = open('跨id复用');
  await c.ready;
  const stolen = registerProof('someone-else');
  c.send({ type: 'register', device_id: 'intruder-3', role: 'viewer', key_proof: stolen });
  const err = await c.waitFor(typeIs('error'));
  const reg = c.inbox.find(typeIs('registered'));
  if (err && !reg) ok('证明不能跨 device_id 复用');
  else bad('证明不能跨 device_id 复用', reg ? '竟然注册成功了' : '没有拒绝');
  c.close();
}

// 4. 未注册连接请求设备列表：应得不到任何设备列表
{
  const c = open('未注册查询');
  await c.ready;
  c.send({ type: 'list_devices' });
  const list = await c.waitFor(typeIs('device_list'), 0, 4000);
  if (!list) ok('未注册连接拿不到设备列表');
  else bad('未注册连接拿不到设备列表', `泄露了 ${list.devices?.length} 台设备`);
  c.close();
}

// ── 二、历史遗留检查：旧客户端（不带 key_proof 字段） ──────────────
console.log('\n【二】设备列表与配对');

const viewer = open('viewer');
await viewer.ready;
viewer.send({
  type: 'register', device_id: 'viewer-e2e', role: 'viewer',
  key_proof: registerProof('viewer-e2e'),
});
const registered = await viewer.waitFor(typeIs('registered'));
if (registered) ok('带正确接入证明可注册', registered.device_id);
else { bad('带正确接入证明可注册', '未收到 registered'); }
if (!registered) {
  console.log('\n注册都失败，后续用例无从执行。');
  process.exit(1);
}

// 5. 注册后可见设备列表
viewer.send({ type: 'list_devices' });
const list = await viewer.waitFor(typeIs('device_list'));
if (list && list.devices.some((d) => d.device_id === HOST_ID)) {
  ok('注册后可枚举在线设备', list.devices.map((d) => d.device_id).join(', '));
} else {
  bad('注册后可枚举在线设备', list ? `列表里没有 ${HOST_ID}` : '未收到 device_list');
}

// ── 三、设备配对（挑战应答）───────────────────────────────────────
console.log('\n【三】设备配对挑战应答');

// 6. 不带令牌连接：host 应回挑战而非直接建会话
let mark = viewer.inbox.length;
viewer.send({ type: 'connect', from: 'viewer-e2e', to: HOST_ID });
const challenge = await viewer.waitFor(typeIs('auth_required'), mark);
let nonce = null;
if (challenge?.nonce) {
  nonce = challenge.nonce;
  ok('无令牌连接被要求配对', `nonce ${nonce.slice(0, 12)}…`);
} else {
  bad('无令牌连接被要求配对', challenge ? '没有带 nonce' : '未收到 auth_required');
}
// 关键：此刻绝不能已经建会话（有 offer 就等于准入形同虚设）
const earlyOffer = viewer.inbox.slice(mark).find(typeIs('sdp'));
if (!earlyOffer) ok('未配对时不推流（没有 offer）');
else bad('未配对时不推流', '收到 offer，说明准入没生效');

// 7. 错误的配对证明：应被拒
if (nonce) {
  mark = viewer.inbox.length;
  viewer.send({ type: 'auth_response', from: 'viewer-e2e', to: HOST_ID, proof: 'f'.repeat(64) });
  const denied = await viewer.waitFor(typeIs('auth_denied'), mark);
  const granted = viewer.inbox.slice(mark).find(typeIs('auth_granted'));
  if (denied && !granted) ok('错误配对码被拒', denied.reason);
  else bad('错误配对码被拒', granted ? '竟然通过了' : '未收到 auth_denied');
}

// 8. nonce 一次性：同一个 nonce 重放应失败
{
  mark = viewer.inbox.length;
  viewer.send({ type: 'connect', from: 'viewer-e2e', to: HOST_ID });
  const ch2 = await viewer.waitFor(typeIs('auth_required'), mark);
  if (ch2?.nonce) {
    const goodProof = pairProof(ch2.nonce);
    mark = viewer.inbox.length;
    viewer.send({ type: 'auth_response', from: 'viewer-e2e', to: HOST_ID, proof: goodProof });
    const granted = await viewer.waitFor(typeIs('auth_granted'), mark);
    if (granted?.token) ok('正确配对码通过并签发令牌', `token ${granted.token.slice(0, 12)}…`);
    else bad('正确配对码通过并签发令牌', '未收到 auth_granted');

    // 重放同一个 proof：nonce 已被消费，必须失败
    mark = viewer.inbox.length;
    viewer.send({ type: 'auth_response', from: 'viewer-e2e', to: HOST_ID, proof: goodProof });
    const replayDenied = await viewer.waitFor(typeIs('auth_denied'), mark);
    if (replayDenied) ok('重放同一证明无效（nonce 一次性）', replayDenied.reason);
    else bad('重放同一证明无效', '重放竟然又通过了');
  } else {
    bad('第二次挑战', '未收到 auth_required');
  }
}

// 9. 带令牌连接：应直接放行并开始推流（免输配对码）
{
  const grantedToken = viewer.inbox.find(typeIs('auth_granted'))?.token;
  if (grantedToken) {
    mark = viewer.inbox.length;
    viewer.send({ type: 'connect', from: 'viewer-e2e', to: HOST_ID, token: grantedToken });
    const offer = await viewer.waitFor(typeIs('sdp'), mark);
    if (offer?.sdp?.includes('m=video')) {
      ok('持令牌直接放行并开始会话', `offer ${offer.sdp.length} 字节`);
      // offer 里该有的东西顺带核一遍：公网候选与 H264
      const hasSrflx = /typ srflx/.test(offer.sdp);
      const hasH264 = /H264/.test(offer.sdp);
      if (hasH264) ok('offer 含 H264 视频');
      else bad('offer 含 H264 视频', '没有 H264 行');
      if (hasSrflx) ok('offer 含公网候选（srflx）', '跨网络具备打洞条件');
      else bad('offer 含公网候选（srflx）', '只有内网候选，跨网络连不上');
    } else {
      bad('持令牌直接放行并开始会话', offer ? '收到的 sdp 不是视频 offer' : '未收到 offer');
    }
  } else {
    bad('持令牌直接放行', '上一步没拿到令牌，跳过');
  }
}

// 10. 伪造令牌：应回到配对流程而不是放行
//
// 判定只看「有没有收到新的挑战」，不去找窗口里有没有 offer。
// 这个用例最初写成「收到挑战 且 没有 offer」，结果误报了一次失败：上一步
// 建立的会话会继续产生 ICE 等消息，它们落在同一个窗口里，被当成了新放行的证据。
// 实际上 admit 的结果是互斥的——要么放行、要么发挑战，不可能两者都做，
// 所以「收到挑战」本身就足以证明没有被放行。少一个条件反而更准。
{
  mark = viewer.inbox.length;
  viewer.send({ type: 'connect', from: 'viewer-e2e', to: HOST_ID, token: 'attacker-token' });
  const back = await viewer.waitFor(typeIs('auth_required'), mark);
  const granted = viewer.inbox.slice(mark).find(typeIs('auth_granted'));
  if (back && !granted) ok('伪造令牌不被认可，退回配对');
  else bad('伪造令牌不被认可', granted ? '竟然签发了令牌' : '未回到配对流程');
}

viewer.close();

console.log(`\n通过 ${passed} 项，失败 ${failed} 项`);
process.exit(failed === 0 ? 0 : 1);

// ─────────────────────────────────────────────────────────────────
// 前置服务（在仓库根目录另开两个终端，或直接用下面这段）：
//
//   # 1) 信令服务器（8445，启用接入鉴权）
//   MAKURIS_ACCESS_KEY=test-access-key-abc123 PORT=8445 \
//     ./target/release/makuris-signaling
//
//   # 2) 被控端（同密钥、固定配对码、独立日志目录，避免干扰日常实例）
//   MAKURIS_SIGNALING=ws://127.0.0.1:8445/ws \
//   MAKURIS_ACCESS_KEY=test-access-key-abc123 \
//   MAKURIS_DEVICE_ID=host-test-01 \
//   MAKURIS_PAIRING_CODE=135790 \
//   MAKURIS_LOG_DIR=/tmp/makuris-e2e \
//     ./target/release/makuris-host
// ─────────────────────────────────────────────────────────────────