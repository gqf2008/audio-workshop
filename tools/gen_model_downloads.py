#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""从上游 audio.cpp 的 model_specs 生成**随应用分发**的模型下载清单。

## 它解决什么
P7 的下载队列（串行 / 断点续传 / 校验后提交）已经能跑，但真机上它是个**死功能**：
下载入口只在服务清单的模型带 `url` 时才出现，而本机 `server.json` 里 13 个模型
**一个都没有 url**，于是「下载」按钮一个都不出现——机制有了、没有数据源。

上游 `model_specs/*.json` 才是下载链接的 source of truth（CHARTER §6）。本脚本把它
投影成 `config/model-downloads.json`，由应用 `include_str!` 打进二进制。

## 口径：为什么不照抄 `model_fetch.py` 的「family + 上游默认包」
`tools/model_fetch.py` 的映射是「产品 id → family → 上游 `install <family>`」，让上游按
`packages[].default` 选包。这条口径在本产品登记的模型上会**取错权重**（对着 spec 实测）：

| 产品 id | family | 上游 default 包 | 产品 path 指向 | 只按 family 会下到 |
|---|---|---|---|---|
| audio8-tts | audio8_tts | 0.6B q8_0 | 0.6B | ✅ 一致 |
| index-tts2 | index_tts2 | **2.0** q8_0 | **2.5** | ❌ 2.0 |
| qwen3-asr | qwen3_asr | **1.7B** q8_0 | **0.6B** | ❌ 1.7B |
| stable-audio-small-music | stable_audio | **medium** q8_0 | **small-music** | ❌ medium |

（其余 family 的 default 恰好与产品一致，所以这条错法只在个别模型上暴露——
正是"只按 family 映射"最危险的样子。）

所以本脚本改用产品自己的 ground truth：`config/models.schema.yaml` 里每个模型的
`path`（与 `server.json` 的 `path` 同源）。**映射规则 = 按落点匹配**：

    schema 的 path 去掉 `${models_root}/` 之后那一段
        == 包的 target_directory + （files[i] 去掉 strip_prefix）

- 相等 → 这个包就是该模型的下载源。能区分 0.1B/0.6B、2.0/2.5、medium/small-music。
- schema path 正好等于包的 `target_directory`（gen 类模型 path 指目录）→ 目录匹配；
  候选多于一个时按 `precision_preference` → `q8_0` 的顺序挑，挑的结果写进 note。
- 匹配不上（`yue2` / `sheetsage2` 上游根本没有 spec；`audio8-tts-01b` 的 0.1B
  上游没有对应包）→ `status = "no-source"`，**不猜 URL**。
- 包没有可用下载源（`kind == "unsupported"`，如 `audio8-asr` 是 CC-BY-NC-4.0 需本地转换）
  → `no-source`，并把上游给的原因原样带上。

`strip_prefix` 这条与上游 `tools/model_manager_v2.py::stripped_path` 逐字一致——
它就是上游决定"文件落在 target_directory 下哪一层"的那条规则（本机磁盘布局已核对）。

## 稳定输出
- **不联网**：`size` / `sha256` 一律留空（下载器退化成按响应 Content-Length 校验）。
  生成必须离线可复现，否则同一份输入每次跑出的字节都不一样。
- 键序固定（`sort_keys`）+ 固定缩进 + 行尾换行 → 同输入两次运行逐字节相同。
- `--check` 只重算不写盘，与盘上的文件比对；不一致退出 1（发现"改了 spec 忘了重生成"）。

用法:
  python3 tools/gen_model_downloads.py            # 生成 config/model-downloads.json
  python3 tools/gen_model_downloads.py --check    # 校验已提交的清单是否与上游一致
  AUDIOCPP_DIR=/path/to/audio.cpp python3 tools/gen_model_downloads.py
"""

import argparse
import json
import sys
from pathlib import Path
from urllib.parse import quote

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent
sys.path.insert(0, str(HERE))

# 复用「上游 checkout 在哪」的唯一口径（环境变量 AUDIOCPP_DIR 优先，其次 <repo>/../audio.cpp）
from model_fetch import find_upstream  # noqa: E402

SCHEMA_PATH = REPO_ROOT / "config" / "models.schema.yaml"
OUT_PATH = REPO_ROOT / "config" / "model-downloads.json"

HF_ENDPOINT = "https://huggingface.co"
MS_ENDPOINT = "https://www.modelscope.cn"
DOWNLOADABLE_KINDS = ("huggingface_snapshot", "modelscope_snapshot")
# 同目录多个量化档、且产品没声明 precision_preference 时的兜底顺序：
# 先按 q8_0（体积/质量折中，本产品在 audio8-tts 上也首选它），再按包 id 定序（确定性）。
FALLBACK_PRECISION = "q8_0"


def load_schema(path=SCHEMA_PATH):
    try:
        import yaml
    except ImportError:  # 与 model_fetch.py 同一兜底
        sys.exit("需要 PyYAML：pip3 install pyyaml")
    with open(path, encoding="utf-8") as f:
        return yaml.safe_load(f)


def load_specs(specs_dir: Path) -> dict:
    """family → spec。重复 family 直接报错，不静默取一个。"""
    by_family = {}
    for path in sorted(specs_dir.glob("*.json")):
        spec = json.loads(path.read_text(encoding="utf-8"))
        family = spec.get("family") or path.stem
        if family in by_family:
            raise SystemExit(
                f"model_specs 里 family 重复：{path.name} 与 {by_family[family]['_spec_file']} 都是 {family}"
            )
        spec["_spec_file"] = path.name
        by_family[family] = spec
    if not by_family:
        raise SystemExit(f"{specs_dir} 里没有 *.json")
    return by_family


def merged_download(spec: dict, package: dict) -> dict:
    """包自己的 download 覆盖 package_defaults.download（与上游 merged_download 同序）。"""
    merged = dict(spec.get("package_defaults", {}).get("download") or {})
    merged.update(package.get("download") or {})
    return merged


def stripped_local(remote: str, strip_prefix: str):
    """上游 stripped_path 的等价物：strip_prefix 之后的相对路径；前缀对不上返回 None。"""
    prefix = (strip_prefix or "").rstrip("/")
    if not prefix:
        return remote
    if remote == prefix or not remote.startswith(prefix + "/"):
        return None
    return remote[len(prefix) + 1 :]


def package_local_paths(package: dict) -> list:
    """这个包会把文件放到模型目录下的哪些相对路径（上游 target_directory + stripped_path）。"""
    target = str(package.get("target_directory", "")).strip("/")
    out = []
    for remote in package.get("files") or []:
        rel = stripped_local(
            remote, package.get("strip_prefix") or ""
        )
        if rel:
            out.append(f"{target}/{rel}")
    return out


def package_revision(download: dict) -> str:
    rev = str(download.get("revision") or "").strip()
    if rev:
        return rev
    return "master" if download.get("kind") == "modelscope_snapshot" else "main"


def download_url(download: dict, remote: str):
    """直链：与上游 `hf_url` / `ms_url` 的形状一致（含逐段 quote）。"""
    kind = download.get("kind")
    repo = str(download.get("repo") or "").strip()
    if not repo or kind not in DOWNLOADABLE_KINDS:
        return None
    path = "/".join(quote(part, safe="") for part in remote.split("/"))
    rev = quote(package_revision(download), safe="")
    if kind == "modelscope_snapshot":
        return f"{MS_ENDPOINT}/models/{repo}/resolve/{rev}/{path}"
    return f"{HF_ENDPOINT}/{repo}/resolve/{rev}/{path}"


def package_view(spec: dict, package: dict) -> dict:
    download = merged_download(spec, package)
    files = []
    for remote in package.get("files") or []:
        files.append({"remote_path": remote, "url": download_url(download, remote)})
    kind = str(download.get("kind") or "")
    gated = bool(download.get("gated", False))
    return {
        "id": package.get("id", ""),
        "display_name": package.get("display_name", ""),
        "precision": package.get("precision", ""),
        "format": package.get("format", ""),
        "target_directory": package.get("target_directory", ""),
        "local_paths": package_local_paths(package),
        "default": bool(package.get("default", False)),
        "kind": kind,
        "repo": download.get("repo", ""),
        "revision": package_revision(download) if kind in DOWNLOADABLE_KINDS else "",
        "gated": gated,
        "reason": str(download.get("reason") or ""),
        "files": files,
        # 「有没有一个真的能点的下载入口」= 认识这个 kind + 有 repo + 没有 gated + 每个文件都拼得出 URL
        "downloadable": bool(
            kind in DOWNLOADABLE_KINDS
            and str(download.get("repo") or "").strip()
            and not gated
            and files
            and all(f["url"] for f in files)
        ),
    }


def path_tail(raw: str) -> str:
    """schema/server.json 的 path 去掉 ${models_root} 之后的那一段。"""
    return str(raw or "").replace("${models_root}", "").strip().strip("/")


def choose_package(candidates: list, tail: str, precision_preference: list):
    """用产品自己的落点选包。返回 (包 | None, 说明)。"""
    exact = [p for p in candidates if tail in p["local_paths"]]
    matched_by = "落点精确匹配"
    if not exact:
        exact = [p for p in candidates if p["target_directory"] == tail]
        matched_by = "目录匹配（模型 path 指目录）"
    if not exact:
        ids = ", ".join(p["id"] for p in candidates)
        return None, f"没有落点等于 {tail} 的包（该 family 的可下载包：{ids}）"
    if len(exact) == 1:
        return exact[0], f"{matched_by}：{exact[0]['id']}"

    for pref in precision_preference or []:
        hit = [p for p in exact if p["precision"] == pref]
        if len(hit) == 1:
            return hit[0], f"{matched_by}，同处多个量化档，按 precision_preference={pref} 选中 {hit[0]['id']}"
    hit = [p for p in exact if p["precision"] == FALLBACK_PRECISION]
    if len(hit) == 1:
        return hit[0], (
            f"{matched_by}，同处多个量化档且 schema 未声明 precision_preference，"
            f"按 {FALLBACK_PRECISION} 优先选中 {hit[0]['id']}"
        )
    ids = ", ".join(sorted(p["id"] for p in exact))
    return None, f"{matched_by}但同处有多个量化档且挑不出唯一一个：{ids}"


def build_model(schema_model: dict, model_id: str, specs: dict) -> dict:
    family = str(schema_model.get("family") or "").strip()
    tail = path_tail(schema_model.get("path", ""))
    preference = list(schema_model.get("precision_preference") or [])
    spec = specs.get(family)

    if spec is None:
        where = f"family={family}" if family else "（schema 也没登记 family）"
        return {
            "id": model_id,
            "family": family,
            "spec": "",
            "path": tail,
            "status": "no-source",
            "note": f"上游 model_specs 里没有 {where} 的 spec —— 暂无下载源，不猜地址",
            "entry": None,
            "packages": [],
        }

    packages = [package_view(spec, p) for p in spec.get("packages") or []]
    candidates = [p for p in packages if p["downloadable"]]
    if not candidates:
        reasons = sorted({p["reason"] for p in packages if p["reason"]})
        note = "上游没有给这个 family 任何可下载包"
        if reasons:
            note += "：" + "；".join(reasons)
        return {
            "id": model_id,
            "family": family,
            "spec": spec["_spec_file"],
            "path": tail,
            "status": "no-source",
            "note": note,
            "entry": None,
            "packages": packages,
        }

    entry, why = choose_package(candidates, tail, preference)
    status = "downloadable" if entry else "no-source"
    if not entry:
        why = f"{why} —— 暂无下载入口，不猜地址"
    return {
        "id": model_id,
        "family": family,
        "spec": spec["_spec_file"],
        "path": tail,
        "status": status,
        "note": why,
        "entry": entry,
        "packages": packages,
    }


def build(schema: dict, specs: dict) -> dict:
    models = []
    for model_id in sorted(schema.get("models") or {}):
        models.append(build_model(schema["models"][model_id], model_id, specs))
    # 自检：标了 downloadable 就必须有一条能点的入口，且入口有落点与 URL
    for m in models:
        if m["status"] == "downloadable":
            entry = m["entry"]
            assert entry, f"{m['id']}: status=downloadable 但没有 entry"
            assert entry["local_paths"], f"{m['id']}: entry 没有落点"
            assert entry["files"] and all(f["url"] for f in entry["files"]), f"{m['id']}: entry 缺 URL"
        if m["status"] == "no-source":
            assert m["entry"] is None, f"{m['id']}: no-source 却带着 entry"
    return {
        "version": 1,
        "source": {
            "generated_by": "tools/gen_model_downloads.py",
            "specs": "model_specs/*.json",
            "spec_count": len(specs),
            "schema": "config/models.schema.yaml",
            "note": "由生成脚本产出，勿手改；size/sha256 刻意留空（生成不联网）",
        },
        "models": models,
    }


def render(payload: dict) -> str:
    """固定键序 + 固定缩进 + 行尾换行 = 逐字节稳定。"""
    return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description="生成随应用分发的模型下载清单")
    ap.add_argument("--check", action="store_true", help="只校验盘上的清单是否与上游一致，不写盘")
    ap.add_argument("--out", default=str(OUT_PATH), help=f"输出路径（默认 {OUT_PATH}）")
    a = ap.parse_args()

    upstream = find_upstream()
    specs_dir = Path(upstream) / "model_specs"
    schema = load_schema()
    specs = load_specs(specs_dir)
    text = render(build(schema, specs))
    out = Path(a.out)

    if a.check:
        current = out.read_text(encoding="utf-8") if out.exists() else ""
        if current == text:
            print(f"OK：{out} 与上游 model_specs 一致")
            return 0
        print(f"STALE：{out} 与上游 model_specs 不一致，请重跑 tools/gen_model_downloads.py", file=sys.stderr)
        return 1

    out.write_text(text, encoding="utf-8")
    payload = json.loads(text)
    n = sum(1 for m in payload["models"] if m["status"] == "downloadable")
    print(f"已写出 {out}：{len(payload['models'])} 个产品模型，其中 {n} 个有下载入口")
    return 0


if __name__ == "__main__":
    sys.exit(main())
