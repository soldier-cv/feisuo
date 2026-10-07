"""守卫：**不得存在"只降级本地、不通知对端"的解除配对通路**。

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

## 为什么是"删掉"而不是"对齐"

两条冗余通路可以"对齐语义"，但那只是把分叉挪了个位置 ——
下一个人加新分支时又会分叉。**删掉一条，分叉在编译期就没了。**

这与 `blocked` 那一轮是同一个教训：**删掉一条冗余通路，
比让它和另一条"保持一致"更彻底。**

## 扫描范围与判据

扫 `core/src` + `desktop/src-tauri/src` + `ui/src`（含 `.vue` / `.ts`），
**6 类标记必须全为 0**。刻意同时盯**类型层**（`remove_device` /
`remove_trusted_device` 的方法定义与调用）、**界面层**
（`removeTrustedDevice`）与**存储层**（`DELETE FROM trusted_devices`）。

> 标记只有 6 类是**刻意的**：这一轮把"删掉"做实了，所以路径比
> `guard_blocked_dimension.py` 少得多。变异测试证明每类都真的会被红 ——
> 少但有效，优于多而有一条是摆设。

## 用法

    python scripts/guard_no_local_only_unpair.py
"""
import io, os, re, sys

MARKERS = [
    # —— 类型层：那条 DELETE 通路本身 ——
    ('TrustStore::remove_device',        r'\bfn\s+pub fn remove_device\b'),
    ('engine.remove_trusted_device',     r'\.remove_trusted_device\s*\('),
    ('pub fn remove_trusted_device',     r'pub fn remove_trusted_device\b'),
    ('remove_trusted_device 命令',        r'remove_trusted_device'),
    # —— 界面层 ——
    ('bridge removeTrustedDevice',       r'removeTrustedDevice'),
    # —— 存储层：任何"把行删掉"的 SQL ——
    ('DELETE FROM trusted_devices',      r'DELETE\s+FROM\s+trusted_devices'),
]

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


def main():
    hits = {}
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
                for i, ln in enumerate(text.split('\n'), 1):
                    if is_comment(ln):
                        continue
                    for name, pat in MARKERS:
                        if re.search(pat, ln):
                            hits.setdefault(name, []).append(
                                '%s:%d' % (path.replace('\\', '/'), i))

    total = sum(len(v) for v in hits.values())
    if total == 0:
        print(u'✓ 只有一条解除配对通路（`unpair_device` 双向）；'
              u'本地单向 DELETE 通路 %d 类标记全为 0' % len(MARKERS))
        return 0

    print(u'✗ 存在「只降级本地、不通知对端」的解除配对通路，共 %d 处:' % total)
    for name, _pat in MARKERS:
        if name in hits:
            print(u'  %s (%d):' % (name, len(hits[name])))
            for loc in hits[name][:6]:
                print(u'      %s' % loc)
            if len(hits[name]) > 6:
                print(u'      ... 还有 %d 处' % (len(hits[name]) - 6))
    return 1


if __name__ == '__main__':
    sys.exit(main())
