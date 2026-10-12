#!/usr/bin/env node
/**
 * 公网信令链路探针：验证「隧道 → 信令」这条公网路径在鉴权开启后仍可用。
 *
 * 为什么要在让用户拿手机测之前先跑这个：公网链路上任何一环出问题，在手机上的
 * 表现都只是「连不上」，分不清是隧道没通、信令拒绝注册、还是密钥不对。这个脚本
 * 把三个环节拆开逐个给证据：
 *   ① 隧道能否升级为 WebSocket（Cloudflare 快速隧道对 WS 的支持不是必然的）
 *   ② 带正确接入证明能否注册并拿到设备列表（能列出 host-mac-01 才算真通）
 *   ③ 错误证明是否被拒（鉴权若形同虚设，公网暴露就等于把设备列表公开）
 *
 * 用法：node scripts/public_link_probe.mjs <wss地址> <接入密钥>
 * 密钥从命令行传入而不写死：它是要分发给各设备的凭证，不该出现在仓库里。
 */

import { createHmac } from "node:crypto";

const url = process.argv[2];
const key = process.argv[3];

if (!url || !key) {
    console.error("用法: node scripts/public_link_probe.mjs <wss地址> <接入密钥>");
    process.exit(2);
}

/** 与 proto/src/lib.rs 的 access::register_proof 保持同一算法。 */
function registerProof(key, deviceId) {
    return createHmac("sha256", key).update(`register|${deviceId}`).digest("hex");
}

const results = [];

/** 建立一条连接并在超时内收集消息，返回收到的消息数组。 */
function probe(deviceId, proof) {
    return new Promise((resolve) => {
        const inbox = [];
        let settled = false;
        const done = (outcome) => {
            if (settled) return;
            settled = true;
            clearTimeout(timer);
            try { ws.close(); } catch { /* 已关闭 */ }
            resolve({ ...outcome, inbox });
        };

        let ws;
        try {
            ws = new WebSocket(url);
        } catch (e) {
            resolve({ ok: false, reason: `WebSocket 构造失败: ${e.message}`, inbox });
            return;
        }

        const timer = setTimeout(() => done({ ok: false, reason: "超时未收到任何响应" }), 12000);

        ws.onopen = () => {
            const msg = { type: "register", device_id: deviceId, role: "viewer" };
            if (proof !== null) msg.key_proof = proof;
            ws.send(JSON.stringify(msg));
            // 注册与列表是两个来回，稍等片刻再请求，避免服务端还未完成注册
            setTimeout(() => ws.send(JSON.stringify({ type: "list_devices" })), 400);
        };
        ws.onmessage = (ev) => {
            inbox.push(ev.data);
            // 拿到设备列表说明这一轮的目的已达成，不必等超时
            if (String(ev.data).includes("device_list")) done({ ok: true });
        };
        ws.onerror = () => done({ ok: false, reason: "WebSocket 错误（地址不可达或 TLS 失败）" });
        ws.onclose = (ev) => done({ ok: false, reason: `连接被关闭（code=${ev.code}）` });
    });
}

console.log(`目标: ${url}\n`);

// ① 带正确证明注册
const good = await probe("probe-viewer", registerProof(key, "probe-viewer"));
const sawList = good.inbox.some((m) => String(m).includes("device_list"));
const sawReject = good.inbox.some((m) => String(m).includes("error") || String(m).includes("denied"));
results.push([
    "① 隧道 + 带正确证明注册",
    sawList && !sawReject,
    good.inbox.length ? good.inbox.join("\n     ").slice(0, 400) : good.reason || "无响应",
]);

// ② 错误证明必须被拒
const bad = await probe("probe-evil", registerProof("wrong-key", "probe-evil"));
const badRejected = bad.inbox.some((m) => String(m).includes("error") || String(m).includes("denied"))
    || !bad.inbox.some((m) => String(m).includes("device_list"));
results.push([
    "② 错误证明应被拒绝",
    badRejected,
    bad.inbox.length ? bad.inbox.join("\n     ").slice(0, 300) : (bad.reason || "无响应"),
]);

console.log();
let failed = 0;
for (const [name, ok, detail] of results) {
    if (!ok) failed++;
    console.log(`${ok ? "✅" : "❌"} ${name}`);
    console.log(`     ${detail}`);
}

console.log();
if (failed === 0) {
    console.log("✅ 公网信令链路可用：隧道可承载 WebSocket，鉴权生效，设备可被发现。");
} else {
    console.log(`❌ 有 ${failed} 项未通过，公网链路尚不可用。`);
}
process.exit(failed === 0 ? 0 : 1);