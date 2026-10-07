"""审计 `feisuoBridge.invoke` 的参数与 Tauri 命令签名是否对得上。

## 为什么需要这个脚本

Tauri v2 的 `invoke(cmd, args)` 是**按名字**把 args 里的每个 key
映射到 Rust 侧同名（snake_case）参数上的。**多余的 key 会被静默丢弃**：
不报错、不警告、不进日志。

于是"前端加了参数、后端忘了加"这类断链在运行期完全隐形 ——
功能表现为"没生效"，而不是"报错"。本项目就中过一次：
`send_files` 少声明 `dest_sub_path`，穿梭发送的落点因此从未生效过，
而界面上看不出任何异常。

这个脚本把两边的参数名对齐后做差集，把隐形断链变成编译期/检查期可见。

用法（在仓库根目录执行）：
    python scripts/audit-invoke-args.py
退出码 0 = 全部对得上；1 = 有对不上的。
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
BRIDGE = ROOT / "ui" / "src" / "api" / "feisuoBridge.ts"

# 所有可能声明 Tauri 命令的 Rust 文件。
# 扫**整个目录**而不是白名单：只扫一部分的话，"未在 Rust 侧找到"那一栏
# 就会混进"定义在别的文件里"的正常情况，让这栏失去筛选价值 ——
# 而这栏恰恰是判断"invoke 了一个根本不存在的命令"的唯一依据。
DESKTOP_SRC = ROOT / "desktop" / "src-tauri" / "src"

# 扫描时跳过的文件（非命令定义处，扫了只会拖慢）。
RUST_SKIP = {"main.rs"}

# invokeSafe / invoke 的包装层。第一个参数是命令名。
INVOKE_RE = re.compile(r'\b(?:invokeSafe|invoke)<[^>]*>\s*\(\s*"([a-z0-9_]+)"\s*,\s*\{', re.M)

CMD_RE = re.compile(
    # 属性行之后可能夹着 `#[...]`、`///` 文档注释、`//` 普通注释和空行，
    # 它们都要能跳过 —— 只认紧挨着 fn 的话，
    # `#[command]` 与 `fn` 之间恰好有一行文档注释的命令就会被漏掉，
    # 而漏掉的方向是"报成未找到"，让人以为命令不存在。
    r"#\[(?:tauri::command|command)\][^\n]*\n"
    r"(?:[ \t]*(?:#\[|\s*///|\s*//)[^\n]*\n)*"
    r"[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([a-z0-9_]+)\s*\(",
    re.M,
)


def to_snake(name: str) -> str:
    """camelCase / kebab-case → snake_case，对齐 Tauri 的命名转换。"""
    name = name.replace("-", "_")
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def _literal_end(src: str, i: int) -> int:
    """`src[i]` 是引号时，返回该字面量的**结束下标之后**的位置。

    ## 必须区分 `'a'` 与 `'a` （Rust 生命周期）

    `State<'_, AppState>` 里的 `'` **不是**字符字面量的开头。
    早先的扫描器一律把 `'` 当字符串起点，于是这一行之后配平全跑飞：
    命令要么被整条丢弃（报成"未找到"），要么匹配到后面某个 `)`、
    拿到**错误的参数集** —— 后者会漏报真实的断链，比前者危险得多。

    判据：字符字面量一定是**定长 3**（`'x'` 或 `'\\'`），
    生命周期则是 `'` + 标识符 + 非 `'`。
    """
    q = src[i]
    if q in "\"`":
        j = i + 1
        while j < len(src) and src[j] != q:
            j += 2 if src[j] == "\\" else 1
        return min(j + 1, len(src))
    # q == "'"
    if i + 2 < len(src) and src[i + 2] == "'":
        return i + 3
    return i + 1  # Rust 生命周期，当作普通字符


def _match_paren(src: str, open_idx: int) -> int:
    """返回与 `src[open_idx]`（必须是 `(`）配对的 `)` 的下标。"""
    depth = 0
    i = open_idx
    while i < len(src):
        ch = src[i]
        if ch in "\"'`":
            i = _literal_end(src, i)
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def _split_top_level(s: str) -> list[str]:
    """按**顶层**逗号切分（嵌套的 `(), <>, {}, []` 与字面量内部不算）。"""
    parts: list[str] = []
    depth = 0
    angle = 0
    start = 0
    i = 0
    while i < len(s):
        ch = s[i]
        if ch in "\"'`":
            i = _literal_end(s, i)
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == "<":
            angle += 1
        elif ch == ">":
            angle = max(0, angle - 1)
        elif ch == "," and depth == 0 and angle == 0:
            parts.append(s[start:i])
            start = i + 1
        i += 1
    tail = s[start:]
    if tail.strip():
        parts.append(tail)
    return parts


def parse_invoke_args(src: str) -> dict[str, set[str]]:
    """命令名 → 该次调用传出的参数 key 集合（camelCase 原样）。"""
    out: dict[str, set[str]] = {}
    for m in INVOKE_RE.finditer(src):
        cmd = m.group(1)
        brace = src.index("{", m.end() - 1)
        # 花括号配平（`_match_paren` 只认 `(`，这里单独走一遍）。
        depth = 0
        end = -1
        i = brace
        while i < len(src):
            ch = src[i]
            if ch in "\"'`":
                i = _literal_end(src, i)
                continue
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    end = i
                    break
            i += 1
        if end < 0:
            continue
        obj = src[brace + 1 : end]
        keys: set[str] = set()
        for seg in _split_top_level(obj):
            km = re.match(r"\s*([A-Za-z_$][A-Za-z0-9_$]*)\s*:", seg)
            if km:
                keys.add(km.group(1))
            elif re.match(r"\s*\.\.\.", seg):
                # 展开运算符：键名在别处定义，本处无法静态判定，跳过。
                pass
        out.setdefault(cmd, set()).update(keys)
    return out


def parse_rust_params(src: str) -> dict[str, set[str]]:
    """命令名 → 该命令声明的参数名集合。"""
    out: dict[str, set[str]] = {}
    for m in CMD_RE.finditer(src):
        fn = m.group(1)
        open_paren = src.index("(", m.end() - 1)
        close = _match_paren(src, open_paren)
        if close < 0:
            continue
        params = src[open_paren + 1 : close]
        names: set[str] = set()
        for seg in _split_top_level(params):
            # 类型里可能含 `::`（`serde_json::Value`、`State<'_, AppState>`），
            # 所以"名字: 类型"之间不能简单排除冒号 —— 只要求**第一个**冒号
            # 之后是类型即可。
            pm = re.match(r"\s*([a-z_][a-z0-9_]*)\s*:\s*\S", seg.strip())
            if pm:
                names.add(pm.group(1))
        out[fn] = names
    return out


def _selftest() -> int:
    """解析器自检。

    ## 为什么审计工具需要自检

    这个工具的输出是"**没找到问题**"——而一个坏掉的解析器恰好也输出
    "没找到问题"。本项目里已经踩过两次：把开括号算进深度导致参数名
    一个都没解析出来（报出一堆假断链）；把 Rust 生命周期 `'a` 当成字符串
    导致配平跑飞（漏掉真断链）。

    两种失效方向的**症状恰好相反**，肉眼很难从输出里看出是工具坏了
    还是代码真有问题。所以工具必须能自证。
    """
    cases: list[tuple[str, dict[str, set[str]], str, set[str]]] = [
        (
            "基础 + 注入参数",
            {"a": {"x", "state"}},
            '#[tauri::command]\npub fn a(x: String, state: State<\'_, S>) {}',
            {"x", "state"},
        ),
        (
            "属性与 fn 之间夹文档注释",
            {"a": {"paths", "state"}},
            '/// 注释\n#[tauri::command]\npub fn a(paths: Vec<String>, state: S) {}',
            {"paths", "state"},
        ),
        (
            "生命周期里的 ' 不得打乱括号配平",
            {"a": {"v", "state"}},
            "#[tauri::command]\npub fn a(v: Value, state: tauri::State<'_, crate::AppState>) -> R {}\n"
            "#[tauri::command]\npub fn b(only: u8) {}",
            {"v", "state"},
        ),
        (
            "类型里的 :: 与泛型逗号",
            {"a": {"payload", "app"}},
            "#[tauri::command]\npub fn a(payload: serde_json::Value, app: AppHandle) -> R {}",
            {"payload", "app"},
        ),
        (
            "最后一个参数没有尾随逗号也要解析",
            {"a": {"x", "y"}},
            "#[tauri::command]\npub fn a(x: String, y: Option<String>) {}",
            {"x", "y"},
        ),
        (
            "JS 侧：嵌套对象与字符串里的逗号不影响顶层切分",
            {"a": {"cmd", "opts", "n"}},
            'invokeSafe<A>("a", { cmd: "x, y", opts: { k: "v" }, n: 1 })',
            {"cmd", "opts", "n"},
        ),
    ]
    fails = 0
    for name, expected, src, want in cases:
        got = parse_rust_params(src) if src.lstrip().startswith(("#", "pub", "///")) else parse_invoke_args(src)
        if not expected or not want:
            continue
        key = next(iter(expected))
        if got.get(key) != want:
            fails += 1
            print(f"  [FAIL] {name}")
            print(f"         期望 {sorted(want)}")
            print(f"         实得 {sorted(got.get(key, set()))}")
        else:
            print(f"  [ok]   {name}")

    # 负例：必须能**发现**断链，否则"没报"就没有意义。
    neg = parse_invoke_args('invokeSafe<X>("send_files", { files, ghost })')
    if "ghost" in neg.get("send_files", set()):
        print("  [FAIL] 负例：多传的参数没被解析出来")
        fails += 1
    else:
        print("  [ok]   负例：能解析出多传的参数（供差集发现断链）")

    # 端到端负例：**复刻本项目真实发生过的那个 bug**。
    # JS 传 `destSubPath`，Rust 签名里没有 —— 差集必须报出来。
    js = (
        'invokeSafe<S>("send_files", {\n'
        "  targetDeviceId, targetIp, files,\n"
        "  grantCode: grantCode || null,\n"
        "  destSubPath: destSubPath || null,\n"
        "})"
    )
    rs_missing = (
        "#[tauri::command]\n"
        "pub fn send_files(target_device_id: String, target_ip: String,\n"
        "                files: Vec<String>, grant_code: Option<String>,\n"
        "                state: State<'_, AppState>) -> R {}"
    )
    rs_ok = rs_missing.replace(
        "state: State<'_, AppState>",
        "dest_sub_path: Option<String>, state: State<'_, AppState>",
    )
    sent = parse_invoke_args(js).get("send_files", set())
    for label, rs, want_flagged in (("缺参数（应报）", rs_missing, True), ("已修好（不应报）", rs_ok, False)):
        declared = parse_rust_params(rs).get("send_files", set())
        extra = {to_snake(k) for k in sent} - declared
        flagged = "dest_sub_path" in extra
        if flagged != want_flagged:
            print(f"  [FAIL] 端到端负例「{label}」：flagged={flagged}，期望 {want_flagged}")
            fails += 1
        else:
            print(f"  [ok]   端到端负例「{label}」→ extra={sorted(extra)}")

    print()
    if fails:
        print(f"自检失败 {fails} 项 —— 本工具当前的输出**不可信**，先修解析器。")
        return 1
    print("自检全部通过。")
    return 0


def main() -> int:
    if "--selftest" in sys.argv:
        return _selftest()
    if not BRIDGE.exists():
        print(f"找不到 {BRIDGE}", file=sys.stderr)
        return 2
    bridge_src = BRIDGE.read_text(encoding="utf-8")

    rust: dict[str, set[str]] = {}
    rust_file_of: dict[str, str] = {}
    for p in sorted(DESKTOP_SRC.rglob("*.rs")):
        if p.name in RUST_SKIP:
            continue
        for cmd, names in parse_rust_params(p.read_text(encoding="utf-8")).items():
            # 同名命令出现在多处时取**参数并集**并记住出处，
            # 宁可多报一条"参数在另一个文件里"也不要漏报断链。
            rust[cmd] = rust.get(cmd, set()) | names
            rust_file_of[cmd] = p.name

    calls = parse_invoke_args(bridge_src)

    # 模拟执行 / 非 Tauri 环境下不落到 Rust 的命令，跳过。
    MOCK = {"mock_"}

    missing_cmd: list[str] = []
    extra_arg: list[tuple[str, str]] = []
    unmapped: list[tuple[str, str]] = []

    for cmd in sorted(calls):
        if cmd in MOCK:
            continue
        if cmd not in rust:
            # 可能是 core 层 / 别处定义的命令，或已废弃的调用。
            missing_cmd.append(cmd)
            continue
        declared = rust[cmd]
        for key in sorted(calls[cmd]):
            snake = to_snake(key)
            if snake in declared:
                continue
            if snake in ("state", "app", "app_handle", "window", "webview"):
                # Tauri 注入的参数，invoke 侧不传。
                continue
            extra_arg.append((cmd, key))

    print("=" * 68)
    print("invoke 参数 / Tauri 命令签名 对齐审计")
    print("=" * 68)
    print(f"  bridge 里 invoke 的命令数: {len(calls)}")
    print(f"  Rust 里 #[command] 的命令数: {len(rust)}")

    if extra_arg:
        print()
        print("[断链] 前端传了、后端签名里没有 —— 会被静默丢弃，功能不生效：")
        for cmd, key in extra_arg:
            print(f"  - {cmd}(...)  多传了 `{key}`  （声明在 {rust_file_of.get(cmd, '?')}）")
    else:
        print("\n[OK] 没有多余的参数被丢弃。")

    if missing_cmd:
        print()
        print("[未在 Rust 侧找到] 这些命令在审计的源文件里没有 #[command] 定义")
        print("            （可能定义在别处、也可能已废弃，人工确认）：")
        for cmd in missing_cmd:
            print(f"  - {cmd}")
        unmapped = [m for m in missing_cmd]

    if unmapped and "--strict" in sys.argv:
        print("\n--strict：把未找到的命令也算失败。")
        return 1
    return 1 if extra_arg else 0


if __name__ == "__main__":
    raise SystemExit(main())
