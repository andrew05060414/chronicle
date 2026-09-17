# Chronicle

Chronicle is the public product name for this local-first history system. The
implementation grew from the [byteowlz/hstry](https://github.com/byteowlz/hstry)
engine and is maintained at [andrew05060414/chronicle](https://github.com/andrew05060414/chronicle).

**Spoken / connecting-layer name: Chronicle.** Product home: [andrew05060414/chronicle](https://github.com/andrew05060414/chronicle). Commands: `chronicle` and `hstry` are the same binary. Crate names, `%APPDATA%\hstry\`, and the database path stay `hstry` so upstream merges do not explode. Upstream remains `byteowlz/hstry`.

Universal AI chat history database. Aggregates conversations from local coding agents (Cursor, Codex, Claude Code, Pi, OpenCode, QClaw, WorkBuddy, Antigravity CLI, and others) into a single searchable SQLite database. Optional NAS hub/satellite sync keeps Windows and Mac histories namespaced.

Web ChatGPT / Gemini live ingest is a later milestone; takeout export adapters already exist.

## Features

- Import chat history from multiple sources via pluggable TypeScript adapters
- One-off imports from files or directories with auto-detection
- Full-text search with separate indexes for natural language and code
- Filter by source, workspace, role, and local/remote scope
- Remote sync and search over SSH
- Background service for automatic syncing
- Optional terminal UI (`hstry-tui`) for interactive browsing
- Incremental adapter parsing with cursor-based batching
- Export conversations to adapter formats (markdown/json, pi, opencode, codex, claude-code, etc.)
- Resume past sessions in any coding agent with cross-format conversion
- Deduplicate conversations and export memories to mmry
- JSON output for scripting and MCP integration

## Installation

This fork is installed from source. Upstream Homebrew / AUR / Scoop packages track `byteowlz/hstry` and will not include these 1.0 changes.

### Cargo (this repo)

```bash
git clone https://github.com/andrew05060414/chronicle.git
cd chronicle
cargo install --path crates/hstry-cli
```

This installs both `hstry` and `chronicle`. They share `src/main.rs`.

To install all binaries (CLI, TUI, MCP):

```bash
cargo install --path crates/hstry-cli
cargo install --path crates/hstry-tui
cargo install --path crates/hstry-mcp
```

`hstry-tui` also installs `chronicle-tui`. `chronicle tui` launches whichever TUI binary is on `PATH`.

### Build from Source

```bash
git clone https://github.com/andrew05060414/chronicle.git
cd chronicle
cargo build --release --workspace
```

### Windows (build and usage)

Windows 11 is a first-class target. Paths follow the same `dirs` layout as other platforms:

| Directory | Windows default |
|-----------|-----------------|
| Config / adapters | `%APPDATA%\hstry\` |
| Database | `%LOCALAPPDATA%\hstry\hstry.db` |
| Service state | `%LOCALAPPDATA%\hstry\` |

**Dependencies**

| Dependency | Purpose | Install |
|------------|---------|---------|
| Rust toolchain | Compile | [rustup](https://rustup.rs/) |
| protoc | gRPC proto compile | `winget install Google.Protobuf` |
| Node LTS | Run adapters | `winget install OpenJS.NodeJS.LTS` |
| better-sqlite3 | Cursor / Codex SQLite parsing | In the adapters dir: `npm install` |

**Notes**

- Adapters are executed directly by the Rust runtime via `node` / `bun` / `deno`. **pnpm is not required at runtime.**
- Prefer `js_runtime = "node"` in `config.toml`. Deno cannot load the `better-sqlite3` native module.
- After cloning or updating adapters, copy them into the config directory:

```powershell
# From the repo root
just update-adapters-windows
# or
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/update-adapters.ps1
```

Then install native deps once:

```powershell
cd $env:APPDATA\hstry\adapters
npm install
```

**Build and run**

```powershell
# Needs protoc on PATH (see winget above)
cargo build -p hstry-cli --release
cargo run -p hstry-cli -- scan
cargo run -p hstry-cli -- import $env:USERPROFILE\.codex\sessions
cargo run -p hstry-cli -- search "query"
```

Service uses TCP by default on Windows (`transport = "tcp"`). Do not set `transport = "unix"`.

## Quick Start

```bash
# Quickstart: scan, add sources, and sync
hstry quickstart

# Install Playwright browsers (web automation)
hstry web install

# Login to a web provider (headful for first login)
hstry web login chatgpt

# Sync web providers (uses saved sessions)
hstry web sync --provider chatgpt

# Note: web sync currently supports ChatGPT (including multiple workspaces).
# Claude and Gemini sync support is planned.

# Scan for supported chat history sources
hstry scan

# Add a source (auto-detects adapter)
hstry source add ~/.codex/sessions

# Sync all sources
hstry sync

# Control sync concurrency
hstry sync --parallel 2

# Import a one-off export directory
hstry import ~/Downloads/chatgpt-export

# Search your history
hstry search "how to parse JSON"

# List recent conversations
hstry list --limit 10

# View a specific conversation
hstry show <conversation-id>

# Export a conversation to markdown
hstry export --format markdown --conversations <conversation-id> --output ./conversation.md

# Resume a past session in your preferred coding agent
hstry resume --search "JSON parser" --agent pi

# Resume with time filter
hstry resume --after "yesterday" --workspace myproject

# Browse recent and pick interactively
hstry resume --limit 10
```

## Commands

| Command | Description |
|---------|-------------|
| `quickstart` | Scan known paths, add sources, and sync everything |
| `web install` | Install Playwright browsers for web automation |
| `web login` | Login to a web provider and store session state |
| `web sync` | Sync web providers and import chats |
| `web status` | Show web login and sync status |
| `scan` | Detect chat history sources on the system |
| `sync` | Import conversations from all configured sources in parallel (resets cursor if source is empty) |
| `import <path>` | One-off import with auto-detected adapter |
| `search <query>` | Full-text search across all messages |
| `index` | Build or refresh the search index |
| `list` | List conversations with optional filters (workspace uses substring match) |
| `show <id>` / `read` / `peek` | Display a conversation (bounded pages; `--full` for legacy) |
| `export` | Export conversations to markdown/json or adapter format |
| `resume` | Resume a past session in a coding agent (pi, claude-code, codex, etc.) |
| `dedup` | Deduplicate conversations in the database |
| `source add/list/remove` | Manage import sources |
| `adapters list/add/enable/disable` | Manage adapters |
| `adapters repo ...` | Manage adapter repositories (git/archive/local) |
| `remote add/list/remove/test/fetch/sync/status` | Manage remote hosts and sync |
| `checkpoint create/list/restore/prune` | Rolling compressed snapshots of the live database |
| `backup` | 3-2-1 backup: integrity check, NAS remote push, Oracle `scp`, Google Drive `rclone` |
| `skills audit/list/sync/bootstrap` | Optional Andrew-Skill / ASM proxy (not a memory store) |
| `skill install/status/update` | Install the bundled agent retrieval skill |
| `tui` | Launch `chronicle-tui` / `hstry-tui` |
| `service enable/disable/start/run/restart/stop/status` | Control background sync service |
| `config show/path/edit` | Manage configuration |
| `stats` | Show database statistics |
| `mmry extract` | Export memories to mmry |

Adapter installs are version-pinned to the hstry binary. Run `hstry adapters update`
whenever you upgrade, and the CLI will refuse to sync if adapter manifests do not
match the current hstry version.

## Browser extension

`extension/` contains **hstry sync**, a Chrome MV3 extension that background-syncs conversations from ChatGPT, Claude, Gemini, and Perplexity into your local database. It POSTs new conversations to a running `hstry-api` instance (`http://127.0.0.1:3000/ingest`, token-authenticated).

```bash
hstry-api --port 3000   # start the API, optionally with --token <secret>
```

Load it from `chrome://extensions` with Developer mode enabled (Load unpacked, select `extension/`). Provider toggles, port, and token are configured on the extension's options page. The `hstry web` Playwright commands are the headless alternative to the extension.

## Search Modes

The search command auto-detects query type:

- **Natural language**: Uses porter stemming for English text
- **Code**: Preserves underscores, dots, and path separators

Force a mode with `--mode natural` or `--mode code`.

Scope and filters:

- `--scope local|remote|all` (satellite with `hub_remote` defaults to **remote**; otherwise local)
- `--remote <name>` to target specific remotes (satellite default is `sync.hub_remote`)
- `--source`, `--workspace`, `--role` filters
- `--no-tools` to exclude tool calls
- `--dedup` to collapse similar results
- `--include-system` to include system context (AGENTS.md, etc.)

## Session Resume

The `resume` command opens a past session in your preferred coding agent. It handles
cross-agent format conversion automatically -- a Codex session can be resumed in pi,
a Claude Code session in Codex, etc.

```bash
# Direct resume by conversation ID
hstry resume <conversation-id>

# Search for a session
hstry resume --search "async runtime refactor"

# Browse recent sessions and pick interactively
hstry resume --limit 10

# Filter by time
hstry resume --after "yesterday"
hstry resume --after "2 days ago" --before "today"
hstry resume --after "2026-02-01" --before "2026-03-01"

# Filter by source or workspace
hstry resume --source codex-main --workspace myproject

# Target a specific agent (overrides default_agent from config)
hstry resume --search "refactor" --agent claude-code

# Dry run (show what would happen without writing or launching)
hstry resume --dry-run --search "query"

# JSON output for automation
hstry resume --json --search "query"
```

**How it works:**

1. If the session already belongs to the target agent and the original file exists on disk, it launches directly (zero conversion overhead).
2. Otherwise, it exports the session via the target adapter, places the converted file in the agent's native session directory, and launches the agent.

**Time filter formats:** ISO dates (`2026-03-01`), relative dates (`yesterday`, `today`, `last week`, `last month`), duration expressions (`2 days ago`, `3 weeks ago`, `1 month ago`).

Configure the default agent and per-agent launch commands in `config.toml`:

```toml
[resume]
default_agent = "pi"

[resume.agents.pi]
format = "pi"
command = "pi --session {session_path}"
session_dir = "~/.pi/agent/sessions"

[resume.agents.claude-code]
format = "claude-code"
command = "claude --resume {session_id}"
session_dir = "~/.claude/projects"
```

Command templates support these placeholders: `{session_path}`, `{session_id}`, `{workspace}`.

## Configuration

hstry follows XDG Base Directory specifications on Unix, and the platform defaults from the `dirs` crate on Windows:

| Directory | Unix default | Windows default | Environment Override |
|-----------|--------------|-----------------|---------------------|
| Config | `~/.config/hstry/` | `%APPDATA%\hstry\` | `$XDG_CONFIG_HOME/hstry/` |
| Data | `~/.local/share/hstry/` | `%LOCALAPPDATA%\hstry\` | `$XDG_DATA_HOME/hstry/` |
| State | `~/.local/state/hstry/` | `%LOCALAPPDATA%\hstry\` | `$XDG_STATE_HOME/hstry/` |

Default config: `~/.config/hstry/config.toml` (Windows: `%APPDATA%\hstry\config.toml`)

```toml
"$schema" = "https://raw.githubusercontent.com/andrew05060414/chronicle/main/examples/config.schema.json"

database = "~/.local/share/hstry/hstry.db"
adapter_paths = ["~/.config/hstry/adapters"]
js_runtime = "auto"  # bun, deno, or node

[[adapters]]
name = "codex"
enabled = true

[service]
enabled = false
poll_interval_secs = 30
search_api = true

[search]
index_batch_size = 500

[resume]
default_agent = "pi"

[resume.agents.pi]
format = "pi"
command = "pi --session {session_path}"
session_dir = "~/.pi/agent/sessions"
```

See `examples/config.toml` for all options. Use `hstry config show/path/edit` for config management.

## Service + API

`hstry service` runs a local daemon that keeps the search index warm and exposes a
local-only gRPC search endpoint. The CLI prefers the service when it is running.
Use `hstry service enable/disable/start/run/restart/stop/status` to manage it.

The optional `hstry-api` binary serves a local HTTP API (default `http://127.0.0.1:3000`)
for external integrations (e.g., Octo).

Override service usage with `HSTRY_NO_SERVICE=1`. Override the API URL with
`HSTRY_API_URL` or disable API usage with `HSTRY_NO_API=1`.

## Remote Sync

hstry can sync and search remote databases over SSH. Remotes require `hstry` to
be installed on the host.

```bash
# Add a remote host
hstry remote add laptop user@laptop

# Verify connectivity
hstry remote test laptop

# Fetch the remote database into the local cache
hstry remote fetch --remote laptop

# Search only remote results
hstry search "auth error" --scope remote --remote laptop

# Sync (merge) remote history into the local database
hstry remote sync --remote laptop --direction pull

# Push local staging to a hub (satellite mode; merges by device_id namespace)
hstry remote sync --remote hub --direction push
```

Satellite push merges into the hub database (it does not replace it). Configure
`[[remotes]].database_path` when the hub DB is not the default path, and set
`sync.device_id` per machine. See [`docs/remote-sync.md`](docs/remote-sync.md) for
troubleshooting (spaced paths, concurrent push, verification).

## Terminal UI

Use the optional `hstry-tui` binary for an interactive, three-pane browser.

```bash
cargo install --path crates/hstry-tui
hstry-tui
```

## Supported Sources

### Local Agents & Apps (automatic local storage)

| Adapter | Default Path | Description |
|---------|--------------|-------------|
| `claude-code` | `~/.claude/projects` | Claude Code CLI |
| `codex` | `~/.codex/sessions` | OpenAI Codex CLI |
| `cursor` | `Cursor workspaceStorage` (platform-specific) | Cursor (state.vscdb) |
| `opencode` | `~/.local/share/opencode` | OpenCode |
| `dsh` | `~/.dsh/sessions` | DeepSeek Harness (`session.jsonl` / `.zstd`) |
| `pi` | `~/.pi/agent/sessions` | Pi coding agent |
| `qclaw` | `~/.qclaw/agents` | QClaw / OpenClaw (Pi-style JSONL) |
| `workbuddy` | `~/.workbuddy/projects` | WorkBuddy (event JSONL; v1 skips subagents) |
| `antigravity` | `~/.gemini/tmp`, `~/.gemini/antigravity`, `~/.gemini/antigravity-cli`, `~/.gemini/antigravity-ide` | Gemini CLI JSONL plus Antigravity 2.0 / agy / IDE 1 stores |
| `zcode` | `~/.zcode` | Zcode / ZAI (`cli/db/db.sqlite`) |
| `aider` | Project directories | Aider (finds `.aider.chat.history.md`) |
| `goose` | `~/.local/share/goose/sessions` | Goose (SQLite/JSONL) |
| `jan` | `~/jan/threads` | Jan.ai |
| `lmstudio` | `~/.cache/lm-studio/conversations` | LM Studio |
| `openwebui` | `~/.open-webui/data` (or `/app/backend/data`) | Open WebUI |

### Web Exports (manual download)

| Adapter | Source | Export Location |
|---------|--------|-----------------|
| `chatgpt` | ChatGPT | Settings > Data controls > Export |
| `claude-web` | Claude.ai | Settings > Export data |
| `gemini` | Gemini | google.com/takeout > Gemini Apps |

Point these adapters at the extracted export directory (e.g., `~/Downloads/chatgpt-export`).

## Adapters

Adapters are TypeScript modules that parse chat history from specific tools. Each adapter implements:

- `detect(path)` - Check if a path contains valid data
- `parse(path, options)` - Extract conversations and messages

Add custom adapters by placing them in `adapter_paths`, or manage repositories with:

```bash
hstry adapters repo add-git community https://example.com/adapters.git
hstry adapters update
```

## Workspace Structure

```
crates/
  hstry-core/     # Database, config, models
  hstry-runtime/  # TypeScript adapter execution
  hstry-cli/      # Command-line interface
  hstry-tui/      # Terminal UI (ratatui)
  hstry-mcp/      # MCP server
  hstry-api/      # HTTP API (axum)
```

## Development

```bash
just check-all       # Format, lint, and test
just test            # Run tests only
just clippy          # Lint only
just update-adapters # Copy latest adapters to ~/.config/hstry/adapters
just update-adapters-windows # Windows: copy to %APPDATA%\hstry\adapters
```
## Chronicle connecting layer

`chronicle` is the speakable name for this fork's CLI. It owns the conversation archive (`search` / `peek` / `show` / `sync` / `remote` / `checkpoint` / `backup`) and can optionally proxy skill install/audit to an external Andrew-Skill checkout.

```bash
chronicle search "query" --scope local
chronicle backup --dry-run
chronicle tui
```

The skills commands are opt-in and are not required by the core CLI. Set
`CHRONICLE_SKILL_ROOT` to an Andrew-Skill checkout before using them; the
integration also requires PowerShell and `asm` on `PATH`:

```bash
CHRONICLE_SKILL_ROOT=/path/to/Andrew-Skill chronicle skills audit
```

3-2-1 backup uses the **configured live database** only. Set the database path
in your own config; no private machine path is required or assumed.
`--encrypt` requires `CHRONICLE_BACKUP_KEY`; there is no default passphrase. If
`rclone` is missing, the Google Drive step is `skipped` (not `ok`).

Not on this bus: CTX, AMS, Mem0, the jobs tracker, knowledge-hub.

Daily NAS sync can optionally chain offsite copies via `hstry-daily-sync.ps1 -Offsite`.

## Contributing

Issues live in `.trx/`. See [CHANGELOG.md](CHANGELOG.md) for release notes.

## Release Notes

See [CHANGELOG.md](CHANGELOG.md) for the full list of changes.

## Release Process

Tags are cut from the branch selected by the release owner. Upstream
Homebrew/AUR references for `byteowlz/hstry` are retained as historical
infrastructure notes; they are not a claim that Chronicle packages are
published. See [docs/RELEASE.md](docs/RELEASE.md) before changing automation.

## Attribution

See [docs/ATTRIBUTION.md](docs/ATTRIBUTION.md) for the distinction between the
upstream engine, fork changes, and Chronicle's product structure and
integration. This project also references ideas from **cross-agent-session-search
(cass)** by Jeffrey Emanuel. Source:
https://github.com/Dicklesworthstone/coding_agent_session_search (MIT License).

## License

MIT
