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

`chronicle native discover/capture/verify/replicate/pull/restore` manages source snapshots and Restic repositories. `restore --target <directory>` extracts and verifies files in isolation. `restore --into <app> --dry-run` previews a native installation plan; omitting `--dry-run` saves a plan, and `--apply-plan` applies it.

### Disaster recovery from the NAS

When a machine is lost or rebuilt, native agent snapshots can be retrieved directly from the remote Restic repository on the NAS:

1. Install Chronicle on the new machine:
   ```bash
   cargo install --path crates/hstry-cli
   ```
2. Write a minimal configuration pointing to the remote repository (e.g. `chronicle-native.toml`):
   ```toml
   restic = "/path/to/restic"
   password_file = "/path/to/password-file"
   remote_repository = "rest:http://nas.local:8000/chronicle"
   data_root = "/path/to/data-root"
   ```
3. Pull the snapshot from the NAS:
   ```bash
   # Pull the latest available snapshot across all chronicle-native archives
   chronicle native pull

   # Or pull a specific snapshot ID
   chronicle native pull <snapshot-id>
   ```
   `chronicle native pull` queries the repository, restores into staging under `<root>/staging/pull-<id>-<timestamp>`, verifies the manifest and file hashes, and records the verified snapshot at `<root>/snapshots/<id>`. By default `--from remote` is used; `--from local` pulls from a local repository.
4. Extract or install:
   - For isolated extraction:
     ```bash
     chronicle native restore <snapshot-id> --target /path/to/extracted
     ```
   - For native client installation (guarded by the host version verification registry):
     ```bash
     chronicle native restore <snapshot-id> --into <app> --dry-run
     ```

### Host version gate & verification registry

Client installation is guarded by a strict host version gate backed by the registry `<root>/verified-hosts.json`:

- **Fail-closed**: Any unknown, unverified, or revoked host version fails closed. Production installation into real client directories is refused, and `--force` cannot bypass this gate. Files can only be extracted in isolation via `restore --target <directory>`.
- **Management commands (`native hosts`)**:
  - `hosts record --app <app> --evidence <path>`: Validates a complete UAT drill evidence log (verifying capture, replication, isolation, file restoration, client opening, restart, and continuation) and registers the host version as `verified`.
  - `hosts revoke --app <app> --version <v> --reason <text>`: Appends a `revoked` record for a previously trusted version.
  - `hosts check`: Detects the current host versions against configured native sources, compares each with the registry (`verified`, `unverified`, `revoked`, or `unknown`), and writes `<root>/hosts-check.json` (also surfaced in `chronicle native status`).
- **Drill mode**: Unverified or revoked versions are permitted to build and apply installation plans *only* under isolated drill conditions: every plan target must reside within an ancestor directory containing a `.chronicle-drill` marker file, and no target may fall within standard client data roots under the user's home directory. Plans generated in this mode are tagged `"drill": true`, and drill isolation constraints are strictly re-verified upon plan execution.

### Automated host drill script (`scripts/native-drill.ps1`)

To verify client recovery end-to-end and qualify a host version for registry recording, run `scripts/native-drill.ps1`. It automates the full drill lifecycle (session creation, capture, replication to a dedicated drill NAS repository, local data wipe, pull from remote, restore-install into an isolated avatar profile, and continuation checks):

```powershell
# Preflight validation (guards, commands, and version probe; no client launch, no NAS access)
pwsh -NoProfile -File scripts/native-drill.ps1 `
    -App <codex|claude-code|grok|cursor> `
    -DrillRoot '/path/to/isolated-drill-root' `
    -NasRepository 'rest:http://nas.local:8000/chronicle-drill' `
    -Restic '/path/to/restic' `
    -ChronicleExe 'chronicle' `
    -Preflight

# Full automated drill with automatic host registration upon passing
pwsh -NoProfile -File scripts/native-drill.ps1 `
    -App <codex|claude-code|grok|cursor> `
    -DrillRoot '/path/to/isolated-drill-root' `
    -NasRepository 'rest:http://nas.local:8000/chronicle-drill' `
    -Restic '/path/to/restic' `
    -ChronicleExe 'chronicle' `
    -Register
```

#### Drill repository isolation and credentials
- **Dedicated repository**: `-NasRepository` must be dedicated exclusively to drills and must never point to a production backup repository.
- **Dedicated password**: The script maintains an isolated 32-byte cryptographic random password at `<DrillRoot>/.restic-drill-password` (generated on first run and reused on subsequent runs). Production password files are never used or read. If this password file is lost, simply specify a new drill repository path.
- **Path guards and extra roots**: Real client directories under `$HOME` and `AppData` are strictly forbidden as drill roots or targets. Additional paths to protect (such as private repositories or secret directories) can be supplied via `-ProtectedRoot <string[]>`, and any path in `CHRONICLE_NATIVE_ROOT` is automatically protected. All deletion operations are validated to strictly reside inside `DrillRoot`.

### Per-host unsupported items

During capture (recorded as manifest exclusions) and native install (enforced as plan refusal), unsupported files fail closed according to `is_unsupported_path`:

- **Generic (all hosts)**:
  - Process locks and sockets: files ending with `.lock`, `.sock`, or `.pid`.
  - SQLite locks and journals: files ending with `-wal`, `-shm`, or `-journal`.
- **claude-code**:
  - Credential files: `.claude.json` and `credentials.json`.
  - Transient runtime state: paths containing `cache`, `telemetry`, `tmp`, `mcp-daemons`, or `plugins` segments.
- **antigravity**:
  - Credentials and tokens: file names containing `credential`, `token`, or `auth` (case-insensitive).
  - In-flight checkpoints: paths containing `checkpoints/in-flight`.
- **grok**:
  - Terminal buffers and compaction locks: paths containing `terminal` or `compaction` segments.
  - Prompt history cache: `prompt_history.jsonl`.
  - Authentication state: file names containing `cookie` or `auth` (case-insensitive).

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

## Deployment (isolated harness)

Background watch execution is managed via `scripts/native-service.ps1`:

```powershell
# Preview deployment without scheduler calls (always safe)
pwsh -NoProfile -File scripts/native-service.ps1 -Action Install -NativeConfig 'C:/path/to/chronicle-native.toml' -DryRun

# Explicit switch required to run against Windows Task Scheduler
pwsh -NoProfile -File scripts/native-service.ps1 -Action Install -NativeConfig 'C:/path/to/chronicle-native.toml' -IReallyMeanIt

# Check service status or uninstall
pwsh -NoProfile -File scripts/native-service.ps1 -Action Status -DryRun
pwsh -NoProfile -File scripts/native-service.ps1 -Action Uninstall -DryRun
```

### Safety and Idempotency Guarantees
- **Safety gate**: Running `Install` or `Uninstall` without `-DryRun` is strictly refused unless the explicit `-IReallyMeanIt` switch is provided. `-DryRun` executes zero Task Scheduler or registry calls.
- **Single-instance**: The scheduled task runs with `MultipleInstances = IgnoreNew` to ensure only one watch process executes at a time.
- **Idempotent registration**: The task is registered at a fixed user-level path (`\Chronicle\NativeWatch`). Re-running `Install` updates the task in-place and never registers duplicate tasks. Overlapping tasks matching `(chronicle|hstry).*native.*watch` are reported without being modified or clobbered.
- **Current user credentials**: Executes under the current user (`LogonType = Interactive`) without elevation and without stored credentials.

### Known Issues
- **Console window visibility**: `chronicle.exe` is a console application, so a logon task displays a console window for its entire lifetime. Hiding it (for example, via `conhost --headless`, a background launcher, or a windowless runner) is **NOT DONE** and is to be decided in the authorized drill.

### Authorized drill checklist (NOT DONE)
The following deployment drills require explicit production authorization and live host execution, and remain **NOT DONE**:
- [ ] **Reboot catch-up**: Live machine restart drill verifying `\Chronicle\NativeWatch` starts at user logon and immediately catches up pending changes. (NOT DONE)
- [ ] **NAS offline catch-up**: Live network drill verifying background watcher logs remote replication failure when NAS is unreachable and catches up once NAS reconnects. (NOT DONE)
- [ ] **Disk-full drill**: Live drill verifying staging write failures refuse capture without deleting existing history or manifests. (NOT DONE)
- [ ] **Overlapping task migration**: Inspecting the user's existing production scheduled tasks and safely replacing overlapping legacy watch tasks. (NOT DONE)
- [ ] **Console window suppression**: Selecting and testing a windowless launcher or headless host strategy for the background watch task. (NOT DONE)

