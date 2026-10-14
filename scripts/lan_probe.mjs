// 局域网直连端点的只读探针：确认 ws://<host>:8444/ws 仍能正常握手、
// 注册、并返回设备列表。
//
// 为什么要单独写：iOS 侧的现象（点局域网电脑连不上）可能出在三个地方——
// 手机构不到这台电脑、端点不再服务、或两端 id 对不上。这个探针从 Mac 本机
// 直连端点，一次性排除掉「端点本身坏了」这一整类原因，不必靠用户在手机上
// 反复试。它只发 register 与 list_devices，不发起 Connect，因此不会写令牌、
// 不会改动任何持久状态。
//
// 用法：MAKURIS_LAN_URL=ws://192.168.1.100:8444/ws node scripts/lan_probe.mjs
const URL = process.env.MAKURIS_LAN_URL || 'ws://192.168.1.100:8444/ws';
const ID = 'probe-' + Date.now();

console.log(`连接 ${URL}（device_id=${ID}）`);
const ws = new WebSocket(URL);

ws.onopen = () => {
  console.log('✅ WebSocket 已建立，发送 register');
  ws.send(JSON.stringify({ type: 'register', device_id: ID, role: 'viewer' }));
};

ws.onmessage = (e) => {
  console.log('← ' + e.data);
  let msg;
  try { msg = JSON.parse(e.data); } catch { return; }
  if (msg.type === 'registered') {
    console.log('→ 已注册，请求设备列表');
    ws.send(JSON.stringify({ type: 'list_devices' }));
  }
  if (msg.type === 'device_list') {
    const ids = (msg.devices || []).map((d) => d.device_id);
    console.log(`📋 设备列表: ${JSON.stringify(ids)}`);
    // Bonjour 广播里的 id 必须与这里完全一致，否则手机点的那台电脑
    // 永远无法在列表中被匹配到（resolvePendingJoin 会一直等下去）
    console.log(ids.includes('host-mac-01')
      ? '✅ 列表含 host-mac-01（与 Bonjour 广播 id 一致）'
      : '❌ 列表不含 host-mac-01 —— id 与 Bonjour 广播不一致！');
    ws.close();
  }
};

ws.onerror = (e) => console.log('❌ WebSocket 错误: ' + (e.message || e.type));
ws.onclose = (e) => {
  console.log(`连接关闭 code=${e.code} reason=${e.reason || '(空)'}`);
  process.exit(0);
};

setTimeout(() => {
  console.log('⏱ 超时（6s）——端点未完成任何响应');
  process.exit(1);
}, 6000);
