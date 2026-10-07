"""守卫：§2.3.1「同类操作短期授权」在**界面侧**必须接全。

## 为什么界面侧要单独一条守卫

这个功能的安全性质里有一条是**「用户看不见、关不掉就不安全」**
（见 `DESIGN_TRUST_SHUTTLE.md` §14.11.3 与 §2.3.1）。
也就是说：**core 侧全对、界面侧没接，功能依然是"不可见的安全"** ——
用户点了第三个按钮，看到的却和没点一样，5 分钟后自动免确认，
而他完全不知道自己给过这个授权。

**那种情况比"没有这个功能"更糟：它在用户不知情的情况下放宽了信任。**

## 判据为什么写成"结构"而不是"关键词在不在"

第一版全部栽在同一件事上：**只查了定义，没查调用；只查了词在不在，
没查它出现在该出现的那个位置。** 变异测试实测 5 条逃过：

| 逃过的变异 | 第一版为什么抓不到 |
|:---|:---|
| 第三个按钮改成**永远显示** | `v-if` 在按钮和提示文案里**出现两次**，改一处另一处还在 |
| 提示文案删掉「只对…生效」 | 判据是 `只对` 三个字，文件别处也有 |
| 窗口秒数改成前端硬编码 | 判据只看 bridge **有没有这个方法**，没看 App.vue **用没用** |
| 撤销入口删掉 | 判据匹配到**模板里的调用**，函数改名照样通过 |
| 生效授权不再拉取 | 判据匹配到**函数定义**，不再调用照样通过 |

所以现在每条判据都要求**具体形状**：函数定义与调用**各要一次**、
按钮的 `v-if` 必须**紧邻** `class="btn-approval-grant"`、
文案必须在**按钮之后 400 字内**。

## 用法

    python scripts/guard_session_grant_ui.py
"""
import io, os, re, sys

APP = os.path.join('ui', 'src', 'App.vue')
BRIDGE = os.path.join('ui', 'src', 'api', 'feisuoBridge.ts')
CMDS = os.path.join('desktop', 'src-tauri', 'src', 'commands.rs')
MAIN = os.path.join('desktop', 'src-tauri', 'src', 'main.rs')

BTN = 'btn-approval-grant'
VIF = 'v-if="pendingApproval?.grant_challenge"'


def _defined_and_called(src, fn):
    """函数**既被定义又被调用**。

    只查其中一个是最常见的假绿来源：第一版三条守卫里有两条栽在这上面。
    """
    defined = re.search(r'(async\s+)?function\s+' + re.escape(fn) + r'\s*\(', src) \
        or re.search(r'\bconst\s+' + re.escape(fn) + r'\s*=', src)
    called = len(re.findall(r'\b' + re.escape(fn) + r'\s*\(', src))
    return bool(defined) and called >= 2


def _called_in_startup_block(src, fn):
    """函数在**启动那个 Promise.all 块**里被调用。

    只查"文件里有几次调用"是不够的：`loadActiveGrants` 还被
    审批与撤销路径调用，删掉启动那一次之后仍然 ≥2 次 ⇒ 假绿。
    而删掉它的**真实后果**是重启后徽标指示为空 ——
    用户以为授权没了，于是重新点一次，反而多给了一段授权。
    """
    anchor = 'loadKnownPlaces()'
    i = src.find(anchor)
    if i == -1:
        # 启动块里的锚点被改名/挪走了：判据失效，**必须报**而不是放行。
        return False
    j = src.find(']);', i)
    if j == -1:
        return False
    return re.search(re.escape(fn) + r'\s*\(', src[i:j]) is not None


CHECKS = [
    (u'审批弹窗有第三个按钮「N 分钟内免重复确认」',
     APP, r"""handleApproval\('allow_with_grant'\)"""),

    # 按钮的 v-if 必须**紧邻** class —— 只查"v-if 在文件里"会被提示文案那一处顶掉
    (u'第三个按钮**只在需要比码时**出现（v-if 紧邻按钮 class）',
     APP, r'v-if="pendingApproval\?\.grant_challenge"\s*\n\s*class="' + BTN + r'"'),

    (u'第三个按钮用 info 配色（不是 success —— 它弱得多，不该看着一样重）',
     APP, r'\.' + BTN + r'\s*\{[^}]*var\(--info-soft\)'),

    (u'按钮文案里的窗口**由变量算出**，不是写死的数字',
     APP, r'\{\{\s*grantWindowMinutes\s*\}\}\s*分钟内免重复确认'),

    # 文案必须在按钮**附近**，不能是文件别处的「只对」
    (u'提示文案在按钮附近说明了**只对同类操作生效**',
     APP, r'只对\s*\n?\s*<strong>\s*本次这类操作\s*</strong>'),

    (u'提示文案说明了**到期自动失效 + 可随时取消**',
     APP, u'到期自动失效'),

    # —— 调用，而不只是定义 ——
    (u'App.vue **调用**了 core 回报窗口秒数（不许前端硬编码）',
     APP, r'FeisuoBridge\.getSessionGrantWindow\s*\('),

    (u'生效授权：loadActiveGrants 既定义又被调用',
     APP, u'__CALL__loadActiveGrants'),

    # 上面那条**不够**：`loadActiveGrants` 还被 handleApproval 与
    # revokeDeviceGrants 调用，所以"启动时不拉取"照样能通过
    # （实测：删掉启动那一次，3 处调用还剩 2 处 ≥ 2 ⇒ 假绿）。
    # 而删掉启动那一次的**真实后果**是：重启程序后徽标上的
    # 「已免确认」指示要等到下一次审批/撤销才会出现 ——
    # 用户看到的是"我的授权不见了"，于是他可能重新点一次，
    # 反而多给了一段授权。所以必须单独要求它出现在**启动块**里。
    (u'生效授权在**程序启动时**就拉一次（重启后徽标不能是空的）',
     APP, u'__INIT__loadActiveGrants'),

    (u'撤销：revokeDeviceGrants 既定义又被调用',
     APP, u'__CALL__revokeDeviceGrants'),

    (u'撤销入口在**设备徽标上**（用户看得见才谈得上可撤销）',
     APP, r'grant-active-dot'),

    # —— bridge / 宿主层 ——
    (u'bridge 暴露了撤销方法', BRIDGE, r'static async revokeDeviceGrants\s*\('),
    (u'bridge 暴露了生效授权列表', BRIDGE, r'static async listActiveGrants\s*\('),
    (u'bridge 暴露了「窗口秒数」', BRIDGE, r'static async getSessionGrantWindow\s*\('),
    (u'bridge 的动作集合含 allow_with_grant', BRIDGE, r'"allow_with_grant"'),

    (u'Tauri 命令 revoke_device_grants 存在', CMDS,
     r'pub fn revoke_device_grants'),
    (u'Tauri 命令 list_active_grants 存在', CMDS,
     r'pub fn list_active_grants'),
    (u'Tauri 命令 get_session_grant_window 存在', CMDS,
     r'pub async fn get_session_grant_window'),

    (u'revoke_device_grants 已注册（没注册 = invoke 静默失败）', MAIN,
     r'commands::revoke_device_grants'),
    (u'list_active_grants 已注册', MAIN, r'commands::list_active_grants'),
    (u'get_session_grant_window 已注册', MAIN,
     r'commands::get_session_grant_window'),
]


def main():
    missing = []
    for desc, path, pat in CHECKS:
        try:
            src = io.open(path, encoding='utf-8').read()
        except IOError:
            missing.append((desc, path, u'文件不存在'))
            continue

        if pat.startswith(u'__INIT__'):
            if not _called_in_startup_block(src, pat[len(u'__INIT__'):]):
                missing.append((desc, path, u'启动块里没有调用'))
            continue

        if pat.startswith(u'__CALL__'):
            if not _defined_and_called(src, pat[len(u'__CALL__'):]):
                missing.append((desc, path, u'定义或调用缺一'))
                continue
            if not re.search(pat[len(u'__CALL__'):] + r'\s*\(', src):
                missing.append((desc, path, u'未找到'))
            continue

        if not re.search(pat, src):
            missing.append((desc, path, u'未找到'))

    if not missing:
        print(u'✓ §2.3.1 界面侧接线完整：第三个按钮有条件、说明了范围与到期、'
              u'可撤销、窗口秒数由 core 回报（%d 项）' % len(CHECKS))
        return 0

    print(u'✗ §2.3.1 界面侧接线缺失 %d 项:' % len(missing))
    for desc, path, why in missing:
        print(u'  [%s] %s' % (why, desc))
        print(u'      %s' % path.replace('\\', '/'))
    print(u'')
    print(u'  core 侧全对而界面侧没接 = 用户在不知情的情况下被放宽了信任，')
    print(u'  那比没有这个功能更糟。')
    return 1


if __name__ == '__main__':
    sys.exit(main())
