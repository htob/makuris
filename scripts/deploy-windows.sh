#!/usr/bin/env bash
# 把 Windows 被控端部署到台式机。
#
# 动机：手工部署踩过两个坑，都不该再踩第二遍——
#  1. 单独拷 exe 会静默死亡。mingw 交叉编译的产物动态依赖 libstdc++-6.dll
#     （它自己又依赖 libwinpthread-1.dll），Windows 加载阶段找不到就直接终止
#     进程，没有输出、没有退出码，看起来像「程序跑不起来」。本脚本自动解析
#     exe 的依赖并从 mingw 目录把需要的 DLL 一并打包。
#  2. 经 SSH 直接启动的进程会随会话关闭被回收，而且落在 session 0 —— GDI 采集
#     在 session 0 只能拿到全黑。所以必须由计划任务（InteractiveToken）托管。
#
# 用法：
#   scripts/deploy-windows.sh                # 编译 + 部署 + 重启 + 自检
#   scripts/deploy-windows.sh --no-build     # 跳过编译，只重新部署
#
# 环境变量（都有默认值）：
#   MAKURIS_HOST_SSH   SSH 目标，默认 user@192.168.1.223
#   MAKURIS_WIN_USER   远端 Windows 用户名，默认 user
#   MAKURIS_WIN_HOME   远端用户目录，默认 C:/Users/<MAKURIS_WIN_USER>
#   MAKURIS_WIN_DIR    远端部署目录，默认 <WIN_HOME>/makuris/target/release
#   MAKURIS_TASK       计划任务名，默认 MakurisHost
#
# 本机真实值放 scripts/.deploy.env（已被 .gitignore 排除，不进版本库）；
# 脚本启动时自动加载它，仓库里只保留通用占位值，开源后不泄露个人信息。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# 加载本地私密配置（不存在则跳过，脚本按内置占位默认值运行）
if [[ -f "$REPO_ROOT/scripts/.deploy.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$REPO_ROOT/scripts/.deploy.env"
  set +a
fi

# 远端部署目录必须与计划任务执行的 run.bat 里的路径一致。
#
# run.bat 里写死的是 <WIN_HOME>\makuris\target\release\makuris-host.exe，
# 所以这里也必须传到这里。第一版默认传到 .../makuris/dist，上传顺利完成、
# 自检也都通过，但进程跑的其实还是旧文件——「部署成功」是假的。
# 自检里会打印进程的实际 Path，就是为了让这种错位能被看出来。
WIN_USER="${MAKURIS_WIN_USER:-user}"
WIN_HOME="${MAKURIS_WIN_HOME:-C:/Users/$WIN_USER}"
SSH_TARGET="${MAKURIS_HOST_SSH:-user@192.168.1.223}"
REMOTE_DIR="${MAKURIS_WIN_DIR:-$WIN_HOME/makuris/target/release}"
TASK_NAME="${MAKURIS_TASK:-MakurisHost}"
TARGET="x86_64-pc-windows-gnu"
BIN_NAME="makuris-host"
STAGE="$REPO_ROOT/dist/windows"

DO_BUILD=1
[[ "${1:-}" == "--no-build" ]] && DO_BUILD=0

say() { printf '\n\033[1m=== %s ===\033[0m\n' "$1"; }

# ---------- 1. 环境自检 ----------
say "检查交叉编译环境"
MINGW_PREFIX="$(brew --prefix mingw-w64 2>/dev/null || true)"
if [[ -z "$MINGW_PREFIX" ]]; then
  echo "缺少 mingw-w64。安装：brew install mingw-w64" >&2
  exit 1
fi
# brew 的 mingw 组织方式（踩过一次坑，写清楚）：
#   <prefix>/toolchain-x86_64/bin/                      编译器 + 部分运行时 DLL
#   <prefix>/toolchain-x86_64/x86_64-w64-mingw32/lib/   目标三元组专属库与 DLL
# 最初写成 toolchain-x86_64/x86_64-w64-mingw32/bin/gcc，多套了一层目录，
# 结果是「找不到编译器」直接退出——所以下面把两个根分开命名，别再用一个变量套。
MINGW_ROOT="$MINGW_PREFIX/toolchain-x86_64"
MINGW_TARGET_DIR="$MINGW_ROOT/x86_64-w64-mingw32"
GCC="$MINGW_ROOT/bin/x86_64-w64-mingw32-gcc"
OBJDUMP="$MINGW_ROOT/bin/x86_64-w64-mingw32-objdump"
[[ -x "$GCC" ]] || { echo "找不到 $GCC" >&2; exit 1; }

if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "缺少 target ${TARGET}，正在安装…"
  rustup target add "$TARGET"
fi
echo "mingw: $($GCC --version | head -1)"
echo "target: $TARGET"

# ---------- 2. 编译 ----------
if [[ $DO_BUILD -eq 1 ]]; then
  say "交叉编译 $BIN_NAME"
  cargo build --release --target "$TARGET" -p makuris-host --bin "$BIN_NAME"
fi

EXE="$REPO_ROOT/target/$TARGET/release/$BIN_NAME.exe"
[[ -f "$EXE" ]] || { echo "编译产物不存在：$EXE" >&2; exit 1; }
file "$EXE" | grep -q "PE32+" || { echo "产物不是 Windows 可执行文件" >&2; exit 1; }

# ---------- 3. 收集依赖 DLL ----------
# 从 exe 的导入表解析依赖，剔除 Windows 自带系统库，剩下的（mingw 运行时）
# 必须与 exe 同目录分发，否则加载阶段静默终止。
say "解析依赖 DLL"
SYSTEM_DLL_RE='^(advapi32|gdi32|kernel32|user32|userenv|ws2_32|ntdll|bcrypt|bcryptprimitives|iphlpapi|crypt32|ole32|oleaut32|shell32|shlwapi|setupapi|version|winmm|dwmapi|d3d11|dxgi|powrprof|api-ms-.*|kernelbase|secur32|winspool|comdlg32|comctl32|msvcrt|rpcrt4|cfgmgr32|dnsapi|netapi32|wtsapi32)\.dll$'

# 收集依赖闭包，而不是只看 exe 的直接导入表。
#
# 为什么必须递归：exe 直接只依赖 libstdc++-6.dll，但那个 DLL 自己又依赖
# libgcc_s_seh-1.dll 与 libwinpthread-1.dll。只看第一层就会漏掉后两个——
# Windows 在加载 libstdc++ 时找不到它们，整个进程被静默终止（无输出、无退出码），
# 第一版脚本正是这么漏的，还得靠手工补传两次才跑起来。
deps_of() {
  "$OBJDUMP" -p "$1" 2>/dev/null \
    | awk -F'DLL Name: ' '/DLL Name:/{print tolower($2)}' \
    | sort -u \
    | grep -Ev "$SYSTEM_DLL_RE" || true
}

# mingw 运行时 DLL 散在三处，缺一处就会漏文件（三个都实测过）：
#   <root>/bin/                  编译器自带的那批
#   <target>/lib/                libstdc++-6.dll、libgcc_s_seh-1.dll
#   <target>/bin/                libwinpthread-1.dll（只查前两处会漏掉它）
find_dll() {
  local dep="$1" cand
  for cand in "$MINGW_ROOT/bin/$dep" \
              "$MINGW_TARGET_DIR/lib/$dep" \
              "$MINGW_TARGET_DIR/bin/$dep"; do
    if [[ -f "$cand" ]]; then
      printf '%s\n' "$cand"
      return 0
    fi
  done
  return 1
}

rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "$EXE" "$STAGE/"
# 托盘助手脚本与 exe 同目录分发：host 启动时按 current_exe 同级目录找
# makuris-tray.ps1 并拉起（见 host/src/menubar.rs Windows 分支）。
# 必须保持 UTF-8 带 BOM——Windows PowerShell 5.1 无 BOM 会按 GBK 解码中文。
cp "$REPO_ROOT/scripts/windows/makuris-tray.ps1" "$STAGE/"

# 广度优先展开依赖闭包。注意两点：
#  1. 不用 mapfile —— macOS 自带的 bash 3.2 没有它（实测报 command not found）。
#  2. 判断分支一律写成 if，不用 `cond && action`：后者在条件不成立时整体返回 1，
#     配合 `set -e` 会让脚本无声退出。
copied=()
missing=()
queue=()
while IFS= read -r d; do
  if [[ -n "$d" ]]; then queue+=("$d"); fi
done < <(deps_of "$EXE")

while [[ ${#queue[@]} -gt 0 ]]; do
  dep="${queue[0]}"
  queue=("${queue[@]:1}")

  # 闭包里可能有环（libstdc++ 与 libgcc 互相引用），已收过就跳过
  already=0
  for c in "${copied[@]:-}"; do
    if [[ "$c" == "$dep" ]]; then already=1; break; fi
  done
  if [[ $already -eq 1 ]]; then continue; fi

  if src="$(find_dll "$dep")"; then
    cp "$src" "$STAGE/"
    copied+=("$dep")
    while IFS= read -r d; do
      if [[ -n "$d" ]]; then queue+=("$d"); fi
    done < <(deps_of "$src")
  else
    missing+=("$dep")
  fi
done

if [[ ${#missing[@]} -gt 0 ]]; then
  echo "⚠ 这些依赖在 mingw 目录中找不到，请手工核对：${missing[*]}" >&2
fi

if [[ ${#copied[@]} -eq 0 ]]; then
  echo "无 mingw 运行时依赖（已静态链接），单文件即可部署"
else
  echo "需随 exe 分发：${copied[*]}"
fi
ls -lh "$STAGE" | tail -n +2 | awk '{print "  ", $9, $5}'

# ---------- 4. 停进程 + 上传 ----------
# 顺序反了就会失败：正在运行的 exe 锁住自己，scp 覆盖时直接报
# `dest open "...makuris-host.exe": Failure` 并中途退出。
# 第一版把上传放在停止之前，实测就走不过去——所以先停，再传。
say "停止旧进程（否则 exe 被占用，覆盖上传会失败）"
ssh -o BatchMode=yes -o ConnectTimeout=15 "$SSH_TARGET" \
  'powershell -NoProfile -Command "Get-Process makuris-host -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep -Seconds 2; Write-Output stopped"'

say "上传到 $SSH_TARGET:$REMOTE_DIR"
ssh -o BatchMode=yes -o ConnectTimeout=10 "$SSH_TARGET" \
  "if not exist \"${REMOTE_DIR//\//\\}\" mkdir \"${REMOTE_DIR//\//\\}\"" >/dev/null
scp -o BatchMode=yes -o ConnectTimeout=10 "$STAGE"/* "$SSH_TARGET:$REMOTE_DIR/" >/dev/null
ssh -o BatchMode=yes -o ConnectTimeout=10 "$SSH_TARGET" "dir /b \"${REMOTE_DIR//\//\\}\""

# ---------- 4.5 确保信令地址已配置 ----------
# 不设的话被控端会退回内置默认值 ws://127.0.0.1:8443/ws —— 那是「信令服务器
# 就在本机」的假设，在只有一台电脑做被控端的开发机上碰巧成立，一到真机就必然
# 失效：日志里全是连不上，而界面上只表现为「外面的网络也能连」那一节永远空着。
#
# 为什么用 setx（写注册表）而不是往 run.bat 里塞 set：run.bat 是 LF 行尾且含中文，
# cmd 按 GBK 逐行解析，实测把 set 行插进去后 exe 读到的仍是默认值。setx 写的是
# 用户级持久环境变量，由计划任务启动的进程自动继承，不依赖批处理的解析行为。
#
# 只在缺失时设置，不覆盖用户已有的值——用户可能故意指向公网隧道地址。
# 服务器已 TLS-only（2026-10-12 起明文 ws 直接被掐断，客户端表现为 -1005
# 反复重试），默认值必须是 wss。
SIGNALING_URL="${MAKURIS_SIGNALING:-wss://signal.example.com:8443/ws}"
DEVICE_ID="${MAKURIS_DEVICE_ID:-host-win-01}"

# 接入密钥：信令服务器开启密钥校验后，注册报文必须附 key_proof
# （HMAC-SHA256(密钥, "register|" + device_id)），没配密钥的被控端会被服务器
# 直接拒绝。这个失败极其安静——被控端日志只有一行
#   signal: Error { message: "接入密钥不正确…" }
# 进程照常在跑、8444 照常监听、局域网 Bonjour 直连照常能用，唯独「外面的网络
# 也能连」那一栏永远看不到这台电脑。第一版脚本只配了信令地址和设备名，漏了
# 密钥，Windows 端就这么在公网列表里消失了整整一天。
#
# 不硬编码密钥：优先用调用者传入的环境变量，其次从本机正在跑的信令服务器进程
# 环境里读（服务器首次启动会生成并持久化，读它拿到的就是当前生效值）。
ACCESS_KEY="${MAKURIS_ACCESS_KEY:-}"
if [[ -z "$ACCESS_KEY" ]]; then
  SIG_PID="$(pgrep -f makuris-signaling 2>/dev/null | head -1 || true)"
  if [[ -n "$SIG_PID" ]]; then
    ACCESS_KEY="$(ps eww -p "$SIG_PID" 2>/dev/null | tr ' ' '\n' \
      | sed -n 's/^MAKURIS_ACCESS_KEY=//p' | head -1)"
  fi
fi
# 信令迁云后（2026-10），本机不再有 makuris-signaling 进程，上面的 pgrep
# 回落恒为空。当前生效密钥持久化在本机 host 状态目录（host 首启自动生成、
# 与云端一致，14:55 已实测比对本机与云端 journalctl 的密钥值相同）。
if [[ -z "$ACCESS_KEY" ]]; then
  KEY_FILE="$HOME/Library/Application Support/Makuris/access-key.txt"
  if [[ -f "$KEY_FILE" ]]; then
    ACCESS_KEY="$(tr -d '[:space:]' < "$KEY_FILE")"
  fi
fi

say "确认信令地址、设备名与接入密钥（缺失则写入用户级环境变量）"
# 两个变量各走一条独立命令。第一版把两个变量塞进一个 PowerShell 哈希表
# 循环里，单跑没问题、在脚本里却让整段以非零码退出——配合 set -e 使脚本
# 停在「已停进程、尚未启动」之间，被控端就这么留在停机状态。排查成本远高于
# 省下的这一行，所以改成最直白的写法：能一眼看出每条命令要做什么。
for kv in "MAKURIS_SIGNALING=$SIGNALING_URL" "MAKURIS_DEVICE_ID=$DEVICE_ID" "MAKURIS_ACCESS_KEY=$ACCESS_KEY"; do
  key="${kv%%=*}"
  val="${kv#*=}"
  # 空值一律跳过。密钥读不到时（本机没跑信令服务器、也没传 MAKURIS_ACCESS_KEY）
  # 宁可什么都不写——被控端会在日志里明确报「接入密钥不正确」，比 setx 一个空串
  # 把用户已有的正确配置抹掉要好查得多。
  if [[ -z "$val" ]]; then
    echo "  ⚠ $key 没有可用值，跳过（被控端将连不上开了密钥校验的信令服务器）"
    continue
  fi
  cur="$(ssh -o BatchMode=yes -o ConnectTimeout=15 "$SSH_TARGET" \
    "powershell -NoProfile -Command \"[Environment]::GetEnvironmentVariable('$key','User')\"" 2>/dev/null | tr -d '\r')"
  if [[ "$cur" == "$val" ]]; then
    echo "  $key 已是 $val"
  else
    ssh -o BatchMode=yes -o ConnectTimeout=15 "$SSH_TARGET" \
      "setx $key \"$val\"" >/dev/null
    echo "  $key 已设为 ${val}（原值：${cur:-未设置}）"
  fi
done

# 同步 makuris-run.bat 里的 set 行。这一步不能省：计划任务的入口是 bat，
# cmd 的 `set` 会覆盖上面 setx 刚写入的用户级环境变量（子进程继承的是父 cmd
# 的活动变量，而不是注册表级变量）。2026-10-12 部署实测踩过：setx 自检全绿、
# 进程却仍连 bat 里写死的旧局域网地址，云端永远等不到注册。
# sync-runbat.ps1 幂等：已一致输出 ALREADY_OK，缺失则插入 set 行。
RUN_BAT="${MAKURIS_RUN_BAT:-$WIN_HOME/makuris-run.bat}"
say "同步 run.bat 的信令地址（防 cmd set 覆盖 setx，见脚本注释）"
scp -q -o BatchMode=yes -o ConnectTimeout=10 "$REPO_ROOT/scripts/windows/sync-runbat.ps1" \
  "$SSH_TARGET:$WIN_HOME/sync-runbat.ps1" >/dev/null
ssh -o BatchMode=yes -o ConnectTimeout=20 "$SSH_TARGET" \
  "powershell -NoProfile -ExecutionPolicy Bypass -File $WIN_HOME/sync-runbat.ps1 -Url \"$SIGNALING_URL\" -BatPath \"$RUN_BAT\"" \
  | tr -d '\r'

# ---------- 4.6 确保防火墙放行局域网入站 ----------
# 这一步是必须的，不是可选优化。踩过一次的现场：公网 TURN 中继一切正常、
# iPhone 能出画，唯独局域网直连必然超时，本机 8444 明明在 LISTENING。
#
# 根因（2026-10-12 用 netsh wfp show filters 导出内核过滤器表定案）：
#   host 首次监听时，防火墙弹窗若被点了「取消」，系统会为该 exe 静默写入
#   两条永久 Block 规则（名字形如 "TCP Query User{...}...\makuris-host.exe"，
#   Profile=Public）。而 Windows 防火墙按 WFP 权重裁决，Block(w=10) 永远压过
#   Allow(w=9)——所以只加 Allow 规则是无效的，必须先把 Block 删掉。
#   另外若本机 PrivateProfile 注册表键缺失，网络虽显示 Private 也仍按 Public
#   档「默认阻止入站」执行，于是除 22 外所有入站（含 ICMP）都不通。
#
# ensure-firewall.ps1 幂等完成三件事：删掉绑定本 exe 的入站 Block、清理历史
# 重复的 Allow 变体、确保恰好留下 TCP 8444 + UDP(ICE 媒体) 两条 Profile=Any
# Allow，并回读核验 block_left=0。只针对 makuris-host.exe，不动整机防火墙开关。
#
# 远端 exe 路径与 REMOTE_DIR 一致（正斜杠形式即可，PowerShell 两种都认）。
say "确保防火墙放行局域网入站（删 Block + 建 Allow，幂等）"
scp -q -o BatchMode=yes -o ConnectTimeout=10 "$REPO_ROOT/scripts/windows/ensure-firewall.ps1" \
  "$SSH_TARGET:$WIN_HOME/ensure-firewall.ps1" >/dev/null
# 用 -File 加位置参数（-Command 拼接在 cmd 引号下易被截断，实测踩过）。
# 必须把远端 exe 的确切路径作为第一个参数传入：Allow 规则按程序路径限定，
# REMOTE_DIR 是本脚本唯一的事实来源。靠 ps1 里的默认值兜底的话，一旦改了
# 部署目录，规则就会指向不存在的旧路径 —— 局域网照样连不上且毫无报错。
# 该脚本以非零码表示「仍有 Block 残留」，这里不让它中断整个部署，
# 只把结果显式打出来——被控端在公网仍可用，局域网不可用是可接受的降级。
if ssh -o BatchMode=yes -o ConnectTimeout=25 "$SSH_TARGET" \
    "powershell -NoProfile -ExecutionPolicy Bypass -File $WIN_HOME/ensure-firewall.ps1 \"$REMOTE_DIR/$BIN_NAME.exe\"" \
    | tr -d '\r'; then
  echo "  ✅ 防火墙已放行（局域网直连可用）"
else
  echo "  ⚠ 防火墙规则未完全就绪：局域网直连可能仍超时（公网中继不受影响）。"
  echo "    手工排查：Get-NetFirewallRule -DisplayName '*makuris*' 应无 Block 项。"
fi

# ---------- 5. 启动计划任务 ----------
say "启动计划任务 $TASK_NAME"
ssh -o BatchMode=yes -o ConnectTimeout=15 "$SSH_TARGET" \
  "schtasks /run /tn $TASK_NAME"
sleep 8

# ---------- 6. 自检 ----------
# 打印进程实际加载的 Path 并与期望路径对比。第一版脚本把文件传到 dist，
# 而 run.bat 执行的是 target\release：上传成功、进程存活、端口在听，各项全绿，
# 但跑的根本不是刚部署的文件——只看「活着吗」发现不了这种假成功。
say "自检"
EXPECTED_WIN="${REMOTE_DIR//\//\\}\\makuris-host.exe"
ssh -o BatchMode=yes -o ConnectTimeout=15 "$SSH_TARGET" \
  "powershell -NoProfile -Command \"\$p=Get-Process makuris-host -ErrorAction SilentlyContinue; if(\$p){Write-Output ('process pid=' + \$p.Id + ' session=' + \$p.SessionId); Write-Output ('path=' + \$p.Path); if(\$p.Path -eq '$EXPECTED_WIN'){Write-Output 'PATH_OK'}else{Write-Output ('PATH_MISMATCH expected=$EXPECTED_WIN')}}else{Write-Output 'process NOT_RUNNING'}; netstat -an | Select-String ':8444' | Select-Object -First 1; \$t=Get-ScheduledTask -TaskName $TASK_NAME -ErrorAction SilentlyContinue; if(\$t){Write-Output ('task state=' + \$t.State + ' triggers=' + \$t.Triggers.Count)}\""

echo
echo "部署完成。iPhone 上打开 App 应能看到这台电脑；若要点进去才有画面，"
echo "说明 Bonjour 未广播（用 dns-sd -L <实例名> _makuris._tcp local. 复查 TXT 里的 ip/port）。"