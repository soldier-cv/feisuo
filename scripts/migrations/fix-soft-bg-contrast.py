# ==============================================================
#  已完成的一次性迁移 —— 请勿重跑
# ==============================================================
# 这段迁移早已做完, 令牌已收敛。**重跑会造成破坏**:
# 它按"当前文件长什么样"来决定怎么改, 而当前文件已经是迁移后的结果,
# 再跑一遍会把已经收敛的值改回发散状态（或直接报一堆无意义的差异）。
#
# 保留它的唯一理由是**知识**: 头注里记着"为什么是这个值" ——
# 为什么 6px 而不是 5px、为什么 12 种圆角必须收敛成 4 档。
# 这些判断只看代码是推不出来的。
#
# 真正在 CI 里跑的检查是别的东西:
#   - 圆角是否用令牌      -> theme_guard.rs
#   - 硬编码颜色是否清干净 -> theme_guard.rs
#   - 对比度是否达标      -> scripts/check-contrast.py
# 迁移脚本本身不参与任何检查。
#
"""把「品牌色当填充」与「可读色当文字」这两类用法区分开。

## 为什么需要这一步

浅色主题的 `--accent` 是 `#0e9a6e`。它作为**填充**（按钮底、进度条）配
`--accent-contrast` 深绿文字是达标的（4.62:1）；但同一个品牌色被直接拿去
当**文字**时到处不达标：

- 压在 `--accent-soft`（合成后接近白）上 → 2.81:1
- 压在 `--bg-sidebar`（#e9edf4）上 → 3.05:1
- 压在 `--bg-window`（#f3f5f9）上 → 3.28:1

而品牌色当文字用的地方**恰好集中在最需要看清的位置**：面包屑链接、
文件类型图标、状态标签、速度读数、错误提示。

## 两组新令牌，各有各的适用底色

| 令牌族 | 压在什么上 | 浅色值方向 |
| :--- | :--- | :--- |
| `--*-on-soft` | `*-soft` / `--bg-active` 半透明叠层 | 更深 |
| `--*-text` | 实心容器 / 无背景（继承） | 更深 |

深色主题下两族都等于品牌色本身（本来就够亮），浅色下都必须压深。

## 为什么不用全局替换

`color: var(--accent)` 在样式表里出现 40+ 次，其中大部分是**正确的**
（压在实心底上的图标，两套主题都达标）。全局替换会把这些也改掉，
在深色主题上引入新的不一致。所以按规则判底色形态，逐条改。

## 为什么用整体正则而不是逐行解析

第一版按行配平花括号，结果**单行规则全被跳过**
（`.dir-header a { color: var(--accent); }` 这种），
而它们恰恰是文字类问题最集中的一批。脚本还输出"没有需要修改的规则"，
把没检查当成了通过。

现在用一条正则扫过整个 `<style>` 块（`[^{}]*` 天然跨行），
在 `body` 内部做替换 —— 单行与多行规则走**同一条**路径。

用法：
    python scripts/fix-soft-bg-contrast.py [--dry]
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = ROOT / "ui" / "src" / "App.vue"

# 半透明叠层底 -> 该用哪一支前景
ON_SOFT = {
    "--accent-soft": "--accent-on-soft",
    "--bg-active": "--accent-on-soft",
    "--info-soft": "--info-on-soft",
    "--warn-soft": "--warn-on-soft",
    "--danger-soft": "--danger-on-soft",
    "--success-soft": "--success-on-soft",
}
# 实心容器 / 透明底 -> 该用哪一支前景
TEXT = {
    "--accent": "--accent-text",
    "--info": "--info-text",
    "--warn": "--warn-text",
    "--danger": "--danger-text",
    "--success": "--success-text",
    "--purple": "--purple-text",
    "--green": "--green-text",
}
# 半透明底色令牌
TRANSPARENT_BG = set(ON_SOFT)


def family(tok: str) -> str:
    """去掉变体后缀，取族名。"""
    if tok == "--bg-active":
        return "--accent"
    for suf in (
        "-on-soft",
        "-text",
        "-hover",
        "-border",
        "-glow",
        "-contrast",
        "-strong",
        "-soft",
    ):
        if tok.endswith(suf):
            return tok[: -len(suf)]
    return tok


def main() -> int:
    raw = TARGET.read_text(encoding="utf-8")
    style_at = raw.index("<style")
    head, sty = raw[:style_at], raw[style_at:]

    # 单行与多行规则都命中：`[^{}]*` 允许跨行。
    rule_re = re.compile(r"([^{}]+)\{([^{}]*)\}")
    changed: list[tuple[str, str, str]] = []

    def fix_body(sel: str, body: str) -> str:
        bgm = re.search(r"background:\s*var\((--[a-z-]+)\)", body)
        cm = re.search(r"(?<![\w-])color:\s*var\((--[a-z-]+)\)", body)
        if not cm:
            return body
        old_fg = cm.group(1)

        # `color-mix(in srgb, var(--accent) 12%, transparent)` 与
        # `--accent-soft` 是**同一层意思**（品牌色的低透明度叠层），
        # 合成结果只差 alpha 的具体值。所以按 `-on-soft` 那一支处理。
        #
        # 漏了它，`.endpoint-chip.is-active`（已选中的端点）与
        # `.scope-vol.is-on`（已勾选的卷）就一直挂着 2.87:1 ——
        # 而这两个是用户确认"我选中的到底是哪个"的唯一视觉线索。
        mixm = re.search(r"color-mix\([^)]*var\((--[a-z-]+)\)", body)
        if mixm and family(old_fg) == family(mixm.group(1)):
            new_fg = family(old_fg) + "-on-soft"
            if new_fg == old_fg:
                return body
            changed.append((sel.strip().split("\n")[-1].strip(), old_fg, new_fg))
            return body.replace(f"var({old_fg})", f"var({new_fg})", 1)

        if bgm and bgm.group(1) in TRANSPARENT_BG:
            bg_tok = bgm.group(1)
            # 同族才换：--accent-soft 底 + --accent 前景。
            # --text-secondary 压 --accent-soft 本身达标，不动。
            if family(old_fg) != family(bg_tok):
                return body
            new_fg = ON_SOFT[bg_tok]
        else:
            # 实心底色或**无 background**（透明，继承容器）→ -text 那一支。
            #
            # 第三种最容易被漏：`.dir-header a`（面包屑链接）、
            # `.entry-status-badge`（10.5px 状态标签）、`.danger-action`
            # 全是 `background: none`，而它们是**文字**，最需要压得住浅底。
            new_fg = TEXT.get(old_fg)
            if not new_fg:
                return body

        if new_fg == old_fg:
            return body
        changed.append((sel.strip().split("\n")[-1].strip(), old_fg, new_fg))
        return body.replace(f"var({old_fg})", f"var({new_fg})", 1)

    def repl(m: re.Match) -> str:
        """原样重建该规则，只换 body 里的颜色。

        ⚠️ 必须**原样重建**（`m.group(0)` 的结构部分逐字保留）。
        早先写成 `"{" + body + "}"` 重建，把选择器整段丢掉了 ——
        于是 `.dir-header a { color: var(--accent); }` 变成
        `{ color: var(--accent-text); }`：**规则名没了，声明成了顶层**。
        后果是整个 `<style>` 被结构性破坏（`@keyframes` 里也丢了名字），
        而 `check-contrast.py` 直接 `index("<style")` 抛 ValueError。
        """
        new_body = fix_body(m.group(1), m.group(2))
        if new_body == m.group(2):
            return m.group(0)
        start, end = m.span(2)
        return m.group(0)[: start - m.start()] + new_body + m.group(0)[end - m.start() :]

    new_sty = rule_re.sub(repl, sty)

    if not changed:
        print("没有需要修改的规则")
        return 0
    if "--dry" in sys.argv:
        print("（--dry，未写盘）将修改 %d 条：" % len(changed))
        for sel, a, b in changed:
            print("  %-46s %s -> %s" % (sel[:46], a, b))
        return 0

    TARGET.write_text(head + new_sty, encoding="utf-8", newline="")
    print(f"已改 {len(changed)} 条规则：")
    for sel, a, b in changed:
        print("  %-46s %s -> %s" % (sel[:46], a, b))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
