# 代码结构索引（`code_map`）与 LSP 接入指南

BaiCode 采用两层互补的代码理解方案：

1. **内置 `code_map` 工具（零外部依赖、开箱即用）**：基于 `.gitignore` 感知遍历 + 多语言符号提取 + 跨文件引用排序，为本地/远端模型提供紧凑的 Repo Map、单文件大纲和跨文件引用查询。
2. **LSP MCP 扩展（编译器级精确语义）**：在需要精确跨文件跳转、类型推导、接口实现查找或实时编译诊断时，通过 MCP 挂载各语言官方 LSP（如 `rust-analyzer`、`pyright`、`gopls`、`clangd`、`tsserver`）。

---

## 一、内置 `code_map` 工具

模型在会话中可直接调用 `code_map`，支持 3 种模式，覆盖 Rust、Python、TypeScript/JavaScript、Go、C/C++、Java/Kotlin：

### 1. `mode = "repo_map"`（默认）
* 扫描工作区或指定子目录（自动尊重 `.gitignore`，跳过 `.git`、`.BaiCode`、`target`、`node_modules` 等目录）。
* 提取每个源码文件的符号定义，并统计跨文件引用频次，按重要度倒序输出紧凑骨架。
* 默认限制输出在 `max_files = 30`、`max_chars = 12000` 内，适配 32K 上下文本地模型（如 BaiInf Qwen3.6-35B）。
* 支持传入可选 `symbol` 作为聚焦加权关键词，把相关模块排到最前。

### 2. `mode = "file_outline"`
* 传入 `path`（相对工作区路径），列出该文件所有类、结构体、接口、函数与方法及其行号，方便在读取大文件前先锁定目标行范围。

### 3. `mode = "find_references"`
* 传入 `symbol`（及可选 `path` 限定子目录），将全库匹配结果自动拆分为 **`## Definitions`**（定义点）与 **`## References`**（引用点），避免人工翻看海量 `rg` 文本。

---

## 二、通过 MCP 接入精确 LSP（可选增强）

对于大型多模块项目，如果希望模型获得与 IDE（如 Zed / VS Code）同级别的编译器级引用查找和诊断能力，可在 `~/.BaiCode/config.toml` 中配置 LSP MCP 服务器。

### 方案 A：使用 Serena（多语言 LSP MCP，基于 `uvx`）

本机已安装 `uvx` 时，在 `~/.BaiCode/config.toml` 中添加：

```toml
[mcp_servers.serena]
command = "uvx"
args = [
  "--from", "git+https://github.com/oraios/serena",
  "serena", "start-mcp-server",
  "--context", "ide-assistant",
  "--project", "."
]
```

### 方案 B：使用 `mcp-language-server`（直连指定语言 LSP）

安装对应语言服务器（如 `rust-analyzer`、`pyright`、`gopls`、`clangd`）及 `mcp-language-server` 后，在 `~/.BaiCode/config.toml` 中按语言配置，例如 Rust 项目：

```toml
[mcp_servers.rust_lsp]
command = "mcp-language-server"
args = [
  "--workspace", ".",
  "--lsp", "rust-analyzer"
]
```

配置完成后，可在交互模式运行 `/mcp` 或执行 `baic mcp list` 确认 LSP 工具已成功挂载。
