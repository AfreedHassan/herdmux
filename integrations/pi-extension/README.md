# herdmux Pi runtime extension

Pi extension that registers the live `pi` process with herdmux so tmux pane scans can map Pi session IDs to exact panes. It also sends live Pi agent events with the latest user prompt so the sidebar does not have to wait for JSONL scanning.

## Usage

Run Pi with the extension:

```bash
pi --extension /path/to/herdmux/integrations/pi-extension/herdmux-runtime.ts
```

Or copy/symlink the file into one of Pi's extension locations:

- `~/.pi/agent/extensions/`
- `.pi/extensions/`

## What it does

The extension POSTs the current Pi runtime identity to herdmux on localhost:

- `POST /api/runtime/pi/upsert` on `session_start`
- heartbeat every 5 seconds while Pi is alive
- `POST /api/runtime/pi/delete` on `session_shutdown`
- `POST /api/agent-event` on `before_agent_start` with `status=running` and `lastUserPrompt`
- `POST /api/agent-event` on `agent_end` with `status=done`

By default it talks to `http://127.0.0.1:7391`.
Override with:

```bash
export HERDMUX_URL=http://127.0.0.1:7391
```
