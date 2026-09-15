//! 音频作坊核心库：与 UI 无关的部分。
//!
//! - [`text_layer`]：数字读法规范化 + 发音词典（与 Python 侧 tools/audio_config.py 同规则）
//! - [`audio_client`]：audiocpp_server 客户端（错误体透出、503 退避重试）
//! - [`dub`]：配音链路（切句 / 逐句合成 / 拼装 + 句子级时间轴 / 单句重录）

pub mod audio_client;
pub mod dub;
pub mod text_layer;

pub use audio_client::{Client, ClientError};
pub use dub::{split_sentences, srt_timestamp, Project, Sentence};
pub use text_layer::{apply_dictionary, cardinal, digits_zh, normalize, verbalize};
