//! 音频作坊核心库：与 UI 无关的部分。
//!
//! - [`text_layer`]：数字读法规范化 + 发音词典（与 Python 侧 tools/audio_config.py 同规则，
//!   由 `tests/parity_cases.txt` 对照固定）
//! - [`audio_client`]：audiocpp_server 客户端（错误体透出、按状态码判 503 退避重试）
//! - [`dub`]：配音链路（切句 / 逐句合成 / 拼装 + 句子级时间轴 / 单句重录）
//! - [`bgm`]：BGM 生成 / 时长对齐 / ducking / 三轨导出
//! - [`song`]：yue2 / ace-step 文生歌与 sheetsage2 翻唱链路
//! - [`eval`]：质检（ASR 回读可懂度 + 首个差异定位；口径与 CHARTER M0 一致）
//! - [`ref_audio`]：参考音频时长读取 + 30s 硬上限判据（克隆请求的前置护栏）

pub mod audio_client;
pub mod bgm;
pub mod dub;
pub mod eval;
pub mod ref_audio;
pub mod separate;
pub mod song;
pub mod text_layer;

pub use audio_client::{
    memory_shortfall, memory_shortfall_note, parse_insufficient_memory, Client, ClientError,
    InsufficientMemory, MemoryShortfall, VoiceClone, VoiceSource, DEFAULT_ASR_MODEL,
    MISSING_REFERENCE_PATH, SYNTH_REQUEST_OPTIONS,
};
pub use bgm::{
    assemble_bgm, bgm_artifacts_from_disk, bgm_only_artifacts, generate_segments,
    generate_segments_stoppable, mix_project, BgmArtifacts, BgmOptions, BgmRun,
};
pub use dub::{
    split_sentences, srt_timestamp, Assembled, Project, Sentence, DEFAULT_PUNCTUATION,
    REDO_SEED_STEP,
};
pub use eval::{diff_snippet, intelligibility, Intelligibility};
pub use ref_audio::{
    reference_duration_seconds, reference_over_limit, reference_too_long, REFERENCE_MAX_SECONDS,
};
pub use separate::{
    separate_tracks, Progress as SeparationProgress, SeparatedTracks, SeparationOutcome,
    SeparationRequest, DEFAULT_MODEL as DEFAULT_SEPARATION_MODEL,
};
pub use song::{generate_cover, generate_song, transcribe_abc, SongModel, SongOptions};
pub use text_layer::{apply_dictionary, cardinal, digits_zh, normalize, verbalize};
