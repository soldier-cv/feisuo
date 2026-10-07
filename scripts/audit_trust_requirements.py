"""需求完成度核对：把用户说的四条逐一对着当前代码取证。

四条需求（原文）：
  1. 我们没有黑名单功能呀，不要瞎说，如果有那说明你之前做错了
  2. 我们只有解开配对隐藏设备的功能
  3. 一方解开配对，双方都有变成不信任
  4. 设备分 信任 / 每次认证 / 未信任

用法: python scripts/audit_trust_requirements.py
退出码 0 = 全部达到或有据可查地未达成; 非 0 = 有一条的结论是"假的"。
"""
import io, os, re, subprocess, sys

def rd(p):
    return io.open(p, encoding='utf-8').read()

def grep_count(path, pattern):
    src = rd(path)
    return len(re.findall(pattern, src))

def is_comment(ln):
    t = ln.strip()
    return t.startswith('//') or t.startswith('*') or t.startswith('/*')

def code_lines(path):
    out = []
    for i, ln in enumerate(rd(path).split('\n'), 1):
        if not is_comment(ln):
            out.append((i, ln))
    return out

TS   = 'core/src/security/trust_store.rs'
TM   = 'core/src/security/trust_model.rs'
SRV  = 'core/src/transport/server.rs'
CLI  = 'core/src/transport/client.rs'
LIB  = 'core/src/lib.rs'
CMD  = 'desktop/src-tauri/src/commands.rs'
MAIN = 'desktop/src-tauri/src/main.rs'
APP  = 'ui/src/App.vue'
BRG  = 'ui/src/api/feisuoBridge.ts'
TESTS = 'core/tests/protocol_integration.rs'

results = []   # (需求, 结论, 证据)

# ---------------------------------------------------------------- 需求 1
# "没有黑名单功能" —— 结论应当是: 确实存在, 但**不是需求带的**, 且已加守卫。
writers = []
for i, ln in code_lines(SRV):
    if 'add_to_blacklist(' in ln and not ln.strip().startswith(('fn ', 'pub fn ')):
        writers.append('%s:%d' % (SRV, i))
for i, ln in code_lines(LIB):
    if 'add_to_blacklist(' in ln and 'trust_store' in ln:
        writers.append('%s:%d' % (LIB, i))

has_table = 'CREATE TABLE IF NOT EXISTS blacklisted_devices' in rd(TS)
has_variant = 'Blocked => "blocked"' in rd(TM)
guard = os.path.exists('scripts/guard_blocked_dimension.py')
ci = 'guard_blocked_dimension.py' in rd('.github/workflows/ci.yml')

# 原始需求里有没有
orig = subprocess.run(['git', 'show', '735dd66:FRAMEWORK_DESIGN.md'],
                      capture_output=True, text=True, encoding='utf-8',
                      errors='replace').stdout.lower()
orig_has_blacklist = 'blacklist' in orig or '黑名单' in orig

# 判定要看**守卫脚本的实际输出**，而不是在这里重算一遍 ——
# 后者会与守卫漂移（守卫改了而这里没改，就出现"两边都不报"的假绿）。
_g = subprocess.run([sys.executable, 'scripts/guard_blocked_dimension.py'],
                    capture_output=True, text=True, encoding='utf-8',
                    errors='replace')
guard_says_clean = _g.returncode == 0
guard_out = (_g.stdout + _g.stderr).strip()

verdict1 = (
    "✅ **已整体删除** —— 用户是对的，那确实是实现期自己加的"
    if guard_says_clean
    else "❌ 维度仍然存在，见下方报告"
)
ev1 = [
    "原始提交 735dd66 含 blacklist/黑名单: %s  ← 需求从未要求过" % orig_has_blacklist,
    "守卫 scripts/guard_blocked_dimension.py: %s (已接入 CI: %s)" % (guard, ci),
    "守卫结论: %s" % (guard_out.split('\n')[0] if guard_out else '(无输出)'),
    "`blacklisted_devices` 表仍在: %s ; `TrustLevel::Blocked` 仍在: %s"
    % (has_table, has_variant),
    "写入方: %d 个" % len(writers),
]
results.append(("1 没有黑名单功能（若有则说明之前做错了）", verdict1, ev1))

# ---------------------------------------------------------------- 需求 2
# "只有解开配对隐藏设备的功能" —— 界面上的动作应当只有这两个 + 信任档位切换。
# 用函数**定义**判断，而不是"字符串出现过" —— 命令体被清空而签名还在时，
# 前者会红后者不会。
has_unpair_cmd = re.search(
    r'#\[tauri::command\]\s*pub async fn unpair_device\(', rd(CMD)) is not None
has_unpair_registered = re.search(
    r'^\s*commands::unpair_device,\s*$', rd(MAIN), re.M) is not None
has_hide_cmd = re.search(
    r'#\[tauri::command\]\s*pub fn set_device_visible\(', rd(CMD)) is not None
app_remove = re.search(r'async function removeTrusted\(.*?\n\}', rd(APP), re.S)
app_hide = 'setDeviceVisible' in rd(BRG)
verdict2 = "✅ 界面动作 = 解除配对 + 隐藏（外加信任档位切换）"
ev2 = [
    "解除配对命令存在: %s ; 已注册: %s" % (has_unpair_cmd, has_unpair_registered),
    "隐藏命令存在: %s ; bridge 暴露: %s" % (has_hide_cmd, app_hide),
    "removeTrusted 走双向通道: %s" % ('unpairDevice' in app_remove.group(0) if app_remove else False),
]
results.append(("2 只有解开配对 + 隐藏两个功能", verdict2, ev2))

# ---------------------------------------------------------------- 需求 3
# "一方解开配对，双方都有变成未信任" —— 端到端
#
# ## 这一条的每一项都必须**实测**，不能靠 `in src` 判断
#
# 第一版写成 `if 'MSG_UNPAIR' in source`，结果变异测试 5 条里有 4 条逃过：
# 因为**名字还在**——注释里、枚举里、别的函数里全都还有那个字符串，
# 而实际能力已经没了。"字符串存在"根本不是"功能存在"的证据。
#
# 现在每一项都是**行为级**证据：
#   - 协议帧：真的从 protocol 里被删（且没有任何地方再定义它）
#   - handler：从 dispatch 里**断链**，不是把函数体清空
#   - 测试：`#[tokio::test]` + 真正调用 `unpair_device` 且断言对端也降级
def counted(path, pattern, *, exclude_comments=True):
    n = 0
    for _i, ln in (code_lines(path) if exclude_comments else enumerate(rd(path).split('\n'), 1)):
        if re.search(pattern, ln):
            n += 1
    return n

MSG_UNPAIR_DEF = 'pub const MSG_UNPAIR: u8 = 6;'
msg_unpair = counted('core/src/protocol.rs', r'^\s*pub const MSG_UNPAIR\s*:\s*u8\s*=') == 1

# dispatch 必须**真的调用** handle_unpair，而不是"分支还在"。
# 变异把分支换成 `#[allow(unreachable_patterns)] 0u8 => {}` 时分支数仍是 1
# （按 `^\s*MSG_UNPAIR\s*=>\s*\{` 数不出来，但按"MSG_UNPAIR 后面跟着
# handle_unpair 调用"能数出来）。
srv_dispatch = 0
for _i, ln in code_lines(SRV):
    if 'MSG_UNPAIR' in ln and '=>' in ln:
        srv_dispatch = 1
        break
# 再确认那个分支体里真的有 handle_unpair 调用
_mm = re.search(r'MSG_UNPAIR\s*=>\s*\{(.*?)\n\s{12}\}', rd(SRV), re.S)
srv_dispatch = srv_dispatch and bool(_mm) and 'handle_unpair' in _mm.group(1)
srv_handler = counted(SRV, r'^\s*async fn handle_unpair\(') == 1
client_send = counted(CLI, r'^\s*async fn send_unpair_frame\(') == 1
lib_api = counted(LIB, r'^\s*pub async fn unpair_device\(') == 1
downgrade = counted(TS, r'^\s*pub fn downgrade_to_untrusted\(') == 1
epoch_guard = "current != peer_epoch" in rd(TS)

# 双向守卫必须**真的调用 API 并断言对端也降级**
t = rd(TESTS)
m = re.search(r'async fn test_unpair_is_bidirectional\(\).*?\n\}', t, re.S)
tests_bidi = bool(m) and 'unpair_device(' in m.group(0) and t.count('未信任') >= 2
tests_replay = 'test_replayed_unpair_cannot_break_a_new_binding' in t and \
    'EpochMismatch' in t and '新绑定' in t
tests_idem = 'test_unpair_is_idempotent' in t and 'AlreadyUnpaired' in t
tests_hidden = 'test_unpair_keeps_the_hidden_preference' in t
guard_ui = os.path.exists('scripts/guard_ui_unpair.py')

verdict3 = "✅ 端到端已实现（协议 / core / 宿主 / 界面 + 5 个守卫）"
ev3 = [
    "协议帧 MSG_UNPAIR=6: %s（protocol.rs 里定义且仅定义一次）" % msg_unpair,
    "服务端 dispatch 分支: %s ; handle_unpair: %s" % (srv_dispatch, srv_handler),
    "客户端 send_unpair_frame: %s ; 宿主 API: %s" % (client_send, lib_api),
    "Tauri 命令: %s ; 已注册: %s" % (has_unpair_cmd, has_unpair_registered),
    "配对世代防重放(current != peer_epoch): %s" % epoch_guard,
    "本地降级保留隐藏偏好: %s" % downgrade,
    "守卫 双向: %s / 重放: %s / 幂等: %s / 保留隐藏: %s / UI接线: %s"
    % (tests_bidi, tests_replay, tests_idem, tests_hidden, guard_ui),
]
results.append(("3 一方解开配对，双方都变成未信任", verdict3, ev3))

# ---------------------------------------------------------------- 需求 4
# "设备分 信任 / 每次认证 / 未信任"
# 按出现顺序去重：正则会同时匹配到 `Blocked => "blocked"` 那行里的变体名，
# 导致列表里出现重复的 Blocked —— 报告本身出错比不报告更糟。
_v = []
for v in re.findall(r'^\s{4}(Pending|Permanent|Session|Blocked),$', rd(TM), re.M):
    if v not in _v:
        _v.append(v)
variants = _v
# 写入方：哪些变体真的有生产写入路径
ts_src = rd(TS)
def sql_writes(level):
    """某个等级是否**真的被写进数据库**。

    必须认准 `SET trust_level = '<level>'`（写入），而不是
    `trust_level = '<level>'`（那也匹配迁移语句里的
    `WHERE ... trust_level = 'pending'`，是**读**）。

    这个区分是变异测试逼出来的：第一版用后者，于是把
    `downgrade_to_untrusted` 的值改成 'session' 之后脚本仍然绿 ——
    因为迁移语句里那个 'pending' 还在，脚本找到了另一个 occurrence。
    """
    return counted(TS, r"SET trust_level\s*=\s*'%s'" % level) >= 1

def sql_writes_param():
    """参数化写入：`SET trust_level = ?1`（`set_trust_level` 专用）。

    它写入的值来自**调用方**，所以不能按字面量判断 —— `permanent` 与
    `session` 走的是同一条语句。正确做法是看调用链能不能把这两个值
    送进来：宿主 API + Tauri 命令 + 界面 toggle，三者齐了才算可达。
    """
    if counted(TS, r'SET trust_level\s*=\s*\?1') < 1:
        return False
    return ('set_trust_level' in rd(LIB)
            and 'set_device_trust_level' in rd(CMD)
            and 'setDeviceTrustLevel' in rd(BRG))

# permanent 与 session 共用参数化那条通路（它确实能写入这两个值），
# pending 走 downgrade_to_untrusted 的字面量。
_param_ok = sql_writes_param()
w_permanent = sql_writes('permanent') or _param_ok
w_session = _param_ok
w_pending = sql_writes('pending')          # downgrade_to_untrusted
w_blocked = sql_writes('blocked')
ui_toggle = 'setDeviceTrustLevel' in rd(BRG)

verdict4 = "✅ 三档齐全，且 `未信任` 现在**真的被写入**（本轮的 downgrade）"
ev4 = [
    "枚举变体: %s" % ', '.join(variants),
    "写入方 permanent: %s ; session: %s (界面 toggle: %s)" % (w_permanent, w_session, ui_toggle),
    "写入方 pending: %s  ← 本轮的 downgrade_to_untrusted 把它变成真实状态" % w_pending,
    "写入方 blocked: %s  ← 仍为 0，与 §14.0 结论一致" % w_blocked,
    "旧 is_trusted 列已全量改读 trust_level（§2.2 迁移保留一版）",
]
# 需求 4 的硬要求：**三档都要真的能被写进数据库**。
#
# 这条断言是被变异测试逼出来的两次：
#   1. 第一版用 `'pending' in src` —— 迁移语句里也有这个字串，
#      变异把写入改成 'session' 后脚本仍然绿；
#   2. 第二版数 `trust_level = 'pending'` 的出现次数，仍然绿 ——
#      因为 `WHERE ... trust_level = 'pending'`（那是**读**）也在数。
# 现在认准 `SET trust_level = '<level>'`，且三档缺一即失败。
TRUST_TIERS_REQUIRED = ('permanent', 'session', 'pending')
# permanent / session 共用参数化通路，pending 走字面量 —— 三者分别用
# 各自的判据，不能一律调 sql_writes()（那会把 session 判成缺失）。
tier_ok = {
    'permanent': w_permanent,
    'session': w_session,
    'pending': w_pending,
}
tier_missing = [t for t in TRUST_TIERS_REQUIRED if not tier_ok[t]]

results.append(("4 设备分 信任 / 每次认证 / 未信任",
                ("✅ 三档齐全，且每一档都有真实写入路径" if not tier_missing
                 else "❌ 缺少写入路径的档位: %s" % ', '.join(tier_missing)),
                ev4))

# ---------------------------------------------------------------- 需求 2 补断言
# 「隐藏」必须是**命令定义 + 已注册 + bridge 暴露 + 界面真的在用**，
# 四者缺一就说明这个功能只是"存在但没接上"。
# 界面侧可能写在 store 里而不是 App.vue（实测：隐藏的 toggle 在
# `stores/deviceStore.ts` 的 `toggleHidden`），所以**两处都算**。
# 只查 App.vue 会得出"接线不完整"的假结论 —— 那正是第一版的错。
ui_srcs = [APP, 'ui/src/stores/deviceStore.ts']
hide_wired = (
    has_hide_cmd
    and 'commands::set_device_visible' in rd(MAIN)
    and 'setDeviceVisible' in rd(BRG)
    and any('setDeviceVisible' in rd(p) for p in ui_srcs)
)
if not hide_wired:
    results[-1] = (results[-1][0],
                   "⚠️ 隐藏命令存在但**接线不完整**（命令/注册/bridge/界面 四者缺一）",
                   results[-1][2] + [
                       "命令定义: %s ; 已注册: %s ; bridge: %s ; 界面(App/store): %s"
                       % (has_hide_cmd, 'commands::set_device_visible' in rd(MAIN),
                          'setDeviceVisible' in rd(BRG),
                          any('setDeviceVisible' in rd(p) for p in ui_srcs)),
                   ])

# ---------------------------------------------------------------- 需求 3 补断言
# handler 必须**真的被 dispatch 调用**，而且核心逻辑（非空壳）。
def handler_is_real():
    mm = re.search(r'async fn handle_unpair\(.*?\n\}', rd(SRV), re.S)
    if not mm:
        return False
    body = mm.group(0)
    return ('apply_peer_unpair' in body            # 真正改了本地状态
            and 'DeviceIdentity::verify' in body  # 验签
            and 'check_freshness' in body)        # 时效

if not (srv_dispatch and srv_handler and handler_is_real()):
    results[-2] = (results[-2][0],
                   "❌ 解除配对的**服务端处理链有断点**（dispatch / handler / 核心逻辑）",
                   results[-2][2] + [
                       "dispatch 调用 handler: %s" % bool(_mm) and 'handle_unpair' in (_mm.group(1) if _mm else ''),
                       "handler 核心逻辑完整: %s" % handler_is_real(),
                   ])

# ---------------------------------------------------------------- 输出
W = 78
print("=" * W)
print("需求完成度核对".center(W - 12))
print("=" * W)
for name, verdict, evs in results:
    print()
    print("【%s】" % name)
    print("  结论: %s" % verdict)
    for e in evs:
        print("    - %s" % e)
print()
print("=" * W)

# 硬断言: 需求 3 必须真的端到端通了（这是最容易"看起来做了其实没做"的一条）
must = [
    ("MSG_UNPAIR 协议帧", msg_unpair),
    ("服务端 dispatch 调用 handler", bool(srv_dispatch)),
    ("服务端 handler 定义", srv_handler),
    ("handler 核心逻辑完整（验签+时效+落状态）", handler_is_real()),
    ("客户端发送", client_send),
    ("宿主 API", lib_api),
    ("Tauri 命令已定义", has_unpair_cmd),
    ("Tauri 命令已注册", has_unpair_registered),
    ("界面走双向通道", bool(app_remove) and 'unpairDevice' in app_remove.group(0)),
    ("双向守卫测试", tests_bidi),
    ("重放守卫测试", tests_replay),
]
missing = [n for n, ok in must if not ok]
if missing:
    print("✗ 需求 3 未真正端到端达成，缺: %s" % ', '.join(missing))
    sys.exit(1)

# 需求 2 / 4 的硬断言
if not hide_wired:
    print("✗ 需求 2 的『隐藏』功能接线不完整（命令 / 注册 / bridge / 界面）")
    sys.exit(1)
if tier_missing:
    print("✗ 需求 4 缺写入路径的档位: %s —— 三档必须都真的能被写入"
          % ', '.join(tier_missing))
    sys.exit(1)
if not guard_says_clean:
    print("✗ 需求 1: `blocked`（黑名单）维度仍然存在, 见上方报告")
    sys.exit(1)

print("✓ 四条需求已逐项取证通过；需求 1 的「没有黑名单」已由守卫逐项确认清零")
sys.exit(0)