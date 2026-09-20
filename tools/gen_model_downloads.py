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

## 体积（`bytes`）从哪来
- **默认离线**：不联网。每个文件的 `bytes` **沿用盘上清单里已有的值**（按 URL 对齐），
  没有就留 `null`——不填 0、不拿别的档累加、不猜。
- `--fetch-sizes`：联网对每个文件发一次 `HTTP HEAD`，把最终落点的 `Content-Length` 写进
  `files[].bytes`；包级 `packages[].bytes` 是文件之和，**任一文件未知就是 `null`**
  （不拿"已知的那几个"冒充整包体积）。取不到（404 / 没头 / 超时 / 跳转成环）一律 `null`，
  失败原因在跑完的汇总里逐条打印。
- **只认最终 2xx 那跳的头**：HF 的 `/resolve/` 先回 302，那一跳的 `content-length` 是
  *跳转响应体*的长度（实测 1038 B）——拿它当权重体积会得到"每个模型都是 1 KB"这种
  看起来正常、实际全错的数字（脚本自己跟跳转，且**不把 HEAD 降级成 GET**）。
- **辅助权重**（`session_options`，如 `qwen3_asr.forced_aligner_model_path`）也会折成
  `aux_bytes`：它们常常是**另一个 family** 的包，按落点在全部 spec 里反查；查不到就把键名
  记进 `aux_unresolved`，不做无根据的估算。

## 哈希（sha256）从哪来
- **默认离线**：不联网。每个文件的 `sha256` **沿用盘上清单里已有的值**（按 URL 对齐），
  没有就留 `null`——不填假值、不拿别的档推算、不算本地文件。
- `--fetch-hashes`：联网对每个 `huggingface_snapshot` 文件查 HF tree API
  （`GET /api/models/{repo}/tree/{revision}?recursive=1`），按 `path` 对齐取 `lfs.oid`——
  那是 LFS 真 sha256（64 位十六进制）。**非 LFS 文件只有 git blob 的 sha1（40 位），
  不是权重哈希，不许拿来冒充**；取不到（HTTP 错误 / 树里没有这个路径 / 非 LFS /
  LFS 条目缺 oid）一律 `null`，失败原因在跑完的汇总里逐条打印。
  `modelscope_snapshot` 不取哈希（没有 LFS oid 概念），同样写 `null` 并说明原因。
  同一 `(repo, revision)` 一次生成只请求一次（进程内缓存，含失败）。
- `--fetch-hashes` 可与 `--fetch-sizes` 组合；两者都与 `--check` 互斥（`--check` 不联网）。

## 稳定输出
- 键序固定（`sort_keys`）+ 固定缩进 + 行尾换行 → 同输入两次运行逐字节相同。
- `--check` 只重算不写盘（**也不联网**：体积/哈希沿用盘上已有的值），与盘上的文件比对；
  不一致退出 1（发现"改了 spec 忘了重生成"）。`--check` 与 `--fetch-sizes` / `--fetch-hashes`
  互斥。

用法:
  python3 tools/gen_model_downloads.py                        # 离线生成（体积/哈希沿用盘上已有值）
  python3 tools/gen_model_downloads.py --fetch-sizes          # 联网补体积后生成
  python3 tools/gen_model_downloads.py --fetch-hashes         # 联网补哈希后生成
  python3 tools/gen_model_downloads.py --fetch-hashes --fetch-sizes
  python3 tools/gen_model_downloads.py --check                # 校验已提交的清单是否与上游一致
  AUDIOCPP_DIR=/path/to/audio.cpp python3 tools/gen_model_downloads.py
"""

import argparse
import json
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from urllib.parse import quote

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent
sys.path.insert(0, str(HERE))

# 复用「上游 checkout 在哪」的唯一口径（环境变量 AUDIOCPP_DIR 优先，其次 <repo>/../audio.cpp）
from model_fetch import find_upstream  # noqa: E402
import _console  # noqa: F401  副作用 import：stdout→UTF-8，见该模块注释（Windows 上必须）

SCHEMA_PATH = REPO_ROOT / "config" / "models.schema.yaml"
OUT_PATH = REPO_ROOT / "config" / "model-downloads.json"

HF_ENDPOINT = "https://huggingface.co"
HF_API_ENDPOINT = "https://huggingface.co/api"
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


def package_bytes(files: list):
    """包级体积 = 文件之和；**任一文件未知就是 None**（不拿已知项冒充整包）。"""
    if not files:
        return None
    total = 0
    for f in files:
        n = f.get("bytes")
        if not isinstance(n, int):
            return None
        total += n
    return total


def package_view(
    spec: dict, package: dict, size_of, *, want_sizes=None, want_hashes=None, hash_of=None
) -> dict:
    """一个包的投影。

    `want_sizes`：这个包要不要去向 `size_of` 要体积。默认 = "可下载才要" ——
    不可下载的包（gated / 上游不支持）匿名 HEAD 必然 401，白跑还会在日志里制造噪音。
    辅助权重（`resolve_aux`）显式传 `True`：它们不是产品模型，但估算要用。
    `want_hashes` / `hash_of`：同上，问的是 HF tree API 的 `lfs.oid`（真 sha256）。
    辅助权重不传 `hash_of`：它们的哈希不在产物里（`aux_files` 只有 url/bytes），
    应用也不拿它们做下载校验。
    """
    download = merged_download(spec, package)
    kind = str(download.get("kind") or "")
    gated = bool(download.get("gated", False))
    urls = [download_url(download, remote) for remote in package.get("files") or []]
    # 「有没有一个真的能点的下载入口」= 认识这个 kind + 有 repo + 没有 gated + 每个文件都拼得出 URL
    downloadable = bool(
        kind in DOWNLOADABLE_KINDS
        and str(download.get("repo") or "").strip()
        and not gated
        and urls
        and all(urls)
    )
    fetch = downloadable if want_sizes is None else want_sizes
    fetch_hash = downloadable if want_hashes is None else want_hashes
    files = [
        {
            "remote_path": remote,
            "url": url,
            # 取不到就是 None：调用方只管填值，不编造
            "bytes": size_of(url) if (fetch and url) else None,
            "sha256": hash_of(download, remote, url) if (fetch_hash and url and hash_of) else None,
        }
        for remote, url in zip(package.get("files") or [], urls)
    ]
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
        # 包级总计（见 package_bytes：不完整就是 null）
        "bytes": package_bytes(files),
        "downloadable": downloadable,
    }


# ---------------------------------------------------------------------------
# 体积：HTTP HEAD（自己跟跳转，只认最终 2xx 那跳的头）
# ---------------------------------------------------------------------------
SIZE_TIMEOUT_SECONDS = 20.0
SIZE_MAX_HOPS = 6
SIZE_UA = "audio-workshop-gen-model-downloads/1.0"
REDIRECT_CODES = (301, 302, 303, 307, 308)


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """不自动跟跳转：urllib 的跳转处理器会把 HEAD 降级成 GET，

    对着 2 GB 权重跑就等于"为了量体积把整份下回来"。
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: D102
        return None


_SIZER = urllib.request.build_opener(_NoRedirect)


def _linked_size(headers):
    """HF 的 `/resolve/` 会在 302 上带 `x-linked-size`（被链接文件的真实字节数）。"""
    raw = headers.get("x-linked-size") if headers else None
    try:
        return int(raw) if raw is not None else None
    except (TypeError, ValueError):
        return None


def head_content_length(url, *, timeout=SIZE_TIMEOUT_SECONDS, opener=None):
    """HEAD 取最终落点的 Content-Length；取不到 → (None, 原因)。

    **只看最终 2xx 那跳的头**：302 那跳的 `content-length` 是**跳转响应体**的长度
    （实测 1038 B），拿它当权重体积，每个模型都会"恰好 1 KB"——静默全错。
    最终落点没给 `Content-Length` 时，退回跳转链上 HF 给的 `x-linked-size`。
    """
    opener = opener or _SIZER
    seen = set()
    current = url
    linked = None
    for _ in range(SIZE_MAX_HOPS):
        if current in seen:
            return None, "跳转成环"
        seen.add(current)
        request = urllib.request.Request(
            current, method="HEAD", headers={"User-Agent": SIZE_UA}
        )
        try:
            response = opener.open(request, timeout=timeout)
        except urllib.error.HTTPError as error:
            linked = linked if linked is not None else _linked_size(error.headers)
            code = error.code
            location = error.headers.get("Location") if error.headers else None
            # 只要头，但得显式关掉：不然每次 4xx/跳转都留一个没关的响应体句柄
            error.close()
            if code in REDIRECT_CODES:
                if not location:
                    return None, f"HTTP {code} 没带 Location"
                current = urllib.parse.urljoin(current, location)
                continue
            return None, f"HTTP {code}"
        except Exception as error:  # noqa: BLE001 —— 网络层任何异常都如实记原因
            return None, type(error).__name__
        with response:
            if response.status != 200:
                return None, f"HTTP {response.status}"
            raw = response.headers.get("Content-Length")
            if raw is None:
                if linked is not None:
                    return linked, "x-linked-size"
                return None, "没有 Content-Length"
            try:
                size = int(raw)
            except ValueError:
                return None, f"Content-Length 不是整数：{raw}"
            return (size, "content-length") if size >= 0 else (None, "Content-Length 为负")
    return None, f"跳转超过 {SIZE_MAX_HOPS} 跳"


def read_existing_hashes(path) -> dict:
    """盘上清单里的 `URL → sha256`：离线生成时"哈希沿用已有的值"就靠它。

    与 `read_existing_sizes` 一样按 URL 对齐；`aux_files` 里没有 sha256 字段，
    所以这里只看 `models[].packages[].files`。
    """
    try:
        payload = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    hashes = {}

    def remember(entries):
        for entry in entries or []:
            url, sha = entry.get("url"), entry.get("sha256")
            if url and isinstance(sha, str) and sha.strip():
                hashes[url] = sha.strip()

    for model in payload.get("models") or []:
        for package in model.get("packages") or []:
            remember(package.get("files"))
    return hashes


def read_existing_sizes(path) -> dict:
    """盘上清单里的 `URL → bytes`：离线生成时"体积沿用已有的值"就靠它。"""
    try:
        payload = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    sizes = {}

    def remember(entries):
        for entry in entries or []:
            url, size = entry.get("url"), entry.get("bytes")
            if url and isinstance(size, int) and size >= 0:
                sizes[url] = size

    for model in payload.get("models") or []:
        for package in model.get("packages") or []:
            remember(package.get("files"))
        # 辅助权重不在 models[].packages 里（它是**别的 family** 的包），
        # 单列一份，离线重生成时才有地方把它的体积沿用回来。
        remember(model.get("aux_files"))
    return sizes


# ---------------------------------------------------------------------------
# 哈希：HF tree API 的 lfs.oid（LFS 真 sha256）
# ---------------------------------------------------------------------------
TREE_TIMEOUT_SECONDS = 30.0
TREE_UA = "audio-workshop-gen-model-downloads/1.0 (hashes)"


def lfs_oid(entry: dict):
    """tree API 一个文件条目 → `(sha256 | None, 原因 | None)`。

    只有 `lfs.oid` 是 LFS 真 sha256（64 位十六进制）。非 LFS 文件（存在 git 里的小文件）
    只有 git blob 的 `oid`（40 位 sha1）——**那不是权重哈希**，拿它冒充 sha256 会让
    校验对着错误的期望值跑，所以：没有 `lfs` 对象 / `lfs` 里没有 `oid` 一律 None + 原因。
    """
    lfs = entry.get("lfs")
    if not isinstance(lfs, dict):
        return None, "非 LFS 文件（没有 lfs.oid）"
    oid = lfs.get("oid")
    if not isinstance(oid, str) or not oid.strip():
        return None, "LFS 条目缺 oid"
    return oid.strip(), None


class HfTreeFetcher:
    """按 (repo, revision) 缓存 HF tree API 的取回结果（含失败）。

    同一次生成里同一 repo@revision 只会请求一次；失败也缓存（重试也不会换一个答案，
    不在这台机器上打 HF 的限流）。
    """

    def __init__(self):
        self._cache = {}
        self._opener = urllib.request.build_opener()

    def hash_for(self, repo: str, revision: str, remote: str):
        """`remote`（spec 里的远端相对路径）→ `(sha256 | None, 原因 | None)`。"""
        key = (repo, revision)
        if key not in self._cache:
            self._cache[key] = self._fetch(repo, revision)
        by_path, reason = self._cache[key]
        if by_path is None:
            return None, reason
        entry = by_path.get(remote) or by_path.get(remote.lstrip("/"))
        if entry is None:
            return None, f"tree 里没有这个路径（{repo}@{revision}）"
        return lfs_oid(entry)

    def _fetch(self, repo: str, revision: str):
        url = (
            f"{HF_API_ENDPOINT}/models/{quote(repo, safe='/')}/tree/"
            f"{quote(revision, safe='')}?recursive=1"
        )
        request = urllib.request.Request(url, headers={"User-Agent": TREE_UA})
        try:
            with self._opener.open(request, timeout=TREE_TIMEOUT_SECONDS) as response:
                if response.status != 200:
                    return None, f"HTTP {response.status}"
                payload = json.load(response)
        except urllib.error.HTTPError as error:
            code = error.code
            error.close()
            return None, f"HTTP {code}"
        except Exception as error:  # noqa: BLE001 —— 网络层任何异常都如实记原因
            return None, type(error).__name__
        if not isinstance(payload, list):
            return None, "tree API 没回数组"
        by_path = {}
        for entry in payload:
            if isinstance(entry, dict) and entry.get("type") == "file":
                path = entry.get("path")
                if isinstance(path, str) and path:
                    by_path[path] = entry
        return by_path, None


# ---------------------------------------------------------------------------
# 辅助权重：session_options 的落点 → 包（可能跨 family）
# ---------------------------------------------------------------------------
def package_local_index(specs: dict):
    """`落点 → [(family, 包)]` 的全局索引。

    辅助权重常常是**另一个 family** 的包（`qwen3_asr.forced_aligner_model_path`
    指向 `qwen3_forced_aligner` 的权重），所以索引要跨全部 spec 建。
    """
    by_path, by_dir = {}, {}
    for family in sorted(specs):
        for package in specs[family].get("packages") or []:
            target = str(package.get("target_directory", "")).strip("/")
            by_dir.setdefault(target, []).append((family, package))
            for local in package_local_paths(package):
                by_path.setdefault(local, []).append((family, package))
    return by_path, by_dir


def aux_local_path(declared, model_tail: str):
    """`session_options` 的值 → 相对模型根的落点；生成期推不出来就返回 None。"""
    raw = str(declared or "").strip()
    if not raw:
        return None
    prefix = "${models_root}/"
    if raw.startswith(prefix):
        return raw[len(prefix):].strip("/") or None
    # 绝对路径 / 其它占位符 / 向上跳：生成期推不出服务会去哪找，如实不解析
    if raw.startswith("${") or raw.startswith("/") or ".." in raw.split("/"):
        return None
    # 相对路径：服务按"path 指文件就用它的父目录、指目录就用它自己"解析
    name = model_tail.rsplit("/", 1)[-1]
    base = model_tail.rsplit("/", 1)[0] if "." in name else model_tail
    return f"{base}/{raw}".strip("/") if base else raw


def resolve_aux(schema_model: dict, model_tail: str, specs: dict, by_path, by_dir, size_of):
    """把辅助权重折成 `(字节合计, 未解析的键名, 已解析的文件表)`。

    一个落点对上多个包（同目录多量化档）时**不猜**服务会加载哪个，记进未解析。
    文件表（`aux_files`）要写进产物：辅助权重是**别的 family** 的包，不出现在
    `models[].packages` 里，不单列一份的话离线重生成就没有地方沿用它的体积。
    """
    options = schema_model.get("session_options") or {}
    total = 0
    unresolved = []
    files = []
    for key in sorted(options):
        local = aux_local_path(options[key], model_tail)
        hits = (by_path.get(local) or by_dir.get(local) or []) if local else []
        if len(hits) != 1:
            unresolved.append(key)
            continue
        family, package = hits[0]
        view = package_view(specs[family], package, size_of, want_sizes=True)
        if view["bytes"] is None:
            unresolved.append(key)
            continue
        total += view["bytes"]
        files.extend({"url": f["url"], "bytes": f["bytes"]} for f in view["files"])
    return total, unresolved, files


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


def build_model(
    schema_model: dict,
    model_id: str,
    specs: dict,
    size_of,
    hash_of=None,
    by_path=None,
    by_dir=None,
) -> dict:
    family = str(schema_model.get("family") or "").strip()
    tail = path_tail(schema_model.get("path", ""))
    preference = list(schema_model.get("precision_preference") or [])
    spec = specs.get(family)
    # 辅助权重：任何返回分支都带上（它们在界面上是"估算偏低"的依据）
    aux_bytes, aux_unresolved, aux_files = resolve_aux(
        schema_model, tail, specs, by_path or {}, by_dir or {}, size_of
    )

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
            "aux_bytes": aux_bytes,
            "aux_unresolved": aux_unresolved,
            "aux_files": aux_files,
        }

    packages = [
        package_view(spec, p, size_of, hash_of=hash_of) for p in spec.get("packages") or []
    ]
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
            "aux_bytes": aux_bytes,
            "aux_unresolved": aux_unresolved,
            "aux_files": aux_files,
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
        "aux_bytes": aux_bytes,
        "aux_unresolved": aux_unresolved,
        "aux_files": aux_files,
    }


def build(schema: dict, specs: dict, size_of, hash_of=None) -> dict:
    by_path, by_dir = package_local_index(specs)
    models = []
    for model_id in sorted(schema.get("models") or {}):
        models.append(
            build_model(
                schema["models"][model_id],
                model_id,
                specs,
                size_of,
                hash_of=hash_of,
                by_path=by_path,
                by_dir=by_dir,
            )
        )
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
            "note": (
                "由生成脚本产出，勿手改；sha256 来自 HF tree API 的 lfs.oid（--fetch-hashes），"
                "bytes 来自 HTTP HEAD（--fetch-sizes）；取不到就是 null（不填 0、不拿别的档推算）"
            ),
        },
        "models": models,
    }


def render(payload: dict) -> str:
    """固定键序 + 固定缩进 + 行尾换行 = 逐字节稳定。"""
    return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description="生成随应用分发的模型下载清单")
    ap.add_argument("--check", action="store_true", help="只校验盘上的清单是否与上游一致，不写盘、不联网")
    ap.add_argument(
        "--fetch-sizes",
        action="store_true",
        help="联网对每个文件发 HTTP HEAD 取真实体积（默认离线：沿用盘上清单里已有的值）",
    )
    ap.add_argument(
        "--fetch-hashes",
        action="store_true",
        help="联网对每个 HF 文件查 tree API 取 LFS 真 sha256（默认离线：沿用盘上清单里已有的值）",
    )
    ap.add_argument("--out", default=str(OUT_PATH), help=f"输出路径（默认 {OUT_PATH}）")
    a = ap.parse_args()
    if a.check and (a.fetch_sizes or a.fetch_hashes):
        sys.exit(
            "--check 不联网（体积/哈希沿用盘上已有值），不能和 --fetch-sizes / --fetch-hashes 一起用"
        )

    upstream = find_upstream()
    specs_dir = Path(upstream) / "model_specs"
    schema = load_schema()
    specs = load_specs(specs_dir)
    out = Path(a.out)

    if a.fetch_sizes:
        # 第一趟不联网：只**记账**——`package_view` 会告诉我们要问哪些 URL
        # （可下载的包 + 辅助权重），这一步把集合收下来，避免对 gated 仓库白跑 HEAD。
        wanted = set()

        def collect(url):
            if url:
                wanted.add(url)
            return None

        build(schema, specs, collect)
        fetched, failed = {}, {}

        def size_of(url):
            if not url:
                return None
            size, how = head_content_length(url)
            if size is None:
                failed[url] = how
            else:
                fetched[url] = size
            return size

    else:
        existing = read_existing_sizes(out)
        kept, missing = {}, []

        def size_of(url):
            if not url:
                return None
            size = existing.get(url)
            if size is None:
                missing.append(url)
            else:
                kept[url] = size
            return size

    if a.fetch_hashes:
        # 按 (repo, revision) 缓存 tree API 的取回结果：同一次生成里同一仓库只请求一次。
        fetcher = HfTreeFetcher()
        fetched_hashes, failed_hashes = {}, {}

        def hash_of(download, remote, url):
            if not url:
                return None
            kind = str(download.get("kind") or "")
            if kind != "huggingface_snapshot":
                failed_hashes[url] = (
                    f"{kind or '未知 kind'}：只对 huggingface_snapshot 取 LFS oid，不猜"
                )
                return None
            sha, why = fetcher.hash_for(
                str(download.get("repo") or ""), package_revision(download), remote
            )
            if sha is None:
                failed_hashes[url] = why
            else:
                fetched_hashes[url] = sha
            return sha

    else:
        existing_hashes = read_existing_hashes(out)
        kept_hashes, missing_hashes = {}, []

        def hash_of(download, remote, url):
            if not url:
                return None
            sha = existing_hashes.get(url)
            if sha is None:
                missing_hashes.append(url)
            else:
                kept_hashes[url] = sha
            return sha

    text = render(build(schema, specs, size_of, hash_of))

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
    if a.fetch_sizes:
        nulls = sum(
            1
            for m in payload["models"]
            for p in m["packages"]
            for f in p["files"]
            if f["bytes"] is None
        )
        print(
            f"体积：{len(wanted)} 条需要体积，其中 {len(fetched)} 条取到、{len(failed)} 条取不到；"
            f"产物里共 {nulls} 条为 null（含不可下载的包，那些根本没去探）"
        )
        for url in sorted(failed):
            print(f"  null ← {failed[url]}  {url}", file=sys.stderr)
    else:
        print(f"体积：**这次没联网**，{len(kept)} 条沿用清单里已有的值；{len(set(missing))} 条清单里没有 → null")
    if a.fetch_hashes:
        nulls = sum(
            1
            for m in payload["models"]
            for p in m["packages"]
            for f in p["files"]
            if f["sha256"] is None
        )
        print(
            f"哈希：{len(fetched_hashes)} 条取到、{len(failed_hashes)} 条取不到；"
            f"产物里共 {nulls} 条为 null（含不可下载的包，那些根本没去探）"
        )
        for url in sorted(failed_hashes):
            print(f"  null ← {failed_hashes[url]}  {url}", file=sys.stderr)
    else:
        print(
            f"哈希：**这次没联网**，{len(kept_hashes)} 条沿用清单里已有的值；"
            f"{len(set(missing_hashes))} 条清单里没有 → null"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
