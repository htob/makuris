// 只读探针：经公网隧道连信令服务器，列出当前注册的设备。
//
// 为什么需要它：隧道只转发「信令」（TCP 8443），媒体流是另一条完全独立的路
// （UDP + ICE 打洞，不经过隧道）。所以「Windows 能不能通过隧道连」要拆成两问：
//   ① 信令层：Windows host 有没有注册到信令服务器、iPhone 在外网能不能看到它
//   ② 媒体层：iPhone 能不能通过 ICE 打洞连上 Windows 的 UDP 端口
// 本脚本只回答 ①，且绝不发 Connect——避免触发 host 的全局会话互斥把正在用的
// 会话踢掉（host 侧 ACTIVE_SESSION 是全局唯一的）。
//
// 注册必须附 key_proof：HMAC-SHA256(接入密钥, "register|" + device_id) 的小写 hex。
// 这是服务器防枚举的设计——没有密钥的人连不上信令，也就看不到家里有哪些电脑。
//
// 用法：SIG=wss://<隧道域名>/ws ACCESS_KEY=<密钥> node scripts/list_devices_probe.mjs
import { createHmac } from 'node:crypto';

const URL = process.env.SIG || 'ws://127.0.0.1:8443/ws';
const KEY = process.env.ACCESS_KEY || '';
const DEVICE_ID = 'probe-readonly';

if (!KEY) {
  console.log('RESULT=NO_ACCESS_KEY 需要 ACCESS_KEY 环境变量（服务器已开启密钥校验）');
  process.exit(2);
}

const proof = createHmac('sha256', KEY)
  .update('register|')
  .update(DEVICE_ID)
  .digest('hex');

const ws = new WebSocket(URL);
const timer = setTimeout(() => {
  console.log('RESULT=TIMEOUT 15s 内没拿到设备列表');
  process.exit(1);
}, 15000);

ws.onopen = () => {
  console.log(`[probe] 已连上信令: ${URL}`);
  ws.send(JSON.stringify({ type: 'register', device_id: DEVICE_ID, role: 'viewer', key_proof: proof }));
};

ws.onmessage = (ev) => {
  let m;
  try { m = JSON.parse(ev.data); } catch { return; }
  if (m.type === 'registered') {
    console.log('[probe] 注册成功（密钥校验通过）');
    ws.send(JSON.stringify({ type: 'list_devices' }));
  } else if (m.type === 'device_list') {
    console.log(`[probe] 设备列表（共 ${m.devices.length} 台）:`);
    for (const d of m.devices) {
      console.log(`   - ${d.device_id}  role=${d.role}  platform=${d.platform ?? 'null'}`);
    }
    const win = m.devices.find((d) => d.device_id === 'host-win-01');
    const mac = m.devices.find((d) => d.device_id === 'host-mac-01');
    console.log(`RESULT= win:${win ? 'REGISTERED' : 'MISSING'} mac:${mac ? 'REGISTERED' : 'MISSING'}`);
    clearTimeout(timer);
    ws.close();
    process.exit(0);
  } else if (m.type === 'error') {
    console.log(`[probe] 服务器报错: ${m.message}`);
    console.log('RESULT=SERVER_ERROR');
    clearTimeout(timer);
    process.exit(1);
  }
};

ws.onerror = (e) => {
  console.log(`[probe] WebSocket 错误: ${e.message || e.type}`);
  console.log('RESULT=WS_ERROR');
};
