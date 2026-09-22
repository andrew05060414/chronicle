# Restore the archive and native data

Two recovery contracts are separate. Archive restore makes the HSTRY search database usable. Native restore recovers original agent files through `chronicle native restore`; never copy another machine's agent directories blindly.

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

## Native recovery

`chronicle native discover/capture/verify/replicate/restore` manages source snapshots and Restic repositories. `restore --target <directory>` extracts and verifies files in isolation. `restore --into <app> --dry-run` previews a native installation plan; omitting `--dry-run` saves a plan, and `--apply-plan` applies it. Production native installation currently fails closed for every host version; only synthetic tests enable the installation prototype. `--force` cannot bypass the host-version gate. Recovery is accepted only after file verification, client opening after restart, and a test continuation where the host supports it.

### Implementation audit (2026-09-22)

Restart handoff and dispatch instructions: [native-recovery-handoff.md](native-recovery-handoff.md). Task status and full worker envelopes live in `.trx/issues.jsonl`; the handoff is a reading guide, not a second task tracker.

The full native-recovery delivery is **not complete**. File extraction is not client restoration. Neither `applied-files` nor a successful Restic restore proves sidebar registration, restart behavior, or continuation.

Current tested behavior:

- Cursor capture creates fresh SQLite pages containing only recognized conversation keys in `ItemTable` and `cursorDiskKV`. Non-session rows (including NULL keys), other tables, and unknown artifacts are excluded. The manifest labels this `sqlite-session-projection`, not a full database image. Conversation payloads remain unchanged; this is not transcript redaction. A real projection was captured and independently checked with `scripts/verify-cursor-projection.py`: SQLite integrity, manifest SHA-256 and size, allowed tables and every stored key passed (167,727 session records, 1,994,354,688 bytes; 257,148 non-session source rows excluded). This does not establish native-client usability.
- Cursor fingerprints inspect only `state.vscdb` and its WAL. SQLite WAL changes trigger new capture even if the database file itself is unchanged. SHM/WAL files are not copied as standalone recovery artifacts.
- Native watch reconciles on startup, after five seconds of quiet, after at most sixty seconds of continuous events, and every five minutes. A separate worker backs up pending local snapshots and attempts pending NAS copies every five minutes. Errors remain pending; local receipt is saved before attempting remote copy.
- Extraction verifies identity, relative paths, sizes and hashes, preflights conflicts, accepts identical existing content, and removes newly created files on a returned failure. Saved plans fingerprint target contents, not merely the target pathname. Abrupt-process-crash journaling and full native installation are still outstanding.
- Search preserves distinct conversation/message identities even when text, title, and source match. A failed sole requested store returns an error; a surviving store in combined search remains usable with a coverage warning. The five-second local-refresh deadline includes opening the search database.

Remaining scope from the accepted plan, requiring implementation or stronger evidence before completion:

1. Complete source/component coverage and dependency manifests, including verified optional non-credential settings, skills, plugins, project associations and integrations. An enum or arbitrary configured source path does not establish this contract.
2. Harden the synthetic installation prototype for all five hosts: durable source type independent of source existence, correct path mapping, exact version/schema contracts, conflict rejection, index registration, transactional rollback and interrupted-install recovery. Basic Codex/Cursor merging and file-only host paths have fixture tests; production installation remains disabled. In particular, existing-target rollback must not export mixed credential databases into the backup repository.
3. Complete and verify per-source collection/acknowledgement/contact/error/pending state, startup/reconnection sync behavior and collection changes during upload. Existing optional provenance fields alone do not prove state is recorded.
4. Validate background deployment using a real user-level startup task, persistent failure reporting, restart/reconnection tests and baseline protection before replacing overlapping tasks. The new watch implementation is not yet deployed evidence.
5. Cursor snapshot `0afca009-7911-4ed8-8121-4601a64aa5bd` includes 1,376 composerHeaders and 167,727 key/value session records; 1,992,683,520 bytes, SHA-256 `769bd4f18837fd10134c8c7a99a83b6a214fbb6e85186bc0409b411e8fcd9758`. Local Restic receipt `a804accc1dce8fb4bf7d2847a76d35bf9824d7ff5cb2fcbea537a83e7d22c117`; NAS receipt `c413ba3233506510061534d3b6af48c3af458b88a25a0c1241c15950c3a1792d`, confirmed 2026-09-22T21:43:20Z. Independent NAS restoration, Restic verification, SQLite integrity, key audit and hash comparison passed. Real native-client open/restart/continuation checks remain for all five hosts.
6. Run the integrated required gate after remaining code stabilizes. No commit, publication or complete-plan claim follows from the focused checks alone.
