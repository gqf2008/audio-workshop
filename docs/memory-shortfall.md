# 内存不足（503 insufficient_memory）处理

## 识别

服务端只在 JSON 的 `error.type == "insufficient_memory"` 时算结构化 OOM。应用只读这个字段，不从人类可读的 message 里抠数字或关键词。原始 `message` 会完整保留，里面通常已经包含模型名、估算内存、headroom 和当前可用内存。

唯一实现位于 `crates/aw-core/src/audio_client.rs`：

- `memory_shortfall(body)`：结构化识别；
- `memory_shortfall_note(body)`：唯一用户文案，包含三个动作；
- `ClientError::Display`：配音 / BGM / 歌曲 / 质检等所有走 audiocpp 客户端的路径消费同一份文案；人声分离若上游错误体带 `insufficient_memory`，也经 `separation_error_note` 复用同一入口。
- `ClientError::is_insufficient_memory()`：给配音队列标 `error: oom` 用。

非 JSON、空 body、别的 `error.type` 都不会被说成 OOM；普通 503 仍按原有退避重试策略处理。

## 配音队列

内存不足是明确拒绝，不是服务忙：`post_with_retry` 不会对 OOM 请求自动重试。该句在 `project.json` 里标成：

```text
error: oom: 内存不足（OOM）：...释放内存后继续：① 释放模型内存（会卸载服务上所有已加载模型，先确认没有其它任务在用）；② 把模型降到 q4_0 量化档；③ 关掉其它占内存的应用。
```

这一句保留在队列里，其他已成功句子不受影响。用户点「继续合成」时，worker 只取 `error:` 的工程句子下标重跑，done 句子不会再次请求。

## 释放模型内存按钮

OOM 句子展开后会出现「释放模型内存」按钮。点击后先弹二次确认，明确说明它会卸载服务上所有模型的常驻内存、可能影响其它正在使用该服务的任务；只有确认才发请求，取消什么都不做。它只在用户确认时调用：

```text
POST /v1/tasks/unload_all_models
```

按钮在有任务在飞时拒绝执行；服务不可达、非 2xx、返回体异常都会如实展示失败，不会假装卸载成功。成功时会展示服务返回的 `unloaded` 名称；空数组会显示「当前没有可卸载的已加载模型」。

本批不自动调用卸载，也不改服务端内存守卫阈值，不做自动降量化档重跑。
