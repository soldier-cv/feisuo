"""守卫：所有会进 CI 的检查脚本都必须加固 stdout。

## 为什么需要它

这些脚本的输出全是中文（给读 CI 日志的人看）。而 Python 在 Windows 上
默认用**控制台代码页**编码 stdout：

- 中文 Windows：cp936（GBK）—— 没事；
- **英文 Windows / GitHub 的 `windows-latest` runner：cp1252** ——
  `print("对比度检查")` 直接抛 `UnicodeEncodeError`，退出码 1。

后果：**CI 里 5 个检查步骤全红，而被检查的代码完全正确**。
这类失败最难查 —— 日志里只有一个编码异常，看不出跟被检查的代码有没有关系。

本项目已实测复现：强制 `PYTHONIOENCODING=cp1252` 跑 `check-contrast.py`
立刻 `UnicodeEncodeError`、退出码 1。

## 为什么不能只靠 workflow 里的 `PYTHONUTF8=1`

那只覆盖 CI。同一批脚本还要能在开发机上手动跑（中文 Windows 没事，
换一台英文系统就炸）、在别的机器 / 别的 CI 上跑。所以**每个脚本自己保证**
是唯一可靠的做法；workflow 里那道是 belt-and-braces。

## 检查什么

对 `scripts/` 下每个**会在 CI 里被调用**的脚本（见下方 CI_SCRIPTS）：

1. 必须 `import` / 调用 `_ci_utf8` 的 `force_utf8_stdout()`；
2. 加固调用必须出现在 `main()` 定义**之前**（否则主流程里已经 print 过）；
3. `_ci_utf8.py` 自身必须存在。

第 2 条是关键：`force_utf8_stdout()` 写在 `if __name__ == "__main__"`
之后等于没写。

## 用法

    python scripts/check-ci-encoding.py
退出码 0 = 全部加固；1 = 有脚本没加固。
"""

from __future__ import annotations

import io
import sys
from pathlib import Path

# _ci_utf8_inline —— 本脚本的 stdout 加固是**内联**的, 不是 import:
# 它自己就要检查 "_ci_utf8 有没有被正确使用", import 自己会成环。
# 内联的原因和做法见下方注释。
#
# ⚠️ 为什么这里必须自己来一遍: 本脚本输出全是中文, 而它在英文 Windows /
# GitHub 的 windows-latest runner 上同样会抛 UnicodeEncodeError。
# 一个"检查别人有没有加固"的脚本自己没加固, 是最难发现的讽刺。
try:  # pragma: no cover —— 只是加固, 不影响逻辑
    if (sys.stdout.encoding or "").lower().replace("-", "") not in ("utf8", "utf"):
        _buf = getattr(sys.stdout, "buffer", None)
        if _buf is not None:
            sys.stdout = io.TextIOWrapper(
                _buf, encoding="utf-8", errors="replace", line_buffering=True
            )
except Exception:  # pragma: no cover
    pass

ROOT = Path(__file__).resolve().parent.parent
SCRIPTS = ROOT / "scripts"
HELPER = SCRIPTS / "_ci_utf8.py"

# 会在 ci.yml 里被调用的脚本。与 ci.yml 保持一致 ——
# 这里多列几个无害（没被调用就不必加固），少列几个才是问题。
CI_SCRIPTS = [
    "check-css-stray-declarations.py",
    "check-contrast.py",
    "audit-ui-invariant.py",
    "audit-invoke-args.py",
    # 原先这里还有 check-ps1-encoding.py：它守的是 build.ps1 /
    # build-native.ps1 的 BOM。那两个脚本已按用户要求删除，这个守卫
    # 随之删除 —— 文件不在后它会**一路绿着**却什么也没检查，
    # 是最坏的那种守卫：看着有防护，实际没有。
    "check-ci-encoding.py",  # 本脚本自己
]


def main() -> int:
    problems: list[str] = []

    if not HELPER.is_file():
        print(f"找不到 {HELPER}", file=sys.stderr)
        return 2

    # helper 必须真的做了事: 一个空壳 helper 会让所有脚本"已加固"却照样炸。
    helper_src = HELPER.read_text(encoding="utf-8")
    if "def force_utf8_stdout" not in helper_src:
        problems.append(
            f"{HELPER.name} 里没有 force_utf8_stdout() —— "
            "所有脚本都会 import 失败，或者（更糟）import 到一个什么都不做的壳"
        )
    if 'encoding="utf-8"' not in helper_src.replace("'", '"'):
        problems.append(
            f"{HELPER.name} 没有真的用 utf-8 包装 stdout —— "
            "外壳在、内容是空话，等于没加固"
        )

    checked = 0
    for name in CI_SCRIPTS:
        p = SCRIPTS / name
        if not p.is_file():
            problems.append(f"{name}: 文件不存在（ci.yml 引用了它？）")
            continue
        if name == HELPER.name:
            continue
        src = p.read_text(encoding="utf-8")
        checked += 1

        # 本脚本自己也不能免: 它同样 print 中文, 在英文 runner 上一样会炸。
        # 不能 `import _ci_utf8`(那是 import 自己 —— 循环), 所以标记一个
        # 内联标记, 由第 1 项检查确认它在。
        if name == HELPER.name or name == Path(__file__).name:
            if "_ci_utf8_inline" in src:
                continue

        # 本脚本自己也不能免: 它同样 print 中文。
        # 不能用 import _ci_utf8 的写法（那是循环），所以内联同样的三行，
        # 并由下面的第 1 项检查确认它在。
        if "_ci_utf8" not in src and "_ci_utf8_inline" not in src:
            problems.append(
                f"{name}: 没有加固 stdout —— 在英文 Windows / GitHub runner 上，"
                "中文输出会抛 UnicodeEncodeError，退出码 1，"
                "CI 变红而代码完全正确"
            )
            continue

        call_at = src.find("force_utf8_stdout()")
        if call_at < 0:
            problems.append(
                f"{name}: import 了 _ci_utf8 但**没调用** force_utf8_stdout()"
            )
            continue

        # 必须早于 main() 定义 —— 否则 main 里的第一个 print 就已经炸了。
        main_at = src.find("def main(")
        if main_at >= 0 and call_at > main_at:
            problems.append(
                f"{name}: force_utf8_stdout() 出现在 def main( 之后 —— "
                "main 里的第一个 print 早就已经用旧编码炸掉了，等于没加固"
            )

    print("=" * 72)
    print("CI 检查脚本 stdout 编码加固")
    print("=" * 72)
    print(f"  检查 {checked} 个脚本 + 1 个 helper")
    if problems:
        print()
        for x in problems:
            print("  [未加固] " + x)
        print()
        print(f"共 {len(problems)} 处问题。")
        return 1
    print()
    print("全部加固：英文 Windows / GitHub runner 上不会因中文输出而误报。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
