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
"""把 `border-radius` 收敛到四个令牌。

## 为什么值得做

收敛前样式表里有 **12 种**圆角：4/5/6/7/8/9/10/12/14/50%/99px/999px。

相邻容器差 1px 时，用户看不出"这是有意的层级"，只看出"圆角不一致" ——
5px 与 6px 差的那 1px 不传达任何信息。7px 只出现一次（`.scope-mode`），
9px 只出现在 `.scope-foot` 的"只有下两角圆"里：这些都是随手写的痕迹。

映射规则（**按尺寸角色**，不是就近取整）：

| 现有值 | 令牌 | 角色 |
| :--- | :--- | :--- |
| `999px` `99px` | `--radius-pill` | 胶囊：进度条、标签、开关轨道 |
| `10px` `12px` `14px` | `--radius-lg` | 大容器：面板、弹窗、穿梭分栏 |
| `5px` `6px` `7px` `8px` | `--radius-md` | 卡片 / 按钮 / 输入框 |
| `4px` | `--radius-sm` | 小徽标、图标按钮 |

`50%` **不动** —— 圆形头像、状态点、雷达圈靠它成圆，映射到任何
长度值都会变成椭圆。`0 0 9px 9px`（只圆下两角）单独处理：它的 9px 是"容器底角"，
按角色应等于 `--radius-lg` 的**数值**。CSS 变量不能插进
`border-radius: 0 0 X X` 这种多值简写里（`var()` 整体是一个值，
`0 0 var(--radius-lg) var(--radius-lg)` 语法非法），所以
`0 0 9px 9px` → `0 0 10px 10px`，并在下面写明它对应 `--radius-lg`。
留 9px 会被下一个人当成"又一种圆角"重新引入。

## 用法

    python scripts/consolidate-radius.py [--dry]

先 `--dry` 看清单再落盘。落盘后由 `check-contrast.py` 与
`check-css-stray-declarations.py` 复核。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = ROOT / "ui" / "src" / "App.vue"

# 数值 -> 令牌
MAP = {
    "999px": "--radius-pill",
    "99px": "--radius-pill",
    "10px": "--radius-lg",
    "12px": "--radius-lg",
    "14px": "--radius-lg",
    "8px": "--radius-md",
    "7px": "--radius-md",
    "6px": "--radius-md",
    "5px": "--radius-md",
    "4px": "--radius-sm",
}
# 不动的：靠它们成圆的形状
KEEP = {"50%"}


def main() -> int:
    raw = TARGET.read_text(encoding="utf-8")
    style_at = raw.index("<style")
    head, sty = raw[:style_at], raw[style_at:]

    changed: list[tuple[str, str, str]] = []

    def repl(m: re.Match) -> str:
        rule = m.group(0)
        sel = m.group(1).strip().split("\n")[-1].strip()

        def sub(rm: re.Match) -> str:
            val = rm.group(1).strip()
            if val in KEEP:
                return rm.group(0)
            # 只圆下两角：多值简写里插不进 var()，用数值并注明对应关系。
            # 9px 不在 MAP 里（它只出现在这一处），但按角色它是
            # "容器底角" = --radius-lg，所以显式给出目标值 10px。
            if re.match(r"^0\s+0\s+(\d+)px\s+(\d+)px$", val):
                if val.replace(" ", "") == "009px9px":
                    changed.append((sel, val, "0 0 10px 10px  (对应 --radius-lg)"))
                    return "border-radius: 0 0 10px 10px;"
                return rm.group(0)
            tok = MAP.get(val)
            if not tok:
                return rm.group(0)
            # 已是令牌的不动（幂等）
            if val == tok:
                return rm.group(0)
            changed.append((sel, val, tok))
            return f"border-radius: var({tok});"

        return re.sub(r"border-radius:\s*([^;]+);", sub, rule)

    # `[^{}]+` 允许跨行，单行与多行规则都命中
    new_sty = re.sub(r"([^{}]+)\{([^{}]*)\}", repl, sty)

    if not changed:
        print("没有需要修改的规则（可能已经收敛过）")
        return 0
    if "--dry" in sys.argv:
        print("（--dry，未写盘）将修改 %d 处：" % len(changed))
        for sel, a, b in changed:
            print("  %-38s %-8s -> var(%s)" % (sel[:38], a, b))
        return 0

    TARGET.write_text(head + new_sty, encoding="utf-8", newline="")
    print(f"已改 {len(changed)} 处：")
    for sel, a, b in changed:
        print("  %-38s %-8s -> var(%s)" % (sel[:38], a, b))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
