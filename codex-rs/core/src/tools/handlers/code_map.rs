use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::code_map_spec::create_code_map_tool;
use crate::tools::handlers::selected_executor;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use ignore::WalkBuilder;
use regex_lite::Regex;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

const DEFAULT_MAX_FILES: usize = 30;
const MAX_ALLOWED_FILES: usize = 200;
const DEFAULT_MAX_CHARS: usize = 12_000;
const MIN_ALLOWED_CHARS: usize = 1_000;
const MAX_ALLOWED_CHARS: usize = 60_000;
const MAX_SCAN_FILES: usize = 2_000;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_SYMBOLS_PER_FILE_IN_MAP: usize = 25;

pub struct CodeMapHandler;

struct CodeMapToolOutput {
    text: String,
    success: bool,
}

impl ToolOutput for CodeMapToolOutput {
    fn log_output(&self) -> String {
        self.text.clone()
    }

    fn success_for_logging(&self) -> bool {
        self.success
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let mut output = FunctionCallOutputPayload::from_text(self.text.clone());
        output.success = Some(self.success);
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        serde_json::json!({
            "content": self.text,
            "success": self.success,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CodeMapMode {
    RepoMap,
    FileOutline,
    FindReferences,
}

impl Default for CodeMapMode {
    fn default() -> Self {
        Self::RepoMap
    }
}

#[derive(Debug, Deserialize)]
struct CodeMapArgs {
    #[serde(default)]
    mode: CodeMapMode,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    max_files: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
}

impl ToolExecutor<ToolInvocation> for CodeMapHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("code_map")
    }

    fn spec(&self) -> ToolSpec {
        create_code_map_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl CoreToolRuntime for CodeMapHandler {}

impl CodeMapHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            environments,
            step_context,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "code_map handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: CodeMapArgs = serde_json::from_str(&arguments).map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to parse code_map arguments: {err}"))
        })?;

        let turn_environment = selected_executor(&environments, &step_context.environments)?;
        let workspace_root = turn_environment.cwd().to_path_buf();
        let result = tokio::task::spawn_blocking(move || execute_code_map(&workspace_root, args))
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("code_map worker failed: {err}"))
            })?;

        match result {
            Ok(text) => Ok(boxed_tool_output(CodeMapToolOutput {
                text,
                success: true,
            })),
            Err(message) => Err(FunctionCallError::RespondToModel(message)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
    COrCpp,
    JavaOrKotlin,
}

impl Language {
    fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "tsx" | "mts" | "cts" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "go" => Some(Self::Go),
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hxx" => Some(Self::COrCpp),
            "java" | "kt" | "kts" => Some(Self::JavaOrKotlin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct SymbolDef {
    name: String,
    line: usize,
    signature: String,
}

#[derive(Debug)]
struct IndexedFile {
    rel_path: String,
    symbols: Vec<SymbolDef>,
    identifiers: HashSet<String>,
}

fn execute_code_map(workspace_root: &Path, args: CodeMapArgs) -> Result<String, String> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let target_path = resolve_target_path(&canonical_root, args.path.as_deref())?;
    let max_files = args
        .max_files
        .unwrap_or(DEFAULT_MAX_FILES)
        .clamp(1, MAX_ALLOWED_FILES);
    let max_chars = args
        .max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(MIN_ALLOWED_CHARS, MAX_ALLOWED_CHARS);

    match args.mode {
        CodeMapMode::FileOutline => {
            if args.path.is_none() {
                return Err("mode='file_outline' requires the 'path' parameter".to_string());
            }
            if !target_path.is_file() {
                return Err(format!(
                    "path '{}' is not a regular file",
                    display_rel_path(&canonical_root, &target_path)
                ));
            }
            render_file_outline(&canonical_root, &target_path, max_chars)
        }
        CodeMapMode::RepoMap => render_repo_map(
            &canonical_root,
            &target_path,
            args.symbol.as_deref(),
            max_files,
            max_chars,
        ),
        CodeMapMode::FindReferences => {
            let symbol = args
                .symbol
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "mode='find_references' requires a non-empty 'symbol' parameter".to_string()
                })?;
            render_find_references(&canonical_root, &target_path, symbol, max_chars)
        }
    }
}

fn resolve_target_path(canonical_root: &Path, raw_path: Option<&str>) -> Result<PathBuf, String> {
    let candidate = match raw_path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            let path = Path::new(p);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                canonical_root.join(path)
            }
        }
        None => canonical_root.to_path_buf(),
    };

    let canonical_target = candidate
        .canonicalize()
        .map_err(|err| format!("failed to access path '{}': {err}", candidate.display()))?;

    if !canonical_target.starts_with(canonical_root) {
        return Err(format!(
            "path '{}' is outside the workspace root",
            canonical_target.display()
        ));
    }

    Ok(canonical_target)
}

fn display_rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_source_files(root: &Path, target: &Path) -> Vec<(PathBuf, Language)> {
    if target.is_file() {
        return Language::from_path(target)
            .map(|lang| vec![(target.to_path_buf(), lang)])
            .unwrap_or_default();
    }

    let mut files = Vec::new();
    let walker = WalkBuilder::new(target)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | ".BaiCode" | ".codex" | "node_modules" | "target" | "dist" | "build"
            )
        })
        .build();

    for entry in walker.flatten() {
        if files.len() >= MAX_SCAN_FILES {
            break;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if !path.starts_with(root) {
            continue;
        }
        let Some(lang) = Language::from_path(&path) else {
            continue;
        };
        if let Ok(meta) = fs::metadata(&path)
            && meta.len() <= MAX_FILE_BYTES
        {
            files.push((path, lang));
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn render_file_outline(root: &Path, path: &Path, max_chars: usize) -> Result<String, String> {
    let lang = Language::from_path(path).ok_or_else(|| {
        format!(
            "unsupported source file extension for '{}'",
            display_rel_path(root, path)
        )
    })?;
    let content = fs::read_to_string(path).map_err(|err| format!("failed to read file: {err}"))?;
    let symbols = extract_symbols(lang, &content);
    let rel = display_rel_path(root, path);

    let mut out = String::new();
    let _ = writeln!(out, "# File outline: {rel} ({} symbols)", symbols.len());
    if symbols.is_empty() {
        out.push_str("(no top-level symbols detected)\n");
        return Ok(out);
    }

    for sym in &symbols {
        let line = format!("L{}: {}\n", sym.line, sym.signature);
        if out.len() + line.len() > max_chars {
            out.push_str("... (truncated by max_chars budget)\n");
            break;
        }
        out.push_str(&line);
    }
    Ok(out)
}

fn render_repo_map(
    root: &Path,
    target: &Path,
    focus_symbol: Option<&str>,
    max_files: usize,
    max_chars: usize,
) -> Result<String, String> {
    let source_files = collect_source_files(root, target);
    if source_files.is_empty() {
        return Ok(format!(
            "No supported source files found under '{}'.",
            display_rel_path(root, target)
        ));
    }

    let mut indexed: Vec<IndexedFile> = Vec::with_capacity(source_files.len());
    let mut defined_symbol_set: HashSet<String> = HashSet::new();

    for (path, lang) in &source_files {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let symbols = extract_symbols(*lang, &content);
        for sym in &symbols {
            if is_rankable_symbol(&sym.name) {
                defined_symbol_set.insert(sym.name.clone());
            }
        }
        let identifiers = extract_identifiers(&content);
        indexed.push(IndexedFile {
            rel_path: display_rel_path(root, path),
            symbols,
            identifiers,
        });
    }

    // Count in how many files each defined symbol appears.
    let mut file_presence: HashMap<&str, usize> = HashMap::new();
    for file in &indexed {
        for ident in &file.identifiers {
            if defined_symbol_set.contains(ident) {
                *file_presence.entry(ident.as_str()).or_insert(0) += 1;
            }
        }
    }

    let focus_lower = focus_symbol
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    let mut ranked: Vec<(usize, usize, &IndexedFile)> = indexed
        .iter()
        .filter(|f| !f.symbols.is_empty())
        .map(|file| {
            let mut cross_file_refs: usize = 0;
            let mut seen_defs: HashSet<&str> = HashSet::new();
            let mut focus_bonus: usize = 0;

            for sym in &file.symbols {
                if let Some(focus) = focus_lower.as_deref()
                    && sym.name.to_ascii_lowercase().contains(focus)
                {
                    focus_bonus += 500;
                }
                if is_rankable_symbol(&sym.name) && seen_defs.insert(sym.name.as_str()) {
                    let total_files = file_presence.get(sym.name.as_str()).copied().unwrap_or(0);
                    cross_file_refs += total_files.saturating_sub(1);
                }
            }
            if let Some(focus) = focus_lower.as_deref()
                && file.rel_path.to_ascii_lowercase().contains(focus)
            {
                focus_bonus += 200;
            }
            let score = focus_bonus + cross_file_refs * 3 + file.symbols.len();
            (score, cross_file_refs, file)
        })
        .collect();

    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.rel_path.cmp(&b.2.rel_path)));

    let total_indexed = indexed.len();
    let total_with_symbols = ranked.len();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Repo map: {} (scanned {} files, {} with symbols; ranked by cross-file references)",
        if target == root {
            ".".to_string()
        } else {
            display_rel_path(root, target)
        },
        total_indexed,
        total_with_symbols
    );

    let mut shown_files = 0usize;
    let mut truncated = false;

    for (_score, cross_refs, file) in ranked.into_iter().take(max_files) {
        let header = if cross_refs > 0 {
            format!("\n{} (cross-file refs: {cross_refs})\n", file.rel_path)
        } else {
            format!("\n{}\n", file.rel_path)
        };
        if out.len() + header.len() > max_chars {
            truncated = true;
            break;
        }
        out.push_str(&header);
        shown_files += 1;

        for sym in file.symbols.iter().take(MAX_SYMBOLS_PER_FILE_IN_MAP) {
            let line = format!("  L{}: {}\n", sym.line, sym.signature);
            if out.len() + line.len() > max_chars {
                truncated = true;
                break;
            }
            out.push_str(&line);
        }
        if truncated {
            break;
        }
        if file.symbols.len() > MAX_SYMBOLS_PER_FILE_IN_MAP {
            let more = format!(
                "  ... (+{} more symbols; use mode='file_outline' on this file)\n",
                file.symbols.len() - MAX_SYMBOLS_PER_FILE_IN_MAP
            );
            if out.len() + more.len() <= max_chars {
                out.push_str(&more);
            }
        }
    }

    if truncated || shown_files < total_with_symbols {
        let _ = writeln!(
            out,
            "\n(showing {shown_files}/{total_with_symbols} files; narrow with 'path' or 'symbol', or inspect a file with mode='file_outline')"
        );
    }

    Ok(out)
}

fn render_find_references(
    root: &Path,
    target: &Path,
    symbol: &str,
    max_chars: usize,
) -> Result<String, String> {
    let source_files = collect_source_files(root, target);
    let mut definitions: Vec<String> = Vec::new();
    let mut references: Vec<String> = Vec::new();

    for (path, lang) in &source_files {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        if !content.contains(symbol) {
            continue;
        }
        let rel = display_rel_path(root, path);
        let def_lines: HashSet<usize> = extract_symbols(*lang, &content)
            .into_iter()
            .filter(|s| s.name == symbol)
            .map(|s| s.line)
            .collect();

        for (idx, line_text) in content.lines().enumerate() {
            let line_no = idx + 1;
            if !line_contains_word(line_text, symbol) {
                continue;
            }
            let snippet = truncate_snippet(line_text.trim(), 160);
            let entry = format!("{rel}:{line_no}: {snippet}");
            if def_lines.contains(&line_no) {
                definitions.push(entry);
            } else {
                references.push(entry);
            }
        }
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Symbol lookup: `{symbol}` ({} definitions, {} references)",
        definitions.len(),
        references.len()
    );

    out.push_str("\n## Definitions\n");
    if definitions.is_empty() {
        out.push_str("(none found)\n");
    } else {
        for def in &definitions {
            let line = format!("{def}\n");
            if out.len() + line.len() > max_chars {
                out.push_str("... (truncated by max_chars budget)\n");
                return Ok(out);
            }
            out.push_str(&line);
        }
    }

    out.push_str("\n## References\n");
    if references.is_empty() {
        out.push_str("(none found)\n");
    } else {
        for reference in &references {
            let line = format!("{reference}\n");
            if out.len() + line.len() > max_chars {
                out.push_str("... (truncated by max_chars budget)\n");
                break;
            }
            out.push_str(&line);
        }
    }

    Ok(out)
}

fn is_rankable_symbol(name: &str) -> bool {
    name.len() >= 3
        && !matches!(
            name,
            "new"
                | "get"
                | "set"
                | "run"
                | "main"
                | "test"
                | "tests"
                | "init"
                | "from"
                | "into"
                | "default"
                | "clone"
                | "fmt"
                | "drop"
                | "build"
                | "parse"
                | "next"
                | "len"
                | "push"
                | "pop"
                | "call"
                | "handle"
        )
}

fn extract_identifiers(content: &str) -> HashSet<String> {
    let mut idents = HashSet::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if i - start >= 3
                && let Ok(ident) = std::str::from_utf8(&bytes[start..i])
            {
                idents.insert(ident.to_string());
            }
        } else {
            i += 1;
        }
    }
    idents
}

fn line_contains_word(line: &str, word: &str) -> bool {
    let bytes = line.as_bytes();
    let wlen = word.len();
    let mut start = 0;
    while let Some(pos) = line[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
        let after_idx = abs + wlen;
        let after_ok = after_idx == bytes.len() || !is_ident_byte(bytes[after_idx]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn truncate_snippet(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_len.saturating_sub(1)).collect();
    format!("{truncated}…")
}

fn clean_signature(line: &str) -> String {
    let trimmed = line.trim();
    let stripped = trimmed
        .trim_end_matches('{')
        .trim_end_matches(':')
        .trim_end();
    truncate_snippet(stripped, 120)
}

fn extract_symbols(lang: Language, content: &str) -> Vec<SymbolDef> {
    let mut symbols = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        if let Some(name) = match_symbol_name(lang, line, trimmed) {
            symbols.push(SymbolDef {
                name,
                line: idx + 1,
                signature: clean_signature(line),
            });
        }
    }
    symbols
}

fn match_symbol_name(lang: Language, raw_line: &str, trimmed: &str) -> Option<String> {
    match lang {
        Language::Rust => {
            static RE: OnceLock<Regex> = OnceLock::new();
            let re = RE.get_or_init(|| {
                Regex::new(
                    r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?(?:fn|struct|enum|trait|type|mod)\s+([A-Za-z_][A-Za-z0-9_]*)",
                )
                .expect("valid rust symbol regex")
            });
            if let Some(caps) = re.captures(trimmed) {
                return Some(caps[1].to_string());
            }
            static IMPL_RE: OnceLock<Regex> = OnceLock::new();
            let impl_re = IMPL_RE.get_or_init(|| {
                Regex::new(r"^(?:unsafe\s+)?impl(?:<[^>]*>)?\s+(?:[A-Za-z0-9_:<>]+\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)")
                    .expect("valid rust impl regex")
            });
            impl_re.captures(trimmed).map(|caps| caps[1].to_string())
        }
        Language::Python => {
            static RE: OnceLock<Regex> = OnceLock::new();
            let re = RE.get_or_init(|| {
                Regex::new(r"^(?:async\s+)?(?:def|class)\s+([A-Za-z_][A-Za-z0-9_]*)")
                    .expect("valid python symbol regex")
            });
            re.captures(trimmed).map(|caps| caps[1].to_string())
        }
        Language::TypeScript | Language::JavaScript => {
            static RE: OnceLock<Regex> = OnceLock::new();
            let re = RE.get_or_init(|| {
                Regex::new(
                    r"^(?:export\s+(?:default\s+)?)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(?:function\*?|class|interface|type|enum)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
                )
                .expect("valid ts/js symbol regex")
            });
            if let Some(caps) = re.captures(trimmed) {
                return Some(caps[1].to_string());
            }
            static ARROW_RE: OnceLock<Regex> = OnceLock::new();
            let arrow_re = ARROW_RE.get_or_init(|| {
                Regex::new(
                    r"^(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][A-Za-z0-9_$]*)\s*=>",
                )
                .expect("valid arrow function regex")
            });
            arrow_re.captures(trimmed).map(|caps| caps[1].to_string())
        }
        Language::Go => {
            static RE: OnceLock<Regex> = OnceLock::new();
            let re = RE.get_or_init(|| {
                Regex::new(
                    r"^(?:func\s+(?:\([^)]*\)\s+)?([A-Za-z_][A-Za-z0-9_]*)|type\s+([A-Za-z_][A-Za-z0-9_]*)\s+(?:struct|interface))",
                )
                .expect("valid go symbol regex")
            });
            re.captures(trimmed).and_then(|caps| {
                caps.get(1)
                    .or_else(|| caps.get(2))
                    .map(|m| m.as_str().to_string())
            })
        }
        Language::COrCpp => {
            static TYPE_RE: OnceLock<Regex> = OnceLock::new();
            let type_re = TYPE_RE.get_or_init(|| {
                Regex::new(r"^(?:typedef\s+)?(?:class|struct|enum|namespace)\s+([A-Za-z_][A-Za-z0-9_]*)")
                    .expect("valid c/cpp type regex")
            });
            if let Some(caps) = type_re.captures(trimmed) {
                return Some(caps[1].to_string());
            }
            // Only match top-level C/C++ function definitions to avoid control-flow statements.
            if raw_line.starts_with(' ') || raw_line.starts_with('\t') {
                return None;
            }
            static FN_RE: OnceLock<Regex> = OnceLock::new();
            let fn_re = FN_RE.get_or_init(|| {
                Regex::new(r"^[A-Za-z_][A-Za-z0-9_:<>*&\s]+\s+([A-Za-z_][A-Za-z0-9_]*)\s*\([^;]*$")
                    .expect("valid c/cpp fn regex")
            });
            fn_re.captures(trimmed).and_then(|caps| {
                let name = &caps[1];
                (!matches!(name, "if" | "for" | "while" | "switch" | "catch"))
                    .then(|| name.to_string())
            })
        }
        Language::JavaOrKotlin => {
            static RE: OnceLock<Regex> = OnceLock::new();
            let re = RE.get_or_init(|| {
                Regex::new(
                    r"^(?:(?:public|private|protected|internal|open|abstract|final|static|sealed|data|override|suspend)\s+)*(?:class|interface|enum|record|object|fun)\s+([A-Za-z_][A-Za-z0-9_]*)",
                )
                .expect("valid java/kotlin symbol regex")
            });
            re.captures(trimmed).map(|caps| caps[1].to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn repo_map_ranks_referenced_files_higher() {
        let dir = TempDir::new().expect("temp dir");
        fs::write(
            dir.path().join("core.rs"),
            "pub struct EngineConfig {}\npub fn start_engine(cfg: EngineConfig) {}\n",
        )
        .expect("write core.rs");
        fs::write(
            dir.path().join("app.rs"),
            "use crate::core::{EngineConfig, start_engine};\npub fn run_app() { start_engine(EngineConfig {}); }\n",
        )
        .expect("write app.rs");
        fs::write(
            dir.path().join("cli.rs"),
            "use crate::core::start_engine;\npub fn boot_cli() { start_engine(); }\n",
        )
        .expect("write cli.rs");
        fs::write(dir.path().join("unused.rs"), "pub fn lonely_helper() {}\n")
            .expect("write unused.rs");

        let out = execute_code_map(
            dir.path(),
            CodeMapArgs {
                mode: CodeMapMode::RepoMap,
                path: None,
                symbol: None,
                max_files: Some(10),
                max_chars: Some(10_000),
            },
        )
        .expect("repo_map succeeds");

        let core_pos = out.find("core.rs").expect("core.rs present");
        let unused_pos = out.find("unused.rs").expect("unused.rs present");
        assert!(
            core_pos < unused_pos,
            "core.rs should rank before unused.rs:\n{out}"
        );
        assert!(out.contains("start_engine"));
    }

    #[test]
    fn file_outline_lists_python_and_ts_symbols() {
        let dir = TempDir::new().expect("temp dir");
        fs::write(
            dir.path().join("service.py"),
            "class AuthService:\n    async def verify_token(self, token):\n        return True\n\ndef helper():\n    pass\n",
        )
        .expect("write service.py");

        let out = execute_code_map(
            dir.path(),
            CodeMapArgs {
                mode: CodeMapMode::FileOutline,
                path: Some("service.py".to_string()),
                symbol: None,
                max_files: None,
                max_chars: None,
            },
        )
        .expect("file_outline succeeds");

        assert!(out.contains("L1: class AuthService"));
        assert!(out.contains("L2: async def verify_token(self, token)"));
        assert!(out.contains("L5: def helper()"));
    }

    #[test]
    fn find_references_separates_definitions_and_references() {
        let dir = TempDir::new().expect("temp dir");
        fs::write(
            dir.path().join("math.ts"),
            "export function computeScore(x: number) { return x * 2; }\n",
        )
        .expect("write math.ts");
        fs::write(
            dir.path().join("main.ts"),
            "import { computeScore } from './math';\nconst score = computeScore(42);\nconst computeScoreExtra = 1;\n",
        )
        .expect("write main.ts");

        let out = execute_code_map(
            dir.path(),
            CodeMapArgs {
                mode: CodeMapMode::FindReferences,
                path: None,
                symbol: Some("computeScore".to_string()),
                max_files: None,
                max_chars: None,
            },
        )
        .expect("find_references succeeds");

        assert!(out.contains("## Definitions\nmath.ts:1:"));
        assert!(out.contains("main.ts:1:"));
        assert!(out.contains("main.ts:2:"));
        assert!(!out.contains("computeScoreExtra"));
    }

    #[test]
    fn rejects_path_outside_workspace() {
        let dir = TempDir::new().expect("temp dir");
        let err = execute_code_map(
            dir.path(),
            CodeMapArgs {
                mode: CodeMapMode::RepoMap,
                path: Some("/".to_string()),
                symbol: None,
                max_files: None,
                max_chars: None,
            },
        )
        .expect_err("should reject path outside workspace");
        assert!(err.contains("outside the workspace root"));
    }
}
