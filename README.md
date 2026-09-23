# plugin-agent-codex

OpenAI Codex CLI agent launcher plugin for Basalt.

Exposes OpenAI Codex CLI (`codex`) as a Basalt AI agent with model selection, reasoning effort variants, MCP server wiring, and stream-json parsing.

## Provides
- `agent-launcher@codex/v1`

## Features
- Dynamic model selection (`codex models` / `gpt-4o`, `o1`, `o3-mini`)
- Reasoning effort variants (`low`, `medium`, `high`)
- MCP tool integration
- JSON stream event parsing (`agent_parse_line`)
