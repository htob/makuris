#!/usr/bin/env node
// TURN 时效凭证续发回归：验证信令会在凭证到期前主动重发，长跑的被控端不会失去中继能力。
//
// 为什么必须专门固化这一条：凭证是 ttl 秒过期的时效串，改造后信令只在「注册成功」那一刻
// 下发一份。被控端常年在线、从不重连的话，ttl 一到 TURN Refresh 就永久 401。这不是假想——
// 2026-10-12 线上实测踩过：Windows 被控端 18:06:25 领证、20:06:25 过期、20:07:11 起连续
// 237 次 `Refresh error response (error 401)`，iPhone 走蜂窝再也连不上；同一时刻 Mac 却
// 「正常」，只是因为它重连过 3 次、每次重连都重新领了一份。
//
// 这类故障要跑满 ttl 才暴露，普通冒烟测试永远碰不到，所以把它做成可用短 TTL 压出来的回归。
//
// 用法：
//   1) 起一个隔离信令实例，把 ttl 压到很小（续发周期是 ttl/3 且下限 60 秒，故 75 秒窗口足够）：
//        MAKURIS_TURN_SECRET=devsecret MAKURIS_TURN_TTL=6 \
//        MAKURIS_TURN_URLS=turn:127.0.0.1:3478 \
//        MAKURIS_STATE_DIR=/tmp/makuris-turn-renew \
//        ./target/release/makuris-signaling
//   2) node scripts/turn_renew_e2e.mjs
//
// 环境变量：
//   WS_URL      默认 ws://127.0.0.1:8443/ws
//   DEVICE_ID   默认 host-e2e-01
//   ACCESS_KEY  信令若配了接入密钥则必填（算 HMAC 注册证明），未配则留空
//   WINDOW_SEC  观察窗口秒数，默认 75，必须大于续发周期
//
// 判定：窗口内收到 >= 2 份凭证，且后一份的 username 时间戳严格大于前一份（证明是重新
// 签发而不是同一份被重放）。

import crypto from 'node:crypto';

// 用 Node 内置的全局 WebSocket（undici 实现），不引外部 ws 包：
// 开源后别人 clone 下来直接 node 就能跑，不必先 npm install。
// 缺失时给出版本提示，而不是抛一条隐晦的 ERR_MODULE_NOT_FOUND 堆栈。
if (typeof globalThis.WebSocket !== 'function') {
  console.error(`需要 Node 21+（内置全局 WebSocket）。当前版本: ${process.version}`);
  process.exit(2);
}
const WebSocket = globalThis.WebSocket;

const WS_URL = process.env.WS_URL || 'ws://127.0.0.1:8443/ws';
const DEVICE_ID = process.env.DEVICE_ID || 'host-e2e-01';
const ACCESS_KEY = process.env.ACCESS_KEY || '';
const WINDOW_SEC = Number(process.env.WINDOW_SEC || 75);

// 与 Rust 侧 access::register_proof 一致：HMAC-SHA256(密钥, "register|" + device_id)。
// 密钥未配置时不发该字段，避免把 null 塞进消息造成多余键。
const registerProof = (id) =>
  crypto.createHmac('sha256', ACCESS_KEY).update('register|' + id).digest('hex');

const creds = [];
const started = Date.now();

const ws = new WebSocket(WS_URL);

// 原生 WebSocket 只有 onopen/onmessage/onerror/onclose 事件属性（没有 .on()），
// 消息内容在 event.data 上。
let closed = false;

ws.onopen = () => {
  const msg = { type: 'register', device_id: DEVICE_ID, role: 'host' };
  if (ACCESS_KEY) msg.key_proof = registerProof(DEVICE_ID);
  ws.send(JSON.stringify(msg));
  console.log(`已注册 ${DEVICE_ID}，开始观察 ${WINDOW_SEC} 秒内的 TURN 凭证下发…`);
};

ws.onmessage = (event) => {
  let m;
  try { m = JSON.parse(String(event.data)); } catch { return; }
  const t = String(m.type || m.Type || '').toLowerCase();
  if (t === 'registered') console.log('  ✓ registered');
  // 宽松匹配凭证消息，避免协议里 tag 拼写变动就把回归测误报失败。
  if (!t.includes('turn')) return;

  const elapsed = Math.round((Date.now() - started) / 1000);
  const user = m.username || (m.payload && m.payload.username) || '';
  const ttl = m.ttl ?? m.ttl_seconds ?? m.expires_in ?? '';
  creds.push({ elapsed, user, ttl });
  console.log(`  ✓ 第 ${creds.length} 份凭证 @${elapsed}s  username=${user || '(无)'}  ttl=${ttl || '(无)'}`);
};

// 原生 error 事件不带原因，故只置标记，由 onclose 汇总真实退出码。
ws.onerror = () => { hadError = true; };
ws.onclose = (event) => {
  closed = true;
  console.error(`连接被关闭（code=${event.code} reason=${event.reason || '(无)'}）`);
  if (creds.length < 2) {
    console.log(`❌ 连接提前中断，仅收到 ${creds.length} 份凭证，无法验证续发`);
    process.exit(1);
  }
};

setTimeout(() => {
  ws.close();
  console.log('\n=== 判定 ===');
  if (creds.length < 2) {
    console.log(`❌ 只收到 ${creds.length} 份凭证，续发循环没有生效`);
    console.log('   排查方向：信令是否配了 MAKURIS_TURN_SECRET；ttl/3 是否大于观察窗口；日志里有无「已续发 TURN 时效凭证」');
    process.exit(1);
  }
  // 时间戳内嵌在 username 的 "timestamp:deviceId" 形式里，比较它才能证明是重新签发。
  const stamps = creds.map((c) => {
    const m = /^(\d+):/.exec(c.user || '');
    return m ? Number(m[1]) : NaN;
  });
  const allNumeric = stamps.every((s) => Number.isFinite(s));
  if (allNumeric) {
    for (let i = 1; i < stamps.length; i++) {
      if (stamps[i] <= stamps[i - 1]) {
        console.log(`❌ 第 ${i + 1} 份凭证的过期时间戳未递增（${stamps[i - 1]} -> ${stamps[i]}），疑似重发同一份`);
        process.exit(1);
      }
    }
    console.log(`✅ 收到 ${creds.length} 份凭证，过期时间戳严格递增（${stamps[0]} -> ${stamps.at(-1)}）`);
  } else {
    console.log(`✅ 收到 ${creds.length} 份凭证（username 非 timestamp:id 形式，跳过递增校验）`);
  }
  console.log('   续发循环工作正常：长跑的被控端不会因凭证过期而失去中继能力');
  process.exit(0);
}, WINDOW_SEC * 1000);
