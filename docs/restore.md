# Restore the archive and native data

Two recovery contracts are separate. Archive restore makes the search database usable again: a dated copy of the NAS hub SQLite file is put where hstry can search it. Native backup keeps the agents' own session files; `chronicle native restore` extracts them into an isolated directory. Never copy another machine's Cursor/Codex/agy directories into place blindly.

## What to restore

| File | Role |
|------|------|
| NAS hub `hstry.db` | Live merged archive (`<device-id>:*` sources) |
| Cloud Drive snapshot | Dated copy of that hub file (rclone / NAS Cloud Sync) |
| This machine's `staging.db` | Only this machine's latest ingest. Recreate by `hstry sync` after restore. |

Hard rule: **never overwrite a satellite's `staging.db` with the hub snapshot and then `remote sync --direction push`.** The hub file already has `{device_id}:` prefixes. Pushing it again double-namespaces or clobbers other devices.

## Windows reinstall or a new Windows PC (10 minutes to search)

1. Install hstry 1.0+ from this fork (`cargo install --path crates/hstry-cli`) and copy adapters (`just update-adapters-windows`).
2. Copy the **latest hub snapshot** to a search-only path, for example `C:/path/to/archive-restore.db`.
   - Prefer the configured NAS hub path.
   - If the NAS is down: use the off-site copy of that file.
3. Point this machine at the restore file **without** replacing staging:

```powershell
# Temporary search against the restored hub copy
$env:HSTRY_NO_SERVICE = "1"
hstry --database "C:/path/to/archive-restore.db" search "a phrase you remember" --limit 5
```

Or set `database = "D:/Data/hstry/archive-restore.db"` in a throwaway config. Keep the daily satellite `database = "D:/Data/hstry/staging.db"`.

4. Recreate local collection: `hstry source add` the tools on this PC, `hstry sync`, then `hstry remote sync --direction push` **only staging**.

If the hub itself was lost: restore the off-site snapshot to the configured hub database path first, then satellites can search and push as usual.

## Hub checkpoints (local rollback)

On the hub (NAS):

```bash
hstry checkpoint create
hstry checkpoint list
hstry checkpoint restore hstry-YYYYMMDD-HHMMSS
# writes a search-only copy next to the live db (`hstry-win.restore.db` by default)
```

The hub service creates a daily checkpoint when `[checkpoint] enabled = true`, tags Sunday copies as weekly, and prunes compressed archives to `max_total_bytes` (default 10 GiB). Failed `integrity_check` copies are discarded.

Restore never writes `staging.db`. `--live` replaces the hub file — stop `hstry service` first.

Off-site Drive copies remain optional and out of band (rclone / Feiniu Cloud Sync). hstry does not speak Drive.

## Native backup

`chronicle native` keeps the agents' own session files (Codex, Cursor, Claude Code, Antigravity, Grok). It does not depend on the search database or `config.toml`, so a broken archive never blocks it.

- `discover` lists the sources; `capture` writes an immutable snapshot under `<root>/snapshots/<id>` with a manifest of sizes and SHA-256 hashes; `watch` captures after changes settle.
- `replicate` stores the snapshot in the local Restic repository and copies it to the remote one; `status` shows pending changes and replication failures.
- `verify <id>` rechecks every file against the manifest.
- `restore <id> --target <dir>` extracts a snapshot into an isolated directory. It refuses to overwrite different content, accepts identical files so a retry is safe, and removes files it created if extraction fails. `--dry-run` writes nothing; a saved plan is checked against the snapshot and the target before it is applied.

Credentials, runtime locks, sockets and transient caches are never captured. Cursor is captured as a projection of recognized conversation keys only, not a full `state.vscdb` image.

Writing snapshots back into a live agent profile (native installation) is not part of this release. To use recovered sessions, extract them and move the files you need while the agent is closed.

The root is `data_root` in the native config, else `CHRONICLE_NATIVE_ROOT`, else the platform data directory. See `examples/chronicle-native.toml`.

### Disaster recovery from the NAS

1. Install Chronicle on the new machine and write a native config with `restic`, `password_file`, `remote_repository` and `data_root`.
2. Pull a snapshot:
   ```bash
   chronicle native --native-config chronicle-native.toml pull <snapshot-id>
   chronicle native --native-config chronicle-native.toml pull --host <old-hostname>
   ```
   Without a snapshot id, pull takes the newest snapshot, but refuses when the repository holds snapshots from more than one machine unless `--host` names one. The snapshot is restored into staging, verified, then published under `<root>/snapshots/<id>`. An existing local copy is verified instead of downloaded; a corrupt one is never overwritten. `--from local` pulls from the local repository.
3. Extract it: `chronicle native restore <snapshot-id> --target <dir>`.

### Per-host exclusions

- All hosts: `.lock`, `.sock`, `.pid`, and SQLite `-wal`/`-shm`/`-journal` files.
- claude-code: `.claude.json`, `credentials.json`, and `cache`, `telemetry`, `tmp`, `mcp-daemons`, `plugins` segments.
- antigravity: names containing `credential`, `token` or `auth`; `checkpoints/in-flight`.
- grok: `terminal` and `compaction` segments, `prompt_history.jsonl`, names containing `cookie` or `auth`.

### Background watch (Windows)

`scripts/native-service.ps1` registers `chronicle native watch` as a current-user logon task. It refuses to run without `-DryRun` (preview, no scheduler calls) or `-IReallyMeanIt`. The task runs single-instance at `\Chronicle\NativeWatch`, and re-running `Install` updates it in place.

## Session resume (priority B)

After the archive is searchable on this PC, `hstry resume` / export can feed a conversation back into a **local** agent on this same OS. That is optional and not the restore acceptance test. Acceptance is: `hstry search` hits the restored conversations.
