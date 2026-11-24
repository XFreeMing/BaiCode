# 查看详细的模型调用日志与分析指南

本文档说明如何查看、导出和分析 BaiCode (`baic`) 的模型调用日志，用于排查提示词问题、定位工具调用失败点以及优化 Agent 执行流程。

---

## 日志来源总览

| 维度 | 存储位置 / 命令 | 适用场景 |
| --- | --- | --- |
| 完整会话轨迹（Rollout） | `~/.BaiCode/sessions/YYYY/MM/DD/rollout-*.jsonl` | 复盘完整对话、推理过程、工具调用入参与返回、Token 消耗 |
| 非交互实时事件流 | `baic exec --json "..."` | 脚本化采集单次任务的完整事件与结果 |
| 模型可见 Prompt 快照 | `baic debug prompt-input` | 检查实际拼装发给模型的系统/开发者指令与上下文 |
| 底层 HTTP / SSE 追踪 | `RUST_LOG="codex_api=trace,codex_core=debug"` | 排查 SSE 原始事件、重试、超时、上下文压缩与底层调度 |
| 持久化运行日志库 | `~/.BaiCode/logs_2.sqlite` (`logs` 表) | 按时间、线程 ID 或日志级别检索历史运行日志 |
| 本地推理服务侧日志 | BaiInf / Ollama / LM Studio 服务端日志 | 查看 100% 原始 HTTP 请求与响应 Body |

> 若设置了环境变量 `BAICODE_HOME`，上述路径中的 `~/.BaiCode` 对应 `$BAICODE_HOME`。

---

## 1. 查看完整会话轨迹（Rollout JSONL）

BaiCode 每次运行都会在 `~/.BaiCode/sessions/YYYY/MM/DD/` 下生成一份 `rollout-<时间>-<thread_id>.jsonl` 文件。在交互式 TUI 中输入 `/rollout` 可直接打印当前会话的文件路径。

### 1.1 查找最新会话文件

```sh
ls -t ~/.BaiCode/sessions/*/*/*/rollout-*.jsonl | head -n 5
```

### 1.2 Rollout 中的核心记录类型

每行是一条带 `timestamp`、`ordinal`、`type`、`payload` 的 JSON 对象，常见类型包括：

* `session_meta`：会话 ID、工作目录、运行来源等元信息。
* `world_state` / `turn_context`：当前轮次的模型配置、协作模式与上下文状态。
* `response_item`：实际进出模型的消息项，按 `payload.type` 分为：
  * `message`（`role`: `developer` / `user` / `assistant`）
  * `reasoning`（模型思考文本）
  * `function_call`（工具名与参数 JSON）
  * `function_call_output`（工具执行输出）
* `token_usage_record` / `event_msg` (`token_count`)：每轮的输入、缓存命中与输出 Token 统计。

### 1.3 常用 `jq` 分析命令

假设先将目标文件赋值给变量：

```sh
ROLLOUT=$(ls -t ~/.BaiCode/sessions/*/*/*/rollout-*.jsonl | head -n 1)
```

1. **查看本次会话所有工具调用（工具名与参数）**：

```sh
jq -r 'select(.type=="response_item" and .payload.type=="function_call")
  | "\(.timestamp)  \(.payload.name)  \(.payload.arguments)"' "$ROLLOUT"
```

2. **查看工具调用返回结果（定位报错或无效输出）**：

```sh
jq -r 'select(.type=="response_item" and .payload.type=="function_call_output")
  | "[\(.timestamp)] call_id=\(.payload.call_id)\n\(.payload.output)\n---"' "$ROLLOUT"
```

3. **查看每轮 Token 消耗与缓存命中情况**：

```sh
jq 'select(.type=="event_msg" and .payload.type=="token_count")
  | {ts: .timestamp, usage: .payload.info.total_token_usage}' "$ROLLOUT"
```

4. **查看模型思考过程（Reasoning）与最终回复**：

```sh
jq -r 'select(.type=="response_item" and (.payload.type=="reasoning" or (.payload.type=="message" and .payload.role=="assistant")))
  | "\(.timestamp) [\(.payload.type)] \(.payload.content)"' "$ROLLOUT"
```

---

## 2. 非交互执行时实时采集 JSON 事件流

在自动化脚本或批量评测中，直接使用 `--json` 将结构化事件输出到标准输出（可重定向保存）：

```sh
baic exec --json "运行当前仓库测试并总结结果" | tee run-events.jsonl
```

---

## 3. 检查实际发送给模型的 Prompt 拼装结果

如果想确认当前配置、项目 `BAICODE.md`、技能（Skills）最终拼装出什么输入给模型，运行：

```sh
baic debug prompt-input "示例用户问题" | jq .
```

也可以配合 `-c` 覆盖配置项进行对比验证，例如：

```sh
baic debug prompt-input -c model_provider="baiinf" "示例用户问题" | jq .
```

---

## 4. 开启底层 HTTP / SSE 原始日志追踪

需要排查流式响应解析、网络重试、上下文压缩或工具调度细节时，通过 `RUST_LOG` 提高日志级别：

```sh
RUST_LOG="codex_api=trace,codex_core=debug" baic exec "你的任务" 2> baic-trace.log
```

* `codex_api=trace`：输出每一条原始 SSE 事件（`SSE event: ...`）与解析状态。
* `codex_core=debug`：输出会话轮次、上下文窗口与工具执行调度信息。

如果只关心 SSE 原始事件，可以单独过滤：

```sh
grep "SSE event:" baic-trace.log | head -n 30
```

---

## 5. 查询本地 SQLite 日志库（`logs_2.sqlite`）

BaiCode 会将运行日志写入 `~/.BaiCode/logs_2.sqlite` 的 `logs` 表（包含 `ts`、`level`、`target`、`feedback_log_body`、`thread_id`、`file`、`line` 等字段）。

### 5.1 使用 `sqlite3` 直接查询

查看最近 30 条日志：

```sh
sqlite3 -header -column ~/.BaiCode/logs_2.sqlite "
SELECT datetime(ts, 'unixepoch', 'localtime') AS time,
       level,
       target,
       substr(feedback_log_body, 1, 120) AS body
FROM logs
ORDER BY ts DESC, ts_nanos DESC, id DESC
LIMIT 30;
"
```

按指定会话 `thread_id` 检索错误或警告日志：

```sh
sqlite3 -header -column ~/.BaiCode/logs_2.sqlite "
SELECT datetime(ts, 'unixepoch', 'localtime') AS time,
       level,
       target,
       feedback_log_body
FROM logs
WHERE thread_id = '<你的thread_id>'
  AND level IN ('WARN', 'ERROR')
ORDER BY ts ASC, ts_nanos ASC;
"
```

### 5.2 使用仓库内置的 `logs_client`

在源码目录中也可以直接运行内置日志客户端进行查询：

```sh
cd codex-rs
cargo run --locked -p codex-cli --bin logs_client -- --help
```

---

## 6. 本地推理服务侧原始报文（BaiInf / Ollama）

当 BaiCode 连接本机推理服务（如 `http://127.0.0.1:18367/v1`）时，可以在推理服务侧直接开启请求日志，或在本地起一层透明代理记录 `/v1/responses` 的完整 HTTP Request / Response Body，用于核对：

* 实际发出的 `instructions`、工具 Schema（`tools`）与历史消息列表；
* 模型的原始推理输出、tool call JSON 格式是否出现截断或格式偏移；
* 本机服务的首字延迟（TTFT）与吞吐速度。

---

## 7. 面向流程优化的排查建议

优化 Agent 流程时，建议按以下顺序查看日志：

1. **先看 `baic debug prompt-input`**：确认系统提示词、`BAICODE.md` 和注入上下文是否过多或存在冲突。
2. **再看 `rollout-*.jsonl` 中的 `function_call` 与 `function_call_output`**：统计哪类工具频繁报错、重复调用或返回内容过大。
3. **检查 `token_count` 趋势**：观察输入 Token 增长速度和缓存命中率（`cached_input_tokens`），定位上下文膨胀节点。
4. **遇到解析异常或中断时再开 `RUST_LOG="codex_api=trace"`**：确认是模型输出格式问题还是 SSE 传输/超时问题。
