# Chronicle 档案层模型（通用部署）

把多台机器上的 AI 对话收成一份统一搜索档案；原始文件由独立的 `chronicle-backup` 模块保护。
不是聊天 App，不是记忆系统。记忆（Agent Memory）是这份档案的客户。

本页描述通用部署合同。搜索采集优先，原始数据恢复说明见 [`restore.md`](./restore.md)。项目为什么变成这样、对话里拍过的板，见 [`project-thread.md`](./project-thread.md)。

---

## 四层，不要揉

```
  IDE / CLI / harness / 网页导出
              │  adapters（只解析，不改原工具）
              ▼
     本机 staging.db  ──push device_id──►  NAS hub hstry.db
                                              │
                                              ├── rclone / 飞牛 Cloud Sync ──► 云盘快照（Google Drive）
                                              └── Search API / CLI / MCP ──► 人 + 其他 agent
                                                                              │
                                                                              └── Agent Memory（以后）
```

| 层 | 职责 | 1.0 现状 | 1.x |
|----|------|----------|-----|
| 采集 | 各工具对话 → 规范化会话 | Cursor/Codex/… + **Antigravity 三根** + **dsh** + **zcode** | 本机没有数据的工具不预做 |
| 档案 | 一台机一份 staging，NAS 一份合并 hub | `device_id` 命名空间 merge | 保持 dumb：不抽取记忆 |
| 原生备份/恢复 | 原生日志、数据库及恢复依赖的版本副本 | `chronicle-backup` + Restic | 见 [`restore.md`](./restore.md) |
| 检索 | 跨工具、跨设备搜 | 本机＋配置 hub 的 CLI FTS + Search API | 失败时报告覆盖范围 |

---

## 拓扑（live）

| 角色 | 位置 | 作用 |
|------|------|------|
| Windows satellite `<windows-device>` | `C:/path/to/staging.db` | 只收这台机刚解析到的源 |
| Mac/Linux satellite `<unix-device>` | `~/.local/share/hstry/staging.db` | 同上 |
| NAS hub | `<hub-db-path>` | **唯一 live 合并目标** |
| 云盘 | Google Drive 等 | **冷快照**，不是第二份 live hub |

`remote sync push` = 把本机 staging **merge** 进 hub（源变成 `{device_id}:{source_id}`）。  
这不是备份。备份是同一份 hub 文件再复制一份带日期的副本。

---

## 恢复合同

优先级：**A 档案恢复 > B 会话 resume**。都在**要用它的那台电脑**上做。

- Windows 坏了 → 在这台 Windows（或一台新 Windows）上下载档案库，搜索可用。
- 不要把 A 机的 Cursor/Codex/agy 目录还原到 B 机。环境不同，那不是 A。
- B（`hstry resume` / export）只在同一类环境、本机 agent 路径还在或你愿意从档案写回去时做。

硬规则：**禁止把带设备前缀的 hub 全库覆盖 staging.db 然后再 push。**
恢复下来的副本只给本机搜索（或先救回 NAS）。本机继续 `hstry sync` 本地工具，只把**这台机新采到的**推回 hub。

云盘同步用 rclone / NAS Cloud Sync。hstry 不实现 Drive API。

---

## 采集合同

1. 一个工具一个 canonical root。禁止把 A 工具的目录登记成 B adapter。
2. 网上能 merge 进来的，只 merge **某种对话格式怎么解析**（adapter / schema），不整仓合 cass、agy-reader。
3. 网页 ChatGPT / Gemini **直播**仍后置；takeout adapter 已有。
4. 新 adapter 的验收：本机 `detect` + `parse`（时间戳整数 ms）+ `source add` + `sync` 能搜到。

---

## 本机采集覆盖（2026-08-21 盘点）

### 已有 adapter，继续用

| 工具 | Adapter | 默认路径 | 备注 |
|------|---------|----------|------|
| Cursor | `cursor` | `%APPDATA%\Cursor\User\globalStorage` | 已在用 |
| Codex | `codex` | `~\.codex` | 已在用 |
| Claude Code | `claude-code` | `~\.claude\projects` | |
| OpenCode | `opencode` | `~\.local\share\opencode` | **adapter 已有**；本机 `opencode.db` ~66MB（2026-08-19）。下一刀是确认已 `source add` 并能 sync，不是重写解析器 |
| Pi / QClaw / WorkBuddy | 各 adapter | `~\.pi` / `~\.qclaw` / `~\.workbuddy` | |
| Antigravity **旧** Gemini CLI | `antigravity` | `~\.gemini\tmp` | JSONL；本机约 29 个 session，最近 2026-08-16。**不是现在主力** |
| Aider / Goose / Hermes / Jan / LM Studio / Open WebUI | 各 adapter | 见 README | 有 adapter；本机未必有数据 |
| ChatGPT / Claude.ai / Gemini takeout | 导出 adapter | Downloads | 手动导出 |

### 缺口（本机有数据）— 1.1 已补

同一 `antigravity` adapter 现在登记四个 canonical root（旧 tmp JSONL 仍保留）。`dsh` 读 `~\.dsh\sessions`。`zcode` 读 `~\.zcode\cli\db\db.sqlite`。OpenCode 不重写解析器，只做 source + sync 回归。

Antigravity 现在是 **三套独立会话库**，现有 adapter 一个都没读：

| 表面 | 本机路径 | 规模（2026-08-21） | 格式 |
|------|----------|-------------------|------|
| **Antigravity 2.0 应用** | `~\.gemini\antigravity\conversations\` | 162× `.db` + 19× 旧 `.pb`，今天还在写 | SQLite `steps.step_payload` blob（protobuf） |
| **Antigravity CLI（agy）** | `~\.gemini\antigravity-cli\conversations\` | 86× `.db`，昨天还在写 | **同一套 schema** |
| **Antigravity IDE 1** | `~\.gemini\antigravity-ide\conversations\` | 8× `.pb`，停在 2026-05 | 旧 protobuf 文件 |

`%APPDATA%\Antigravity` 和 `%APPDATA%\Antigravity IDE` 是 Electron 壳，**不是**对话正文。对话在 `~\.gemini\antigravity*`。

DeepSeek Harness（`dsh`）：

| 表面 | 本机路径 | 规模 | 格式 |
|------|----------|------|------|
| **dsh 会话** | `~\.dsh\sessions\<workspace-slug>\session-<uuid>\session.jsonl.zstd` | 约 40 个会话（pxread 最多） | zstd 压缩 JSONL（magic `28 B5 2F FD`） |
| 旧 `~\.deepseek\sessions` | 空 | 忽略 | 不是现在这条 harness |

Zcode / ZAI：

| 表面 | 本机路径 | 规模 | 格式 |
|------|----------|------|------|
| **Zcode CLI** | `~\.zcode\cli\db\db.sqlite` | 本机约 27 session / 1410 message（2026-08-21） | SQLite `session` + `message.data` + `part.data`（OpenCode 同款） |

`cli/agents/*/transcript.jsonl` 是流式 delta，不是权威正文。

可参考、不要整仓合入：

- [agy-reader](https://github.com/mjacobs/agy-reader)（MIT）：2.0 / CLI 同 schema；daemon RPC 解密旧 `.pb`
- [txcript antigravity 格式笔记](https://docs.rs/crate/txcript/latest/source/docs/formats/antigravity.md)：`conversations/<uuid>.db` + protobuf field

优先 **离线读 SQLite**（2.0 和 agy 主力已是 `.db`）。daemon / `.pb` 只覆盖旧 IDE 1 和未迁移文件。

---

## 1.x 采集顺序

1. **Antigravity 会话库** — 已做：`antigravity` 三个 store root + 旧 tmp JSONL。
2. **DeepSeek Harness** — 已做：`dsh` / `~\.dsh\sessions`。
3. **OpenCode 回归** — adapter 已有；确认 source + sync。
4. 再按「本机还有目录、还在写」补。没有数据的不预做。 **Zcode 已补**：`zcode` / `~\.zcode`。

TUI overhaul、cursor harden、Drive 快照脚本：**不挡**上面 1–3。快照脚本在主力源能 sync 之后补。

---

## 明确不做（现在）

- 把 hstry 做成第二套 ChatGPT / 多活第二 hub
- 在仓库里写 Google Drive SDK
- 把 Agent Memory 抽取策略做进 hstry
- 跨机器还原 agent 工作目录
- 为了「主流」去适配本机没有的工具
