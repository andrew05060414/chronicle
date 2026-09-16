# Mac Satellite Handoff（直接交给 Mac 上的 AI）

把下面「PROMPT」整段复制给 Mac 上的 Cursor / Claude / Codex。不要改拓扑，不要合并两份 NAS 库。

---

## PROMPT

```text
你在 Andrew 的 Mac（device_id=macbook）上工作。把这台 Mac 做成 hstry 1.0.0 satellite：采集本机 AI 对话，merge push 到飞牛 NAS 的 live hub。

用 Bash。不要装、不要调用 PowerShell。中文向我汇报。

## 目标

本机 staging.db 采集 → `hstry remote sync --direction push` → NAS live 库出现 `macbook:*` 源，且原有 `arknights:*` 仍在。

## 非目标（禁止）

- 不要把 NAS 上的 `hstry.db` 和 `hstry-win.db` 合成一份或互相覆盖。
- 不要把 hub 整库拷到本机 staging.db 再 push（会双重命名空间或覆盖另一台机器）。
- 不要改 NAS 的 `sync.mode`（必须保持 hub）。
- 不要开启与 Windows 同时的 auto_sync。第一次成功 push 之前 `auto_sync = false`。
- 不要恢复 / 覆盖本机 Cursor、Codex、Claude 的工作目录。
- 不要提交 git、不要 push 到 GitHub，除非我明确说。

## 已确认事实（2026-08-22）

- NAS：`admin@memini-b506.tail76a98f.ts.net`（备选：局域网 `memini-b506` / `192.168.0.102`）
- NAS 二进制：`hstry 1.0.0`（fork `andrew05060414/chronicle`，`release/1.0` @ `f33b831`）
- NAS live 配置：`~/.config/hstry/config.toml` 里
  `database = "/vol1/1000/Code/hstry backup/hstry-win.db"`
  `sync.mode = "hub"`
- Live 库现状：仅 `arknights:*`，15 源 / 2768 会话 / 126047 消息
- 同目录 `hstry.db`（195MB，8/16）是旧 Mac 冷档案。不是 live。不要 push 到它。
- 仓库：https://github.com/andrew05060414/chronicle  tag/branch `v1.0.0`
- 本机 staging：`~/.local/share/hstry/staging.db`
- 本机配置：`~/.config/hstry/config.toml`
- Adapters：`~/.config/hstry/adapters`，`js_runtime = "node"`

## 硬规则

1. Mac `[[remotes]].database_path` 必须与 NAS `database=` 一字不差：
   `/vol1/1000/Code/hstry backup/hstry-win.db`
2. `[sync].device_id = "macbook"`，`mode = "satellite"`。
3. 第一次 push 前：Windows 不能同时 auto push。若无法确认 Windows 已停 service / `auto_sync=false`，停止并问我，不要 push。
4. 第一次 push 前在 NAS 上备份 live 库（路径含空格，必须加引号）。
5. push 是 merge，不是上传覆盖。

## 步骤

### 0. 摸底（先读后写）

```bash
hstry -V || true
which hstry node || true
test -f ~/.config/hstry/config.toml && sed -n '1,120p' ~/.config/hstry/config.toml
ssh -o BatchMode=yes admin@memini-b506.tail76a98f.ts.net echo OK || ssh memini-b506 echo OK
```

SSH 不通就停，把错误贴给我。不要改 NAS 上的 database 路径。

### 1. 安装 hstry 1.0.0 + adapters

`hstry -V` 必须是 `1.0.0`。否则：

```bash
# 已有 rustup 就用这个；没有再 brew install rust
cargo install --git https://github.com/andrew05060414/chronicle --tag v1.0.0 hstry-cli
hstry -V   # → 1.0.0

brew install node   # 若还没有
mkdir -p ~/.config/hstry/adapters ~/.local/share/hstry
git clone --depth 1 --branch v1.0.0 https://github.com/andrew05060414/chronicle /tmp/hstry-1.0.0
cp -R /tmp/hstry-1.0.0/adapters/. ~/.config/hstry/adapters/
cd ~/.config/hstry/adapters && npm install
```

### 2. 写本机 config

写入 `~/.config/hstry/config.toml`（若已有文件，先备份为 `config.toml.bak-$(date +%Y%m%d-%H%M%S)`）。`auto_sync` 第一次必须 false。

```toml
database = "~/.local/share/hstry/staging.db"
js_runtime = "node"
adapter_paths = ["~/.config/hstry/adapters"]
sources = []
adapters = []
remotes = []

[[remotes]]
name = "nas"
host = "admin@memini-b506.tail76a98f.ts.net"
enabled = true
database_path = "/vol1/1000/Code/hstry backup/hstry-win.db"

[sync]
mode = "satellite"
device_id = "macbook"
hub_remote = "nas"
auto_sync = true
auto_sync_interval_secs = 300

[service]
enabled = true
poll_interval_secs = 600
search_api = true
transport = "tcp"
```

Tailscale SSH 要浏览器验证时，把 `host` 改成 `memini-b506`（若 `~/.ssh/config` 已有局域网别名）。

### 3. SSH 免密（若第 0 步 BatchMode 失败）

```bash
test -f ~/.ssh/id_ed25519 || ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519 -N ""
ssh-copy-id admin@memini-b506.tail76a98f.ts.net
ssh admin@memini-b506.tail76a98f.ts.net echo OK
```

### 4. NAS 上备份 live 库

```bash
ssh admin@memini-b506.tail76a98f.ts.net 'cd "/vol1/1000/Code/hstry backup" && cp -a hstry-win.db "hstry-win.db.pre-mac-push-$(date +%Y%m%d-%H%M%S)" && ls -l hstry-win.db hstry-win.db.pre-mac-push-* | tail'
```

确认 `hstry.db` 和 `hstry-win.db` 都还在、体积没变成 0。

### 5. 本机采集

只 add 实际存在的目录：

```bash
hstry scan
hstry source add "$HOME/Library/Application Support/Cursor/User/globalStorage"
hstry source add "$HOME/.codex/archived_sessions"
hstry source add "$HOME/.claude/projects"
hstry source add "$HOME/.local/share/opencode"
hstry source add "$HOME/.pi/agent/sessions"
# 目录不存在就跳过，不要编路径

hstry source list
hstry dedup --cross-source || true
hstry source prune-cursor --auto-remove || true
hstry sync
hstry stats
```

### 6. Push（确认 Windows 已停自动推之后才做）

```bash
hstry remote test nas
hstry remote sync --remote nas --direction push -v
```

`-v` 里必须能看出 fetch 的是 `hstry-win.db`，而不是空库或 `hstry.db`。若出现对空库跑 `001_initial_schema`，立刻停，不要继续 upload。

### 7. 验收（把输出原样贴回）

本机：

```bash
hstry -V
hstry service status || true
hstry stats
hstry source list
hstry search "test" --scope remote --remote nas --limit 3
```

NAS：

```bash
ssh admin@memini-b506.tail76a98f.ts.net hstry stats
ssh admin@memini-b506.tail76a98f.ts.net hstry source list
```

通过标准：

- 本机 `hstry -V` = 1.0.0
- config 里 `database_path` 指向 `hstry-win.db`
- NAS `hstry stats` 同时有 `arknights:*` 和 `macbook:*`
- `arknights` 会话数不应从 2768 掉成接近 0
- 两份 NAS 库文件都还在（`hstry.db` 与 `hstry-win.db`）

第一次 push 成功前不要把 `auto_sync` 改成 true，不要写 LaunchAgent。

## 汇报格式

1. 做了什么（安装 / 改 config / 采集 / push）
2. 关键命令输出（`hstry -V`、config 里 database_path、本机 stats、NAS stats / source list 摘要）
3. 未做或跳过的 source
4. 风险或没做的事（Windows 是否已停 push、auto_sync 仍为 false）
```

---

## 本地对照（Mac worker 不需要再读也能干）

更完整的多机手册：[`nas-hub-setup.md`](./nas-hub-setup.md)。  
push 语义：[`remote-sync.md`](./remote-sync.md)。
