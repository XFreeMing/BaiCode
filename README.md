# BaiCode

BaiCode 是基于 Codex CLI 的个人本地编程工具，支持交互式终端和 `codex exec`。模型通过本机的 Responses API 接入。默认选择 Ollama，也可以配置 LM Studio 或其他兼容的本机服务。

## 从源码构建

在仓库目录中运行。首次构建需要下载或预先缓存 Rust 依赖。

```sh
cd codex-rs
cargo build --locked -p codex-cli --bin codex
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

从要操作的项目目录启动构建产物 `codex-rs/target/debug/codex`。交互模式直接执行该文件，单次执行可追加 `exec "说明这份仓库的入口和测试方式"`。

本项目采用 [Apache-2.0 许可证](LICENSE)。
