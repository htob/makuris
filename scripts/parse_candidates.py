import re,sys

# 解析被控端日志中每个会话的 ICE 候选与连通性结局。
# 用法: parse_candidates.py <日志路径> [起始时间戳]
# 输出每个会话：对端(answer SDP + trickle/补加)候选的类型与地址、本机 srflx、
# rtc_ice 的候选对检查与提名情况、最终 Connected/Failed 与推帧数。

path = sys.argv[1]
since = sys.argv[2] if len(sys.argv) > 2 else ''

raw = re.sub(r'\x1b\[[0-9;]*m', '', open(path, 'rb').read().decode('utf-8', 'replace'))
L = raw.splitlines()

CAND = re.compile(r'a=candidate:(\S+)\s+(\d+)\s+(\w+)\s+\d+\s+([\w.:%-]+)\s+(\d+)\s+typ\s+(\w+)')

starts = [i for i, l in enumerate(L)
          if '建立会话' in l and re.match(r'^2026-\d\d-\d\dT', l) and (not since or l >= since)]
print('会话数: %d' % len(starts))

for n, i in enumerate(starts):
    e = starts[n + 1] if n + 1 < len(starts) else len(L)
    body = L[i:e]
    res, frames = '?', '-'
    for l in body:
        m = re.search(r'共推送 (\d+) 帧', l)
        if m:
            frames = m.group(1)
        if 'connection state: Connected' in l:
            res = '成功'
        if 'connection state: Failed' in l:
            res = '失败'

    peer = []      # 对端候选（answer SDP 内联 + 补加 + trickle）
    mine_srflx = []
    for l in body:
        m = CAND.search(l)
        if m:
            peer.append((m.group(6), m.group(4)))
        if '补加候选' in l or 'trickle 候选' in l:
            m2 = re.search(r'candidate:(\S+)\s+\d+\s+(\w+)\s+\d+\s+([\w.:%-]+)\s+(\d+)\s+typ\s+(\w+)', l)
            if m2:
                peer.append((m2.group(5), m2.group(3)))
        sm = re.search(r'srflx candidate ([\w.:%-]+):(\d+)', l)
        if sm:
            mine_srflx.append('%s:%s' % (sm.group(1), sm.group(2)))

    v4 = sorted({a for t, a in peer if ':' not in a})
    v6 = sorted({a for t, a in peer if ':' in a})
    srfl = sorted({a for t, a in peer if t == 'srflx'})
    kinds = {}
    for t, a in peer:
        kinds[t] = kinds.get(t, 0) + 1

    pairs = sum(1 for l in body if 'no candidate pairs' in l)
    nomin = [l for l in body if 'nominat' in l.lower()]
    checks = sum(1 for l in body if 'ice_ice' in l or 'CandidatePair' in l or 'pair' in l.lower())

    print('\n%s  %s  推帧=%s' % (body[0][:19], res, frames))
    print('   对端候选 %d 条 类型=%s' % (len(peer), kinds or '{}'))
    print('     IPv4 : %s' % (v4 or '无'))
    print('     IPv6 : %d 条 %s' % (len(v6), [x[:24] for x in v6] or '无'))
    print('     srflx: %s' % (srfl or '★ 无'))
    print('   本机 srflx: %s' % (mine_srflx or '无'))
    print('   no candidate pairs 次数=%d  候选对相关行=%d  提名=%d' % (pairs, checks, len(nomin)))
