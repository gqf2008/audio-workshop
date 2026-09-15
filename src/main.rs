//! 音频作坊 · 配音工作台（M1 桌面壳原型）
//!
//! 定位：把界面跑起来、把关键交互接上线。**不含真实合成**——
//! 切句、时长、进度、导出全部是桩数据，真实链路在 M0 的
//! `tools/audio_config.py` + `audiocpp_server`（见 CHARTER 第 6/9 节）。
//!
//! 接线关系：
//!   - `ui/app.slint`          主窗口：无边框标题栏 / 场景导航 / 主题切换 / 状态栏
//!   - `ui/dub_workbench.slint` 配音工作台：稿件 / 音色 / 导出 / 句子列表 / 时间轴与任务
//!   - `ui/model.slint`         数据模型（Sentence / Voice），字段对齐配置层产物
//!
//! 桩数据的集中点就在本文件底部：`SAMPLE_SCRIPT`、`VOICES`，
//! 以及 `build_rows` 里的时长估算（中文口播按 0.18 秒/字，约 5.5 字/秒）。

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::include_modules!();

// 把生成的窗口类型适配到组件库的接线契约（固有方法优先于 trait 方法）
slint_pixel::impl_title_bar_ui!(MainWindow);
slint_pixel::impl_resize_ui!(MainWindow);

/// 主循环节拍：40ms。
const TICK_MS: u64 = 40;
/// 每个节拍推进的合成进度（≈ 10 秒跑完，纯演示用）。
const SYNTH_PER_TICK: f32 = 0.004;
/// 中文口播时长估算：秒/字。
const SECS_PER_CHAR: f32 = 0.18;
/// 「单句重录」要等几个节拍才回填结果（≈ 0.5 秒）。
const REDO_TICKS: i32 = 12;

const DEFAULT_PROJECT: &str = "示例工程 · 频道口播";

/// 示例稿（桩）：换真实场景时从剪贴板 / 文件来。
const SAMPLE_SCRIPT: &str = "大家好，欢迎回到音频作坊。今天聊三件事。\
第一，声音是你自己的，素材不出机器，断网也能干活。\
第二，不按字收费，想生成多少就生成多少。\
第三，配音先行，BGM 和歌曲排在后面，成熟一个上一个。";

/// 音色清单（桩）。真实来源是 config/models.schema.yaml：
/// 许可一栏对应 `product_excluded`（红线见 CHARTER 第 5 节：只做下载器，不打包权重）。
const VOICES: [(&str, &str, &str, &str); 4] = [
    (
        "晓晨 · 本地参考",
        "audio8-tts 0.6B · Metal",
        "中文男声，语速稳、断句规矩。",
        "仅自用",
    ),
    (
        "林夏 · 本地参考",
        "audio8-tts 0.6B · Metal",
        "中文女声，偏软，适合知识类。",
        "仅自用",
    ),
    (
        "阿哲 · 参考音频",
        "index-tts2 · voice_ref",
        "需 10 秒参考音频，更贴原声。",
        "仅自用",
    ),
    (
        "旁白 · 通用",
        "audio8-tts 0.6B · Metal",
        "中性旁白，纪录片式叙述。",
        "可商用",
    ),
];

/// 导航到非配音场景时的提示（配音先行，其余占位）。
const SCENE_NOTES: [&str; 4] = [
    "配音工作台",
    "BGM 场景：M2 接入（与配音同框，自动 ducking）",
    "歌曲场景：M4 接入（yue2 / ace-step，降级为彩蛋）",
    "素材库：M4 接入（工程版本 / 音色库 / 发音词典库）",
];

/// 跨回调共享的桩状态（用 Cell 免去 RefCell 借用冲突）。
struct Stub {
    /// 正在「单句重录」的行号（-1 = 没有）
    redo_row: Cell<i32>,
    /// 距回填结果还剩几个节拍
    redo_left: Cell<i32>,
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;

    let rows: Rc<VecModel<Sentence>> = Rc::new(VecModel::default());
    let stub = Rc::new(Stub {
        redo_row: Cell::new(-1),
        redo_left: Cell::new(0),
    });

    // ── 初始数据 ──
    let voices = voice_rows();
    let names: Vec<SharedString> = voices.iter().map(|v| v.name.clone()).collect();
    ui.set_voice_names(ModelRc::from(Rc::new(VecModel::from(names))));
    ui.set_voices(ModelRc::from(Rc::new(VecModel::from(voices))));
    ui.set_export_dir(export_dir().into());
    ui.set_project_name(DEFAULT_PROJECT.into());
    ui.set_sentences(ModelRc::from(rows.clone()));
    rebuild(&ui, &rows, SAMPLE_SCRIPT);
    ui.set_status_text("就绪：示例稿已切句，点「开始合成」跑一遍流程".into());

    // ── 窗口控制：拖拽 / 最小化 / 最大化 / 关闭 / 四边缩放 ──
    slint_pixel::install_title_bar_controls(&ui);
    slint_pixel::install_window_resize(&ui);

    wire_theme(&ui);
    wire_script(&ui, &rows);
    wire_sentence_actions(&ui, &rows, &stub);
    wire_run(&ui, &rows);
    wire_export(&ui);

    // ── 主循环节拍：合成进度 / 单句重录回填 / 试听播放头 ──
    // timer 是 main 的局部变量，存活到 ui.run() 返回之后，无需泄漏。
    let timer = Timer::default();
    {
        let weak = ui.as_weak();
        let rows = rows.clone();
        let stub = stub.clone();
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(TICK_MS),
            move || {
                if let Some(ui) = weak.upgrade() {
                    tick(&ui, &rows, &stub);
                }
            },
        );
    }

    ui.run()
}

// ===========================================================================
// 回调接线
// ===========================================================================

fn wire_theme(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.on_theme_picked(move |scheme| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_theme_scheme() == scheme {
            return;
        }
        let label = if scheme == "dark" { "暗色" } else { "浅色" };
        ui.set_theme_scheme(scheme);
        ui.set_status_text(format!("主题已切换：{label}").into());
    });

    let weak = ui.as_weak();
    ui.on_scene_changed(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let note = SCENE_NOTES.get(i.max(0) as usize).copied().unwrap_or("");
        ui.set_status_text(note.into());
    });
}

fn wire_script(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>) {
    let weak = ui.as_weak();
    ui.on_project_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_status_text(format!("工程名：{}", ui.get_project_name()).into());
    });

    let weak = ui.as_weak();
    let rows1 = rows.clone();
    ui.on_script_edited(move || {
        let Some(ui) = weak.upgrade() else { return };
        let text = ui.get_script_text();
        rebuild(&ui, &rows1, &text);
        ui.set_status_text(
            format!(
                "稿件已更新：{} 字 / {} 句",
                ui.get_char_count(),
                rows1.row_count()
            )
            .into(),
        );
    });

    let weak = ui.as_weak();
    let rows2 = rows.clone();
    ui.on_use_sample(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_script_text(SAMPLE_SCRIPT.into());
        rebuild(&ui, &rows2, SAMPLE_SCRIPT);
        ui.set_status_text(format!("已载入示例稿：{} 句", rows2.row_count()).into());
    });

    let weak = ui.as_weak();
    let rows3 = rows.clone();
    ui.on_clear_script(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_script_text("".into());
        rebuild(&ui, &rows3, "");
        ui.set_status_text("稿件已清空，粘一段口播稿试试".into());
    });

    let weak = ui.as_weak();
    let rows4 = rows.clone();
    ui.on_resplit(move || {
        let Some(ui) = weak.upgrade() else { return };
        let text = ui.get_script_text();
        rebuild(&ui, &rows4, &text);
        let n = rows4.row_count();
        ui.set_status_text(format!("已重新切句：{n} 句").into());
        toast(&ui, &format!("已重新切句：{n} 句"));
    });
}

fn wire_sentence_actions(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, stub: &Rc<Stub>) {
    let weak = ui.as_weak();
    let model1 = rows.clone();
    ui.on_select_sentence(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_selected(i);
        if let Some(row) = model1.row_data(i.max(0) as usize) {
            ui.set_status_text(
                format!(
                    "已选中第 {} 句 · 起始 {} · 时长 {}",
                    i + 1,
                    row.start_label,
                    row.duration_label
                )
                .into(),
            );
        }
    });

    let weak = ui.as_weak();
    let model2 = rows.clone();
    ui.on_preview_one(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(row) = model2.row_data(i.max(0) as usize) else {
            return;
        };
        let total = ui.get_total_duration().max(0.001);
        ui.set_selected(i);
        ui.set_playhead((row.start / total).clamp(0.0, 1.0));
        ui.set_playing(true);
        ui.set_status_text(format!("试听第 {} 句：{}", i + 1, row.text).into());
    });

    let weak = ui.as_weak();
    let model3 = rows.clone();
    let stub1 = stub.clone();
    ui.on_redo_one(move |i| {
        if i < 0 {
            return;
        }
        let Some(ui) = weak.upgrade() else { return };
        let idx = i as usize;
        if model3.row_data(idx).is_none() {
            return;
        }
        ui.set_selected(i);
        set_status(&model3, idx, "合成中");
        stub1.redo_row.set(i);
        stub1.redo_left.set(REDO_TICKS);
        ui.set_status_text(format!("单句重录中：第 {} 句（只重跑这一句）", i + 1).into());
    });

    let weak = ui.as_weak();
    ui.on_seek(move |p| {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_playhead(p.clamp(0.0, 1.0));
    });
}

fn wire_run(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>) {
    let weak = ui.as_weak();
    let model4 = rows.clone();
    ui.on_start_run(move || {
        let Some(ui) = weak.upgrade() else { return };
        let n = model4.row_count();
        if n == 0 {
            ui.set_status_text("稿件为空：先粘稿子或点「载入示例稿」".into());
            return;
        }
        for i in 0..n {
            set_status(&model4, i, "待合成");
        }
        ui.set_progress(0.0);
        ui.set_playhead(0.0);
        ui.set_playing(false);
        ui.set_done_count(0);
        ui.set_has_result(false);
        ui.set_running(true);
        ui.set_status_text("合成中 · audio8-tts 0.6B · Metal（桩）".into());
    });

    let weak = ui.as_weak();
    ui.on_stop_run(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_running(false);
        ui.set_status_text("已停止合成".into());
    });

    let weak = ui.as_weak();
    ui.on_preview_all(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_playhead(0.0);
        ui.set_playing(true);
        ui.set_status_text("试听全篇（桩音频）".into());
    });

    let weak = ui.as_weak();
    ui.on_speed_changed(move |v| {
        let Some(ui) = weak.upgrade() else { return };
        let label = format!("{v:.2}x");
        ui.set_speed_label(label.clone().into());
        ui.set_status_text(format!("语速 {label}（真实链路写回模型参数）").into());
    });
}

fn wire_export(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.on_export_wav(move || {
        let Some(ui) = weak.upgrade() else { return };
        let stem = file_stem(&ui.get_project_name());
        let path = format!("{}/{}.wav", ui.get_export_dir(), stem);
        ui.set_status_text(format!("已导出整段 WAV：{path}（桩：未真正落盘）").into());
        toast(&ui, &format!("WAV 已导出：{stem}.wav"));
    });

    let weak = ui.as_weak();
    ui.on_export_srt(move || {
        let Some(ui) = weak.upgrade() else { return };
        let stem = file_stem(&ui.get_project_name());
        let path = format!("{}/{}.srt", ui.get_export_dir(), stem);
        ui.set_status_text(format!("已导出逐句 SRT：{path}（桩：未真正落盘）").into());
        toast(&ui, &format!("SRT 已导出：{stem}.srt"));
    });
}

// ===========================================================================
// 主循环节拍
// ===========================================================================

fn tick(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, stub: &Rc<Stub>) {
    if ui.get_running() {
        let next = ui.get_progress() + SYNTH_PER_TICK;
        if next >= 1.0 {
            ui.set_progress(1.0);
            ui.set_running(false);
            ui.set_has_result(true);
            for i in 0..rows.row_count() {
                set_status(rows, i, "已合成");
            }
            ui.set_done_count(rows.row_count() as i32);
            ui.set_status_text("合成完成：可试听、可导出 WAV / SRT".into());
        } else {
            ui.set_progress(next);
            apply_progress(ui, rows, next);
        }
    }

    if stub.redo_left.get() > 0 {
        let left = stub.redo_left.get() - 1;
        stub.redo_left.set(left);
        if left == 0 {
            let row = stub.redo_row.get();
            stub.redo_row.set(-1);
            if row >= 0 {
                set_status(rows, row as usize, "已合成");
                ui.set_status_text(format!("第 {} 句重录完成", row + 1).into());
            }
        }
    }

    if ui.get_playing() {
        let total = ui.get_total_duration().max(0.001);
        let next = ui.get_playhead() + (TICK_MS as f32 / 1000.0) / total;
        if next >= 1.0 {
            ui.set_playhead(0.0);
            ui.set_playing(false);
            ui.set_status_text("试听结束".into());
        } else {
            ui.set_playhead(next);
        }
    }
}

fn apply_progress(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, progress: f32) {
    let n = rows.row_count();
    if n == 0 {
        return;
    }
    let done = ((progress * n as f32) as usize).min(n);
    for i in 0..n {
        let want = if i < done {
            "已合成"
        } else if i == done {
            "合成中"
        } else {
            "待合成"
        };
        set_status(rows, i, want);
    }
    ui.set_done_count(done as i32);
    if done < n {
        ui.set_selected(done as i32);
    }
    ui.set_status_text(format!("合成中 · 已完成 {done}/{n} 句").into());
}

// ===========================================================================
// 桩数据构造（接真实链路时只改这一段）
// ===========================================================================

/// 稿件 → 逐句模型：估时长、排起始时间、填展示用文案。
fn build_rows(lines: &[String]) -> Vec<Sentence> {
    let mut start = 0.0_f32;
    let mut rows = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let duration = (line.chars().count() as f32 * SECS_PER_CHAR).max(0.6);
        rows.push(Sentence {
            no: i as i32 + 1,
            text: line.as_str().into(),
            status: "待合成".into(),
            duration,
            start,
            duration_label: format!("{duration:.1}s").into(),
            start_label: clock_label(start).into(),
        });
        start += duration;
    }
    rows
}

/// 按 。！？；与换行切句（口播稿常一行一句）。
fn split_sentences(text: &str) -> Vec<String> {
    const ENDERS: [char; 6] = ['。', '！', '？', '；', '!', '?'];
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '\n' {
            push_trimmed(&mut out, &mut cur);
            continue;
        }
        cur.push(ch);
        if ENDERS.contains(&ch) {
            push_trimmed(&mut out, &mut cur);
        }
    }
    push_trimmed(&mut out, &mut cur);
    out
}

fn push_trimmed(out: &mut Vec<String>, cur: &mut String) {
    let line = cur.trim();
    if !line.is_empty() {
        out.push(line.to_string());
    }
    cur.clear();
}

fn voice_rows() -> Vec<Voice> {
    VOICES
        .iter()
        .map(|(name, engine, note, license)| Voice {
            name: (*name).into(),
            engine: (*engine).into(),
            note: (*note).into(),
            license: (*license).into(),
        })
        .collect()
}

fn export_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{home}/Documents/音频作坊")
}

// ===========================================================================
// 小工具
// ===========================================================================

/// 重算整条链路的派生状态（切句 / 字数 / 总时长 / 复位运行态）。
fn rebuild(ui: &MainWindow, rows: &Rc<VecModel<Sentence>>, text: &str) {
    let built = build_rows(&split_sentences(text));
    let total: f32 = built.iter().map(|row| row.duration).sum();
    let chars = text.chars().count();

    rows.set_vec(built);
    ui.set_total_duration(if total > 0.0 { total } else { 1.0 });
    ui.set_total_label(clock_label(total).into());
    ui.set_char_count(chars as i32);
    ui.set_est_duration(if chars == 0 {
        "--".into()
    } else {
        clock_label(total).into()
    });
    ui.set_selected(if rows.row_count() > 0 { 0 } else { -1 });
    ui.set_done_count(0);
    ui.set_has_result(false);
    ui.set_progress(0.0);
    ui.set_playhead(0.0);
    ui.set_running(false);
    ui.set_playing(false);
}

fn set_status(rows: &Rc<VecModel<Sentence>>, i: usize, status: &str) {
    let Some(mut row) = rows.row_data(i) else {
        return;
    };
    if row.status.as_str() == status {
        return;
    }
    row.status = SharedString::from(status);
    rows.set_row_data(i, row);
}

fn clock_label(secs: f32) -> String {
    let total = secs.max(0.0);
    let minutes = (total / 60.0) as u32;
    let seconds = (total % 60.0) as u32;
    format!("{minutes}:{seconds:02}")
}

fn file_stem(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "未命名工程".to_string()
    } else {
        trimmed.to_string()
    }
}

fn toast(ui: &MainWindow, text: &str) {
    ui.set_toast_text(text.into());
    ui.set_toast_shown(true);
}
