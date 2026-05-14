# Installing Memex for Codex

## Installation

```bash
npm install -g @xiongzubiao/memex
memex install --agent codex
```

The npm step downloads the platform binary, embedding model, tokenizer,
and ONNX Runtime. `memex install --agent codex` merges hooks into
`~/.codex/hooks.json`.

Codex's `codex_hooks` feature flag is stable and enabled by default as
of `codex-cli 0.128.0`. If you're on an older Codex that has it gated,
run `codex features enable codex_hooks` to turn it on.

## Manual setup (build from source)

```bash
git clone https://github.com/xiongzubiao/memex.git
cd memex && cargo install --path cli
# Download the embedding model + tokenizer + ONNX Runtime manually,
# or run plugin/postinstall.js after `cd plugin && npm install`.
memex install --agent codex
```

## Updating

```bash
npm update -g @xiongzubiao/memex
memex install --agent codex   # re-sync hooks if the template changed
```
