#!/usr/bin/env python3
"""按会话边界严格统计 ICE 成败与候选来源，用于回答「4G 到底成功过没有」。

为什么需要它：早期用 `awk` 区间匹配 + 「往后找第一个 Connected」判定成败，
把下一个会话的成功算到了上一个失败会话头上（同一天内会话密集时尤其严重），
得出过「纯 4G 13 次成功 12 次」这种错误结论。本脚本按「为 X 建立会话」严格
切段，每段只认自己的结局，并按候选地址判定 iPhone 当时究竟走的是 Wi-Fi 还是
蜂窝——两者混在一起统计，结论必然错。

用法：python3 scripts/session_audit.py <去色后的 host.log> \
        [--wifi 192.168.1.100] [--cell 10.0.0.90]
"""
import argparse
import re
import sys

CAND_RE = re.compile(
    r"a=candidate:(\S+)\s+(?P<foundation>\d+)\s+(?P<proto>\w+)\s+(?P<prio>\d+)\s+"
    r"(?P<ip>[\w.:%-]+)\s+(?P<port>\d+)\s+typ\s+(?P<typ>\w+)"
    r"(?:\s+raddr\s+(?P<raddr>[\w.:%-]+)\s+rport\s+(?P<rport>\d+))?"
)
TS_RE = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)")
SESSION_RE = re.compile(r"为 (\S+) 建立会话")
SRFLX_RE = re.compile(r"srflx candidate (\S+?)（经 STUN 收集）")
FRAMES_RE = re.compile(r"共推送 (\d+) 帧")


def classify(ip, wifi, cell):
    """把候选地址归到 Wi-Fi / 蜂窝 / IPv6 / 其它。"""
    if ip.startswith("fe80:"):
        return "ipv6-linklocal"
    if ":" in ip:
        return "ipv6"
    if ip == wifi:
        return "wifi"
    if ip == cell:
        return "cell-internal"
    return "public-ipv4"


def audit(lines, wifi, cell):
    """按会话边界切段，返回每段的统计。"""
    starts = [i for i, l in enumerate(lines) if SESSION_RE.search(l)]
    segs = []
    for n, s in enumerate(starts):
        e = starts[n + 1] if n + 1 < len(starts) else len(lines)
        body = lines[s:e]
        seg = {
            "ts": (TS_RE.match(lines[s]).group(1) if TS_RE.match(lines[s]) else "?"),
            "viewer": SESSION_RE.search(lines[s]).group(1),
            "host_srflx": None,
            "answers": [],
            "result": "未收尾",
            "frames": None,
            "no_pairs": 0,
        }
        for l in body:
            m = SRFLX_RE.search(l)
            if m and not seg["host_srflx"]:
                seg["host_srflx"] = m.group(1)
            if "收到 answer SDP" in l:
                seg["answers"].append([])
            elif seg["answers"] and not l.startswith("20"):
                cm = CAND_RE.match(l.strip())
                if cm:
                    seg["answers"][-1].append(cm.groupdict())
            if "no candidate pairs" in l:
                seg["no_pairs"] += 1
            if "connection state: Connected" in l and seg["result"] == "未收尾":
                seg["result"] = "成功"
            if "connection state: Failed" in l and seg["result"] == "未收尾":
                seg["result"] = "失败"
            fm = FRAMES_RE.search(l)
            if fm:
                seg["frames"] = int(fm.group(1))
        segs.append(seg)
    return segs


def net_of(cands, wifi, cell):
    """按候选构成判断 iPhone 当时用的网络。"""
    kinds = {classify(c["ip"], wifi, cell) for c in cands}
    kinds |= {classify(c["raddr"], wifi, cell) for c in cands if c["raddr"]}
    has_wifi = "wifi" in kinds
    has_cell = "cell-internal" in kinds or "public-ipv4" in kinds
    if has_wifi and not has_cell:
        return "Wi-Fi"
    if has_wifi and has_cell:
        return "Wi-Fi+蜂窝"
    if has_cell:
        return "纯蜂窝"
    return "未知"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--wifi", default="192.168.1.100")
    ap.add_argument("--cell", default="10.0.0.90")
    a = ap.parse_args()

    lines = open(a.log, errors="replace").read().splitlines()
    segs = [s for s in audit(lines, a.wifi, a.cell) if s["answers"]]
    if not segs:
        print("没有可分析的会话", file=sys.stderr)
        return 1

    print("%-20s %-6s %-11s %-22s %s" % (
        "会话时间", "结局", "iPhone网络", "候选类型", "host srflx"))
    print("-" * 96)
    for s in segs:
        best = max(s["answers"], key=len)
        kinds = {}
        for c in best:
            k = classify(c["ip"], a.wifi, a.cell)
            kinds["%s/%s" % (c["typ"], k)] = kinds.get("%s/%s" % (c["typ"], k), 0) + 1
        res = s["result"]
        if res == "成功" and s["frames"] == 0:
            res = "成功?0帧"
        print("%-20s %-6s %-11s %-22s %s" % (
            s["ts"], res, net_of(best, a.wifi, a.cell),
            ",".join("%s×%d" % (k, v) for k, v in sorted(kinds.items()))[:22],
            s["host_srflx"] or "-"))

    print("-" * 96)
    from collections import defaultdict
    agg = defaultdict(lambda: [0, 0])
    for s in segs:
        best = max(s["answers"], key=len)
        net = net_of(best, a.wifi, a.cell)
        ok = 1 if s["result"] == "成功" and (s["frames"] or 0) > 0 else 0
        agg[net][0] += 1
        agg[net][1] += ok
    for net, (tot, ok) in sorted(agg.items()):
        print("  %-10s 共 %2d 次，真正出画 %2d 次" % (net, tot, ok))

    # 交叉表：纯蜂窝会话里，iPhone 是否拿到了公网 srflx 与结局的关系。
    # 这是判断「蜂窝下到底靠什么连通」的唯一硬依据——若成功的那些都有 srflx
    # 而失败的全没有，说明瓶颈在运营商对 STUN 的放行，而不是我们的代码。
    print()
    print("纯蜂窝会话：iPhone 有无公网 srflx × 结局")
    print("-" * 60)
    cross = defaultdict(lambda: [0, 0])
    detail = []
    for s in segs:
        best = max(s["answers"], key=len)
        if net_of(best, a.wifi, a.cell) != "纯蜂窝":
            continue
        pub = [c["ip"] for c in best if c["typ"] == "srflx"]
        key = "有 srflx" if pub else "无 srflx"
        ok = 1 if s["result"] == "成功" and (s["frames"] or 0) > 0 else 0
        cross[key][0] += 1
        cross[key][1] += ok
        detail.append((s["ts"], key, s["result"], s["frames"] or 0,
                       pub[0] if pub else "-"))
    for key, (tot, ok) in sorted(cross.items()):
        print("  %-10s 共 %2d 次，出画 %2d 次" % (key, tot, ok))
    print()
    print("  %-20s %-10s %-5s %-7s %s" % ("会话", "srflx", "结局", "帧数", "公网地址"))
    for row in detail:
        print("  %-20s %-10s %-5s %-7d %s" % row)
    return 0


if __name__ == "__main__":
    sys.exit(main())
