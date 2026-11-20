import path from "node:path";

export function codexPathOverride() {
  return (
    process.env.BAICODE_EXECUTABLE ??
    path.join(process.cwd(), "..", "..", "codex-rs", "target", "debug", "codex")
  );
}
