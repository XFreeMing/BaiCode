# BaiDec（类 Jev 决策协议）× BaiInf（Qwen3.6-35B-A3B Q4）本地编程架构与微调闭环指南

本文档说明 BaiCode 如何通过 **System-1 决策引擎（BaiDec / `Intern-Decision-4B`）** 与 **System-2 代码生成引擎（BaiInf / `Qwen3.6-35B-A3B Q4`）** 协同跑通本地编程场景，并基于在线轨迹构建持续微调与校准闭环。

---

## 一、双引擎协同架构（System-1 决策路由 + System-2 受限生成）

本地量化模型（如 `Qwen3.6-35B-A3B` Q4 MoE，单 token 激活参数约 3B）在同时挂载十余个工具 Schema 时容易出现注意力稀释、盲目跑 `grep/cat` 跳过结构化索引、补丁格式偏移或未验证即提前结束等问题。

为此，BaiCode 将每一步采样拆分为两阶段：

1. **System-1 工具识别与动作决策（`BaiDec`，`POST /v1/decisions`）**
   - 实现位置：[baidec.rs](../codex-rs/core/src/tools/baidec.rs)
   - 输入压缩后的结构化 `state`（用户目标、当前步数 `step_index`、是否存在未验证修改 `has_unverified_edit`、最近工具调用结果、内置 `code_map` 与 LSP MCP 可用性及候选工具说明）。
   - 在**单次因果前向**（AI 300L 实测约 `160 ms`）内同时输出 4 个类型化决策头及校准概率：
     - `turn_action`（`choice`: `call_tool` / `final_answer`）
     - `primary_tool`（`choice`: `code_map`、LSP MCP 工具、`apply_patch`、`exec_command`、`update_plan` 等 + `none`）
     - `code_map_mode`（`choice`: `repo_map` / `file_outline` / `find_references`）
     - `needs_verification`（`noul`: 是否仍需执行构建/测试验证）
   - 护栏策略（Guardrails）：
     - 若存在尚未验证的补丁修改（`has_unverified_edit == true`）且 `needs_verification` 概率较高，阻止中等置信度的过早 `final_answer`，保留 `exec_command` 完成测试验证；
     - 若上一步同一工具刚发生失败，自动并入 `code_map` / `exec_command` 恢复候选，防止死循环；
     - 若 `BaiDec` 未启动、繁忙或超时，自动无损回退到标准全量工具 + `tool_choice = "auto"`（Fail-Open）。

2. **System-2 专注填参与代码生成（`BaiInf`，`POST /v1/responses`）**
   - 在 `enforce` 模式下，BaiCode 仅向 `Qwen3.6-35B-A3B Q4` 暴露 `BaiDec` 选出的 1~3 个高概率工具，设置 `tool_choice = "required"`（或完成态 `tool_choice = "none"`），并在当前请求尾部注入瞬态 `<baidec_decision>` 提示（包含推荐的 `code_map` 子模式，不污染持久化历史）。
   - 针对 `BaiInf`（`llama-server`）的 `/v1/responses` 协议特性，新增函数式补丁模式 `ApplyPatchToolType::Function`（见 [apply_patch_spec.rs](../codex-rs/core/src/tools/handlers/apply_patch_spec.rs)），使 `apply_patch` 以标准 `"type": "function"` JSON 工具形态参与 `BaiDec` 路由与 `llama-server` 语法约束。
   - 专属提示词定义在 [qwen_3_6_35b_a3b_prompt.md](../codex-rs/core/qwen_3_6_35b_a3b_prompt.md) 并内置于 [models.json](../codex-rs/models-manager/models.json)（前缀匹配 `qwen3.6-35b-a3b*`）。

---

## 二、`code_map` 与 LSP MCP 双层代码理解接入

1. **第一层：零依赖内置 `code_map`**
   - `BaiDec` 在 `primary_tool` 与 `code_map_mode` 双决策头中显式建模 `repo_map`（仓库拓扑与核心文件排序）、`file_outline`（单文件符号与精确行号）、`find_references`（符号定义与跨文件引用分离）。
2. **第二层：编译器级精确语义 LSP MCP**
   - 当 `~/.BaiCode/config.toml` 按 [code-analysis-and-lsp.md](code-analysis-and-lsp.md) 挂载 `serena` 或 `mcp-language-server`（如 `rust-analyzer`、`pyright`、`gopls`、`clangd`）时，[baidec.rs](../codex-rs/core/src/tools/baidec.rs) 会自动将对应 MCP 工具识别为 `ToolSemanticKind::LspMcp`，在类型消歧、接口实现跳转与编译诊断场景下优先路由给 `Qwen3.6`。

---

## 三、配置示例（`~/.BaiCode/config.toml`）

```toml
model_provider = "baiinf"
model = "qwen3.6-35b-a3b-q4_k_m"
model_context_window = 32768

[model_providers.baiinf]
name = "BaiInf"
base_url = "http://127.0.0.1:8080/v1"
wire_api = "responses"

[baidec]
mode = "enforce"                                 # 可选: "off" | "shadow" | "enforce"
url = "http://127.0.0.1:18190/v1/decisions"
timeout_ms = 5000
max_state_chars = 12000
min_action_confidence = 0.60
min_tool_confidence = 0.30
secondary_tool_threshold = 0.18
max_routed_tools = 3
record_trajectories = true
```

也可通过环境变量临时切换模式做对照实验：
- `BAICODE_BAIDEC_MODE=shadow`：影子模式，请求 `BaiDec` 并记录轨迹，但不改变原工具列表；
- `BAICODE_BAIDEC_MODE=enforce`：强制决策路由模式；
- `BAICODE_BAIDEC_MODE=off`：关闭 `BaiDec`。

---

## 四、BaiDec 编程场景持续微调与校准闭环（越用越好）

每次运行会话时，`BaiCode` 会在 `~/.BaiCode/baidec/trajectories/YYYY/MM/DD/decisions-<thread_id>.jsonl` 成对写入：
- `decision_step`：发送给 `BaiDec` 的完整 `state`、`questions`、校准概率分布与路由计划；
- `step_outcome`：该步实际产生的工具调用、补丁/命令退出码（成功/失败）与是否完成最终回复。

使用 [baidec_flywheel.py](../scripts/baidec_flywheel.py) 执行离线分析、数据清洗与微调导出：

### 1. 查看线上决策质量与延迟报告
```sh
python3 scripts/baidec_flywheel.py report
```

### 2. 导出 `Intern-Decision` 微调训练集与 `BaiDec` 回归评测集
```sh
python3 scripts/baidec_flywheel.py curate --out-dir ./artifacts/baidec-flywheel
```
该命令自动完成两类监督信号提取：
- **正向执行蒸馏（Verified Execution Distillation）**：筛选工具调用成功且最终任务干净收尾的步骤（支持在 `shadow` 模式下用更强教师模型跑仓库任务收集高质量金标，也支持本地高回报自举轨迹），同时自动从 `code_map` 真实入参提取 `code_map_mode` 标签、从后续测试命令回填 `needs_verification` 标签；
- **事后反事实纠错（Counterfactual Hindsight Repair）**：当第 $t$ 步调用失败（例如未看行号直接 `apply_patch` 报错），而第 $t+1$ 步立即通过 `code_map(file_outline)` 成功恢复时，自动将第 $t$ 步的理想决策重标为第 $t+1$ 步的恢复工具，把失败教训直接转化为高质量纠错样本。

产物直接对齐上游格式：
- `intern_decision_sft.jsonl`：100% 兼容 `ai-infra/sources/intern-decision/src/inputs/schema.py::compile_row`，可直接送入 `Intern-Decision-4B` LoRA / 全参 SFT 训练；
- `baidec_replay_cases.json`：100% 兼容 `ai-infra/modules/baidec/tools/scope_replay.py`（包含 `schema`/`suite`/`line`/`request_hash`/`baseline.{gold,native,hf}`），新微调权重转换 GGUF 后须先通过回放零回退门与温度缩放校准（`temperature-scaling`），再激活至 `~/.BaiInfra/BaiDec/current`；
- `review_queue.jsonl`：未自动恢复的异常步骤，供人工或大模型离线复盘标注。

### 3. 自动校准置信度阈值
```sh
python3 scripts/baidec_flywheel.py calibrate --min-recall 0.85
```
输出当前模型在编程轨迹上的校准误差（ECE）并生成最优 `[baidec]` 阈值配置片段。
