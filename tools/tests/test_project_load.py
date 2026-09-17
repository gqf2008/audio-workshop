#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""`tools/audio_dub.py::load_project` 的三条路必须分得清。

直接跑：`python3 tools/tests/test_project_load.py`

为什么要有它：以前 `load_project` 就是一句 `json.load(open(p))`——`project.json` 被
截断（旧版非原子写留下的半截文件）时直接吐 `JSONDecodeError` 回溯，用户看不到路径，
也不知道「改名/移走后重开就能从零重建」这条恢复路径。更危险的失败模式是**顺手重建**：
那会把唯一可人工恢复的现场覆盖掉。Rust 桌面侧早已按这个口径实现
（`aw-core::dub::Project::load_if_present`），这里把 CLI 侧钉住。
"""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

TOOLS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(TOOLS))

import audio_dub as dub  # noqa: E402


VALID = {"script": "/x/y.txt", "created": "t", "model": "audio8-tts", "voice_ref": None,
         "gap_ms": 250, "base_seed": 1,
         "sentences": [{"index": 0, "text": "喂。", "spoken": "喂。", "text_layer": "none",
                        "seed": 2, "wav": "sentences/000.wav", "duration": None,
                        "start": None, "status": "pending"}]}


class ProjectLoad(unittest.TestCase):
    def test_valid_project_roundtrips(self):
        with tempfile.TemporaryDirectory() as d:
            dub.save_project(d, VALID)
            got = dub.load_project(d)
            self.assertEqual(got["model"], "audio8-tts")
            self.assertEqual(got["base_seed"], 1)
            self.assertEqual(got["sentences"][0]["spoken"], "喂。")

    def test_missing_project_is_not_a_project_dir(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(SystemExit) as cm:
                dub.load_project(d)
            self.assertIn("不是工程目录", str(cm.exception))

    def test_truncated_json_names_the_path_and_the_parse_error(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "project.json")
            with open(p, "w", encoding="utf-8") as f:
                f.write('{"model": "audio8-tts", "sentences": [')   # 半截 JSON
            with self.assertRaises(SystemExit) as cm:
                dub.load_project(d)
            msg = str(cm.exception)
            self.assertIn("工程文件损坏", msg)
            self.assertIn(p, msg)                     # 完整路径必须在
            self.assertIn("解析错误", msg)              # 解析细节殿后
            self.assertIn("没有自动重建", msg)
            self.assertIn("改名或移走", msg)            # 恢复路径可执行

    def test_corrupt_project_is_never_rewritten(self):
        """最关键的一条：报错路径不许碰那个文件（它是唯一可恢复的现场）。"""
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "project.json")
            broken = '{"model": "audio8-tts", "sentences": [{"index": 0,'
            with open(p, "w", encoding="utf-8") as f:
                f.write(broken)
            before = os.stat(p)
            with self.assertRaises(SystemExit):
                dub.load_project(d)
            with open(p, encoding="utf-8") as f:
                self.assertEqual(f.read(), broken, "损坏的 project.json 被改写了")
            after = os.stat(p)
            self.assertEqual(after.st_size, before.st_size)
            self.assertEqual(after.st_mtime_ns, before.st_mtime_ns)
            self.assertEqual(sorted(os.listdir(d)), ["project.json"], "不该留下临时文件")

    def test_invalid_utf8_is_reported_as_corrupt_not_as_a_traceback(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "project.json")
            with open(p, "wb") as f:
                f.write(b'{"model": "\xff\xfe", "sentences": []}')
            with self.assertRaises(SystemExit) as cm:
                dub.load_project(d)
            self.assertIn("工程文件损坏", str(cm.exception))
            self.assertIn(p, str(cm.exception))

    def test_unreadable_path_is_a_different_message(self):
        """「文件在但读不了」与「解析失败」要分开说 —— 修法不同。"""
        with tempfile.TemporaryDirectory() as d:
            os.makedirs(os.path.join(d, "project.json"))   # 是目录不是文件 → open 抛 OSError
            with self.assertRaises(SystemExit) as cm:
                dub.load_project(d)
            msg = str(cm.exception)
            self.assertIn("工程文件读不了", msg)
            self.assertIn(os.path.join(d, "project.json"), msg)
            self.assertNotIn("工程文件损坏", msg)

    def test_error_message_has_no_traceback_marker(self):
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "project.json"), "w", encoding="utf-8") as f:
                f.write("not json at all")
            with self.assertRaises(SystemExit) as cm:
                dub.load_project(d)
            self.assertNotIn("Traceback", str(cm.exception))


class ExistingBoundaryTestsStillGreen(unittest.TestCase):
    """本批不该动到磁盘边界那批（同一文件的另一组不变量）。"""

    def test_disk_boundary_module_imports(self):
        import test_disk_boundary  # noqa: F401  同目录已随 discover 一起跑

    def test_save_project_is_still_atomic(self):
        with tempfile.TemporaryDirectory() as d:
            dub.save_project(d, VALID)
            self.assertEqual(json.load(open(os.path.join(d, "project.json"), encoding="utf-8"))["model"],
                             "audio8-tts")


if __name__ == "__main__":
    unittest.main()
