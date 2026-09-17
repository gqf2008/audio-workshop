#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""免费层 CLI 的磁盘边界：逐句 ENOSPC 可恢复 + 拼装前写前空间检查。

直接跑：`python3 tools/tests/test_disk_boundary.py`

为什么要有它：这两条以前都是「进程级抛错退出」—— 磁盘满时句子状态仍是 pending、
工程不落盘，用户看不出是哪句坏了、也不知道要腾多少空间；拼装则要一路写到临时文件
才炸，白等整段拼装。Rust 产品侧（crates/aw-core/src/dub.rs 的 write_failure_note /
StorageFull 分支）早有写前检查与统一文案，这里钉住 CLI 侧与它对口径、别再退化。
"""

import argparse
import errno
import io
import os
import shutil
import sys
import tempfile
import unittest
import wave
from pathlib import Path

TOOLS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(TOOLS))

import audio_dub as dub  # noqa: E402


def _wav_bytes(nframes=800, sr=8000, ch=1):
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(ch)
        w.setsampwidth(2)
        w.setframerate(sr)
        w.writeframes(b"\x00" * (nframes * ch * 2))
    return buf.getvalue()


def _project(d, n=2):
    os.makedirs(os.path.join(d, "sentences"), exist_ok=True)
    prj = {"script": "x.txt", "created": "t", "model": "audio8-tts", "voice_ref": None,
           "gap_ms": 250, "base_seed": 1, "sentences": []}
    for i in range(n):
        prj["sentences"].append({
            "index": i, "text": f"第{i}句。", "spoken": f"第{i}句。", "text_layer": "none",
            "seed": 1 + i, "wav": f"sentences/{i:03d}.wav",
            "duration": None, "start": None, "status": "pending"})
    dub.save_project(d, prj)
    return prj


def _project_with_done(d, n=2, nframes=8000):
    """工程 + 真实可读的逐句 wav（拼装路径要求文件在位且参数自洽）。"""
    prj = _project(d, n=n)
    raw = _wav_bytes(nframes=nframes)
    for s in prj["sentences"]:
        dub.write_atomic(os.path.join(d, s["wav"]), raw)
        s.update({"duration": nframes / 8000.0, "sample_rate": 8000,
                  "channels": 1, "status": "done"})
    dub.save_project(d, prj)
    return prj


def _synth_args(d):
    return argparse.Namespace(dir=d, only=None, redo=False, config=dub.cfgmod.CFG_DEFAULT)


class SynthEnospc(unittest.TestCase):
    """① 逐句落盘 ENOSPC：该句标 error（带 MB 估算）+ 状态落盘 + 本轮收尾。"""

    def test_enospc_marks_sentence_saves_project_and_stops_round(self):
        with tempfile.TemporaryDirectory() as d:
            _project(d, n=3)
            raw = _wav_bytes()
            calls = {"n": 0}
            real_atomic = dub.write_atomic

            def fake_synth(text, model, seed, voice_ref=None, extra=None):
                return raw, None

            def fake_atomic(path, data):
                if not path.endswith(".wav"):   # project.json 也走 write_atomic，别一起算
                    return real_atomic(path, data)
                calls["n"] += 1
                if calls["n"] == 2:             # 第 2 句落盘时磁盘满
                    raise OSError(errno.ENOSPC, "No space left on device")
                return real_atomic(path, data)

            orig_s, orig_a = dub.synth_one, dub.write_atomic
            dub.synth_one, dub.write_atomic = fake_synth, fake_atomic
            try:
                with self.assertRaises(SystemExit):
                    dub.cmd_synth(_synth_args(d))
            finally:
                dub.synth_one, dub.write_atomic = orig_s, orig_a

            prj = dub.load_project(d)            # 能从磁盘读回 → 状态确实落了盘
            st = [s["status"] for s in prj["sentences"]]
            self.assertEqual(st[0], "done", st)
            self.assertTrue(st[1].startswith("error: ENOSPC"), st[1])
            self.assertIn("需要", st[1])
            self.assertIn("MB", st[1])
            # 本轮收尾：第 3 句不再继续合成（也不该留下一个假的 error）
            self.assertEqual(st[2], "pending", st)
            # 失败那句不留半截文件
            self.assertFalse(os.path.exists(os.path.join(d, "sentences", "001.wav")))
            self.assertEqual(calls["n"], 2, "失败后不应继续落盘")

    def test_enospc_sentence_is_retried_on_next_run(self):
        """error 句仍落在「需要合成」集合里 —— 否则释放空间后根本不会重跑。"""
        with tempfile.TemporaryDirectory() as d:
            prj = _project(d, n=2)
            prj["sentences"][0]["status"] = "error: ENOSPC（需要 1.0 MB）"
            dub.save_project(d, prj)
            prj = dub.load_project(d)
            todo = [s for s in prj["sentences"] if s["status"] != "done"]
            self.assertEqual([s["index"] for s in todo], [0, 1])


class AssembleSpaceCheck(unittest.TestCase):
    """② 拼装写前检查剩余空间（不写半截成品、不破坏旧成品）。"""

    def _with_free(self, free_bytes, fn):
        orig = shutil.disk_usage
        real = shutil.disk_usage("/")
        shutil.disk_usage = lambda p: type(real)(real.total, real.used, free_bytes)
        try:
            return fn()
        finally:
            shutil.disk_usage = orig

    def test_refuses_before_writing_when_space_is_short(self):
        with tempfile.TemporaryDirectory() as d:
            _project_with_done(d, n=2)
            out = os.path.join(d, "out")

            def run():
                with self.assertRaises(SystemExit) as cm:
                    dub.cmd_assemble(argparse.Namespace(dir=d))
                return str(cm.exception)

            msg = self._with_free(1, run)        # 只剩 1 字节
            self.assertIn("磁盘空间不足", msg)
            self.assertIn("需要", msg)
            self.assertIn("可用", msg)
            self.assertNotIn("Traceback", msg)
            self.assertFalse(os.path.exists(os.path.join(out, "final.wav")))
            # 连临时文件都不该出现：检查发生在创建之前
            self.assertEqual([f for f in os.listdir(out) if ".tmp" in f], [])

    def test_old_final_survives_when_space_is_short(self):
        with tempfile.TemporaryDirectory() as d:
            _project_with_done(d, n=2)
            out = os.path.join(d, "out")
            os.makedirs(out, exist_ok=True)
            old = os.path.join(out, "final.wav")
            dub.write_atomic(old, _wav_bytes(nframes=16000))
            before = os.path.getsize(old)

            self._with_free(1, lambda: self.assertRaises(
                SystemExit, dub.cmd_assemble, argparse.Namespace(dir=d)))

            self.assertTrue(os.path.exists(old), "旧成品必须还在")
            self.assertEqual(os.path.getsize(old), before, "旧成品必须没被改动")

    def test_enospc_during_write_cleans_temp_file(self):
        with tempfile.TemporaryDirectory() as d:
            _project_with_done(d, n=2)
            out = os.path.join(d, "out")
            orig = os.fsync

            def boom(fd):
                raise OSError(errno.ENOSPC, "No space left on device")

            os.fsync = boom
            try:
                with self.assertRaises(SystemExit) as cm:
                    dub.cmd_assemble(argparse.Namespace(dir=d))
            finally:
                os.fsync = orig

            self.assertIn("磁盘空间不足", str(cm.exception))
            self.assertFalse(os.path.exists(os.path.join(out, "final.wav")))
            leftovers = [f for f in os.listdir(out) if ".tmp" in f]
            self.assertEqual(leftovers, [], f"临时文件没清干净: {leftovers}")


class AtomicWriteTemp(unittest.TestCase):
    """`write_atomic` 失败必须清临时文件（与 Rust 侧 write_atomic / copy_atomic 同一不变式）。"""

    def test_cleans_temp_when_replace_fails(self):
        with tempfile.TemporaryDirectory() as d:
            target = os.path.join(d, "001.wav")
            os.makedirs(target)              # 目标是目录 → os.replace(file → dir) 必然失败
            with self.assertRaises(OSError):
                dub.write_atomic(target, b"payload")
            leftovers = [f for f in os.listdir(d) if ".tmp" in f]
            self.assertEqual(leftovers, [], f"临时文件没清干净: {leftovers}")
            self.assertEqual(os.listdir(target), [], "失败不该把内容塞进目标目录")

    def test_success_path_still_writes_and_leaves_no_temp(self):
        with tempfile.TemporaryDirectory() as d:
            target = os.path.join(d, "ok.wav")
            dub.write_atomic(target, b"payload")
            with open(target, "rb") as f:
                self.assertEqual(f.read(), b"payload")
            self.assertEqual([f for f in os.listdir(d) if ".tmp" in f], [])


class AssembleHappyPath(unittest.TestCase):
    """③ 空间充足时正常拼装（新增检查不能把正常路径挡住）。"""

    def test_assembles_when_space_is_available(self):
        with tempfile.TemporaryDirectory() as d:
            _project_with_done(d, n=2)
            dub.cmd_assemble(argparse.Namespace(dir=d))
            final = os.path.join(d, "out", "final.wav")
            self.assertTrue(os.path.exists(final))
            with wave.open(final, "rb") as w:      # 2×1.0s + 1×0.25s 句间静音
                self.assertEqual(w.getnchannels(), 1)
                self.assertEqual(w.getframerate(), 8000)
                self.assertAlmostEqual(w.getnframes() / 8000.0, 2.25, places=2)
            self.assertTrue(os.path.exists(os.path.join(d, "out", "final.srt")))
            self.assertEqual([f for f in os.listdir(os.path.join(d, "out")) if ".tmp" in f], [])


class FailureNoteWording(unittest.TestCase):
    """文案口径：动作为先、路径殿后（界面/终端截断时丢路径不丢动作）。"""

    def test_enospc_note_has_action_first_and_path_last(self):
        note = dub.write_failure_note("/x/y.wav", 3 * 1048576,
                                      OSError(errno.ENOSPC, "nope"))
        self.assertTrue(note.startswith("磁盘空间不足（需要 3.0 MB）"), note)
        self.assertTrue(note.endswith("路径：/x/y.wav"), note)
        self.assertIn("请释放空间后重跑", note)

    def test_permission_and_missing_path_are_distinguished(self):
        self.assertIn("没有写入权限", dub.write_failure_note("/x", 0, OSError(errno.EACCES, "x")))
        self.assertIn("路径不存在", dub.write_failure_note("/x", 0, OSError(errno.ENOENT, "x")))

    def test_is_enospc_only_matches_enospc(self):
        self.assertTrue(dub.is_enospc(OSError(errno.ENOSPC, "x")))
        self.assertFalse(dub.is_enospc(OSError(errno.EACCES, "x")))


if __name__ == "__main__":
    unittest.main()
