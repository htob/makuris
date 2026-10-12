// 无头 viewer 模拟器：验证 host 的信令驱动 WebRTC 会话。
// 模拟 iOS 客户端：连接信令 → 注册 viewer → 请求设备列表 → 发 Connect → 等待 host 的 offer → 校验含 H264 视频。
// 用法：node scripts/viewer_sim.mjs
const SIGNALING = process.env.MAKURIS_SIGNALING || 'ws://127.0.0.1:8443/ws';
const VIEWER_ID = 'viewer-sim';
const TIMEOUT_MS = 12000;

let gotOffer = false;
const timer = setTimeout(() => {
  if (!gotOffer) {
    console.error('FAIL: timeout waiting for offer from host');
    process.exit(1);
  }
}, TIMEOUT_MS);

const ws = new WebSocket(SIGNALING);

ws.onopen = () => {
  console.log('[sim] connected to signaling');
  ws.send(JSON.stringify({ type: 'register', device_id: VIEWER_ID, role: 'viewer' }));
};

ws.onmessage = (ev) => {
  let msg;
  try { msg = JSON.parse(ev.data); } catch { return; }

  switch (msg.type) {
    case 'registered':
      console.log('[sim] registered as', msg.device_id);
      ws.send(JSON.stringify({ type: 'list_devices' }));
      break;

    case 'device_list': {
      const host = msg.devices.find(d => d.role === 'host');
      if (!host) {
        console.log('[sim] no host yet, retrying in 1s...');
        setTimeout(() => ws.send(JSON.stringify({ type: 'list_devices' })), 1000);
        return;
      }
      console.log('[sim] found host:', host.device_id, '-> sending connect');
      ws.send(JSON.stringify({ type: 'connect', from: VIEWER_ID, to: host.device_id }));
      break;
    }

    case 'sdp':
      console.log('[sim] received SDP:', msg.sdp_type, 'from', msg.from);
      if (msg.sdp_type === 'offer') {
        gotOffer = true;
        const sdp = msg.sdp;
        const hasVideo = sdp.includes('m=video');
        const hasH264 = sdp.toUpperCase().includes('H264');
        console.log('[sim] offer length:', sdp.length, '| hasVideo:', hasVideo, '| hasH264:', hasH264);
        if (hasVideo && hasH264) {
          console.log('SUCCESS: host sent valid H264 video offer via signaling');
          clearTimeout(timer);
          process.exit(0);
        } else {
          console.error('FAIL: offer missing video/H264');
          clearTimeout(timer);
          process.exit(1);
        }
      }
      break;

    case 'ice':
      console.log('[sim] received ICE candidate from', msg.from);
      break;

    default:
      break;
  }
};

ws.onerror = (e) => {
  console.error('[sim] websocket error:', e.message || e);
};
