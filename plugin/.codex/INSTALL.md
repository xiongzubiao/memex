# Installing Memex for Codex

## Installation

```bash
npm install @memverge/memex
```

This downloads the memex binary and embedding model automatically.

## Manual Setup (alternative)

1. Clone and build from source:
   ```bash
   git clone https://github.com/memverge/memex.git
   cd memex && cargo install --path cli
   ```

2. Symlink skills:
   ```bash
   ln -s /path/to/memex/plugin/skills ~/.agents/skills/memex
   ```

## Updating

```bash
npm update @memverge/memex
```
