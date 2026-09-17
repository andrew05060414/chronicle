# Generic NAS hub setup

This is a provider-neutral example for a Chronicle hub with one or more
satellites. Replace every placeholder before running a command. Do not put
hostnames, usernames, private IPs, or real database paths in public issues or
commits.

## Hub variables

```bash
HUB_HOST="user@nas.example.com"
HUB_ROOT="/srv/chronicle"
HUB_DB="$HUB_ROOT/hstry.db"
HUB_ADAPTERS="$HUB_ROOT/adapters"
```

The hub's `database` setting and every satellite's remote `database_path` must
refer to the same live SQLite file. The hub owns that file; satellites push
incremental exports through `hstry hub ingest` and must not overwrite it with
SCP.

## Hub configuration

Create `~/.config/hstry/config.toml` on the hub:

```toml
database = "/srv/chronicle/hstry.db"
js_runtime = "node"
adapter_paths = ["/srv/chronicle/adapters"]

[service]
enabled = true
search_api = true

[sync]
mode = "hub"
auto_sync = false

[checkpoint]
enabled = true
dir = "/srv/chronicle/checkpoints"
```

Install the version-pinned adapters from the Chronicle release that matches
the binary, then verify:

```bash
hstry adapters update
hstry service start
hstry service status
hstry stats
```

## Satellite configuration

Each satellite needs its own stable `device_id` and a remote entry like:

```toml
[[remotes]]
name = "hub"
host = "user@nas.example.com"
enabled = true
database_path = "/srv/chronicle/hstry.db"

[sync]
mode = "satellite"
device_id = "my-laptop"
hub_remote = "hub"
auto_sync = true
auto_sync_interval_secs = 300
```

Before the first push, create a hub checkpoint. After pushing, verify that
`hstry source list` contains the expected `<device-id>:` prefixes and that the
hub conversation count did not drop to a single satellite's count.

For path quoting, incremental merge behavior, and recovery rules, see
[`remote-sync.md`](./remote-sync.md) and [`restore.md`](./restore.md).
