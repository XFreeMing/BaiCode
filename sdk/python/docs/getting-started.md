# Getting Started

Install the locally built SDK and runtime wheels in a Python 3.10 or newer environment.

```bash
pip install --no-index --find-links /path/to/wheels baicode-sdk
```

Set the model provider in `~/.BaiCode/config.toml` using the [BaiInf example](../../../README.md#使用-baiinf). The local engine works without a login.

## Run A Turn

```python
from baicode_sdk import Codex, Sandbox

with Codex() as codex:
    thread = codex.thread_start(sandbox=Sandbox.workspace_write)
    result = thread.run("Say hello in one sentence.")

    print("Thread:", thread.id)
    print("Text:", result.final_response)
    print("Items:", len(result.items))
```

`Thread.run(...)` starts a turn, waits for completion, and returns
`TurnResult`. Plain strings are shorthand for `TextInput(...)`.

Use `Thread.turn(...)` when you need a `TurnHandle` for streaming, steering,
or interrupting an active turn.

For **untrusted content** from another agent, tool, or application, pass an
[`ExternalMessage`](api-reference.md#externalmessage). It retains tool-level
authority and does not establish user authorization or approval. Plain strings
and `TextInput` represent user input.

## Choose Sandbox Access

Use one enum for the initial thread and later turn overrides:

```python
from baicode_sdk import Codex, Sandbox

with Codex() as codex:
    thread = codex.thread_start(sandbox=Sandbox.workspace_write)
    thread.run("Make the requested changes.")
    review = thread.run("Review the diff only.", sandbox=Sandbox.read_only)
```

Available presets:

- `Sandbox.read_only`: read files without allowing writes.
- `Sandbox.workspace_write`: read files and write inside the workspace and
  configured writable roots; this is the normal default for workspace work.
- `Sandbox.full_access`: run without filesystem access restrictions.

When `sandbox=` is omitted, Codex uses its configured default. A turn override
also applies to subsequent turns on that thread.

## Continue A Thread

```python
from baicode_sdk import Codex

with Codex() as codex:
    thread = codex.thread_start()
    thread.run("Summarize Rust ownership in two bullets.")
    result = thread.run("Now explain it to a Python developer.")
    print(result.final_response)
```

To resume a stored thread later:

```python
with Codex() as codex:
    thread = codex.thread_resume("thr_123")
    print(thread.run("Continue where we left off.").final_response)
```

## Use The Async Client

```python
import asyncio

from baicode_sdk import AsyncCodex, Sandbox


async def main() -> None:
    async with AsyncCodex() as codex:
        thread = await codex.thread_start(sandbox=Sandbox.workspace_write)
        result = await thread.run("Continue where we left off.")
        print(result.final_response)


asyncio.run(main())
```

## Get Help

Python's built-in documentation tools cover the curated SDK surface:

```python
import baicode_sdk
from baicode_sdk import Codex, CodexConfig

help(baicode_sdk)
help(Codex)
help(CodexConfig)
```

```bash
python -m pydoc baicode_sdk
```

## Developing From This Repository

Contributors working from a checkout can install development dependencies from
the repository:

```bash
cd sdk/python
uv sync --group dev
source .venv/bin/activate
```

## Next Stops

- [API reference](https://github.com/XFreeMing/BaiCode/blob/main/sdk/python/docs/api-reference.md)
- [FAQ](https://github.com/XFreeMing/BaiCode/blob/main/sdk/python/docs/faq.md)
- [Runnable examples](https://github.com/XFreeMing/BaiCode/blob/main/sdk/python/examples/README.md)
