# BaiCode

BaiCode 是基于 Codex CLI 的个人本地编程工具，支持交互式终端和 `baicode exec`。默认使用本机 Ollama，也支持 BaiInf、LM Studio 和显式配置的远端 Responses API。

## 从源码构建

在仓库目录中运行。首次构建需要下载或预先缓存 Rust 依赖。

```sh
cd codex-rs
cargo build --locked -p codex-cli --bin baicode
```

在 Linux 上，可将构建产物装到用户的命令目录，之后直接运行 `baicode`。

```sh
install -Dm755 target/debug/baicode "$HOME/.local/bin/baicode"
export PATH="$HOME/.local/bin:$PATH"
baicode --version
```

## 使用本机模型

启动具备 Responses API 的本机推理服务，准备已安装且支持工具调用的模型。建议为 BaiCode 使用独立的配置目录，以免读取已有的 Codex 账号和项目设置。

```sh
export CODEX_HOME="$HOME/.baicode"
mkdir -p "$CODEX_HOME"
```

使用 Ollama 时，在 `$CODEX_HOME/config.toml` 中写入：

```toml
model_provider = "ollama"
model = "gpt-oss:20b"
```

使用 LM Studio 时，把 `model_provider` 改成 `lmstudio`。将 `model` 设为本机服务实际返回的模型 ID。BaiCode 不会自动下载模型。

从要操作的项目目录启动构建产物 `codex-rs/target/debug/baicode`。交互模式直接执行该文件，单次执行可追加 `exec "说明这份仓库的入口和测试方式"`。

### 使用 BaiInf

以下配置在本机 BaiInf 0.1.10 和 Qwen3.6-35B-A3B Q4_K_M 上验证过。安装对应运行时并准备好模型权重后，在 ai-infra 仓库的 `modules/baiinf` 目录启动服务，保留这个终端。

```sh
./qwen-q4.sh --runtime "$HOME/.BaiInfra/BaiInf/versions/0.1.10/installation" \
  --port 18367 --ctx-size 32768
```

在运行 BaiCode 的终端设置 `CODEX_HOME="$HOME/.baicode"` 并导出，把该目录下 `config.toml` 的模型配置设为以下值。

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
baicode exec --sandbox workspace-write "运行这个项目的测试，修复失败用例，再运行测试验证。"
```

## 使用远端模型

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

使用远端模型时，提示词、随对话提供的工作区上下文及工具结果会发送到该服务。常规模型请求省略安装 ID、会话标识和 Git 状态等附加元数据。

本项目采用 [Apache-2.0 许可证](LICENSE)。
