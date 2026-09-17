#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""`tools/gen_model_capabilities.py` 的生成物测试。

直接跑：`python3 tools/tests/test_gen_model_capabilities.py`

为什么要有它：随包能力清单是 App 的**兜底来源** —— 它一旦和 schema 漂了，界面上
「该引擎必须提供参考音频」「已知问题」这些提示就会**静默消失**（不报错、也不提示），
正是本批要消灭的失败形态。所以钉三件事：

1. 投影**没有漏键**：`to_server()` 里除结构性键外出现的键，必须都在能力清单里
   （schema 将来多一条能力字段、而清单没跟上 → 红）；
2. 缺省**物化**且带全 5 个键（App 侧按字段回落，靠的就是"清单是一条完整声明"）；
3. 同输入两次渲染**逐字节相同** + 盘上的文件与当前 schema 一致（= `--check`）。
"""

import json
import subprocess
import sys
import unittest
from pathlib import Path

TOOLS = Path(__file__).resolve().parent.parent
REPO_ROOT = TOOLS.parent
sys.path.insert(0, str(TOOLS))

import audio_config  # noqa: E402
import gen_model_capabilities as gen  # noqa: E402

CATALOG_PATH = REPO_ROOT / "config" / "model-capabilities.json"


class ProjectionTest(unittest.TestCase):
    def setUp(self):
        self.cfg = audio_config.load()
        self.server = audio_config.to_server(self.cfg)["models"]
        self.catalog = json.loads(CATALOG_PATH.read_text(encoding="utf-8"))

    def test_no_capability_key_is_dropped_from_the_projection(self):
        """`to_server()` 里除结构性键外的键，必须都被能力清单收下。

        阳性对照：往 schema 的某个模型里加一条 `deprecated: true`（`to_server` 会透传）
        → 本用例红（清单没跟上，App 永远看不到它）。
        """
        for entry in self.server:
            extra = set(entry) - set(gen.STRUCTURAL_KEYS) - set(gen.CAPABILITY_KEYS)
            self.assertFalse(
                extra,
                f"{entry['id']} 的能力键 {sorted(extra)} 没进随包清单："
                "schema→server 多了一条能力字段而 gen_model_capabilities 没跟上（会静默丢失）",
            )

    def test_every_model_carries_all_five_keys_materialised(self):
        """每条记录都要带全 5 个键，且缺省是物化过的具体值（不是"没写就猜"）。"""
        self.assertEqual(
            [m["id"] for m in self.catalog["models"]],
            [m["id"] for m in self.server],
            "模型顺序必须沿用 schema（= server.json 的顺序：默认引擎取第一个不要参考音的）",
        )
        for m in self.catalog["models"]:
            self.assertEqual(
                set(m), {"id", *gen.CAPABILITY_KEYS},
                f"{m['id']} 的键集不对",
            )
            self.assertIsInstance(m["product_excluded"], bool)
            self.assertIsInstance(m["mode"], str)
            self.assertIsInstance(m["role"], str)
            self.assertIsInstance(m["known_issues"], list)
            if m["requires"] is not None:
                self.assertEqual(
                    set(m["requires"]), {"voice_ref"}, f"{m['id']} 的 requires 只有 voice_ref 一项"
                )

    def test_defaults_match_what_the_server_would_omit(self):
        """缺省物化的值 == 服务端"没写这个键"时的语义（否则兜底会引入新语义）。

        这里比的是**当场生成**的结果（不是盘上那份文件）—— 盘上那份由
        `test_checked_in_file_is_up_to_date` 负责，两条各管一件事。
        """
        for entry, cat in zip(self.server, gen.build(self.cfg)["models"]):
            self.assertEqual(
                cat["mode"], entry.get("mode", "offline"), f"{entry['id']} 的 mode 兜底口径不符"
            )
            self.assertEqual(
                cat["product_excluded"], bool(entry.get("product_excluded", False))
            )
            self.assertEqual(cat["role"], entry.get("role", ""))
            self.assertEqual(cat["requires"], entry.get("requires"))
            self.assertEqual(cat["known_issues"], entry.get("known_issues", []))

    def test_product_decisions_are_actually_in_the_catalog(self):
        """产品决策不能只活在注释里：被排除的模型、硬要求、流式专用都要在清单里。"""
        by_id = {m["id"]: m for m in self.catalog["models"]}
        self.assertTrue(by_id["audio8-tts-01b"]["product_excluded"], "0.1b 必须随包标成已排除")
        self.assertTrue(
            by_id["index-tts2"]["requires"] and by_id["index-tts2"]["requires"]["voice_ref"],
            "index-tts2 必须随包带上「硬要求参考音」",
        )
        self.assertEqual(by_id["audio8-tts-stream"]["mode"], "streaming", "流式专用必须随包标出")

    def test_render_is_byte_stable(self):
        """同一份 schema 渲染两次逐字节相同（否则 `--check` 会天天喊 stale）。"""
        first = gen.render(gen.build(audio_config.load()))
        second = gen.render(gen.build(audio_config.load()))
        self.assertEqual(first, second)

    def test_checked_in_file_is_up_to_date(self):
        """盘上的清单必须等于当前 schema 的投影（= `--check` 的判据）。"""
        expected = gen.render(gen.build(audio_config.load()))
        self.assertEqual(
            CATALOG_PATH.read_text(encoding="utf-8"),
            expected,
            "config/model-capabilities.json 与 config/models.schema.yaml 不一致，"
            "请重跑 python3 tools/gen_model_capabilities.py",
        )

    def test_check_mode_exits_zero_on_the_committed_file(self):
        """`--check` 是对外承诺的那条命令，必须真的能用（`RULE_规则可执行性`）。"""
        proc = subprocess.run(
            [sys.executable, str(TOOLS / "gen_model_capabilities.py"), "--check"],
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("OK", proc.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
