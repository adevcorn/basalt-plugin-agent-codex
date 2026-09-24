# plugin-agent-codex

OpenAI Codex CLI agent launcher plugin for Basalt.

Exposes OpenAI Codex CLI (`codex`) as a Basalt AI agent with model selection,
reasoning effort variants, MCP server wiring, and `--json` event parsing.

## Launch contract (verified against `codex-cli 0.156`)

- Fresh turn: `codex exec --json --model {model} {prompt}`
- Resume: `codex exec resume {session_id} --json --model {model} {prompt}`
- Reasoning effort and the Basalt MCP server ride on `-c` config overrides
  emitted by `basalt_agent_prepare_launch` (`model_reasoning_effort`,
  `mcp_servers.basalt.url`). `codex` has no `--reasoning-effort`,
  `--mcp-config`, `--continue`, or `--output-format` flags.
- Non-interactive runs use `--dangerously-bypass-approvals-and-sandbox`
  inside Basalt's disposable shadow workspace (the only approval mode
  `exec resume` also accepts, keeping fresh and resumed turns consistent).

## Provides
- `agent-launcher@codex/v1`

## Features
- Model selection via `--model` (discovery is host-side: `codex` exposes no
  `models` subcommand, so the host falls back to a static list of known
  Codex models)
- Reasoning effort variants (`low`, `medium`, `high`, …) via
  `-c model_reasoning_effort=…` (omitted for the default variant — `codex`
  fatally rejects an empty value)
- MCP tool integration via `-c mcp_servers.basalt.url=…`
- JSONL event parsing (`basalt_agent_parse_line`): `thread.started`,
  `item.started|updated|completed`, `turn.completed|failed`
