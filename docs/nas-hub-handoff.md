# NAS Hub Handoff（交给 NAS worker）

## 任务

在飞牛 NAS 上部署 **hstry Hub**（中心库），供 Windows/Mac satellite 推送聊天记录。

## 已确认变量（Andrew 环境）

```bash
NAS_ROOT="/vol1/1000/Code/hstry backup"
NAS_DB="${NAS_ROOT}/hstry.db"
NAS_ADAPTERS="${NAS_ROOT}/adapters"
NAS_INBOX="${NAS_ROOT}/inbox"          # 可选：手动原始导出
NAS_SSH="admin@memini-b506.tail76a98f.ts.net"   # 推荐（Tailscale FQDN）
# 备选: admin@100.73.230.28  |  admin@memini-b506
HSTRY_VERSION=1.0.0
REMOTE_NAME=nas                        # Win/Mac 固定用此名
```

目录结构：

```text
/vol1/1000/Code/hstry backup/
├── hstry.db          # 中央数据库
├── adapters/         # TypeScript adapters（npm install）
└── inbox/            # 可选
```

> 路径含空格，shell 中务必加引号：`cd "/vol1/1000/Code/hstry backup"`

---

## 执行步骤

```bash
NAS_ROOT="/vol1/1000/Code/hstry backup"
NAS_DB="${NAS_ROOT}/hstry.db"

# 1. 依赖
hstry -V || echo "需安装 hstry $HSTRY_VERSION"
node -v || echo "需 Node LTS（NAS 已有 fnm/Node LTS）"

# 2. 目录
mkdir -p "$NAS_ROOT/adapters" "$NAS_ROOT/inbox" ~/.config/hstry

# 3. 获取 adapters + 二进制（二选一）
git clone --depth 1 --branch v1.0.0 https://github.com/andrew05060414/chronicle /tmp/hstry
cp -r /tmp/hstry/adapters/* "$NAS_ROOT/adapters/"
# hstry 二进制：cargo install --path /tmp/hstry/crates/hstry-cli  或 release 下载

# 4. config
cat > ~/.config/hstry/config.toml << EOF
database = "$NAS_DB"
js_runtime = "node"
adapter_paths = ["$NAS_ROOT/adapters"]
sources = []
adapters = []
remotes = []

[service]
enabled = true
poll_interval_secs = 600
search_api = true
transport = "tcp"

[sync]
mode = "hub"
auto_sync = false

[checkpoint]
enabled = true
dir = "/vol1/1000/Code/hstry backup/checkpoints"
interval_secs = 86400
max_total_bytes = 10737418240
keep_weekly = 4
EOF

# 5. adapter 依赖
cd "$NAS_ROOT/adapters" && npm install

# 6. 启动 + 开机自启（任选）
hstry service start
hstry service status
hstry stats

# 开机自启示例
# (crontab -l 2>/dev/null; echo '@reboot sleep 30 && hstry service start') | crontab -
```

---

## 验收（回报输出）

```bash
hstry -V
hstry service status    # → running
hstry stats             # → 初始可为空
ssh -V                  # OpenSSH 正常
```

---

## Satellite 侧（Win/Mac 用户自己做）

SSH 推荐地址：`admin@memini-b506.tail76a98f.ts.net`

```bash
hstry remote add nas admin@memini-b506.tail76a98f.ts.net
hstry remote test nas
hstry sync
hstry remote sync --remote nas --direction push
hstry search "test" --scope remote --remote nas --limit 3
```

Win/Mac `config.toml` 片段见 `docs/nas-hub-setup.md` Phase 2/3。

**SSH 公钥**：Win/Mac 生成密钥并 `ssh-copy-id` 到 NAS，便于自动 push（无需把私钥给任何人）。

---

## 可选：mmry 定时导入

```bash
0 3 * * * hstry mmry extract --store chat-memories --after "yesterday" --limit 50 >> "$NAS_ROOT/mmry.log" 2>&1
```

---

## 故障

| 症状 | 处理 |
|------|------|
| satellite SSH 失败 | `tailscale status`；NAS `sshd` 是否监听 |
| push 后 stats 不增 | NAS 上 `ls -la "$NAS_DB"`；权限 |
| adapter 空 | `cd "$NAS_ROOT/adapters" && npm install` |
| 路径空格问题 | 全程双引号包裹 `"$NAS_ROOT"` |

## 完整文档

`docs/nas-hub-setup.md`
