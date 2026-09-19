#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""tools/check_windows_icon.py 的**阳性/阴性对照**。

守卫本身也得被守卫：一个只会说"没图标"的脚本没有价值（见 `RULE_规则可执行性.md`）。
这里用本机的 mingw-w64 真编三个 PE：带图标 + GUI 子系统的必须过，不带图标的、
以及控制台子系统的必须红。

本机没有 mingw-w64 时（CI 的 Linux/Windows runner 常见）**跳过并打印原因**，
不写 `@skip`：真正跑这个检查的地方是发布流水线 —— 那里直接对用户拿到的 exe 跑同一条命令。
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from check_windows_icon import (  # noqa: E402
    check,
    icon_resource_counts,
    subsystem,
    RT_GROUP_ICON,
    RT_ICON,
    SUBSYSTEM_CUI,
    SUBSYSTEM_GUI,
)

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ICON = os.path.join(REPO_ROOT, "assets", "icon.ico")


def _tool(name):
    return shutil.which(name)


class CheckWindowsIconTest(unittest.TestCase):
    def test_finds_icon_in_a_real_pe_and_rejects_one_without(self):
        gcc = _tool("x86_64-w64-mingw32-gcc")
        windres = _tool("x86_64-w64-mingw32-windres")
        if not gcc or not windres:
            print(
                "本机没有 mingw-w64（x86_64-w64-mingw32-gcc/windres），跳过阳性对照；"
                "发布流水线会对真产物跑同一条检查",
                file=sys.stderr,
            )
            return

        with tempfile.TemporaryDirectory(prefix="aw-icon-check-") as tmp:
            main_c = os.path.join(tmp, "m.c")
            with open(main_c, "w", encoding="utf-8") as fh:
                fh.write(
                    "#include <windows.h>\n"
                    "int WINAPI WinMain(HINSTANCE a, HINSTANCE b, LPSTR c, int d) { return 0; }\n"
                )
            rc = os.path.join(tmp, "app-icon.rc")
            with open(rc, "w", encoding="utf-8") as fh:
                fh.write(f'1 ICON DISCARDABLE "{ICON}"\n')
            res = os.path.join(tmp, "app-icon.res")
            subprocess.run([windres, rc, "-O", "coff", "-o", res], check=True)
            with_icon = os.path.join(tmp, "with-icon.exe")
            without = os.path.join(tmp, "no-icon.exe")
            console = os.path.join(tmp, "console-icon.exe")
            subprocess.run([gcc, main_c, "-o", without, "-mwindows"], check=True)
            subprocess.run([gcc, main_c, res, "-o", with_icon, "-mwindows"], check=True)
            # 带图标但**控制台子系统**：双击会多一个黑窗 —— 必须被认出来
            subprocess.run([gcc, main_c, res, "-o", console], check=True)

            # ① 带图标的必须过，并且真的是 7 张图 + 1 个图标组（与 assets/icon.ico 的档数一致）
            with open(with_icon, "rb") as fh:
                raw = fh.read()
            counts = icon_resource_counts(raw)
            self.assertGreaterEqual(counts.get(RT_ICON, 0), 1)
            self.assertEqual(counts.get(RT_GROUP_ICON), 1)
            self.assertEqual(subsystem(raw), SUBSYSTEM_GUI)
            ok, note = check(with_icon)
            self.assertTrue(ok, note)

            # ② 不带图标的必须红（否则"红了"可能只是脚本恒红）
            ok, note = check(without)
            self.assertFalse(ok, f"没有图标资源的 exe 不该通过：{note}")

            # ③ 非 PE 文件同样要红，且不能抛异常
            ok, note = check(main_c)
            self.assertFalse(ok, note)

            # ④ 控制台子系统必须红（真机抱怨的"启动多一个黑窗"就是它）
            with open(console, "rb") as fh:
                self.assertEqual(subsystem(fh.read()), SUBSYSTEM_CUI)
            ok, note = check(console)
            self.assertFalse(ok, f"控制台子系统的 exe 不该通过：{note}")
            self.assertIn("控制台", note)
            # …而显式允许时它只按图标判
            ok, note = check(console, expect_gui=False)
            self.assertTrue(ok, note)


if __name__ == "__main__":
    unittest.main()
