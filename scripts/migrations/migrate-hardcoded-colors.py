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
"""把 App.vue <style> 里的硬编码颜色换成主题令牌（一次性迁移脚本）。

迁移后本脚本即失效，保留它是为了让"这 52 处分别换成了什么"有据可查，
而不是散落在一次性的手工编辑里无法复核。

按**文件行号**定点替换，而不是正则全文替换 —— 其中多处字面量
（`rgba(255,255,255,0.1)` 与 `0.08`、`0.09`）在语义上并不等价，
全局替换会改错地方。
"""

import io
import sys
from pathlib import Path

# 行号 -> (该行必须包含的原文片段, 替换后的整行内容)
EDITS: dict[int, tuple[str, str]] = {
    5012: ("--text-dim, #8a8f98", "  color: var(--text-dim);"),
    5016: ("--text-main, #e8eaed", "  color: var(--text-main);"),
    5025: ("rgba(255, 255, 255, 0.08)", "  background: var(--bg-hover-soft);"),
    5039: ("rgba(255, 255, 255, 0.1)", "  background: var(--bg-hover-soft);"),
    5040: ("--text-dim, #8a8f98", "  color: var(--text-dim);"),
    5046: ("rgba(80, 140, 255, 0.16)", "  background: var(--info-soft);"),
    5047: ("color: #7fb0ff;", "  color: var(--info);"),
    5052: ("--text-dim, #8a8f98", "  color: var(--text-dim);"),
    5060: (
        "rgba(255, 255, 255, 0.1); color: var(--text-main, #e8eaed);",
        "  background: var(--bg-hover-soft); color: var(--text-main);",
    ),
    5066: ("--text-dim, #8a8f98", "  color: var(--text-dim);"),
    5073: ("rgba(255, 255, 255, 0.04)", "  background: var(--bg-row-soft);"),
    5074: ("rgba(255, 255, 255, 0.07)", "  border: 1px solid var(--border-subtle);"),
    5082: ("--text-dim, #8a8f98", "  color: var(--text-dim);"),
    5092: (
        "rgba(80, 140, 255, 0.18); color: #7fb0ff;",
        "  background: var(--info-soft); color: var(--info);",
    ),
    5093: (
        "rgba(255, 255, 255, 0.1); color: #9aa0a8;",
        "  background: var(--bg-hover-soft); color: var(--text-tertiary);",
    ),
    5097: ("rgba(80, 200, 140, 0.16)", "  background: var(--success-soft);"),
    5098: ("color: #6fdcaa;", "  color: var(--success);"),
    5099: ("rgba(80, 200, 140, 0.32)", "  border: 1px solid var(--success-border);"),
    5108: ("rgba(80, 200, 140, 0.26)", "  background: var(--success-soft-hover);"),
    5113: ("--text-dim, #8a8f98", "  color: var(--text-dim);"),
    5124: ("rgba(0, 0, 0, 0.28)", "  background: var(--bg-inset-strong);"),
    5125: ("rgba(255, 255, 255, 0.08)", "  border: 1px solid var(--border-subtle);"),
    5146: ("--text-main, #e8eaed", "  color: var(--text-main);"),
    # accent 早就从蓝改成绿了，这两处的 #4a9eff 是**改色前留下的残留**：
    # 深色主题下拖拽落点是"蓝色高亮"，与其他地方的绿色主色对不上。
    5152: ("var(--accent, #4a9eff)", "  outline: 2px solid var(--accent);"),
    5154: ("rgba(74, 158, 255, 0.14)", "  background: var(--accent-soft);"),
    5159: ("rgba(255, 255, 255, 0.25)", "  outline: 2px dashed var(--border-strong);"),
    5173: ("rgba(255, 255, 255, 0.14)", "  background: var(--bg-press-soft);"),
    5175: ("color: #fff;", "  color: var(--text-on-solid);"),
    5181: ("rgba(255, 255, 255, 0.24)", "  background: var(--bg-press-strong);"),
    5311: ("color: #b8860b;", "  color: var(--warn);"),
    5312: ("rgba(184, 134, 11, 0.45)", "  border-color: var(--warn-border);"),
    5314: ("rgba(184, 134, 11, 0.1)", "  background: var(--warn-soft);"),
    5323: ("rgba(0, 0, 0, 0.45)", "  background: var(--overlay-scrim);"),
    5334: ("rgba(0, 0, 0, 0.35)", "  box-shadow: var(--shadow-modal);"),
    5456: ("color: #b8860b;", "  color: var(--warn);"),
    5474: ("rgba(0, 0, 0, 0.35)", "  box-shadow: var(--shadow-modal);"),
    5590: ("color: #fff;", "  color: var(--text-on-solid);"),
    5595: ("color: #fff;", "  color: var(--text-on-solid);"),
    5629: ("rgba(255, 193, 7, 0.45)", "  border: 1px solid var(--warn-border);"),
    5630: ("rgba(255, 193, 7, 0.08)", "  background: var(--warn-soft);"),
    5639: ("color: #ffd54f;", "  color: var(--warn);"),
    5670: ("border: 1px solid #ffd54f;", "  border: 1px solid var(--warn);"),
    5678: ("color: #ef5350;", "  color: var(--danger);"),
    5691: ("color: #7fdcaa;", "  color: var(--success);"),
    5695: ("--text-dim, #8a8f98", "  color: var(--text-dim);"),
    5703: ("rgba(255, 255, 255, 0.09)", "  background: var(--bg-hover-soft);"),
    5704: ("color: #9aa0a8;", "  color: var(--text-tertiary);"),
}


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    target = root / "ui" / "src" / "App.vue"
    raw = target.read_text(encoding="utf-8")
    lines = raw.split("\n")

    problems: list[str] = []
    for lineno, (needle, replacement) in sorted(EDITS.items()):
        if lineno > len(lines):
            problems.append(f"L{lineno}: 超出文件行数 ({len(lines)})")
            continue
        cur = lines[lineno - 1]
        if needle not in cur:
            problems.append(f"L{lineno}: 期望包含 {needle!r}，实得 {cur.strip()!r}")
            continue
        lines[lineno - 1] = replacement

    if problems:
        print("以下行未按预期替换（**没有**写盘）：")
        for p in problems:
            print("  " + p)
        return 1

    enc = "\ufeff" if raw.startswith("\ufeff") else ""
    target.write_text(enc + "\n".join(lines), encoding="utf-8", newline="")
    print(f"已替换 {len(EDITS)} 行")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
