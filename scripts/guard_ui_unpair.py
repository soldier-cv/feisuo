"""守卫：界面上的「解除配对」必须走**双向**通道，而不是旧的纯本地 DELETE。

动机：`removeTrustedDevice` 仍然存在且仍然被 bridge 导出，很容易有人
（或者以后的我）顺手用它。而它与新语义的差别不是实现细节 ——
它是"只改本地一半"，正是本轮要消灭的那个缺陷。
"""
import io, sys, re
# ---- stdout 加固（见 scripts/_ci_utf8.py）----
#
# 本脚本输出是中英混排 + `✓` / `✗` 这类符号。而 Python 在 Windows 上默认用
# **控制台代码页**编码 stdout：中文 Windows 是 cp936，英文 Windows / GitHub
# runner 是 cp1252 —— 两者都编不出 `✓`，`print` 直接抛 UnicodeEncodeError。
#
# ⚠️ 这个坑在本仓库真实发生过，而且方向很反直觉：需要 `✓` 的恰好是**成功**
# 那条路径（"全部通过"的收尾行）。于是"守卫判定通过"在 GBK 控制台上表现为
# **退出码 1** —— 看起来像发现了问题，实际是守卫自己崩了。假红和假绿一样
# 危险：前者让人去改没坏的代码，后者让真问题溜过去。
#
# 必须在**任何** print 之前执行，所以位置固定在 import 之后。
# （`check-ci-encoding.py` 会逐脚本检查这一点，注释掉的调用不算。）
import os as _os
import sys as _sys

_sys.path.insert(0, _os.path.dirname(_os.path.abspath(__file__)))
from _ci_utf8 import force_utf8_stdout  # noqa: E402

force_utf8_stdout()

UI = 'ui/src/App.vue'
BRIDGE = 'ui/src/api/feisuoBridge.ts'
MAIN = 'desktop/src-tauri/src/main.rs'
COMMANDS = 'desktop/src-tauri/src/commands.rs'

def rd(p):
    return io.open(p, encoding='utf-8').read()

fail = []

# ---- 1. App.vue 的 removeTrusted 必须调用 unpairDevice ----
app = rd(UI)
m = re.search(r'async function removeTrusted\(.*?\n\}', app, re.S)
if not m:
    fail.append('App.vue 里找不到 removeTrusted 函数')
else:
    body = m.group(0)
    if 'unpairDevice' not in body:
        fail.append('removeTrusted 没有调用 unpairDevice（仍在走旧的纯本地 DELETE）')
    if 'removeTrustedDevice' in body:
        fail.append('removeTrusted 仍调用 removeTrustedDevice —— 那只改本地一半')
    if 'reason_code' not in body:
        fail.append('removeTrusted 没按 reason_code 分支 —— 界面会只能靠文案猜')

# ---- 2. 必须按 reason_code 分支的四种情形 ----
if m:
    body = m.group(0)
    for code in ['applied', 'epoch_mismatch', 'no_shared_epoch']:
        if code not in body:
            fail.append('removeTrusted 缺少 reason_code == %s 的分支' % code)

# ---- 3. bridge 必须导出 unpairDevice ----
br = rd(BRIDGE)
if 'static async unpairDevice' not in br:
    fail.append('bridge 未导出 unpairDevice')
if 'unpair_device' not in br:
    fail.append('bridge 未调用 unpair_device 命令')

# ---- 4. 命令必须在 generate_handler! 里注册 ----
mn = rd(MAIN)
if 'commands::unpair_device' not in mn:
    fail.append('unpair_device 没有在 generate_handler! 里注册 —— 调用会静默失败')

# ---- 5. 命令实现必须解析端点，而不是让前端传 ip ----
cm = rd(COMMANDS)
if 'pub async fn unpair_device' not in cm:
    fail.append('commands.rs 缺少 unpair_device 实现')
else:
    # 必须**在 unpair_device 的函数体里**用 preferred_endpoint 解析端点，
    # 而不是只在文件别处出现一次就算过。
    mm = re.search(r'pub async fn unpair_device\(.*?\n\}', cm, re.S)
    if not mm:
        fail.append('无法定位 unpair_device 的函数体')
    elif 'preferred_endpoint' not in mm.group(0):
        fail.append('unpair_device 的函数体里没有用 preferred_endpoint 解析端点')

# ---- 6. 不得用 includes() 去猜 reason（那是被拆掉的反模式）----
def is_comment(ln):
    t = ln.strip()
    return t.startswith('//') or t.startswith('*') or t.startswith('/*')

# 注释里**刻意**引用了反例（`includes()`），那是文档不是代码
for path, src in [('App.vue', app), ('feisuoBridge.ts', br)]:
    for ln in src.split('\n'):
        if is_comment(ln):
            continue
        if 'user_message' in ln and ('includes(' in ln or 'indexOf(' in ln):
            fail.append('%s 用文案匹配来判断 reason_code: %s' % (path, ln.strip()))

print('检查项: 6 类')
if fail:
    print('\n'.join('  ✗ ' + x for x in fail))
    sys.exit(1)
print('  ✓ 全部通过: 界面走双向解除、按 reason_code 分支、命令已注册')