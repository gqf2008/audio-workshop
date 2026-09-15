#!/usr/bin/env python3
"""audio-config：模型参数化配置层的引用实现

读 models.schema.yaml（声明式配置）→ 做三件事：
  1. render  生成/校验 server.json（把配置翻译成上游落点）
  2. text    应用文本层：发音词典 + 数字规范化（含上游缺失的年份逐位规则）
  3. run     按场景执行一次（文本 → 规范化 → 服务合成 → wav）

用法:
  audio-config check                     # 校验配置（路径/必填/模型是否已注册）
  audio-config models                    # 列出模型与场景，含已知缺陷
  audio-config text "会议定在 2026 年"    # 看文本层会怎么改写（--model 指定模型策略）
  audio-config render [--write]          # 生成 server.json（默认只显示差异，不写）
  audio-config run dub --text "…" --out a.wav
  audio-config eval [--scene dub]        # 驱动 audio-eval 按配置跑
"""
import argparse, json, os, re, subprocess, sys, urllib.request

CFG_DEFAULT = os.path.join(os.path.dirname(os.path.dirname(os.path.realpath(__file__))), "config", "models.schema.yaml")
SERVER_JSON = os.path.expanduser("~/.local/opt/audio.cpp/server.json")
SERVER = "http://127.0.0.1:8080"

try:
    import yaml
except ImportError:
    sys.exit("需要 pyyaml：python3 -m pip install --user pyyaml")


# ── 配置加载 ────────────────────────────────────────────────────────────
def load(path=CFG_DEFAULT):
    cfg = yaml.safe_load(open(path, encoding="utf-8"))
    root = cfg.get("runtime", {}).get("models_root", "")
    def expand(v):
        if isinstance(v, str): return v.replace("${models_root}", root)
        if isinstance(v, dict): return {k: expand(x) for k, x in v.items()}
        if isinstance(v, list): return [expand(x) for x in v]
        return v
    return expand(cfg)


# ── 文本层：发音词典 + 数字规范化 ───────────────────────────────────────
_CN = "零一二三四五六七八九"
_UNITS = ["", "十", "百", "千"]
_BIG = ["", "万", "亿"]


def cardinal(n):
    """1234 → 一千二百三十四；10086 → 一万零八十六"""
    if n == 0: return "零"
    def u4(x):
        out, zero = "", False
        for i in (3, 2, 1, 0):
            d = (x // 10 ** i) % 10
            if d == 0: zero = True
            else:
                if zero and out: out += "零"
                zero = False
                out += ("十" if (d == 1 and i == 1 and not out) else _CN[d] + _UNITS[i])
        return out
    g, x = [], n
    while x > 0: g.append(x % 10000); x //= 10000
    out = ""
    for i in range(len(g) - 1, -1, -1):
        if g[i] == 0:
            if out and not out.endswith("零"): out += "零"
        else:
            if out and g[i] < 1000 and not out.endswith("零"): out += "零"
            out += u4(g[i]) + _BIG[i]
    return out.rstrip("零") or "零"


def digits_zh(s, telephone=False):
    return "".join("幺" if (telephone and c == "1") else _CN[int(c)] for c in s)


def verbalize(text, numbers=None):
    """数字读法规范化（默认规则集：基数词 + 年份逐位 + 电话幺 + 货币）"""
    n = {"year_digit_by_digit": True, "phone_yao": True, "currency_cardinal": True}
    n.update(numbers or {})
    t = re.sub(r"(?<=\d),(?=\d{3}\b)", "", text)
    if n["phone_yao"]:
        t = re.sub(r"\b1\d{10}\b", lambda m: digits_zh(m.group(0), True), t)
    t = re.sub(r"\b(\d+)\.(\d+)\b", lambda m: cardinal(int(m.group(1))) + "点" + digits_zh(m.group(2)), t)
    t = re.sub(r"(\d+)\s*%", lambda m: "百分之" + cardinal(int(m.group(1))), t)
    if n["year_digit_by_digit"]:
        # 年份/日期里的 4 位数逐位读（上游规则缺失的一项，配置层补上）
        t = re.sub(r"\b(1\d{3}|20\d{2})\s*(?=年)", lambda m: digits_zh(m.group(1)), t)
    t = re.sub(r"\b(\d+)\b", lambda m: cardinal(int(m.group(1))), t)
    return t


def apply_dictionary(text, entries):
    for e in sorted(entries or [], key=lambda x: -len(str(x.get("match", "")))):
        m, r = e.get("match"), e.get("read")
        if m and r: text = str(text).replace(str(m), str(r))
    return text


def normalize(text, cfg, model_id=None):
    """按配置处理文本：词典优先，其次看模型策略（enforce/engine/off）"""
    tcfg = cfg.get("text", {}).get("normalization", {})
    if not tcfg.get("enabled", True): return text, "disabled"
    policy = "enforce"
    if model_id and model_id in cfg.get("models", {}):
        policy = cfg["models"][model_id].get("text_overrides", {}).get("normalization", "enforce")
    text = apply_dictionary(text, tcfg.get("dictionary"))
    if policy in ("engine", "off"):
        return text, f"dictionary-only({policy})"
    return verbalize(text, tcfg.get("numbers")), "dictionary+rules"


# ── server.json 生成 ────────────────────────────────────────────────────
def to_server(cfg):
    models = []
    for mid, m in cfg.get("models", {}).items():
        if m.get("role") == "scoring":      # 打分模型不参与服务注册的业务路由，但仍注册
            pass
        e = {"id": mid, "family": m["family"], "path": m["path"], "task": m["task"],
             "mode": m.get("mode", "offline")}
        if m.get("session_options"): e["session_options"] = m["session_options"]
        if m.get("product_excluded"): e["product_excluded"] = True   # 产品层据此不在 UI 暴露
        models.append(e)
    rt = cfg.get("runtime", {})
    return {
        "host": "127.0.0.1", "port": 8080, "backend": rt.get("backend", "metal"),
        "threads": rt.get("threads", 4), "lazy_load": True, "models": models,
        "max_loaded_models": rt.get("memory", {}).get("max_loaded_models", 4),
        "idle_unload_ms": rt.get("memory", {}).get("idle_unload_ms", 300000),
        "min_free_memory_mb": rt.get("memory", {}).get("min_free_memory_mb", 512),
    }


def cmd_render(a):
    cfg = load(a.config); new = to_server(cfg)
    old = json.load(open(SERVER_JSON)) if os.path.exists(SERVER_JSON) else {}
    oldmap = {m["id"]: m for m in old.get("models", [])}
    newmap = {m["id"]: m for m in new["models"]}
    print("=== 模型注册差异 ===")
    for mid in sorted(set(oldmap) | set(newmap)):
        o, n = oldmap.get(mid), newmap.get(mid)
        if o is None: print(f"  + 新增 {mid}")
        elif n is None: print(f"  - 移除 {mid}")
        elif o != n:
            for k in set(o) | set(n):
                if o.get(k) != n.get(k): print(f"  ~ {mid}.{k}: {str(o.get(k))[:50]} → {str(n.get(k))[:50]}")
    for k in ("backend", "threads", "max_loaded_models", "idle_unload_ms", "min_free_memory_mb"):
        if old.get(k) != new.get(k): print(f"  ~ {k}: {old.get(k)} → {new.get(k)}")
    if a.write:
        if os.path.exists(SERVER_JSON):
            bak = SERVER_JSON + ".bak.audio-config"
            json.dump(old, open(bak, "w"), ensure_ascii=False, indent=2); print(f"  已备份 → {os.path.basename(bak)}")
        json.dump(new, open(SERVER_JSON, "w"), ensure_ascii=False, indent=2)
        print(f"  ✅ 已写入 {SERVER_JSON}（重启服务生效：audio-service server stop && audio-service server ensure）")
    else:
        print("\n（只显示差异；加 --write 才会写入）")


def cmd_text(a):
    cfg = load(a.config)
    out, how = normalize(a.text, cfg, a.model)
    print(f"  原文: {a.text}")
    print(f"  改写: {out}")
    print(f"  策略: {how}" + (f"（模型 {a.model}）" if a.model else ""))


def cmd_models(a):
    cfg = load(a.config)
    print("=== 场景 ===")
    for sid, s in cfg.get("scenarios", {}).items():
        print(f"  {sid:<6} 模型={s.get('model'):<26} pipeline={'→'.join(s.get('pipeline', []))}")
    print("=== 模型 ===")
    for mid, m in cfg.get("models", {}).items():
        pol = m.get("text_overrides", {}).get("normalization", "enforce")
        print(f"  {mid:<26} {m['task']:<4} TN={pol:<8} {m['path'].replace(cfg['runtime']['models_root'], '…')}")
        for iss in m.get("known_issues", []): print(f"      ⚠️  {iss}")


def cmd_check(a):
    cfg = load(a.config); bad = 0
    print(f"配置: {a.config}\nprofile: {cfg.get('profile')}")
    for mid, m in cfg.get("models", {}).items():
        ok = os.path.exists(m["path"])
        if not ok: bad += 1
        print(f"  {'✅' if ok else '❌'} {mid:<26} {m['path']}")
        if m.get("requires", {}).get("voice_ref"): print(f"      ℹ️  需要 --voice-ref")
    try:
        reg = {x["id"] for x in json.load(urllib.request.urlopen(SERVER + "/v1/models", timeout=5))["data"]}
        missing = reg - set(cfg.get("models", {}))
        if missing: print(f"  ℹ️  服务上还有配置未覆盖的模型: {', '.join(sorted(missing))}")
    except Exception as e:
        print(f"  ⚠️  服务不可达（先 audio-service server ensure）: {e}")
    sys.exit(1 if bad else 0)


def cmd_run(a):
    cfg = load(a.config)
    sc = cfg["scenarios"].get(a.scene) or sys.exit(f"未知场景 {a.scene}（可选 {', '.join(cfg['scenarios'])}）")
    mid = sc["model"]; m = cfg["models"][mid]
    text, how = normalize(a.text, cfg, mid)
    print(f"  场景 {a.scene} · 模型 {mid} · 文本层 {how}")
    print(f"  发送: {text[:80]}")
    payload = {"model": mid, "request": {"text": text}}
    payload["request"]["options"] = {k: str(v) for k, v in m.get("request_defaults", {}).items()}
    if sc.get("seed") is not None: payload["request"]["options"]["seed"] = str(sc["seed"])
    if a.voice_ref: payload["request"]["voice_ref"] = a.voice_ref
    req = urllib.request.Request(SERVER + "/v1/tasks/run", data=json.dumps(payload).encode(),
                                 headers={"Content-Type": "application/json"})
    import base64, time
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=1800) as r: resp = json.loads(r.read().decode())
    except Exception as e:
        sys.exit(f"  合成失败: {e}")
    if "audio" not in resp: sys.exit(f"  合成失败: {resp}")
    open(a.out, "wb").write(base64.b64decode(resp["audio"]))
    import wave, io as _io
    with wave.open(_io.BytesIO(base64.b64decode(resp["audio"]))) as w:
        print(f"  ✅ {a.out}  {w.getnframes()/w.getframerate():.1f}s  {w.getframerate()}Hz  耗时 {time.time()-t0:.1f}s")


def cmd_eval(a):
    cfg = load(a.config)
    models = [cfg["scenarios"][a.scene]["model"]] if a.scene else \
             [m for m, v in cfg["models"].items() if v["task"] == "tts"]
    cmd = ["audio-eval", "--models", *models, "--tag", "config"]
    if a.scene and cfg["models"].get(cfg["scenarios"][a.scene]["model"], {}).get("requires", {}).get("voice_ref"):
        cmd += ["--voice-ref", a.voice_ref or sys.exit("该场景模型需要 --voice-ref")]
    print("  运行:", " ".join(cmd)); sys.exit(subprocess.call(cmd))


def main():
    ap = argparse.ArgumentParser(description="模型参数化配置层")
    ap.add_argument("--config", default=CFG_DEFAULT)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("check");  p.set_defaults(f=cmd_check)
    p = sub.add_parser("models"); p.set_defaults(f=cmd_models)
    p = sub.add_parser("text");   p.add_argument("text"); p.add_argument("--model"); p.set_defaults(f=cmd_text)
    p = sub.add_parser("render"); p.add_argument("--write", action="store_true"); p.set_defaults(f=cmd_render)
    p = sub.add_parser("run");    p.add_argument("scene"); p.add_argument("--text", required=True)
    p.add_argument("--out", default="out.wav"); p.add_argument("--voice-ref"); p.set_defaults(f=cmd_run)
    p = sub.add_parser("eval");   p.add_argument("--scene"); p.add_argument("--voice-ref"); p.set_defaults(f=cmd_eval)
    a = ap.parse_args(); a.f(a)


if __name__ == "__main__":
    main()
