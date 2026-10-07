"""检查 CSS 里有"落在规则块之外的裸声明"。

## 为什么需要它

`vite build` 对这类问题只报 `css-syntax-error` **warning**，而一次构建会同时
吐出几十条，人不会去看。更糟的是 **CSS 解析器遇到非法顶层声明会丢弃它并
继续往下解析** —— 整份样式表其余部分照常工作，构建成功，界面大体正常，
只有那一条规则静默失效。

本项目实际踩过：迁移硬编码颜色时按行号替换，误删了 8 条规则的选择器行
（`.status-chip.session`、`.undo-toast-btn:hover` 等），只剩裸声明飘在块外。
后果是「撤销条按钮的悬停态不变色」「已隐藏设备的删除按钮不反白」这类
**只有那一个交互不对**的退化 —— 极难联想到是语法坏了。

## 判据

逐字符走 `<style>` 块（跳过注释），维护花括号深度。深度为 0 时出现的
`prop: value` 行就是裸声明。

深度为 0 的合法内容只有：选择器行（带 `{`）、`@media`/`@keyframes`
（带 `{`）、`}`（闭合）。所以判据不会误伤它们。

只查 `ui/src` 下 `.vue` 的 `<style>` 与 `.css`；脚本不查其它目录。

用法：
    python scripts/check-css-stray-declarations.py
退出码 0 = 干净；1 = 有裸声明。
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from _ci_utf8 import force_utf8_stdout  # noqa: E402  (必须先加固 stdout)

force_utf8_stdout()

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UI_SRC = ROOT / "ui" / "src"

# 顶层合法出现的关键字（`@media ... {` / `@keyframes ... {` / `}`）。
_AT_RULE = re.compile(r"^@[a-zA-Z-]+")
# 裸声明形态：`prop: value`，且**不是**选择器（选择器以 `.`/`#`/`*`/标识符开头）。
_DECL = re.compile(r"^-{0,2}[a-zA-Z][a-zA-Z0-9-]*\s*:")

# 选择器**列表的续行**：以逗号结尾，且冒号后紧跟标识符（伪类）。
#
# 为什么需要单列一条：`button:hover,` 这类续行长得和裸声明一模一样
# —— `_DECL` 只看 `标识符:` 的开头，`button:hover,` 照样命中。但它
# 是合法 CSS（选择器列表跨行写），报出来就是**假警报**。
#
# 假警报在这里的代价具体是什么：这条守卫的价值全在"零噪音"上，
# 一旦开始误报，人就会习惯性地忽略它的输出 —— 于是它连真正该抓的
# 游离声明一起丢掉了。守卫失效的方式往往不是报错，而是被无视。
#
# 判据为什么不能只看"逗号结尾"：那样 `color: red,` 这种**真的**
# 游离声明也会被放过（它同样以逗号收尾）。加上"冒号后紧跟标识符"
# 一条就能分开：
#   `button:hover,` → 冒号后是字母 → 伪类 → 豁免
#   `color: red,`   → 冒号后是空格 → 声明 → 照报
# 伪类名不允许带空格（`: hover` 是语法错），所以这条判据是可靠的。
#
# 残留的模糊地带只有 `color:red,` 这种省略空格的写法，它同样会
# 被豁免 —— 但那本身已是无效 CSS，浏览器照样丢弃，不构成"静默
# 生效却被判错"的实际风险。
_SELECTOR_CONTINUATION = re.compile(r":[a-zA-Z-][a-zA-Z0-9-]*[^;]*,\s*$")


def _style_body(lines: list[str], path: Path) -> list[tuple[int, str]]:
    """返回 `(<1 基行号>, <该行>)`，范围是 `<style>` 块内部（不含标签行）。"""
    if path.suffix == ".css":
        return [(i + 1, ln) for i, ln in enumerate(lines)]
    start = None
    for i, ln in enumerate(lines):
        if "<style" in ln:
            start = i + 1
            break
    if start is None:
        return []
    return [(i + 1, lines[i]) for i in range(start, len(lines))]


def check_file(path: Path) -> list[tuple[int, str]]:
    lines = path.read_text(encoding="utf-8").split("\n")
    body = _style_body(lines, path)

    bad: list[tuple[int, str]] = []
    depth = 0
    i = 0
    while i < len(body):
        lineno, raw = body[i]
        t = raw.strip()

        # 块注释：整段跳过（注释里的 `{}` 与 `prop:` 都不算数）
        if t.startswith("/*"):
            if "*/" not in t:
                i += 1
                while i < len(body) and "*/" not in body[i][1]:
                    i += 1
            i += 1
            continue

        if t.startswith("//"):
            i += 1
            continue

        opens = raw.count("{")
        closes = raw.count("}")

        if (
            depth == 0
            and opens == 0
            and _DECL.match(t)
            and not _AT_RULE.match(t)
            and not _SELECTOR_CONTINUATION.search(t)
        ):
            bad.append((lineno, t))

        depth += opens - closes
        if depth < 0:
            # 多余的 `}`：本身也是语法错，但由 CSS 解析器兜住，
            # 这里只把深度拉回 0 以免后面全部误判。
            depth = 0
        i += 1

    return bad


def main() -> int:
    if not UI_SRC.is_dir():
        print(f"找不到 {UI_SRC}", file=sys.stderr)
        return 2

    files = sorted(
        [p for p in UI_SRC.rglob("*.vue")] + [p for p in UI_SRC.rglob("*.css")]
    )
    if not files:
        print(f"{UI_SRC} 下没有 .vue / .css 文件 —— 扫描器可能已失效", file=sys.stderr)
        return 2

    total = 0
    for f in files:
        bad = check_file(f)
        if not bad:
            continue
        rel = f.relative_to(ROOT)
        print(f"{rel}:")
        for lineno, t in bad:
            print(f"  L{lineno}: {t}")
        total += len(bad)

    print()
    print("=" * 68)
    if total:
        print(f"发现 {total} 处落在规则块之外的裸声明。")
        print("这些声明**不会生效**，而 vite build 只报 warning ——")
        print("CSS 解析器会丢弃它们并继续解析，于是构建成功、界面大体正常，")
        print("只有那一条规则静默失效。典型症状是『只有某个交互不对』。")
        return 1
    print(f"干净：{len(files)} 个文件里没有游离声明。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
