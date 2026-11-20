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
install -Dm755 target/debug/baic "$HOME/.local/bin/baic"
export PATH="$HOME/.local/bin:$PATH"
baic --version
```

## 使用本机模型

启动具备 Responses API 的本机推理服务，准备已安装且支持工具调用的模型。BaiCode 首次启动时会在当前用户的主目录创建 `~/.BaiCode`，用于保存配置、会话和运行数据。若需指定其他已有目录，可设置环境变量 `CODEX_HOME`。

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
