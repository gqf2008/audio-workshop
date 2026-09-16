//! 音频作坊核心库：与 UI 无关的部分。
//!
//! - [`text_layer`]：数字读法规范化 + 发音词典（与 Python 侧 tools/audio_config.py 同规则，
//!   由 `tests/parity_cases.txt` 对照固定）
//! - [`audio_client`]：audiocpp_server 客户端（错误体透出、按状态码判 503 退避重试）
//! - [`dub`]：配音链路（切句 / 逐句合成 / 拼装 + 句子级时间轴 / 单句重录）
//! - [`bgm`]：BGM 生成 / 时长对齐 / ducking / 三轨导出
//! - [`song`]：yue2 / ace-step 文生歌与 sheetsage2 翻唱链路

pub mod audio_client;
pub mod bgm;
pub mod dub;
pub mod song;
pub mod text_layer;

pub use audio_client::{Client, ClientError};
pub use bgm::{assemble_bgm, generate_segments, mix_project, BgmArtifacts, BgmOptions};
pub use dub::{
    split_sentences, srt_timestamp, Assembled, Project, Sentence, DEFAULT_INSTRUCTION,
    DEFAULT_PUNCTUATION, REDO_SEED_STEP,
};
pub use song::{generate_cover, generate_song, transcribe_abc, SongModel, SongOptions};
pub use text_layer::{apply_dictionary, cardinal, digits_zh, normalize, verbalize};
