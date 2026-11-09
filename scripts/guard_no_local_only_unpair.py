"""守卫：**解除配对通路不得删行**；删记录只能是那个独立的「删除设备」动作。

## 它防的是什么

需求是「一方解开配对，**双方**都变成不信任」。而实现期一度存在**两条**
叫"解除配对"的通路：

| 通路 | 通知对端 | 存储效果 | 界面是否用 |
|:---|:--:|:---|:--:|
| `unpair_device`（`MSG_UNPAIR`） | ✅ | `UPDATE` → 行保留、`trust_level=pending`、`visible` 保留 | ✅ |
| `remove_trusted_device` | ❌ | `DELETE` → **行消失**、`visible` 一起丢 | ❌ 但**已注册** |

第二个问题有三层，逐层加重：

1. **它没有任何调用方。** 界面走第一条，Android 侧还没接这个动作。
   于是它是一个**已注册但无人使用**的 Tauri 命令 —— WebView 里
   任何 JS 都能调到它。
2. **它调到的正是需求要消灭的行为。** 单方面 `DELETE` 会让关系不对称，
   而不对称的方向恰好是危险的那一侧：本机以为断了，对方还在静默投文件。
3. **它当时还是「找不到对方地址」那条兜底分支的实现。** 于是
   **有没有 IP 决定了行是被 `UPDATE` 还是被 `DELETE`** ——
   同一个按钮，两种结果，取决于一个用户看不见也控制不了的变量：
   没 IP 时用户刚隐藏的设备会**从主列表里人间蒸发**并丢掉隐藏偏好。

## 2026-11-07 新增：删除设备 ≠ 解除配对

用户改名后，旧的离线条目既不会再上线、也没法靠解除配对清掉，明确要求
补一个删除功能。于是 `DELETE FROM trusted_devices` 现在**有一个**合法去处：
`TrustStore::delete_device`，由界面「删除设备」二次确认后调用，与
`unpair_device` / `downgrade_to_untrusted` 完全分离。

因此本守卫的判据从「任何 DELETE 都必须为 0」收窄为：

- 名字类标记（5 类）仍然全为 0 —— 那条本地单向解除通路不许复活；
- `DELETE FROM trusted_devices` **只允许出现在 `delete_device` 函数体内**；
- 并且全仓库必须**恰好**只有这一个函数在删行（多一个、少一个都算失败）。

少一个 = 删除功能被悄悄删掉；出现在别的函数里 = 解除配对又偷偷删行了。
两种失效都不再能"一路绿"。

## 用法

    python scripts/guard_no_local_only_unpair.py
"""
import io, os, re, sys
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

MARKERS = [
    # —— 类型层：那条 DELETE 通路本身 ——
    ('TrustStore::remove_device',        r'\bfn\s+pub fn remove_device\b'),
    ('engine.remove_trusted_device',     r'\.remove_trusted_device\s*\('),
    ('pub fn remove_trusted_device',     r'pub fn remove_trusted_device\b'),
    ('remove_trusted_device 命令',        r'remove_trusted_device'),
    # —— 界面层 ——
    ('bridge removeTrustedDevice',       r'removeTrustedDevice'),
    # —— 存储层：把行删掉的 SQL（只允许出现在 delete_device 里）——
    ('DELETE FROM trusted_devices',      r'DELETE\s+FROM\s+trusted_devices'),
]

SQL_MARKER = 'DELETE FROM trusted_devices'
DELETE_FN = 'delete_device'
TRUST_STORE = os.path.join('core', 'src', 'security', 'trust_store.rs')

# 允许出现的位置：纯注释行。刻意**不**排除文档字符串 ——
# 文档里提到它是必要的（解释为什么删掉），但它必须以注释形式出现。
TARGETS = [
    (os.path.join('core', 'src'), {'.rs'}),
    (os.path.join('desktop', 'src-tauri', 'src'), {'.rs'}),
    (os.path.join('ui', 'src'), {'.vue', '.ts'}),
]

# 这些名字出现在**注释**里是允许的（在 commands.rs / main.rs / bridge
# 里都有"❌ 已删除"的说明）。判据是：注释行一律跳过。
def is_comment(line):
    t = line.strip()
    return (t.startswith('//') or t.startswith('*')
            or t.startswith('/*') or t.startswith('#'))

# Rust 函数签名行。用来判断 `DELETE FROM trusted_devices` 落在哪个函数体内：
# 只有 `delete_device` 获准，其余（尤其 unpair / downgrade）一律报红。
RUST_FN = re.compile(r'^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)')


def main():
    hits = {}
    sql_fns = set()
    for root, exts in TARGETS:
        if not os.path.isdir(root):
            continue
        for dirpath, _dirs, files in os.walk(root):
            for f in sorted(files):
                if os.path.splitext(f)[1] not in exts:
                    continue
                path = os.path.join(dirpath, f)
                try:
                    text = io.open(path, encoding='utf-8').read()
                except (IOError, UnicodeDecodeError):
                    continue
                current_fn = ''
                for i, ln in enumerate(text.split('\n'), 1):
                    # 只在 Rust 文件里跟踪函数体；TS / Vue 里出现这条 SQL
                    # 一律算越界（current_fn 恒为空）。
                    if path.endswith('.rs'):
                        m = RUST_FN.match(ln)
                        if m:
                            current_fn = m.group(1)
                    if is_comment(ln):
                        continue
                    for name, pat in MARKERS:
                        if not re.search(pat, ln):
                            continue
                        if name == SQL_MARKER:
                            sql_fns.add(current_fn or '(未知函数)')
                            if current_fn == DELETE_FN:
                                continue
                        hits.setdefault(name, []).append(
                            '%s:%d' % (path.replace('\\', '/'), i))

    total = sum(len(v) for v in hits.values())
    if total:
        print(u'✗ 存在「只降级本地、不通知对端」的解除配对通路，共 %d 处:' % total)
        for name, _pat in MARKERS:
            if name in hits:
                print(u'  %s (%d):' % (name, len(hits[name])))
                for loc in hits[name][:6]:
                    print(u'      %s' % loc)
                if len(hits[name]) > 6:
                    print(u'      ... 还有 %d 处' % (len(hits[name]) - 6))
        return 1

    # 正面断言：删行的能力必须存在，且恰好只有一个出口。
    # 少了它，有人把删除功能整段删掉，上面的 6 类标记反而"全 0"一路绿。
    try:
        store = io.open(TRUST_STORE, encoding='utf-8').read()
    except (IOError, UnicodeDecodeError):
        store = ''
    has_delete_api = re.search(r'pub fn delete_device\s*\(', store) is not None
    if not has_delete_api or sql_fns != {DELETE_FN}:
        print(u'✗ 删设备记录的通路不合法：delete_device 存在=%s，删行所在函数=%s'
              % (has_delete_api, sorted(sql_fns)))
        print(u'  要求：core 里有 pub fn delete_device，且只有它含 DELETE FROM trusted_devices')
        return 1

    print(u'✓ 只有一条解除配对通路（`unpair_device` 双向）；'
          u'删记录只由独立动作 `delete_device` 承担，%d 类标记全为 0' % len(MARKERS))
    return 0


if __name__ == '__main__':
    sys.exit(main())
