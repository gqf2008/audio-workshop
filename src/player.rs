//! 校听播放器：rodio 薄封装。
//!
//! 只播本地 wav——逐句试听播 `sentences/xxx.wav`，全篇试听播 `out/final.wav`
//! （没拼成品时退化为按序播全部已合成句）。倍速即时生效（rodio `Sink::set_speed`，
//! 只影响回放，不动合成产物；合成语速是模型参数，属配置层范畴）。
//!
//! 无音频输出（无头/远程会话）时 `Player::disabled()` 兜底：界面照开，
//! 试听给明确报错而不是 panic。

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

pub struct Player {
    /// 流必须活到播放结束：留在这里别动（下划线是刻意的）
    _stream: Option<rodio::OutputStream>,
    sink: Option<rodio::Sink>,
}

impl Player {
    pub fn open() -> Result<Self, String> {
        let (stream, handle) =
            rodio::OutputStream::try_default().map_err(|e| format!("打开音频输出失败: {e}"))?;
        let sink = rodio::Sink::try_new(&handle).map_err(|e| format!("创建播放槽失败: {e}"))?;
        Ok(Self {
            _stream: Some(stream),
            sink: Some(sink),
        })
    }

    /// 无音频输出的兜底：所有播放方法返回明确报错
    pub fn disabled() -> Self {
        Self {
            _stream: None,
            sink: None,
        }
    }

    /// 单曲播放（替换正在播的内容）
    pub fn play_wav(&self, path: &Path) -> Result<(), String> {
        let Some(sink) = &self.sink else {
            return Err("音频输出不可用（无法试听）".into());
        };
        let file = File::open(path).map_err(|e| format!("打不开 {}: {e}", path.display()))?;
        let source = rodio::Decoder::new(BufReader::new(file))
            .map_err(|e| format!("不是可解码的音频 {}: {e}", path.display()))?;
        sink.stop();
        sink.append(source);
        sink.play();
        Ok(())
    }

    /// 队列连播（全篇试听：逐句文件按序追加，天然自动跳下一句）
    pub fn play_many(&self, paths: &[PathBuf]) -> Result<(), String> {
        let Some(sink) = &self.sink else {
            return Err("音频输出不可用（无法试听）".into());
        };
        if paths.is_empty() {
            return Err("没有已合成的句子可播".into());
        }
        sink.stop();
        for p in paths {
            let file = File::open(p).map_err(|e| format!("打不开 {}: {e}", p.display()))?;
            let source = rodio::Decoder::new(BufReader::new(file))
                .map_err(|e| format!("不是可解码的音频 {}: {e}", p.display()))?;
            sink.append(source);
        }
        sink.play();
        Ok(())
    }

    pub fn stop(&self) {
        if let Some(sink) = &self.sink {
            sink.stop();
        }
    }

    pub fn is_playing(&self) -> bool {
        self.sink.as_ref().map(|s| !s.empty()).unwrap_or(false)
    }

    /// 倍速即时生效（rodio 0.20 `Sink::set_speed`）
    pub fn set_speed(&self, v: f32) {
        if let Some(sink) = &self.sink {
            sink.set_speed(v);
        }
    }
}
