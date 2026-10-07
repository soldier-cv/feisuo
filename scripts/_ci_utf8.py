"""让脚本在**任何** stdout 编码下都能打印中文。

## 为什么需要它

这些检查脚本的输出全是中文（给读 CI 日志的人看）。而 Python 在 Windows 上
默认用**控制台代码页**编码 stdout：

- 中文 Windows：cp936（GBK）—— 中文能打出来，没问题；
- **英文 Windows / GitHub 的 `windows-latest` runner：cp1252** ——
  `print("对比度检查")` 直接抛 `UnicodeEncodeError`，脚本以退出码 1 结束。

后果是：CI 里 5 个检查步骤全部变红，而**代码完全正确**。
这类"检查自己把 CI 弄红"的问题最难查 —— 日志里只有一个编码异常，
看不出跟被检查的代码有没有关系。

实测复现（本机强制 `PYTHONIOENCODING=cp1252`）：

    File ".../cp1252.py", line 19, in encode
        return codecs.charmap_encode(input,self.errors,encoding_table)[0]
    UnicodeEncodeError: 'charmap' codec can't encode character

## 为什么修在这里而不是只改 workflow

在 workflow 里设 `PYTHONUTF8=1` 只覆盖 CI。同一批脚本还要能在

- 开发机上手动跑（中文 Windows 没事，换一台英文系统就炸）；
- 别的机器 / 别的 CI 上跑。

所以**每个脚本自己保证**是唯一可靠的做法。`PYTHONUTF8` 可以作为
belt-and-braces 再加一道，但不能代替它。

## 用法

在脚本 `main()` 之前（或紧跟 import 之后）加一行：

    from _ci_utf8 import force_utf8_stdout
    force_utf8_stdout()

## 为什么不直接 `sys.stdout.reconfigure(encoding="utf-8")`

那样也行，但有两个坑：

1. 输出会变成 UTF-8 字节，而**读日志的工具**未必按 UTF-8 解码
   （Windows 控制台默认按 cp1252/cp936 显示，会变成乱码）；
2. GitHub 的 Actions 日志页面是按 UTF-8 渲染的，所以那里其实是对的 ——
   但本地终端就不一定了。

用 `errors="replace"` 的包装能同时躲开"编码失败"和"改坏终端"：
编码不了的中文退化成 `?`，**检查结果本身（退出码）不受影响**。
真正需要无损的场合（写文件）请显式传 `encoding="utf-8"`，本模块不管。
"""

from __future__ import annotations

import io
import sys
from typing import TextIO


def force_utf8_stdout() -> None:
    """把 stdout/stderr 换成"永不因编码失败"的 UTF-8 包装。

    幂等：重复调用只生效一次（避免把已包装的再包一层，
    那会让 ``sys.stdout.encoding`` 的报告值失真）。
    """
    for name in ("stdout", "stderr"):
        stream = getattr(sys, name, None)
        if stream is None:
            continue
        # 已经是我们装的，或本来就是 UTF-8 → 什么都不用做。
        enc = (getattr(stream, "encoding", "") or "").lower().replace("-", "")
        if enc in ("utf8", "utf"):
            continue
        buffer = getattr(stream, "buffer", None)
        if buffer is None:
            # 已经被别人重定向到别的东西（例如 pytest 捕获），别乱动。
            continue
        wrapper = io.TextIOWrapper(
            buffer,
            encoding="utf-8",
            errors="replace",
            line_buffering=True,
        )
        setattr(sys, name, wrapper)


def main() -> None:  # 便于 `python _ci_utf8.py` 自检
    before = sys.stdout.encoding
    force_utf8_stdout()
    after = sys.stdout.encoding
    print(f"stdout 编码: {before} -> {after}")
    print("中文输出正常: 常驻前景/底色对比度检查")

    # 退化路径自检：绕开 force_utf8_stdout，直接拿一个装不下中文的编码
    # 打印中文 —— 加固后的写法必须**不抛**。
    #
    # ⚠️ 这里不能对 sys.stdout 调 `reconfigure` 来模拟：reconfigure 会
    # **丢掉**外层的包装、把裸流换回去，于是后续 print 打到裸流上炸掉
    # （第一版就是这么写的，自检自己先崩了）。
    # 正确做法是在一个独立的包装上验证。
    import io as _io

    hostile = _io.TextIOWrapper(
        _io.BytesIO(), encoding="ascii", errors="strict"
    )
    try:
        hostile.write("中文")
    except UnicodeEncodeError:
        print("自检: 裸 ascii 流确实会抛 UnicodeEncodeError（符合预期）")
    # 而加固后的包装用 errors="replace"，同样内容不会抛：
    safe = _io.TextIOWrapper(
        _io.BytesIO(), encoding="ascii", errors="replace"
    )
    try:
        safe.write("中文")
        print("自检: errors='replace' 的包装不会抛 —— 加固有效")
    except UnicodeEncodeError as e:
        print(f"自检失败: 加固后的包装仍会抛 {e}")


if __name__ == "__main__":
    main()
