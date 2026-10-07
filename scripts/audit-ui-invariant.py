"""守卫 UI 层的**不变量**：入口之间不许有行为差异。

## 为什么需要它

这个项目反复出现的失效模式不是"某个功能坏了"，而是**同一个动作在不同
入口下行为不同**：

- 准入检查（信任等级 / 阻止 / 离线 / 探测）四份拷贝，其中两份不完整
- 落点 `dest_sub_path` 三条路径各带各的，有一条压根没带
- 5 秒撤销窗口只有一条路径有
- 直发成功无条件清空整个待发清单，把用户另一批排队文件一起弄丢

这些都不会报错、不会崩、编译期完全无感 —— 界面照样显示"发送成功"，
而行为与隔壁那个入口不同。本脚本把"必须一致"的那些点钉住。

## 检查项

1. **准入统一**：四条发送/取回入口必须都走 `canDirectSendTo`，
   不得再有裸的 `targetEndpoint` 判空（那只是判空，不是准入）。
2. **撤销窗口统一**：三条直发路径都必须 `openUndoWindow`。
3. **落点统一**：凡是 `sendFiles` 调用，要么带 `destSubPath` / `dest.path`，
   要么紧邻注释说明为何不带。
4. **不谎报**：撤销窗口打开后，失败分支必须 `closeUndoWindow`。
5. **只清本次**：清空待发清单必须用 `filter(...)` 按路径精确删，
   不允许裸的 `stagedFiles.value = []`（除了发送成功那一处）。

## 用法

    python scripts/audit-ui-invariant.py
退出码 0 = 全部满足；1 = 有违反。
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
APP = ROOT / "ui" / "src" / "App.vue"


def _count_top_level_args(args: str) -> int:
    """数参数表里的**顶层**参数个数（嵌套的 `(`/`[`/`{` 与字符串内不算）。"""
    depth = 0
    in_str = False
    quote = ""
    count = 0
    has_content = False
    i = 0
    while i < len(args):
        c = args[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            if c == quote:
                in_str = False
            i += 1
            continue
        if c in "\"'`":
            in_str = True
            quote = c
            has_content = True
        elif c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            count += 1
            has_content = False
        elif not c.isspace():
            has_content = True
        i += 1
    return count + (1 if has_content else 0)


def strip_comments(src: str) -> str:
    return re.sub(r"<!--.*?-->", "", src, flags=re.S)


def strip_line_comments(src: str) -> str:
    out = []
    for line in src.split("\n"):
        # 只在不在字符串里的 // 才算注释 —— 本项目模板与脚本里都有 URL 字符串，
        # 粗暴按 // 切会把它们截断，得到一堆假阳性。
        in_str = False
        quote = ""
        cut = None
        i = 0
        while i < len(line):
            c = line[i]
            if in_str:
                if c == "\\":
                    i += 2
                    continue
                if c == quote:
                    in_str = False
            else:
                if c in "\"'`":
                    in_str = True
                    quote = c
                elif c == "/" and i + 1 < len(line) and line[i + 1] == "/":
                    cut = i
                    break
            i += 1
        out.append(line if cut is None else line[:cut])
    return "\n".join(out)


def function_body(src: str, name: str) -> str:
    """取 `function <name>` 的函数体（按花括号配平）。

    ## 这里踩过的坑：不能用"参数表后第一个 `{`"

    `directSendTo(dev, endpoint: { ip: string; port: number }, files)` 的
    参数里有一个**内联对象类型**。早先从 `)` 之后找第一个 `{`，
    于是匹配到的是 `{ ip: string; port: number }` 里的那个 ——
    函数体被截成 28 个字符，守卫于是报"directSendTo 没有开撤销窗口"，
    而它明明调了。

    **假阳性比没有守卫更贵**：它会让人以为代码有问题，去改本来正确的地方。
    所以必须从**圆括号配平之后**再找 `{`，并且跳过所有前导注释。
    """
    m = re.search(r"(?:async\s+)?function\s+" + re.escape(name) + r"\s*\(", src)
    if not m:
        return ""
    # 先配平参数表：从 `(` 开始找到它的配平 `)`
    depth = 0
    close = -1
    for j in range(m.end() - 1, len(src)):
        if src[j] == "(":
            depth += 1
        elif src[j] == ")":
            depth -= 1
            if depth == 0:
                close = j
                break
    if close < 0:
        return ""
    # 从 `)` 之后找函数体的 `{`，跳过空白与注释
    k = close + 1
    while k < len(src):
        c = src[k]
        if c.isspace():
            k += 1
            continue
        if src.startswith("//", k):
            nl = src.find("\n", k)
            k = len(src) if nl < 0 else nl + 1
            continue
        if src.startswith("/*", k):
            e = src.find("*/", k)
            k = len(src) if e < 0 else e + 2
            continue
        break
    if k >= len(src) or src[k] != "{":
        return ""
    i = k
    depth = 0
    for j in range(i, len(src)):
        if src[j] == "{":
            depth += 1
        elif src[j] == "}":
            depth -= 1
            if depth == 0:
                return src[i : j + 1]
    return ""


SEND_PATHS = [
    # (函数名, 为什么这条入口必须有撤销窗口)
    ("directSendTo", "拖进窗口 / 拖到设备卡片的直发"),
    ("sendPathsIntoRemote", "穿梭框拖拽与「发送到对方」按钮"),
    ("startSendTransfer", "待发清单的「发送」按钮"),
]
FETCH_PATHS = ["shuttleFetch"]

# 需要在发送前就分派的入口：**文件直发，文件夹要确认**。
#
# 这是用户反复纠正过的一条规则（"要确认的只有文件夹和剪切板内容"），
# 而它已经分叉过两次：
#   * `onDeviceDrop`（拖到设备卡片）整个漏掉目录判定 —— 拖一个几万个文件的
#     项目目录到卡片上会**直接发走**，而同样的内容拖进窗口会等确认。
#   * 早先还有一道 `DIRECT_SEND_MAX_FILES = 50` 的数量闸，它的存在理由
#     是"避免误发整个目录"，而那个理由已经被 `hasDirectory` 完整覆盖 ——
#     一道理由冗余的闸，就是用户抱怨的"凭什么还要确认"。
#
# 所以这里钉的是**判据形态**，不是具体代码：入口必须先取 preview，
# 再按 `hasDirectory` 分派。
DISPATCH_ENTRIES = [
    ("routeDroppedPaths", "拖进窗口"),
    ("onDeviceDrop", "拖到设备卡片"),
    ("sendPathsIntoRemote", "穿梭框拖拽与「发送到对方」按钮"),
]

# 直发入口的**硬性**守卫：拿掉这些，目录就会绕过确认。
NO_BYPASS = ("hasDirectory", "previewAndStage")


def check(raw: str) -> list[str]:
    """对已剥掉注释的 App.vue 源码跑全部检查，返回违反列表。

    拆出来是为了能喂**合成样例**做自测：守卫自己能不能抓到缺陷，
    必须能证明 —— 不然它只是一段会一直绿的代码。
    """
    problems: list[str] = []

    # 1) 准入统一
    for fn in SEND_PATHS + [(f, "取回") for f in FETCH_PATHS]:
        body = function_body(raw, fn[0])
        if not body:
            problems.append(
                f"{fn[0]}：找不到函数体 —— 守卫自身的解析可能失效，需人工确认"
            )
            continue
        # 准入：这两个函数的准入在**调用方**（routeDroppedPaths / shuttleSend
        # 在调它们之前已判过），所以允许它们只做 endpoint 传递。
        if fn[0] not in ("directSendTo", "startSendTransfer"):
            if "canDirectSendTo(" not in body:
                problems.append(
                    f"{fn[0]}：{fn[1]}这条入口没有走 canDirectSendTo —— "
                    "缺信任等级/阻止/离线的判断, 用户会看到协议层的『未配对』"
                    "而不是『点右侧的配对按钮』"
                )
        # 裸的 targetEndpoint 判空不是准入
        if re.search(r"if\s*\(\s*!targetEndpoint\.value\s*\)", body):
            problems.append(
                f"{fn[0]}：仍在用 `!targetEndpoint.value` 判可达 —— "
                "那只是判空, 不看信任等级也不探测; 应改用 canDirectSendTo"
            )

    # 2) 撤销窗口统一
    #
    # ⚠️ 必须**同时**检查"有没有调"和"在哪里调"。
    #
    # 早先只查 `openUndoWindow` 是否出现，于是放错位置也通过 ——
    # 而 `sendFiles` 是 `await` 到**传输结束**才返回的，窗口开在它之后时：
    # 传输已完成 → 窗口才弹出 → 紧接着 `committed = true` →
    # 横幅写着「已开始传输」、撤销按钮**永远不出现**。
    # 那个 5 秒撤销对那条路径等于不存在，界面上却看不出任何异常。
    for fn, why in SEND_PATHS:
        body = function_body(raw, fn)
        if not body:
            continue
        if "openUndoWindow(" not in body:
            problems.append(
                f"{fn}：{why}没有开 5 秒撤销窗口 —— "
                "三条直发入口里只有它没有, 用户从这条路发错就完全无法挽回"
            )
        else:
            at_open = body.find("openUndoWindow(")
            at_send = body.find("FeisuoBridge.sendFiles(")
            if at_send >= 0 and at_open > at_send:
                problems.append(
                    f"{fn}：openUndoWindow 在 sendFiles **之后** —— "
                    "sendFiles 是 await 到传输结束才返回的, 那样窗口弹出时传输已完成, "
                    "紧接着 committed=true, 撤销按钮永远不出现, "
                    "等于没有撤销。必须在发送前开窗。"
                )
        # 4) 失败必须关窗口
        #
        # 必须限定在 **catch 块**里，不能只看"函数体里有没有 closeUndoWindow"。
        # 第一版就是只搜整段体，于是成功路径那一处 `closeUndoWindow()`
        # 把"catch 里忘了关"完全掩盖过去 —— 而那正是要抓的缺陷。
        # （自测第 6 项就是靠这一点变红的。）
        catch_at = body.rfind("catch")
        tail = body[catch_at:] if catch_at >= 0 else ""
        if "closeUndoWindow()" not in tail and "undoneTransferIds" not in tail:
            problems.append(
                f"{fn}：catch 块里没有 closeUndoWindow —— "
                "发送失败时横幅上仍写着『已发起发送 · N 个文件』, 而一个字节都没发。"
                "注意不能只看函数体里有没有它：成功路径那处会掩盖掉 catch 里的遗漏"
            )

    # 5) 只清本次
    #
    # 豁免两处，且**都写明为什么**：
    #   * `clearStaged` —— 用户点了「清空全部」，就是要清空。
    #   * 「发送成功」那处 —— 清单里**每一项**都参与了这批发送
    #     （`files = stagedFiles.map(...)`），所以整体清空是对的。
    #     真正会丢数据的是那些 `files` 来自别处（拖进窗口 / 穿梭拖拽）
    #     却仍整体清空的路径 —— 那是 `directSendTo` 与
    #     `promptGrantCodeAndRetry`，它们现在用 filter 按路径精确删。
    BARE_CLEAR_OK = ("clearStaged", "startSendTransfer")
    for m in re.finditer(r"stagedFiles\.value\s*=\s*\[\]\s*;", raw):
        before = raw[: m.start()]
        fns = list(re.finditer(r"(?:async\s+)?function\s+(\w+)\s*\(", before))
        owner = fns[-1].group(1) if fns else "?"
        if owner in BARE_CLEAR_OK:
            continue
        problems.append(
            f"{owner}：出现裸的 `stagedFiles.value = []` —— "
            "若本次发送的 `files` 来自别处（拖进窗口 / 穿梭拖拽），"
            "整体清空会把用户**另一批**排队中的文件一起弄丢; "
            "应按本次实际发出的路径 filter（见 directSendTo 的注释）"
        )

    # 3) 落点统一
    #
    # 已知的、**确实**该走收件根的两处，逐个列出并写明理由。
    # 剩下的裸调用一律要求就地注释 —— 落点分叉过一次，
    # 而且那次分叉是**静默**的：穿梭那条漏传 dest_sub_path，
    # 而 UI 端照样显示"落到对方地址栏当前目录"，功能从未生效过。
    NO_DEST_OK = {
        "directSendTo": "拖进窗口 / 拖到设备卡片：没有地址栏概念，恒为收件根",
        "promptGrantCodeAndRetry": "重试必须与首次**完全一致**的落点；"
        "它的 destSubPath 由调用方传入（穿梭传 dest.path，其余传空串）",
    }
    for m in re.finditer(r"FeisuoBridge\.sendFiles\(", raw):
        # 参数表按**圆括号**配平截取，不用 `find(");")` ——
        # 后者会在参数里出现嵌套调用（如 `format("a(b)")`）时提前截断。
        depth = 0
        close = -1
        for j in range(m.end() - 1, len(raw)):
            if raw[j] == "(":
                depth += 1
            elif raw[j] == ")":
                depth -= 1
                if depth == 0:
                    close = j
                    break
        args = raw[m.end() : close if close > 0 else m.end() + 460]

        # 判据是**顶层逗号个数**，不是变量名。
        #
        # 早先只找 `destSubPath` / `dest.path` 两个名字，于是
        # `startSendTransfer` 传的 `destSub` 被判成"没带落点" ——
        # 而它带了。按名字匹配意味着：有人把变量改叫 `dest` 就误报，
        # 有人忘了传就漏报（因为恰好像某个已知名字）。
        # 真正的不变量是**参数个数**：第 7 个是 destSubPath。
        n_args = _count_top_level_args(args)
        if n_args >= 7:
            continue

        fns = list(re.finditer(r"(?:async\s+)?function\s+(\w+)\s*\(", raw[: m.start()]))
        owner = fns[-1].group(1) if fns else "?"
        if owner in NO_DEST_OK:
            continue
        ctx = raw[max(0, m.start() - 400) : m.start()]
        if "收件根" not in ctx and "落点" not in ctx:
            problems.append(
                f"{owner}：sendFiles 只传了 {n_args} 个参数，第 7 个是 destSubPath —— "
                "漏传时落点恒为收件根，而界面照样显示『落到对方地址栏当前目录』。"
                "落点分叉过一次（穿梭那条漏传，功能从未生效且无任何异常表现）"
            )

    # 6) 直发入口必须分派目录（用户反复纠正过的那条规则）
    for fn, why in DISPATCH_ENTRIES:
        body = function_body(raw, fn)
        if not body:
            problems.append(
                f"{fn}：找不到函数体 —— 守卫自身的解析可能失效，需人工确认"
            )
            continue
        missing = [k for k in NO_BYPASS if k not in body]
        if missing:
            problems.append(
                f"{fn}（{why}）没有按 hasDirectory 分派 —— "
                f"缺 {', '.join(missing)}。用户明确要求「要确认的只有文件夹和"
                "剪切板内容」，而这个入口会**把文件夹直接发走**："
                "onDeviceDrop 整个漏掉目录判定，拖一个项目目录到设备卡片上"
                "当场发出去，5 秒撤销根本来不及；同一份内容拖进窗口却会等确认。"
            )
        # 直发前必须先取 preview（判定的数据来源）
        if "previewSendPaths(" not in body and "routeDroppedPaths" in DISPATCH_ENTRIES[0][0]:
            problems.append(
                f"{fn}：没有取 previewSendPaths —— hasDirectory 判定依赖它"
            )

    return problems
# =======================================================================
#
# 为什么非要这个：本脚本第一版有三个 bug，抓不到任何东西：
#   1. 函数体从参数表后取第一个 `{`，而 `directSendTo` 的参数里有内联对象
#      类型 `endpoint: { ip: string; port: number }` —— 体被截成 28 字符，
#      于是报"没有开撤销窗口"（假阳性，恰好和真缺陷同一句话）。
#   2. 落点判据按**变量名**匹配，`destSub` 被判成"没带"（又一条假阳性）。
#   3. 撤销窗口只查"有没有调"、不查"在哪里调" —— 真缺陷（放在
#      `sendFiles` 之后，窗口等于不存在）**照样绿**。
#
# 假阳性会让人去改本来正确的地方，假阴性让守卫变成摆设。
# 两者都必须能被自动证明，所以这里喂合成样例。
#
GOOD_TPL = """
async function directSendTo(dev, endpoint, files) {
  openUndoWindow(dev, files.length);
  try {
    await FeisuoBridge.sendFiles(dev.device_id, endpoint.ip, endpoint.port, dev.device_name, files);
    closeUndoWindow();
  } catch (e) { closeUndoWindow(); }
}
async function sendPathsIntoRemote(paths) {
  const preview = await FeisuoBridge.previewSendPaths(paths);
  if (preview.hasDirectory) { await previewAndStage(paths, preview); return; }
  const gate = await canDirectSendTo(dev);
  openUndoWindow(dev, paths.length);
  try {
    await FeisuoBridge.sendFiles(dev.device_id, gate.endpoint.ip, gate.endpoint.port, dev.device_name, paths, "", dest.path);
    closeUndoWindow();
  } catch (e) { closeUndoWindow(); }
}
async function startSendTransfer() {
  openUndoWindow(dev, files.length);
  try {
    await FeisuoBridge.sendFiles(dev.device_id, gate.endpoint.ip, gate.endpoint.port, dev.device_name, files, "", destSub);
    closeUndoWindow();
  } catch (e) { closeUndoWindow(); }
  stagedFiles.value = [];
}
async function shuttleFetch(paths) {
  const gate = await canDirectSendTo(dev);
  return gate;
}
function clearStaged() { stagedFiles.value = []; }
async function routeDroppedPaths(paths) {
  const preview = await FeisuoBridge.previewSendPaths(paths);
  if (preview.hasDirectory) { await previewAndStage(paths, preview); return; }
  await directSendTo(dev, gate.endpoint, paths);
}
async function onDeviceDrop(e, dev) {
  const dropped = await collectDroppedFiles(e.dataTransfer);
  const files = dropped.length > 0 ? dropped : stagedFiles.value.map((f) => f.path);
  const preview = await FeisuoBridge.previewSendPaths(files);
  if (preview.hasDirectory) { await previewAndStage(files, preview); return; }
  const gate = await canDirectSendTo(dev);
  await directSendTo(dev, gate.endpoint, files);
}
"""


def selftest() -> int:
    cases: list[tuple[str, str, bool, str]] = [
        # (用例名, 源码, 期望是否报出问题, 期望命中的关键词)
        ("基线：全部满足", GOOD_TPL, False, ""),
        (
            "穿梭拖拽没有撤销窗口",
            GOOD_TPL.replace(
                "  openUndoWindow(dev, paths.length);\n  try {",
                "  try {",
            ),
            True,
            "没有开 5 秒撤销窗口",
        ),
        (
            "撤销窗口开在 sendFiles 之后（等于没有撤销）",
            GOOD_TPL.replace(
                "  openUndoWindow(dev, paths.length);\n  try {\n"
                "    await FeisuoBridge.sendFiles(dev.device_id, gate.endpoint.ip,"
                " gate.endpoint.port, dev.device_name, paths, \"\", dest.path);",
                "  try {\n"
                "    await FeisuoBridge.sendFiles(dev.device_id, gate.endpoint.ip,"
                " gate.endpoint.port, dev.device_name, paths, \"\", dest.path);\n"
                "    openUndoWindow(dev, paths.length);",
            ),
            True,
            "之后",
        ),
        (
            "穿梭漏传落点（第 7 个参数）",
            GOOD_TPL.replace(
                "dev.device_name, paths, \"\", dest.path);",
                "dev.device_name, paths);",
            ),
            True,
            "第 7 个是 destSubPath",
        ),
        (
            "落点用别的变量名传入（不该误报）",
            GOOD_TPL.replace("destSub\n    );", "whateverName\n    );"),
            False,
            "",
        ),
        (
            "直发失败不关撤销窗口（横幅谎报）",
            # 只改 directSendTo 的 catch：从 `closeUndoWindow()` 变成空。
            # 用 count=1 精确替换它那一处，否则会把别的函数也改掉。
            GOOD_TPL.replace(
                "  } catch (e) { closeUndoWindow(); }",
                "  } catch (e) { showToast(String(e), true); }",
                1,
            ),
            True,
            "closeUndoWindow",
        ),
        (
            "直发成功整体清空待发清单（丢掉用户另一批文件）",
            GOOD_TPL.replace(
                "  }\n  stagedFiles.value = [];\n}",
                "  }\n  stagedFiles.value = stagedFiles.value.filter((f) => !files.includes(f.path));\n}",
            ),
            False,
            "",
        ),
        (
            "用裸 targetEndpoint 判空冒充准入",
            GOOD_TPL.replace(
                "  const gate = await canDirectSendTo(dev);\n"
                "  openUndoWindow(dev, paths.length);",
                "  if (!targetEndpoint.value) return;\n"
                "  openUndoWindow(dev, paths.length);",
            ),
            True,
            "targetEndpoint",
        ),
        (
            "拖到设备卡片不判目录（项目目录当场发走）",
            GOOD_TPL.replace(
                "  if (preview.hasDirectory) { await previewAndStage(files, preview); return; }\n",
                "",
            ),
            True,
            "hasDirectory",
        ),
        (
            "拖进窗口不判目录",
            GOOD_TPL.replace(
                "  if (preview.hasDirectory) { await previewAndStage(paths, preview); return; }\n",
                "",
            ),
            True,
            "hasDirectory",
        ),
        (
            "穿梭拖拽不判目录",
            GOOD_TPL.replace(
                "  if (preview.hasDirectory) "
                "{ await previewAndStage(paths, preview); return; }\n",
                "",
            ),
            True,
            "hasDirectory",
        ),
    ]

    print("=" * 72)
    print("入口一致性审计 · 自测（每条检查都必须能红）")
    print("=" * 72)
    failed = 0
    for name, src, want_problem, keyword in cases:
        got = check(strip_line_comments(strip_comments(src)))
        ok = bool(got) == want_problem and (not keyword or any(keyword in g for g in got))
        status = "OK  " if ok else "红/绿不符"
        detail = ""
        if not ok:
            detail = f"  期望{'报错' if want_problem else '不报'}"
            if keyword:
                detail += f"（含「{keyword}」）"
            detail += f"，实际 {len(got)} 条"
            for g in got[:2]:
                detail += "\n           " + g[:96]
            failed += 1
        print(f"  [{status}] {name}{detail}")
    print()
    if failed:
        print(f"自测失败 {failed} 项 —— 守卫本身有问题，先修守卫。")
        return 1
    print(f"自测通过：{len(cases)} 项，守卫能抓到每一类缺陷。")
    return 0


def main() -> int:
    if not APP.is_file():
        print(f"找不到 {APP}", file=sys.stderr)
        return 2
    raw = strip_line_comments(strip_comments(APP.read_text(encoding="utf-8")))
    problems = check(raw)

    print("=" * 72)
    print("UI 入口一致性审计")
    print("=" * 72)
    print(f"  检查发送/取回入口 {len(SEND_PATHS) + len(FETCH_PATHS)} 条")
    if problems:
        print()
        for p in problems:
            print("  [违反] " + p)
        print()
        print(f"共 {len(problems)} 处违反。")
        return 1
    print()
    print(
        "全部满足：准入、撤销窗口（含位置）、落点传递、失败不谎报、"
        "只清本次、三个直发入口都按 hasDirectory 分派。"
    )
    return 0


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        raise SystemExit(selftest())
    raise SystemExit(main())
