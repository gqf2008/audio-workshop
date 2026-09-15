#!/usr/bin/env python3
"""audio.cpp 本地质量评估台（配音/歌曲）

对每个模型跑固定测试文本，用 ASR 回测可懂度，并记录耗时与峰值内存。
结果写入 ~/.local/opt/audio.cpp/eval/，每次运行与上一次对比（回归检测）。

用法:
  audio-eval                      # 默认：全部已注册 TTS 模型 × 中文/英文各 2 轮
  audio-eval --models audio8-tts  # 只测指定模型
  audio-eval --include-gen        # 额外跑歌曲类模型（yue2/ace-step，很慢）
  audio-eval --repeats 1          # 减少轮次（默认 2：首轮含加载，次轮为稳态）
"""
import argparse, base64, json, os, re, subprocess, sys, threading, time, urllib.request, wave, io, difflib


# 与框架 src/framework/text/chinese_normalization.cpp 同一套规则：
#   电话号码/长数字串 → 逐位读（1 读"幺"）；金额/数量 → 读基数词；年份 → 逐位读
_UNITS = ["", "十", "百", "千"]
_BIG = ["", "万", "亿"]


def verbalize_zh(text):
    """中文数字读法规范化——**委托给产品唯一的实现**（tools/audio_config.verbalize）。

    这里曾有一份独立实现（多一条固话规则、年份正则不带"年"前瞻），与产品实际发送的
    文本不一致，且同样带 \b 缺陷 → 评估台测的不是产品链路。评审要求收敛为单一来源。
    """
    import audio_config as _cfg
    return _cfg.verbalize(text)
SERVER = "http://127.0.0.1:8080"
OUTDIR = os.path.expanduser("~/.local/opt/audio.cpp/eval")

TEXTS = {
    # 配音场景：中文长句（含数字、多音字、专名）
    "zh": "三月十七号下午，我们在杭州西湖边的咖啡馆聊了两个小时。AI 语音合成这两年进步很快，但真正难的是把一句话讲得自然——尤其是数字、多音字和专有名词。",
    # 配音场景：英文
    "en": "The quick brown fox jumps over the lazy dog, and then reads a sentence that contains every letter of the alphabet.",
}
# 专项用例：多音字、数字、专名（暴露读法问题）
TRICKY = {
    "多音字": "他去银行取钱，然后步行去医院，路上听着音乐很快乐。重庆的厦门的六安的朋友都说这个方法很好。",
    "数字": "会议定在 2026 年 3 月 17 日下午 3 点 05 分，联系电话 13812345678，报价 1234.56 元，涨幅百分之三十。",
    # 国内稿最常见形态：数字紧贴汉字（旧用例全带空格，曾漏掉 \b 在 Python 下对汉字失效的 bug）
    "数字-紧凑": "会议定在2026年3月17日下午3点05分，联系电话13812345678，报价1234.56元，涨幅30%。",
    "专名缩写": "我们用 AI 和 GPU 做 5G 网络优化，WiFi 信号覆盖了百分之九十的区域，高铁 3 小时能到。",
}

# 歌曲场景（--include-gen）：歌词 + 风格
GEN_CASES = {
    "yue2": {"request": {"lyrics": "[Verse]\n凌晨的厨房还亮着灯\n[Chorus]\n别让我一个人悬在那句也许",
                       "options": {"style": "Mandarin Chinese R&B slow jam", "cot": "off",
                                   "num_inference_steps": "8", "semantic_max_tokens": "260", "seed": "831001"}}},
    "ace-step": {"request": {"text": "Mandarin Chinese R&B slow jam, warm male lead vocal",
                             "lyrics": "[Verse]\n凌晨的厨房还亮着灯",
                             "task_route": "text2music",
                             "options": {"language": "zh", "duration_seconds": "60"}}},
}


def post(path, payload, timeout=1800):
    """返回 (http_status, body)；HTTP 错误也返回 body，便于读 503 的 reason"""
    req = urllib.request.Request(SERVER + path, data=json.dumps(payload).encode(),
                                 headers={"Content-Type": "application/json"})
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read().decode() or "{}"), time.time() - t0
    except urllib.error.HTTPError as e:
        try: body = json.loads(e.read().decode() or "{}")
        except Exception: body = {"error": f"HTTP {e.code}"}
        return e.code, body, time.time() - t0
    except Exception as e:
        return 0, {"error": str(e)}, time.time() - t0


def post_retry(path, payload, tries=6, wait=5.0, timeout=1800):
    """503（内存预检/忙碌）时退避重试——生成与打分都跑在同一台机器上，内存会打架"""
    last = None
    for i in range(tries):
        code, body, wall = post(path, payload, timeout)
        if code == 200:
            return body, wall, None
        last = f"HTTP {code} {body.get('error', body.get('message', ''))}"
        if code != 503:
            break
        time.sleep(wait)
    return None, 0.0, last


def mem_free_pct():
    try:
        out = subprocess.run(["memory_pressure"], capture_output=True, text=True).stdout
        m = re.search(r"free percentage:\s*(\d+)%", out)
        return int(m.group(1)) if m else -1
    except Exception:
        return -1


def unload_all():
    post("/v1/tasks/unload_all_models", {}, timeout=60)
    time.sleep(2)   # 等内存真正回收，否则紧接着的请求会被内存预检拒


def server_pid():
    out = subprocess.run(["lsof", "-nP", "-iTCP:8080", "-sTCP:LISTEN", "-t"],
                         capture_output=True, text=True).stdout.split()
    return int(out[0]) if out else None


def rss_mb(pid):
    try:
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        return int(out) / 1024 if out else 0.0
    except Exception:
        return 0.0


class MemSampler:
    """后台采样服务器进程 RSS，取峰值"""
    def __init__(self):
        self.peak = 0.0; self._stop = False
        self.pid = server_pid()
    def __enter__(self):
        if self.pid:
            self.peak = rss_mb(self.pid)
            threading.Thread(target=self._loop, daemon=True).start()
        return self
    def _loop(self):
        while not self._stop:
            self.peak = max(self.peak, rss_mb(self.pid))
            time.sleep(0.25)
    def __exit__(self, *a):
        self._stop = True


_CN_DIGIT = {"零":0,"〇":0,"O":0,"一":1,"幺":1,"二":2,"两":2,"三":3,"四":4,"五":5,"六":6,"七":7,"八":8,"九":9}
_CN_UNIT = {"十":10,"百":100,"千":1000}
_CN_BIG = {"万":10000,"亿":100000000}


def _cn_run_to_num(run):
    """把一段中文数字（含十百千万亿、点、零）转成阿拉伯写法；失败返回原串"""
    if "点" in run:
        head, _, tail = run.partition("点")
        if all(c in _CN_DIGIT for c in tail) and tail:
            return f"{_cn_run_to_num(head)}.{''.join(str(_CN_DIGIT[c]) for c in tail)}"
        return run
    if not run or any(c not in _CN_DIGIT and c not in _CN_UNIT and c not in _CN_BIG for c in run):
        return run
    if all(c in _CN_DIGIT for c in run):          # 逐字读：二零二六 / 幺三八
        return "".join(str(_CN_DIGIT[c]) for c in run)
    total, section, num = 0, 0, 0                 # 带单位：十七 / 一百二十三 / 三千五百万
    for c in run:
        if c in _CN_DIGIT:
            num = _CN_DIGIT[c]
        elif c in _CN_UNIT:
            section += (num or 1) * _CN_UNIT[c]; num = 0
        elif c in _CN_BIG:
            total = (total + section + num) * _CN_BIG[c]; section = num = 0
    return str(total + section + num)


def normalize_numerals(s):
    """两侧统一成阿拉伯数字，避免"读作中文数字"被当成错误"""
    return re.sub(r"[零〇O一幺二两三四五六七八九十百千万亿]+(?:点[零〇一幺二两三四五六七八九]+)?",
                  lambda m: _cn_run_to_num(m.group(0)), s)


def recall(reference, hypothesis):
    """字符级召回：LCS 对齐中连续 >=2 字的匹配块占比"""
    norm = lambda s: re.sub(r"[^一-鿿A-Za-z0-9]", "", s).lower()   # 保留数字，否则读成中文数字会被误判
    a, b = normalize_numerals(norm(reference)), normalize_numerals(norm(hypothesis))
    if not a: return 0.0, 0, 0
    sm = difflib.SequenceMatcher(None, a, b, autojunk=False)
    matched = sum(bl.size for bl in sm.get_matching_blocks() if bl.size >= 2)
    return matched / len(a) * 100.0, matched, len(a)


def diff_snippet(ref, hyp, width=40):
    """标出首个不匹配处，便于人眼快速定位读错的字"""
    norm = lambda s: re.sub(r"[^\u4e00-\u9fffA-Za-z0-9]", "", s)
    a, b = normalize_numerals(norm(ref)), normalize_numerals(norm(hyp))
    sm = difflib.SequenceMatcher(None, a, b, autojunk=False)
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        if tag != "equal":
            lo = max(0, i1 - 8)
            return f"…{a[lo:i1]}【应为 {a[i1:i2] or '∅'}，读到 {b[j1:j2] or '∅'}】{a[i2:i2+8]}…"
    return ""


def wav_info(raw):
    with wave.open(io.BytesIO(raw)) as w:
        return w.getnframes() / w.getframerate(), w.getframerate()


def generate(model, text, wavpath, voice_ref=None):
    """只生成，返回 (耗时, 峰值MB, 音频秒数, 采样率, 错误)"""
    req = {"text": text}
    if voice_ref: req["voice_ref"] = voice_ref
    with MemSampler() as mem:
        resp, wall, err = post_retry("/v1/tasks/run", {"model": model, "request": req})
        if err and "voice-ref" in str(err) and not voice_ref:
            return None, mem.peak, None, None, err + "（用 --voice-ref 指定参考音色后可测）"
        if err:
            return None, mem.peak, None, None, err
        if isinstance(resp, dict) and "error" in resp:
            return None, mem.peak, None, None, str(resp["error"])
    raw = base64.b64decode(resp["audio"])
    dur, sr = wav_info(raw)
    open(wavpath, "wb").write(raw)
    return wall, mem.peak, dur, sr, None


def transcribe(path, asr_model):
    """打分用 ASR；失败返回 None"""
    r, _, err = post_retry("/v1/tasks/run", {"model": asr_model, "request": {"audio": path}}, timeout=900)
    if err: return None, err
    if isinstance(r, dict) and "error" in r: return None, str(r["error"])
    return r.get("text", ""), None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", nargs="*", default=None)
    ap.add_argument("--texts", nargs="*", default=["zh", "en", "多音字", "数字", "数字-紧凑", "专名缩写"])
    ap.add_argument("--repeats", type=int, default=2)
    ap.add_argument("--include-gen", action="store_true")
    ap.add_argument("--voice-ref", default=None, help="需要参考音色的模型（index-tts2 等）用；也可让报错中的模型自动重试")
    ap.add_argument("--asr-model", default="qwen3-asr", help="打分用 ASR（audio8-asr 小但对合成语音会退化，默认 qwen3-asr）")
    ap.add_argument("--tag", default="")
    ap.add_argument("--raw", action="store_true",
                    help="关闭文本兜底，直接送原文（默认按产品配置启用兜底——评估台应测产品实际发送的文本）")
    ap.add_argument("--detail", action="store_true", help="所有用例都打印原文/读文对照")
    a = ap.parse_args()

    try:
        reg = json.load(urllib.request.urlopen(SERVER + "/v1/models", timeout=10))["data"]
    except Exception as e:
        sys.exit(f"服务不可达（先 audio-service server ensure）: {e}")
    tts = [m["id"] for m in reg if m.get("task") == "tts" and not m["id"].endswith("-stream")]
    models = a.models or tts
    os.makedirs(OUTDIR, exist_ok=True)

    free = mem_free_pct()
    load = os.getloadavg()[0]
    print(f"机器状态: 可用内存 {free}% · 负载 {load:.1f}" + ("   ⚠️ 负载高，耗时数据会偏大" if load > 4 else ""))
    print(f"评估台 · 服务 {SERVER} · 模型 {', '.join(models)} · 打分 ASR {a.asr_model}\n")
    wavdir = os.path.join(OUTDIR, "wav")
    os.makedirs(wavdir, exist_ok=True)
    rows, pending = [], []

    # —— 阶段 1：生成（保持模型常驻，测真实推理耗时）——
    unload_all()
    for model in models:
        for key in a.texts:
            text = TEXTS.get(key) or TRICKY.get(key)
            if not text: continue
            for r in range(1, a.repeats + 1):
                wavpath = os.path.join(wavdir, f"{model}-{key}-{r}.wav")
                spoken = text if a.raw else verbalize_zh(text)
                if not a.raw and spoken != text and r == 1:
                    print(f"      TN: {spoken[:70]}")
                wall, peak, dur, sr, err = generate(model, spoken, wavpath, a.voice_ref)
                if err:
                    hint = "（内存不足，已卸载其他模型）" if "503" in err else ""
                    print(f"  ❌ {model:<16} {key} #{r}: {err}{hint}")
                    rows.append({"model": model, "case": key, "round": r, "error": err})
                    unload_all(); continue
                rtf = wall / dur if dur else 0
                print(f"  {model:<16} {key} #{r}  音频 {dur:5.1f}s  耗时 {wall:6.1f}s  RTF {rtf:5.2f}  峰值 {peak:6.0f}MB")
                rows.append({"model": model, "case": key, "round": r, "audio_s": round(dur, 2),
                             "wall_s": round(wall, 2), "rtf": round(rtf, 2), "peak_mb": round(peak),
                             "sample_rate": sr, "wav": wavpath})
                pending.append((rows[-1], text, wavpath))

    # —— 阶段 2：卸载后统一 ASR 打分（避免与生成模型抢内存）——
    if pending:
        print(f"\n  打分中（卸载生成模型后加载 {a.asr_model}，{len(pending)} 条）…")
        unload_all()
        for row, text, wavpath in pending:
            hyp, err = transcribe(wavpath, a.asr_model)
            if err:
                print(f"  ❌ 打分失败 {row['model']} {row['case']} #{row['round']}: {err}")
                row["asr_error"] = err; continue
            rec, matched, total = recall(text, hyp or "")
            row.update({"recall": round(rec, 1), "matched": matched, "ref_chars": total,
                        "hyp": (hyp or "")[:200]})
            flag = "⚠️" if rec < 70 else "  "
            print(f"  {flag} {row['model']:<16} {row['case']} #{row['round']}  可懂度 {rec:5.1f}%  ({matched}/{total} 字)")
            if rec < 95 or a.detail:
                d = diff_snippet(text, hyp or "")
                if d: print(f"      原: {text[:60]}\n      读: {(hyp or '')[:60]}\n      差: {d}")

    if a.include_gen:
        for model, case in GEN_CASES.items():
            print(f"\n  [gen] {model} …（较慢）")
            with MemSampler() as mem:
                try:
                    resp, wall = post("/v1/tasks/run", {"model": model, "request": case["request"]}, timeout=3600)
                    raw = base64.b64decode(resp["audio"]); dur, sr = wav_info(raw)
                    open(os.path.join(wavdir, f"{model}-gen.wav"), "wb").write(raw)
                    print(f"  {model:<16} gen    音频 {dur:5.1f}s  耗时 {wall:6.1f}s  RTF {wall/dur:5.2f}  峰值 {mem.peak:6.0f}MB")
                    rows.append({"model": model, "case": "gen", "round": 1, "audio_s": round(dur, 2),
                                 "wall_s": round(wall, 2), "rtf": round(wall / dur, 2), "peak_mb": round(mem.peak)})
                except Exception as e:
                    print(f"  ❌ {model}: {e}"); rows.append({"model": model, "case": "gen", "round": 1, "error": str(e)})

    stamp = time.strftime("%Y%m%d-%H%M%S")
    path = os.path.join(OUTDIR, f"results-{stamp}{('-' + a.tag) if a.tag else ''}.json")
    json.dump({"ts": stamp, "server": SERVER, "metric": "recall-v3-numerals", "rows": rows}, open(path, "w"), ensure_ascii=False, indent=1)

    # 与上一次对比
    prev = sorted(f for f in os.listdir(OUTDIR) if f.startswith("results-") and f != os.path.basename(path))
    if prev:
        try: old = json.load(open(os.path.join(OUTDIR, prev[-1])))
        except Exception: old = None
        if old and old.get("metric") != "recall-v3-numerals":
            print("  （上次结果用的是旧口径，本次跳过对比）")
            old = None
        if old is None: prev = []
        oldmap = {(r["model"], r["case"], r["round"]): r for r in (old["rows"] if old else []) if "recall" in r}
        print(f"\n=== 与上次对比（{prev[-1]}）===")
        for r in rows:
            o = oldmap.get((r["model"], r["case"], r["round"]))
            if o and "recall" in r:
                d_rec, d_wall = r["recall"] - o["recall"], r["wall_s"] - o["wall_s"]
                flag = "⚠️" if d_rec < -3 else "  "
                print(f"  {flag} {r['model']:<16} {r['case']} #{r['round']}  可懂度 {d_rec:+.1f}pp  耗时 {d_wall:+.1f}s")
    print(f"\n结果: {path}")


if __name__ == "__main__":
    main()
