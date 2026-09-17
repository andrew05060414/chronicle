# Remote sync (satellite ↔ hub)

Generic reference for `hstry remote sync` push/pull behavior, configuration, and common failure modes.  
**Upstream candidate** — safe to contribute to `byteowlz/hstry` without environment-specific paths.

---

## How push works

`hstry remote sync --direction push` is a **hub-side merge**, not a whole-file replace:

1. The satellite exports conversations/sources changed since the last successful push (`updated_at` watermark in local `search_state`).
2. That small sqlite is SCP'd to the hub `inbox/` directory.
3. The hub runs `hstry hub ingest --file … --namespace {device_id}` under an exclusive file lock and merges into the **live** database (WAL).
4. The hub process keeps the live file open; satellites never SCP-overwrite it.

First push after upgrade (no watermark) still sends the whole staging DB one way. Later pushes are incremental.

`--full` keeps the legacy fetch/merge/SCP-replace path for disaster recovery only. Do not use it while the hub service is running.

If the hub binary is older than this change (`hstry hub` missing), the satellite logs a warning and falls back to `--full` so mixed-version rollouts do not hard-fail. Upgrade the hub first, then the satellites.

Pull is unchanged: fetch hub → merge into local staging with the remote name as namespace.

Conflict resolution inside a namespace uses `updated_at` (newer wins).

---

## Required satellite config

```toml
[[remotes]]
name = "hub"
host = "user@nas.example.com"
enabled = true
# Required when the hub DB is not the default ~/.local/share/hstry/hstry.db
database_path = "/path/to/hub/hstry.db"

[sync]
mode = "satellite"
device_id = "my-laptop"   # push prefix: my-laptop:cursor-abc123
hub_remote = "hub"
auto_sync = true
auto_sync_interval_secs = 300
```

- **`database_path`** — absolute path to the hub’s SQLite file on the remote host. Must match the hub config exactly (including spaces).
- **`device_id`** — stable per machine (`laptop`, `desktop`, `workstation`). Do not rely on the legacy hardcoded `local:` prefix.

---

## Hub paths with spaces

If `database_path` contains spaces (for example `/srv/chronicle data/hstry.db`):

- Remote **shell** commands (`test -f`, `echo`) must quote the path.
- **SCP** on Windows must pass `host:/path with spaces/file` as a **single argv element** — do not wrap the path in shell quotes (`host:'/path'` fails on Windows OpenSSH).

Symptom when broken: push “succeeds” but the hub only contains one machine’s data; verbose logs may show migrations `001_initial_schema` on an empty temp DB (fetch never happened).

---

## Operational rules

| Rule | Why |
|------|-----|
| **Hub owns the live file** | Satellites ingest via `hstry hub ingest`. Do not SCP onto the live sqlite. |
| **Ingest lock** | `hstry hub ingest` takes `{database}.ingest.lock`. Two satellites can `auto_sync` at once; the second waits. |
| **Verify after push** | Hub `hstry stats` should list multiple `{device_id}:*` source prefixes. |
| **Checkpoints** | Hub `hstry checkpoint create` (and the hub service when `[checkpoint] enabled`) writes compressed rollbacks. |

```bash
hstry hub ingest --file inbox/device-….db --namespace device-id --delete
hstry checkpoint create
hstry checkpoint list
hstry checkpoint restore hstry-20260826-040000
```

### Push verification

```bash
# On the hub (or via ssh)
hstry stats
hstry source list
```

Healthy multi-machine hub:

- Sources from each satellite (`laptop:*`, `desktop:*`, …).
- Conversation count ≈ sum of satellites minus cross-device duplicates (usually none).

Failed merge (replace):

- Only one device prefix, or legacy `local:*` only.
- Count matches a single staging DB.

### Recovery

```bash
cd "/path/to/hub"
cp -a hstry.db hstry.db.broken-$(date +%Y%m%d-%H%M%S)
cp -a hstry.db.<known-good-backup> hstry.db
rm -f hstry.db-wal hstry.db-shm
hstry stats
```

If `hstry stats` reports `database disk image is malformed` but the main file looks intact, try removing stale `-wal`/`-shm` first.

---

## Building from source (Windows)

After `cargo build --release -p hstry-cli`, install the binary you actually built:

```powershell
Copy-Item -Force .\target\release\hstry.exe $env:USERPROFILE\.cargo\bin\hstry.exe
```

If `CARGO_TARGET_DIR` points elsewhere (some IDE/sandbox setups), the repo’s `target\release\` may be stale. Confirm with `(Get-Item .\target\release\hstry.exe).LastWriteTime`.

---

## Related CLI

```bash
hstry remote test hub
hstry remote sync --remote hub --direction push -v
hstry search "query" --scope remote --remote hub
hstry dedup --cross-source
hstry source prune-cursor --auto-remove
```
