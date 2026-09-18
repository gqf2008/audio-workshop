#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把 stdout / stderr 切成 UTF-8。

这些工具的**面向用户输出全程中文**（还夹着 ✅ 这类符号），而 Windows 上 Python 的
stdout 默认跟随 locale 编码（常见 cp1252）—— 直接 `print` 就 `UnicodeEncodeError`
崩掉。2026-09-18 三平台 CI 实测：`test_disk_boundary` 的 2 条与
`test_gen_model_capabilities` 的 1 条全栽在这上面，而在 macOS/Linux（默认 UTF-8）
一直看不出问题。

**import 本模块即生效**（副作用式）：因为这些函数也会被**进程内**调用（`tools/tests`
就是直接 import 后调 `cmd_assemble` / `cmd_synth` 的），只放在 `__main__` 里不解决问题。

用 `reconfigure` 而不是重设 `sys.stdout`：后者会把 unittest / pytest 的捕获对象换掉，
拿不到它们的输出。捕获对象没有 `reconfigure` 时静默跳过即可。
"""

import sys

for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8")
    except (AttributeError, OSError, ValueError):
        # 不是 TextIOWrapper（被测试框架包过）或流不可重配：不影响功能，跳过
        pass
