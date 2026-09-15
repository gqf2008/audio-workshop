#!/usr/bin/env python3
"""跨平台路径与推理后端发现（M3）。

优先级：
- server.json：AW_SERVER_CONFIG > 平台配置目录 > macOS 旧路径
- eval 目录：AW_EVAL_DIR > <server.json 所在目录>/eval
- models_root：AW_MODELS_ROOT / AUDIO_WORKSHOP_MODELS_ROOT > schema 值（存在时）> ~/models/audio-workshop
- backend：AW_BACKEND > schema 的显式值（非 auto）> Darwin=metal / nvidia-smi=cuda / 其它=cpu
"""
import os
import shutil
import sys
from pathlib import Path


def legacy_dir() -> Path:
    return Path.home() / ".local" / "opt" / "audio.cpp"


def config_dir() -> Path:
    if os.name == "nt":
        base = os.environ.get("APPDATA") or os.environ.get("LOCALAPPDATA")
        if base:
            return Path(base) / "audio.cpp"
        return Path.home() / "AppData" / "Roaming" / "audio.cpp"
    if sys.platform == "darwin":
        return legacy_dir()
    xdg = os.environ.get("XDG_CONFIG_HOME")
    return Path(xdg) / "audio.cpp" if xdg else Path.home() / ".config" / "audio.cpp"


def server_json_path() -> Path:
    env = os.environ.get("AW_SERVER_CONFIG")
    if env:
        return Path(env).expanduser()
    candidates = [config_dir() / "server.json", legacy_dir() / "server.json"]
    for path in candidates:
        if path.is_file():
            return path
    return candidates[0]


def eval_dir_path() -> Path:
    env = os.environ.get("AW_EVAL_DIR")
    if env:
        return Path(env).expanduser()
    return server_json_path().parent / "eval"


def resolve_models_root(default: str) -> str:
    env = os.environ.get("AW_MODELS_ROOT") or os.environ.get("AUDIO_WORKSHOP_MODELS_ROOT")
    if env:
        return str(Path(env).expanduser())
    if default and Path(default).exists():
        return default
    return str(Path.home() / "models" / "audio-workshop")


def detect_backend(configured: str = "auto") -> str:
    env = os.environ.get("AW_BACKEND")
    if env:
        return env.lower()
    if configured and configured != "auto":
        return configured.lower()
    if sys.platform == "darwin":
        return "metal"
    if shutil.which("nvidia-smi"):
        return "cuda"
    return "cpu"


if __name__ == "__main__":
    print(f"server_json={server_json_path()}")
    print(f"eval_dir={eval_dir_path()}")
    print(f"backend={detect_backend()}")
