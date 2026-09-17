#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""`tools/gen_model_downloads.py` 的体积解析测试。

直接跑：`python3 tools/tests/test_gen_model_downloads.py`

为什么要有它：取体积这件事有一条**会静默全错**的路 —— HF 的 `/resolve/` 先回 302，
那一跳的 `content-length` 是**跳转响应体**的长度（实测 1038 B）。照着"HEAD 一下取
Content-Length"写，每个模型的体积都会变成 1 KB 左右、看起来还挺正常。所以这里用**真
的本地 HTTP 服务器**（含真跳转）来钉住：只认最终 2xx 那跳的 `Content-Length`。
"""

import http.server
import os
import socket
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

TOOLS = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(TOOLS))

import gen_model_downloads as gen  # noqa: E402

# 302 那一跳的响应体长度（HF 实测 1038 B）——就是为了让"拿错那一跳"必定算错而设的
REDIRECT_BODY = b"x" * 1038
REAL_FILE = b"y" * 4096


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_HEAD(self):  # noqa: N802 —— http.server 的约定命名
        route = self.path.split("?")[0]
        if route == "/file.gguf":
            self.send_response(200)
            self.send_header("Content-Length", str(len(REAL_FILE)))
            self.end_headers()
        elif route == "/redirect":
            # 跳转那一跳**带** content-length（跳转响应体），最终落点也有
            self.send_response(302)
            self.send_header("Content-Length", str(len(REDIRECT_BODY)))
            self.send_header("Location", "/file.gguf")
            self.end_headers()
        elif route == "/linked":
            # 跳转带 x-linked-size，但最终落点**没有** Content-Length
            self.send_response(302)
            self.send_header("Content-Length", str(len(REDIRECT_BODY)))
            self.send_header("x-linked-size", "123456789")
            self.send_header("Location", "/no-length")
            self.end_headers()
        elif route == "/no-length":
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
        elif route == "/missing":
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
        elif route == "/unauthorized":
            self.send_response(401)
            self.send_header("Content-Length", "0")
            self.end_headers()
        elif route == "/slow":
            time.sleep(2.0)  # 比客户端 timeout 长
            self.send_response(200)
            self.send_header("Content-Length", "1")
            self.end_headers()
        else:
            self.send_response(500)
            self.send_header("Content-Length", "0")
            self.end_headers()

    def log_message(self, *args):  # 测试输出保持干净
        pass


class HeadSizeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        cls.port = cls.server.server_address[1]
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()

    def url(self, route):
        return f"http://127.0.0.1:{self.port}{route}"

    def test_content_length_is_taken_from_the_file(self):
        size, how = gen.head_content_length(self.url("/file.gguf"))
        self.assertEqual((size, how), (len(REAL_FILE), "content-length"))

    def test_redirect_uses_the_final_hop_not_the_redirect_body(self):
        """阳性对照：把 `head_content_length` 改成"不跟跳转、直接读第一跳" → 本条红
        （会得到 1038，也就是跳转响应体的长度）。"""
        size, how = gen.head_content_length(self.url("/redirect"))
        self.assertEqual(size, len(REAL_FILE))
        self.assertNotEqual(size, len(REDIRECT_BODY), "拿到的是跳转响应体的长度")
        # 而且**必须**跟了跳转才可能拿到这个数
        self.assertEqual(how, "content-length")

    def test_redirect_falls_back_to_x_linked_size(self):
        size, how = gen.head_content_length(self.url("/linked"))
        self.assertEqual((size, how), (123456789, "x-linked-size"))

    def test_missing_content_length_is_null_not_zero(self):
        size, how = gen.head_content_length(self.url("/no-length"))
        self.assertIsNone(size, "没有 Content-Length 必须是 None，不能填 0")
        self.assertIn("Content-Length", how)

    def test_404_and_401_report_the_status(self):
        size, how = gen.head_content_length(self.url("/missing"))
        self.assertIsNone(size)
        self.assertEqual(how, "HTTP 404")
        size, how = gen.head_content_length(self.url("/unauthorized"))
        self.assertIsNone(size)
        self.assertEqual(how, "HTTP 401")

    def test_timeout_is_reported_not_swallowed(self):
        size, how = gen.head_content_length(self.url("/slow"), timeout=0.3)
        self.assertIsNone(size)
        self.assertIn("timeout", how.lower())
        self.assertNotEqual(how, "没有 Content-Length", "超时不能被说成没有长度")


class PureHelperTests(unittest.TestCase):
    def test_package_bytes_is_null_when_any_file_is_unknown(self):
        self.assertEqual(gen.package_bytes([{"bytes": 1}, {"bytes": 2}]), 3)
        self.assertIsNone(gen.package_bytes([{"bytes": 1}, {"bytes": None}]))
        self.assertIsNone(gen.package_bytes([{"bytes": 1}, {}]))
        self.assertIsNone(gen.package_bytes([]))

    def test_aux_local_path_resolution(self):
        # ${models_root}/ 前缀 → 去掉前缀
        self.assertEqual(
            gen.aux_local_path("${models_root}/A-GGUF/a.gguf", "M-GGUF"),
            "A-GGUF/a.gguf",
        )
        # 绝对路径 / 其它占位符 / 向上跳 → 生成期推不出，如实 None
        self.assertIsNone(gen.aux_local_path("/abs/a.gguf", "M-GGUF"))
        self.assertIsNone(gen.aux_local_path("${other_root}/a.gguf", "M-GGUF"))
        self.assertIsNone(gen.aux_local_path("../a.gguf", "M-GGUF"))
        self.assertIsNone(gen.aux_local_path("", "M-GGUF"))
        # 相对路径：model_path 指目录（无扩展名）→ 落在该目录内
        self.assertEqual(gen.aux_local_path("vae.gguf", "Yue2-3B-GGUF"), "Yue2-3B-GGUF/vae.gguf")
        # model_path 指文件 → 落在它的同级
        self.assertEqual(
            gen.aux_local_path("vae.gguf", "Dir/model.gguf"), "Dir/vae.gguf"
        )

    def test_read_existing_sizes_covers_packages_and_aux_files(self):
        payload = {
            "models": [
                {
                    "packages": [
                        {"files": [{"url": "u1", "bytes": 11}, {"url": "u2", "bytes": None}]}
                    ],
                    "aux_files": [{"url": "u3", "bytes": 33}],
                }
            ]
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "c.json"
            path.write_text(gen.render(payload), encoding="utf-8")
            sizes = gen.read_existing_sizes(path)
        self.assertEqual(sizes, {"u1": 11, "u3": 33})
        # 文件不存在 / 坏 JSON 都不能抛
        self.assertEqual(gen.read_existing_sizes(Path("/nonexistent/x.json")), {})


class OfflineRoundTripTests(unittest.TestCase):
    """离线重生成必须**沿用盘上已有的体积**（否则每次离线跑都会把体积抹掉）。"""

    @classmethod
    def setUpClass(cls):
        try:
            upstream = gen.find_upstream()
        except SystemExit:
            upstream = None
        if not upstream or not (Path(upstream) / "model_specs").is_dir():
            raise unittest.SkipTest("本机没有上游 audio.cpp checkout，跳过端到端用例")

    def test_offline_regen_keeps_existing_sizes_byte_for_byte(self):
        committed = gen.OUT_PATH
        if not committed.is_file():
            self.skipTest("没有已提交的清单可对照")
        with tempfile.TemporaryDirectory() as tmp:
            copy = Path(tmp) / "model-downloads.json"
            copy.write_bytes(committed.read_bytes())
            specs = gen.load_specs(Path(gen.find_upstream()) / "model_specs")
            sizes = gen.read_existing_sizes(copy)
            text = gen.render(gen.build(gen.load_schema(), specs, sizes.get))
            self.assertEqual(
                text,
                committed.read_text(encoding="utf-8"),
                "离线（沿用已有体积）重生成的产物应与盘上逐字节一致",
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
