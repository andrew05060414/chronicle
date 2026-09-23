# Chronicle 原生容灾：交接（更新于 2026-09-23）

有代码产物的 8 个任务已整合提交（5 个达到验收关闭，3 个仍有缺口）；真实客户端 UAT、部署演练和最终验收未做。这不是完整计划验收：未 push、未合并、未部署、未注册任何常驻任务。

## 从这里恢复

- 工作分支：`codex/chronicle-native-recovery`；工作区：`chronicle-native-recovery`。提交链：`188232c`（CLI 大栈线程）→ `2074ba0`（sync）→ `871160b`（native 采集/安装/宿主）→ `e194db8`（deploy 脚本）→ `f89d708`（UAT 脚本）。
- 原计划：五类宿主原生保护、Restic 本机和 NAS 副本、本机加 hub 搜索、增量同步状态、原生安装恢复、可选非凭据组件、部署和客户端验收。不得把文件提取成功缩减为完整目标。
- 先读本文件、`docs/restore.md`，再读 `.trx/issues.jsonl` 中 `trx-nr-*` 的任务信封。本机没有 `trx` CLI；该文件按 trx v2 格式追加整行记录，同一 ID 以最后一行为准，`.trx/events.jsonl` 记录每次状态变化和说明。
- 执行者用 `ocx-gemini-3-8-flash`；Windows 上同一时间只允许一个跑 cargo 的执行者，并设 `CARGO_BUILD_JOBS=4`。Gemini 审查几乎总在首轮批准，主线程必须亲自读每个 diff。

## 当前状态

| 任务 | 状态 | 说明 |
|---|---|---|
| `trx-nr-manifest` | closed | 快照自描述；WAL 变化、截断/替换、5 秒合并 60 秒上界、监听/复制故障持久化、版本探测超时和临时 home 均有测试 |
| `trx-nr-sync` | closed | 按变更序号确认推送；失败不推进确认；启动补传、失败 5 分钟重试；5 秒 refresh 不上传 |
| `trx-nr-components` | closed | 允许列表优先；合成凭据陷阱不进快照；插件不安装；projects 只存元数据 |
| `trx-nr-cursor` | closed | 孤儿 bubble/空 profile 记入清单不致采集失败；冲突拒绝、幂等；失败时认证和设置字节不变 |
| `trx-nr-filehosts` | closed | 三宿主删源后恢复全文件哈希一致；未知版本只提取；编码 cwd 必须显式 `--map` |
| `trx-nr-install` | in_progress | 缺：进程被杀后没有代码读取 `journal.json` 继续或回滚（进程内失败回滚已有） |
| `trx-nr-codex` | in_progress | 缺：快照中缺 rollout 的线程被静默跳过，未报告 |
| `trx-nr-uat-codex` / `-cursor` / `-filehosts` | in_progress | 脚本已写，未运行。阻塞：生产版本门禁拒绝所有真实宿主版本，需 Andrew 决定真实版本如何认定为已验证；Antigravity、Grok 无隔离 profile 方法，记为 unsupported-isolation |
| `trx-nr-deploy` | in_progress | `scripts/native-service.ps1` 默认 dry-run，注册需 `-IReallyMeanIt`，从未注册。缺：重启、NAS 离线、磁盘满、替换重叠任务演练；登录任务常驻控制台窗口 |
| `trx-nr-release` | open | 依赖以上全部 |

自动检查（最终树 = 提交后树 `eb63c9b`）：`scripts/pre-pr.ps1` 退出码 0，384 测试通过（`tmp/round3-pre-pr-2.log`）；`just check-all` 退出码 0。中间提交各自通过 `cargo check --workspace --all-targets`。`evidence_cli` 在干净基线上同样栈溢出，由 `188232c` 修复。

## 私有运行数据位置（不得加入 Git）

- 数据与配置：Windows 独立 D 盘 `Data/chronicle-native`。配置已引用用户给定的外部密码文件；不要重建密钥、初始化覆盖仓库或输出密码。
- 本工作区 `tmp/cursor-nas-restore-0afca009`：真实 Cursor NAS 恢复副本，含私人聊天；不可发布。
- `tmp/native-tools`：测试用 Restic；`tmp/native-restic-gate.json`：合成 Restic 验收结果。
- 所有 `tmp/`、`target/`、原始 DB/JSONL/快照和真实运行配置均排除于提交。

## 下一步

1. Andrew 决定版本门禁的"已验证版本"认定方式，之后按 codex → cursor → filehosts 串行跑 UAT 脚本；共享桌面不得同时操纵同一客户端。
2. 补 install 的崩溃恢复路径和 codex 缺失依赖报告，各自关闭任务。
3. 授权环境下做部署演练，基线安全后再替换重叠任务。
4. `trx-nr-release` 逐条对照原计划审计证据；commit/push/merge/deploy 分别授权。

不要重复建 GitHub/Multica 台账。GitHub 上现存的 #19/#24/#25/#26/#27 针对旧 checkpoint/backup 链路，不由本计划关闭。

## 已知风险，不能因测试绿而忽略

- 所有真实宿主自动安装仍被版本门禁拒绝；测试通过不能移除门禁，不得用 `--force` 上生产。
- 可选组件凭据防线基于允许列表与合成陷阱，只有合成证据，不能承诺任意真实配置安全。
- sync：只改来源信息、未改会话时不推送；从 hub 拉回的会话可能被再推一次。
- filehosts 默认额外备份 `~/.gemini/tmp`，依据是仓库 antigravity adapter 的说明，未经真实客户端证实。
- Cursor UAT 脚本会删除隔离 profile 的整个 `state.vscdb`，续聊可能需要重新登录。
- Codex schema、子表和索引的全保真范围只有合成证据。

## 统一任务回报

每个执行者返回：任务 ID、改动文件、测试命令/退出码、边界验证、证据文件、未解决项。未完成不得自改验收标准；不自行 commit/push/merge/deploy，不读密钥，不写真实 profile。主线程核验并整合后才更新任务状态。
