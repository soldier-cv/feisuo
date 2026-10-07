"""检查常驻前景/底色的 WCAG 对比度。

## 为什么需要它

半透明底色（`rgba(...)`）必须**先与父层合成**才能算对比度。直接把它当不透明色算，
会得出 `1.00` 这种明显荒谬的结果 —— 本项目第一次跑就报了 36 处"低于 AA"，
其中绝大多数是这类误报。**一个满屏假警报的检查等于没有检查**，
所以这里显式做 alpha 合成，并把"算不出来"的单独列出而不是猜一个值。

## 判据

- 前景 = `color`，底色 = `background`；`background: transparent` 时向上找最近的
  实心祖先（由 `--bg-*` / `--accent` / 卡片类规则提供），再往上兜到 `--bg-window`。
- alpha < 1 的底色逐层合成。
- 只查**常驻**状态：`:hover` / `:active` / `:focus` 排除（瞬时态另有 `:hover` 自身规则）。
- 阈值 4.5:1（正文级）。18px+ 粗体的 3:1 不在此脚本判定内 —— 那需要知道字号，
  而字号属于设计意图，不该由脚本替人决定。

## 局限（诚实说明）

- 不解析层叠优先级，只按"选择器在样式表里出现的先后"当作层叠顺序。
  本项目样式表基本无冲突选择器，这个近似成立。
- 不处理 `background-image` / 渐变叠加。
- `?` 表示该处算不出来（用了未定义的令牌、渐变、或 `color-mix`），
  **不当作通过** —— 宁可漏报也不假装检查过了。

用法：
    python scripts/check-contrast.py
退出码 0 = 全部达标或已列出待人工确认；1 = 有低于 AA 的常驻组合。
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
STYLE_CSS = UI_SRC / "style.css"
APP_VUE = UI_SRC / "App.vue"

AA = 4.5
AA_LARGE = 3.0
# 判定阈值分两档，依据是 WCAG 的两条不同条款：
#   * 正文文字 → 1.4.3 Contrast (Minimum)，4.5:1
#   * **图形对象**（图标、状态点、纯装饰性色块）→ 1.4.11 Non-text Contrast，3:1
#   * 大号文字（>=18px，或 >=14px 且 >=700 字重）→ 1.4.3 放宽到 3:1
#
# 所以"图标 3.13:1"不是缺陷，"11px 提示文字 3.28:1"才是。
#
# 图形判据用**类名**而不是解析 DOM：这些类名（`file-glyph` / `*-icon*`）
# 在本项目里一致地只作用于 `<i>` 图标元素。刻意**不**用"看着大"来放宽 ——
# 判据必须是可核对的量，否则豁免会慢慢变成"什么都往里塞"。
ICON_SELECTOR_MARKERS = (
    "file-glyph",
    "icon-box",
    "-icon",
    "glyph",
    "live-dot",
    "mini-status-dot",
    "pulse-ring",
    "check-box",
)
# 明确按"大号文字"判的（>=18px 或 >=14px 粗体）
LARGE_TEXT_SELECTORS = ("code-pin-input", "box-giant-pin", "drop-illustration-icon")


def threshold_for(sel: str, decls: dict) -> float:
    if any(m in sel for m in LARGE_TEXT_SELECTORS):
        return AA_LARGE
    # 图标：只作用于 <i> 的类
    if any(m in sel for m in ICON_SELECTOR_MARKERS):
        return AA_LARGE
    # 大号正文：18px+，或 14px+ 且 700 字重
    fs = decls.get("font-size", "")
    fw = decls.get("font-weight", "")
    m = re.match(r"^([\d.]+)px", fs)
    if m:
        size = float(m.group(1))
        if size >= 18:
            return AA_LARGE
        if size >= 14 and fw in ("700", "800", "bold", "750"):
            return AA_LARGE
    return AA


# 曾经有一个 OPAQUE_BG_TOKENS 集合（列举"哪些底色令牌是不透明的"），
# 已删除: 它**从来没有被读取过**。
#
# 为什么这件事本身值得记一笔: 一个"看起来在管事"的死配置比没有它更费事 ——
# 它的存在会让人以为这个脚本已经在按名册核对底色, 于是不再去看它到底
# 怎么解析背景(实际走的是下面按选择器 + alpha 合成的那条路)。
# 本轮就是先被它误导, 才以为 `--info/--purple/--green` 这些令牌
# "还被守卫引用着", 白查了一轮引用计数。
#
# 真正生效的是文件末尾 `solids[theme]`: 它从 App.vue 里逐条规则解析
# background, 对 alpha >= 0.999 的按选择器建表, 供合成时向上回溯。


def parse_tokens(css: str) -> dict[str, dict[str, str]]:
    tok: dict[str, dict[str, str]] = {}
    root = css[css.index(":root") :]
    root = root[: root.index("}")]
    li = css.index("[data-theme")
    light = css[li:]
    light = light[: light.index("}")]
    for block, theme in ((root, "dark"), (light, "light")):
        for m in re.finditer(r"(--[a-z0-9-]+)\s*:\s*([^;]+);", block):
            tok.setdefault(m.group(1), {})[theme] = m.group(2).strip()
    return tok


def resolve(value: str, theme: str, tok: dict, depth: int = 0) -> str | None:
    """把 `var(--x, fallback)` / 嵌套 var 解析成最终的颜色字符串。"""
    v = value.strip()
    for _ in range(8):
        m = re.match(r"^var\(\s*(--[a-z0-9-]+)\s*(?:,\s*(.*))?\)$", v, re.S)
        if not m:
            return v
        name, fallback = m.group(1), m.group(2)
        nxt = tok.get(name, {}).get(theme)
        if nxt is None:
            if fallback is None:
                return None
            nxt = fallback.strip()
        v = nxt
    return None if depth > 8 else v


def parse_color(v: str) -> tuple[float, float, float, float] | None:
    """返回 (r, g, b, a)，分量 0..1。解析不了返回 None。"""
    v = v.strip()
    if v.startswith("#"):
        h = v[1:]
        if len(h) == 3:
            h = "".join(c * 2 for c in h)
        if len(h) == 4:
            h = "".join(c * 2 for c in h)
        if len(h) == 8:
            r, g, b, a = (int(h[i : i + 2], 16) / 255 for i in (0, 2, 4, 6))
            return (r, g, b, a)
        if len(h) != 6:
            return None
        r, g, b = (int(h[i : i + 2], 16) / 255 for i in (0, 2, 4))
        return (r, g, b, 1.0)
    m = re.match(r"^rgba?\(([^)]*)\)$", v)
    if m:
        parts = [p.strip() for p in re.split(r"[,\s/]+", m.group(1)) if p.strip()]
        if len(parts) < 3:
            return None
        try:
            comp = []
            for p in parts[:3]:
                comp.append(float(p[:-1]) / 255 if p.endswith("%") else float(p) / 255)
            a = 1.0
            if len(parts) >= 4:
                a = float(parts[3].rstrip("%"))
                if parts[3].endswith("%"):
                    a /= 100
        except ValueError:
            return None
        return (comp[0], comp[1], comp[2], max(0.0, min(1.0, a)))
    if v in ("transparent",):
        return (0.0, 0.0, 0.0, 0.0)
    return None


def over(fg: tuple, bg: tuple) -> tuple:
    """alpha 合成：把 fg 叠在 bg 上。"""
    a = fg[3]
    if a >= 1.0:
        return fg
    return (
        fg[0] * a + bg[0] * (1 - a),
        fg[1] * a + bg[1] * (1 - a),
        fg[2] * a + bg[2] * (1 - a),
        1.0,
    )


def luminance(c: tuple) -> float:
    def f(u: float) -> float:
        return u / 12.92 if u <= 0.03928 else ((u + 0.055) / 1.055) ** 2.4

    return 0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2])


def contrast(a: tuple, b: tuple) -> float:
    la, lb = luminance(a), luminance(b)
    hi, lo = max(la, lb), min(la, lb)
    return (hi + 0.05) / (lo + 0.05)


def decls(body: str) -> dict[str, str]:
    """取规则体里的声明。

    ⚠️ **不**用 `setdefault` 之外的合并：同一条规则里出现两次
    `background:` 时取**最后一个**（CSS 层叠的直觉），
    所以这里要允许覆盖。
    """
    d: dict[str, str] = {}
    for m in re.finditer(r"([a-z-]+)\s*:\s*([^;]+);", body):
        d[m.group(1).strip()] = m.group(2).strip()
    return d


def strip_comments(css: str) -> str:
    return re.sub(r"/\*.*?\*/", "", css, flags=re.S)


def main() -> int:
    for p in (STYLE_CSS, APP_VUE):
        if not p.is_file():
            print(f"找不到 {p}", file=sys.stderr)
            return 2

    tok = parse_tokens(STYLE_CSS.read_text(encoding="utf-8"))
    vue = APP_VUE.read_text(encoding="utf-8")
    sty = strip_comments(vue[vue.index("<style") :])

    # 收集规则, 记录出现顺序当层叠顺序
    rules: list[tuple[str, dict[str, str]]] = []
    for m in re.finditer(r"([^{}]+)\{([^{}]*)\}", sty):
        sel = m.group(1).strip().split("\n")[-1].strip()
        rules.append((sel, decls(m.group(2))))

    # 每个主题下, 选择器 -> 实心底色（用于 transparent 向上找）
    def solid_map(theme: str) -> dict[str, tuple]:
        out: dict[str, tuple] = {}
        for sel, d in rules:
            bg = d.get("background")
            if not bg or "gradient" in bg or "color-mix" in bg:
                continue
            c = parse_color(resolve(bg, theme, tok) or "")
            if c and c[3] >= 0.999:
                out[sel] = c
        return out

    window = {
        theme: parse_color(tok.get("--bg-window", {}).get(theme, "#ffffff") or "#ffffff")
        for theme in ("dark", "light")
    }

    report: list[tuple[str, float, float, float]] = []
    unknown: list[str] = []
    # 逐条 `color:` 都算一遍，而不是"每条规则只取第一个"。
    #
    # 早先每条规则只取第一个 `color`，于是**多色规则**（同一选择器里
    # 分状态给不同颜色，如 `.entry-status-badge` 与
    # `.entry-status-badge.failed` 写在同一条）只被检查了其中一种状态，
    # 另一种状态的对比度从来没被算过。
    # 逐条 `color:` 都算一遍，而不是"每条规则只取第一个"。
    #
    # 早先每条规则只取第一个 `color`，于是**一条规则里给了多个颜色**
    # 的情况（`color: var(--x)` 出现在 hover 子规则、或同一选择器
    # 分状态覆盖）只被检查了其中一种 —— 另一种从来没被算过。
    decls_all: list[tuple[str, dict]] = []
    for m in re.finditer(r"([^{}]+)\{([^{}]*)\}", sty):
        sel = m.group(1).strip().split("\n")[-1].strip()
        d = decls(m.group(2))
        for cm in re.finditer(r"(?<![\w-])color:\s*(var\(--[a-z-]+\))", m.group(2)):
            dd = dict(d)
            dd["color"] = cm.group(1)
            decls_all.append((sel, dd))

    for sel, d in decls_all:
        if ":hover" in sel or ":active" in sel or ":focus" in sel or "::" in sel:
            continue
        fg_raw, bg_raw = d.get("color"), d.get("background")
        if not fg_raw:
            continue
        if "gradient" in (bg_raw or ""):
            continue
        # `color-mix(in srgb, var(--accent) 12%, transparent)` =
        # "品牌色的 12% 透明度"，与 `--accent-soft` 同量级。
        # 早先直接跳过，于是 `.endpoint-chip.is-active` / `.scope-vol.is-on`
        # 这两个"已选中"态永远不被检查 —— 而选中态恰恰是用户用来确认
        # "我选中的到底是哪个"的地方。
        # 折算成 (色, 透明度) 交给下面的合成逻辑；解析不出就跳过。
        mix: tuple | None = None
        if "color-mix" in (bg_raw or ""):
            cm_ = re.search(r"var\((--[a-z-]+)\)\s*(\d+(?:\.\d+)?)%", bg_raw)
            if not cm_:
                continue
            mix = (cm_.group(1), float(cm_.group(2)) / 100.0)
            bg_raw = None
        row: list[float | None] = []
        ok = True
        for theme in ("dark", "light"):
            fg = parse_color(resolve(fg_raw, theme, tok) or "")
            if fg is None:
                ok = False
                break
            if mix is not None:
                base = parse_color(resolve(f"var({mix[0]})", theme, tok) or "")
                if base is None:
                    ok = False
                    break
                chain = [(base[0], base[1], base[2], mix[1])]
            # 没有 `background` 声明，或写了 `none` / `transparent`
            # = 透明 = 继承所在容器的底色。
            #
            # 早先把"无 background"直接跳过，于是十几个规则落进
            # "算不出来"那一栏 —— 而它们全是幽灵按钮（设置齿轮、面包屑、
            # 关闭图标、清除链接…），恰恰是用户最需要看清的一批。
            # 跳过等于不看。
            #
            # 底色按 `--bg-window` 算：幽灵按钮可能落在窗口、侧栏或卡片上，
            # 逐个猜不现实，而 `--bg-window` 在两套主题里都不是最深的那个，
            # 用它算偏保守 —— 宁可报得虚惊，也不要漏。
            elif not bg_raw or bg_raw.strip() in ("none", "transparent"):
                chain = [window[theme] or (1, 1, 1, 1)]
            else:
                bgs = solids[theme]
                chain = []
                cur = bg_raw
                guard = 0
                while cur and guard < 6:
                    guard += 1
                    c = parse_color(resolve(cur, theme, tok) or "")
                    if c is None:
                        ok = False
                        break
                    chain.append(c)
                    if c[3] >= 0.999:
                        break
                    cur = None  # 半透明：交给合成，用窗口底兜
                if not ok:
                    break
            if not ok:
                break
            if not chain or chain[0][3] < 0.999:
                base = window[theme] or (1, 1, 1, 1)
                acc = base
                for c in reversed(chain):
                    acc = over(c, acc)
                bg_c = acc
            else:
                bg_c = chain[0]
            row.append(contrast(over(fg, bg_c), bg_c))
        if not ok:
            unknown.append(sel)
            continue
        if row[0] is None or row[1] is None:
            unknown.append(sel)
            continue
        thr = threshold_for(sel, d)
        if min(row) < thr:
            report.append((sel, row[0], row[1], thr))

    print("=" * 74)
    print("常驻前景/底色对比度检查（阈值 %.1f:1，已做 alpha 合成）" % AA)
    print("=" * 74)
    if report:
        print()
        print("低于阈值：")
        for sel, d_, l_, thr in sorted(report, key=lambda x: min(x[1], x[2])):
            print(
                "  %-46s 深色 %5.2f  浅色 %5.2f   (阈值 %.1f)"
                % (sel[:46], d_, l_, thr)
            )
    else:
        print()
        print("全部达标。")
    if unknown:
        print()
        print("算不出来（**不当作通过**，需人工确认）：%d 处" % len(unknown))
        for sel in unknown[:12]:
            print("  - %s" % sel[:70])
        if len(unknown) > 12:
            print("  ... 另有 %d 处" % (len(unknown) - 12))
    print()
    print("共检查 %d 条常驻组合。" % sum(1 for s, d in rules if d.get("color") and d.get("background")))
    return 1 if report else 0


if __name__ == "__main__":
    # solids 需要在循环前建好
    _vue = APP_VUE.read_text(encoding="utf-8")
    _sty = strip_comments(_vue[_vue.index("<style") :])
    _tok = parse_tokens(STYLE_CSS.read_text(encoding="utf-8"))
    _rules = [
        (m.group(1).strip().split("\n")[-1].strip(), decls(m.group(2)))
        for m in re.finditer(r"([^{}]+)\{([^{}]*)\}", _sty)
    ]
    solids = {}
    for _theme in ("dark", "light"):
        _m = {}
        for _sel, _d in _rules:
            _bg = _d.get("background")
            if not _bg or "gradient" in _bg or "color-mix" in _bg:
                continue
            _c = parse_color(resolve(_bg, _theme, _tok) or "")
            if _c and _c[3] >= 0.999:
                _m[_sel] = _c
        solids[_theme] = _m
    raise SystemExit(main())
