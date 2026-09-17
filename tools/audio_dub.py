#!/usr/bin/env python3
"""audio-dub：配音链路（M0）

稿子 → 切句 → 文本兜底 → 逐句合成 → 拼装（含句子级时间轴）→ 成品 wav + srt
改一句只重录那一句：audio-dub redo <dir> <序号>

用法:
  audio-dub run 稿子.txt --out ~/dub/2026-09-16        # 一把梭
  audio-dub new 稿子.txt --out <dir>                   # 只建工程（切句+兜底+清单）
  audio-dub synth <dir> [--only 3]                     # 合成（可只合成某句）
  audio-dub redo  <dir> 3 [--text "改后的这一句"]       # 重录某句并重拼
  audio-dub assemble <dir>                             # 重新拼装（不重新合成）
  audio-dub status <dir>                               # 看每句状态/时长/时间轴
"""
import argparse, base64, json, os, re, sys, time, urllib.request, wave, io

HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, HERE)
import audio_config as cfgmod  # 复用配置层：规范化 + 词典 + 场景定义

SERVER = "http://127.0.0.1:8080"


# ── 切句：按中文句末标点，超长再按逗号断 ─────────────────────────────
def split_sentences(text, chunk_cfg):
    punct = chunk_cfg.get("punctuation", "。！？；…")
    max_chars = int(chunk_cfg.get("max_chars", 80))
    text = re.sub(r"[ \t]+", " ", text)
    raw = [s for s in re.split(r"(?<=[" + re.escape(punct) + r"])", text)]
    out = []
    for s in raw:
        s = s.strip()
        if not s: continue
        while len(s) > max_chars:                       # 长句按逗号再切
            cut = max(s.rfind("，", 0, max_chars), s.rfind(",", 0, max_chars), s.rfind("、", 0, max_chars))
            if cut <= 0: break
            out.append(s[:cut + 1].strip()); s = s[cut + 1:].strip()
        if s: out.append(s)
    return out


def post(path, payload, timeout=1800):
    req = urllib.request.Request(SERVER + path, data=json.dumps(payload).encode(),
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        try: return json.loads(e.read().decode())
        except Exception: return {"error": f"HTTP {e.code}"}
    except Exception as e:
        return {"error": str(e)}


def wav_meta(raw):
    with wave.open(io.BytesIO(raw)) as w:
        return w.getframerate(), w.getnchannels(), w.getsampwidth(), w.getnframes()


def synth_one(text, model, seed, voice_ref=None, extra=None):
    req = {"text": text, "options": dict(extra or {})}
    if seed is not None: req["options"]["seed"] = str(seed)
    if voice_ref: req["voice_ref"] = voice_ref
    resp = post("/v1/tasks/run", {"model": model, "request": req})
    if "audio" not in resp:
        return None, str(resp.get("error", resp))[:200]
    return base64.b64decode(resp["audio"]), None



# ── 落盘：一律"临时文件 + fsync + 原子替换" ─────────────────────────────
# 评审指出：原地覆盖 final.wav、无 fsync、project.json 非原子写，都会在崩溃/磁盘满时
# 留下静默损坏的产物或不可读的工程。这里统一收口。
def write_atomic(path, data: bytes):
    tmp = f"{path}.tmp{os.getpid()}"
    with open(tmp, "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


# ── 工程文件 ────────────────────────────────────────────────────────
def load_project(d):
    p = os.path.join(d, "project.json")
    if not os.path.exists(p): sys.exit(f"不是工程目录（缺 project.json）: {d}")
    return json.load(open(p, encoding="utf-8"))


def save_project(d, prj):
    write_atomic(os.path.join(d, "project.json"),
                 json.dumps(prj, ensure_ascii=False, indent=1).encode("utf-8"))


# ── 子命令 ──────────────────────────────────────────────────────────
def cmd_new(a):
    cfg = cfgmod.load(a.config)
    sc = cfg["scenarios"]["dub"]
    model = sc["model"]
    text = open(a.script, encoding="utf-8").read()
    text = re.sub(r"^#.*$", "", text, flags=re.M)          # 去 markdown 标题行
    sents = split_sentences(text, cfg["text"]["chunking"])
    if not sents: sys.exit("稿子里没有可合成的句子")
    os.makedirs(os.path.join(a.out, "sentences"), exist_ok=True)
    prj = {
        "script": os.path.abspath(a.script), "created": time.strftime("%Y-%m-%d %H:%M:%S"),
        "model": model, "voice_ref": a.voice_ref, "gap_ms": a.gap_ms,
        "base_seed": a.seed if a.seed is not None else 831001,
        "sentences": [],
    }
    for i, s in enumerate(sents):
        norm, how = cfgmod.normalize(s, cfg, model)
        prj["sentences"].append({
            "index": i, "text": s, "spoken": norm, "text_layer": how,
            "seed": prj["base_seed"] + i, "wav": f"sentences/{i:03d}.wav",
            "duration": None, "start": None, "status": "pending",
        })
    save_project(a.out, prj)
    print(f"  工程已建: {a.out}")
    print(f"  模型 {model} · 共 {len(sents)} 句 · 间隔 {a.gap_ms}ms · 句级 seed 可复现")
    for s in prj["sentences"][:3]:
        mark = "（已兜底）" if s["spoken"] != s["text"] else ""
        print(f"    [{s['index']:>3}] {s['spoken'][:46]}{mark}")
    if len(sents) > 3: print(f"    … 其余 {len(sents)-3} 句见 project.json")


def cmd_synth(a):
    prj = load_project(a.dir)
    only = set(a.only) if a.only else None
    todo = [s for s in prj["sentences"] if (only is None or s["index"] in only)
            and (a.redo or s["status"] != "done")]
    if not todo: print("  没有需要合成的句子"); return
    print(f"  合成 {len(todo)} 句 · 模型 {prj['model']}")
    t0 = time.time()
    for s in todo:
        # 不带 instruction：audio8_tts / index_tts2 的 spec（options.request）都没这个旋钮，
        # 发了只会被静默忽略（真机同 seed 同文本改 instruction，输出字节完全相同）。
        raw, err = synth_one(s["spoken"], prj["model"], s["seed"], prj.get("voice_ref"))
        if err:
            s["status"] = f"error: {err}"; print(f"    [{s['index']:>3}] ❌ {err[:70]}"); continue
        path = os.path.join(a.dir, s["wav"])
        # write_atomic 保证"写完 len(raw) 字节 + fsync 后才 rename"：返回即完整，
        # 落盘失败会抛异常而不是留下半截文件（此前这里的 getsize 校验恒为假，是装饰）
        write_atomic(path, raw)
        sr, ch, sw, n = wav_meta(raw)
        s.update({"duration": round(n / sr, 3), "sample_rate": sr, "channels": ch,
                  "status": "done"})
        print(f"    [{s['index']:>3}] ✅ {s['duration']:5.2f}s  {s['spoken'][:40]}")
        save_project(a.dir, prj)                   # 逐句落盘：中途被杀不退已完成的部分
    save_project(a.dir, prj)
    print(f"  用时 {time.time()-t0:.1f}s")


def cmd_assemble(a):
    prj = load_project(a.dir)
    done = [s for s in prj["sentences"] if s["status"] == "done"]
    if not done: sys.exit("  还没有已合成的句子")
    # 拼装前逐句校验：文件存在、可读、位深/采样率/声道与元数据一致
    bad = []
    for s in done:
        path = os.path.join(a.dir, s["wav"])
        if not os.path.isfile(path):
            bad.append(f"[{s['index']}] 文件缺失"); continue
        try:
            with wave.open(path, "rb") as w:
                if w.getsampwidth() != 2 or w.getframerate() != s["sample_rate"] or w.getnchannels() != s["channels"]:
                    bad.append(f"[{s['index']}] 参数不符 (位深{w.getsampwidth()*8}bit "
                               f"{w.getframerate()}Hz {w.getnchannels()}ch)")
                else:
                    # 关键：截断文件的两处证据都不在"头"里——头仍合法、参数仍正确、
                    # 头里的帧数也不会被截断改写。唯一可信的判据是**实际字节数**是否
                    # 覆盖头声明的数据量（截断 → 实际 < 应有）。
                    need = 44 + w.getnframes() * w.getnchannels() * w.getsampwidth()
                    actual_bytes = os.path.getsize(path)
                    if actual_bytes < need:
                        bad.append(f"[{s['index']}] 文件被截断 (实际 {actual_bytes} 字节，"
                                   f"头声明需要 {need} 字节)")
        except Exception as e:
            bad.append(f"[{s['index']}] 不可读: {e}")
    if bad:
        sys.exit("  拼装中止，以下句子有问题：\n    " + "\n    ".join(bad))
    rates = {(s["sample_rate"], s["channels"]) for s in done}
    if len(rates) > 1: sys.exit(f"  采样率/声道不一致，无法拼装: {rates}")
    sr, ch = rates.pop()
    gap = int(sr * prj["gap_ms"] / 1000)
    os.makedirs(os.path.join(a.dir, "out"), exist_ok=True)
    final = os.path.join(a.dir, "out", "final.wav")
    final_tmp = f"{final}.tmp{os.getpid()}"
    cursor, srt = 0, []
    with wave.open(final_tmp, "wb") as out:
        out.setnchannels(ch); out.setsampwidth(2); out.setframerate(sr)
        for k, s in enumerate(prj["sentences"]):
            if s["status"] != "done":
                s["start"] = None; continue
            s["start"] = round(cursor / sr, 3)
            with wave.open(os.path.join(a.dir, s["wav"]), "rb") as w:
                frames = w.readframes(w.getnframes())
            out.writeframes(frames)
            cursor += s["duration"] * sr
            if k != len(prj["sentences"]) - 1:
                out.writeframes(b"\x00" * (gap * ch * 2)); cursor += gap
            m0, s0 = divmod(s["start"], 60); m1, s1 = divmod(s["start"] + s["duration"], 60)
            srt.append(f"{len(srt)+1}\n{int(m0):02d}:{s0:06.3f}".replace(".", ",") +
                       f" --> {int(m1):02d}:{s1:06.3f}".replace(".", ",") + f"\n{s['text']}\n")
    with open(final_tmp, "rb+") as f:                  # 成品是本工程最要紧的产物：
        f.flush()                                      # wave 关闭后补一次 fsync，否则
        os.fsync(f.fileno())                           # 掉电可能留下已 rename 到位的空文件
    os.replace(final_tmp, final)                       # 原子替换：旧成品在写完前不受影响
    write_atomic(os.path.join(a.dir, "out", "final.srt"), "\n".join(srt).encode("utf-8"))
    save_project(a.dir, prj)
    print(f"  ✅ {final}  {cursor/sr:.1f}s  共 {len(done)}/{len(prj['sentences'])} 句")
    print(f"  ✅ {os.path.join(a.dir, 'out', 'final.srt')}（句子级时间轴）")


def cmd_redo(a):
    prj = load_project(a.dir)
    hit = [s for s in prj["sentences"] if s["index"] == a.index]
    if not hit: sys.exit(f"  没有第 {a.index} 句")
    s = hit[0]
    if a.text:
        cfg = cfgmod.load(a.config)
        s["text"] = a.text
        s["spoken"], s["text_layer"] = cfgmod.normalize(a.text, cfg, prj["model"])
        print(f"  文本已改: {s['spoken'][:50]}")
    s["seed"] += 1000                     # 换 seed，避免复现同一条不满意的音频
    save_project(a.dir, prj)              # 先落盘：cmd_synth 会从磁盘重载工程
    cmd_synth(argparse.Namespace(dir=a.dir, only=[a.index], redo=True, config=a.config))
    cmd_assemble(argparse.Namespace(dir=a.dir))


def cmd_status(a):
    prj = load_project(a.dir)
    print(f"  工程 {a.dir} · 模型 {prj['model']} · {len(prj['sentences'])} 句")
    total = 0
    for s in prj["sentences"]:
        st = {"done": "✅", "pending": "·"}.get(s["status"], "❌")
        dur = f"{s['duration']:6.2f}s" if s["duration"] else "      "
        start = f"{s['start']:7.2f}s" if s.get("start") is not None else "        "
        total += s["duration"] or 0
        print(f"   {st} [{s['index']:>3}] {start} {dur}  {s['spoken'][:44]}")
    print(f"  合计 {total:.1f}s")


def cmd_run(a):
    cmd_new(a); cmd_synth(argparse.Namespace(dir=a.out, only=None, redo=False, config=a.config))
    cmd_assemble(argparse.Namespace(dir=a.out)); print(f"\n  成品: {os.path.join(a.out,'out','final.wav')}")


def main():
    ap = argparse.ArgumentParser(description="配音链路（M0）")
    ap.add_argument("--config", default=cfgmod.CFG_DEFAULT)
    sub = ap.add_subparsers(dest="cmd", required=True)
    def add(name, fn, **kw):
        p = sub.add_parser(name, **kw); p.set_defaults(f=fn); return p
    p = add("new", cmd_new); p.add_argument("script"); p.add_argument("--out", required=True)
    p.add_argument("--gap-ms", type=int, default=250); p.add_argument("--voice-ref"); p.add_argument("--seed", type=int)
    p = add("synth", cmd_synth); p.add_argument("dir"); p.add_argument("--only", type=int, nargs="*"); p.add_argument("--redo", action="store_true")
    p = add("assemble", cmd_assemble); p.add_argument("dir")
    p = add("redo", cmd_redo); p.add_argument("dir"); p.add_argument("index", type=int); p.add_argument("--text")
    p = add("status", cmd_status); p.add_argument("dir")
    p = add("run", cmd_run); p.add_argument("script"); p.add_argument("--out", required=True)
    p.add_argument("--gap-ms", type=int, default=250); p.add_argument("--voice-ref"); p.add_argument("--seed", type=int)
    a = ap.parse_args(); a.f(a)


if __name__ == "__main__":
    main()
