"""守卫：`blocked`（黑名单）维度**必须不存在**。

## 为什么需要它

原始需求（`FRAMEWORK_DESIGN.md` §5，提交 `735dd66`）里**只有两态**，
`git ls-tree 735dd66` 只有 6 个文件、`grep -i blacklist` **零命中** ——
`blocked` 与 `blacklisted_devices` 都是实现期自己加的（溯源见
`DESIGN_TRUST_SHUTTLE.md` §14.0）。按 §14.11 决策 1 已整体删除，
它的两个真实诉求分别由「解除配对」（§14.5，已双向）与「隐藏」承担。

## 这条守卫盯什么

**13 类标记，任何一类重新出现就让 CI 变红。**

## 它踩过的两个坑（变异测试逼出来的）

1. 第一版只扫 `core/src` 且只认**限定名**（`TrustLevel::Blocked`）。
   于是（a）把裸变体 `Blocked,` 加回枚举、（b）在 `App.vue` 里写
   `trust_level === "blocked"`，两次都**逃过了** ——
   名字出现了，但守卫没在看那个地方。
2. 现在同时扫 `core/src` 与 `ui/src`（含 `.vue` / `.ts`），
   并且既认限定名、也认**声明处**（枚举里的裸 `Blocked,`）。

## 用法

    python scripts/guard_blocked_dimension.py
"""
import io, os, re, sys

# (显示名, 正则)
MARKERS = [
    ('add_to_blacklist',              r'add_to_blacklist\s*\('),
    ('is_ip_blacklisted',             r'is_ip_blacklisted\s*\('),
    ('is_device_blacklisted',         r'is_device_blacklisted\s*\('),
    ('remove_from_blacklist',         r'remove_from_blacklist\s*\('),
    ('list_blacklisted',              r'list_blacklisted\s*\('),
    ('blacklisted_devices 表',         r'blacklisted_devices'),
    ('BlacklistedDevice',             r'BlacklistedDevice'),
    ('TrustLevel::Blocked',           r'TrustLevel::Blocked'),
    ('DenyCode::DeviceBlocked',       r'DenyCode::DeviceBlocked'),
    ('DenyCode::BlocklistCheckFailed', r'DenyCode::BlocklistCheckFailed'),
    ('Presence::Blocked',             r'Presence::Blocked'),
    ('SecurityEventKind::BlockedAttempt', r'BlockedAttempt'),
    ('ApprovalAction::Block',         r'ApprovalAction::Block'),
    # —— 以下两条是第一版漏掉的 ——
    # 裸变体声明（把 `Blocked,` 加回枚举而不写任何限定名引用）
    ('枚举里的裸 Blocked 变体',        r'^\s*Blocked,\s*$'),
    # UI 侧的字符串字面量与 bridge 方法
    ('前端 "blocked" 字面量',          r'''trust_level\s*[!=]==?\s*["']blocked["']'''),
    ('前端 Presence blocked',         r'''Presence[^\n]*["']blocked["']'''),
    ('前端 BlacklistedDevice',        r'BlacklistedDevice'),
    ('前端 getBlacklistedDevices',    r'getBlacklistedDevices'),
    ('前端 removeFromBlacklist',      r'removeFromBlacklist'),
    ('前端 黑名单文案',                r'黑名单'),
]

TARGETS = [
    (os.path.join('core', 'src'), {'.rs'}),
    (os.path.join('desktop', 'src-tauri', 'src'), {'.rs'}),
    (os.path.join('ui', 'src'), {'.vue', '.ts'}),
]


def is_comment(line):
    t = line.strip()
    return t.startswith('//') or t.startswith('*') or t.startswith('/*')


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
        print('✓ `blocked` 维度不存在（%d 类标记全部为 0 处引用）' % len(MARKERS))
        return 0

    print('✗ `blocked` 维度仍然存在，共 %d 处引用:' % total)
    for name, _pat in MARKERS:
        if name in hits:
            print('  %s (%d):' % (name, len(hits[name])))
            for loc in hits[name][:6]:
                print('      %s' % loc)
            if len(hits[name]) > 6:
                print('      ... 还有 %d 处' % (len(hits[name]) - 6))
    return 1


if __name__ == '__main__':
    sys.exit(main())