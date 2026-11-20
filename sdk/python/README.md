# BaiCode Python SDK

Build Python applications that start BaiCode threads, run turns, stream progress,
and control workspace access.

## Install

Install the locally built SDK and runtime wheels from the same directory.

```bash
pip install --no-index --find-links /path/to/wheels baicode-sdk
```

## Quickstart

Configure your model in `~/.BaiCode/config.toml` before starting a thread.

```python
from baicode_sdk import Codex

with Codex() as codex:
    thread = codex.thread_start()
    result = thread.run("Explain this repository in three bullets.")
    print(result.final_response)
```

`thread.run(...)` returns a `TurnResult` containing the final response,
collected items, and token usage.

## Built-In Help

Use Python's standard `help(baicode_sdk)`, `help(Codex)`, or
`python -m pydoc baicode_sdk` documentation tools.

## Documentation

- [Getting started](https://github.com/XFreeMing/BaiCode/blob/main/sdk/python/docs/getting-started.md)
- [API reference](https://github.com/XFreeMing/BaiCode/blob/main/sdk/python/docs/api-reference.md)
- [Untrusted external messages](https://github.com/XFreeMing/BaiCode/blob/main/sdk/python/docs/api-reference.md#externalmessage)
- [FAQ](https://github.com/XFreeMing/BaiCode/blob/main/sdk/python/docs/faq.md)
- [Examples](https://github.com/XFreeMing/BaiCode/blob/main/sdk/python/examples/README.md)

The package is licensed under the
[repository Apache License 2.0](https://github.com/XFreeMing/BaiCode/blob/main/LICENSE).
