# BaiCode

BaiCode 是基于 Codex CLI 的个人本地编程工具，支持交互式终端和 `baic exec`。默认使用本机 Ollama，也支持 BaiInf、LM Studio 和显式配置的远端 Responses API。

## 从源码构建

在仓库目录中运行。首次构建需要下载或预先缓存 Rust 依赖。

```sh
cd codex-rs
cargo build --locked -p codex-cli --bin baic
```

在 Linux 上，可将构建产物装到用户的命令目录，之后直接运行 `baic`。

```sh
../scripts/install/install.sh target/debug/baic
export PATH="$HOME/.local/bin:$PATH"
baic --version
```

## 与 Codex 并行使用

直接运行 `baic` 使用内嵌服务。用户配置、凭据文件、会话和内置技能存放在 `~/.BaiCode`。项目配置使用 `.BaiCode/config.toml`，项目技能放在 `.BaiCode/skills/`，项目指令使用 `BAICODE.md` 或 `BAICODE.override.md`。可执行文件更新时，重新构建并运行上面的本地安装脚本。

两个程序若在同一个工作目录执行编程任务，编辑的是该目录中的同一份源码。如需并行编辑不同版本，可使用独立 Git worktree。

## 使用本机模型

启动具备 Responses API 的本机推理服务，准备已安装且支持工具调用的模型。BaiCode 首次启动时会在当前用户的主目录创建 `~/.BaiCode`，用于保存配置、会话和运行数据。若需指定其他已有目录，可设置环境变量 `BAICODE_HOME`。

使用 Ollama 时，在 `~/.BaiCode/config.toml` 中写入：

```toml
model_provider = "ollama"
model = "gpt-oss:20b"
```

使用 LM Studio 时，把 `model_provider` 改成 `lmstudio`。将 `model` 设为本机服务实际返回的模型 ID。BaiCode 不会自动下载模型。

从要操作的项目目录启动构建产物 `codex-rs/target/debug/baic`。交互模式直接执行该文件，单次执行可追加 `exec "说明这份仓库的入口和测试方式"`。

### 使用 BaiInf

以下配置在本机 BaiInf 0.1.10 和 Qwen3.6-35B-A3B Q4_K_M 上验证过。安装对应运行时并准备好模型权重后，在 ai-infra 仓库的 `modules/baiinf` 目录启动服务，保留这个终端。

```sh
./qwen-q4.sh --runtime "$HOME/.BaiInfra/BaiInf/versions/0.1.10/installation" \
  --port 18367 --ctx-size 32768
```

在 `~/.BaiCode/config.toml` 中配置本机 BaiInf。

```toml
model_provider = "baiinf"
model = "qwen3.6-35b-a3b-q4_k_m"
model_context_window = 32768

[model_providers.baiinf]
name = "BaiInf"
base_url = "http://127.0.0.1:18367/v1"
wire_api = "responses"
```

安装后，在目标 Git 仓库中运行以下命令。

```sh
baic exec --sandbox workspace-write "运行这个项目的测试，修复失败用例，再运行测试验证。"
```

## 使用远端模型

### 从 `.env` 选择云模型

在 `.env` 中设置服务的基础地址、模型 ID 和密钥。`WIRE_API` 默认为 `responses`，BaiCode 会在基础地址后追加 `/responses`。如果服务提供 Anthropic 兼容接口，设置 `WIRE_API="messages"`，BaiCode 会追加 `/messages`。

```dotenv
BASE_URL="https://api.deepseek.com"
MODEL="deepseek-flash"
AUTH_TOKEN="your-deepseek-api-key"
```

需要自定义密钥变量名时，把 `AUTH_TOKEN` 改成所需名称，并添加 `CLOUD_ENV_KEY="新变量名"`。

从 `.env` 所在目录运行以下命令。去掉 `exec` 和提示词即可进入交互模式。

```sh
BAICODE_CLOUD_ENV_FILE="$PWD/.env" baic -c 'model_provider="cloud"' exec "回复 OK"
```

### 从 `config.toml` 选择远端模型

在同一个 `config.toml` 中填写模型服务的地址和模型 ID。以下域名和模型名为占位示例。服务需要密钥时，将密钥放在 `BAICODE_MODEL_API_KEY` 环境变量中，免鉴权服务可以省略 `env_key`。

```toml
model_provider = "personal"
model = "your-model-id"

[model_providers.personal]
name = "Personal model"
base_url = "https://model.example/v1"
wire_api = "responses"
env_key = "BAICODE_MODEL_API_KEY"
```

### 使用 DeepSeek V4.1 Flash 网关

通过 `.env` 配置网关时，设置 `WIRE_API="messages"`，并将 `BASE_URL` 设为 `/messages` 之前的基础地址。使用网关给出的 `MODEL`，例如 `deepseek-v4.1-flash`。

```sh
BAICODE_CLOUD_ENV_FILE="$PWD/.env" baic -c 'model_provider="cloud"' exec --sandbox workspace-write "修复失败的测试并验证"
```

需要用 `baic -p deepseek` 切换模型时，在 `~/.BaiCode/config.toml` 注册网关。

```toml
[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://model.example/v1"
wire_api = "messages"
env_key = "AUTH_TOKEN"
```

让 `~/.BaiCode/.env` 提供 `AUTH_TOKEN`。已有单独维护的 `.env` 文件时，可以将 `~/.BaiCode/.env` 链接到该文件。将下面的配置保存到 `~/.BaiCode/deepseek.config.toml`。

```toml
model_provider = "deepseek"
model = "deepseek-v4.1-flash"
model_reasoning_effort = "high"
```

运行 `baic -p deepseek`。模型 ID 使用服务商提供的名称。BaiCode 为 `deepseek-v4.1-flash` 和 `deepseek-flash` 应用同一套编码指令、推理档位和工具配置。默认使用 `high`，小改动可用 `low`，复杂排查可用 `max`。

使用远端模型时，提示词、随对话提供的工作区上下文及工具结果会发送到该服务。常规模型请求省略安装 ID、会话标识和 Git 状态等附加元数据。

## 调试与模型调用日志

* 查看完整会话轨迹（Rollout）、Prompt 拼装快照、SSE 原始追踪及 SQLite 运行日志的方法见 [docs/model-call-logging.md](docs/model-call-logging.md)。
* 内置代码结构索引（`code_map`）与 LSP MCP 接入指南见 [docs/code-analysis-and-lsp.md](docs/code-analysis-and-lsp.md)。

本项目采用 [Apache-2.0 许可证](LICENSE)。
