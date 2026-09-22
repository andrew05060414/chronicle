# Chronicle 原生容灾：重启交接（2026-09-22）

这是阶段性提交，不是完整计划验收。用户要求先保存已完成工作，电脑重启后再派任务；当前不启动新 worker、不启用新的常驻任务、不发布。

## 从这里恢复

- 工作分支：`codex/chronicle-native-recovery`；工作区：`chronicle-native-recovery`。
- 原计划：五类宿主原生保护、Restic 本机和 NAS 副本、本机加 hub 搜索、增量同步状态、原生安装恢复、可选非凭据组件、部署和客户端验收。不得把文件提取成功缩减为完整目标。
- 先读本文件、`docs/restore.md`，再读取 `.trx/issues.jsonl` 中 `trx-nr-*` 的完整任务信封。CLI `trx` 在交接环境不可用，信封使用仓库现有 JSONL 结构保存。
- 不依赖旧 agent 或进程 ID。两次 worker 都被提供方额度错误（403 spending-limit）终止，没有可接收的审查结论；恢复后先核验路由和额度。
- 默认由一名 `luna_worker` 执行清晰子任务；每个写任务新建独立 worktree。主线程只负责边界、依赖、整合和独立验收。最多四个并行；同一模块的改动按依赖串行。

## 已落地的阶段性结果

- 新 crate、原始采集、SQLite 一致性副本、哈希、Restic 复制、隔离提取与恢复计划框架。
- Cursor 显式会话键及 composerHeaders 筛选，真实数据安全投影、本机/NAS 复制、NAS 独立恢复及逐项校验。确切收据、哈希和行数见 `docs/restore.md`。
- 监听启动核对、变更合并、WAL 检测、独立复制队列；尚未证明常驻部署。
- 本机加 hub 搜索、失败降级、避免跨会话误折叠、刷新超时、一致性导出。状态记录和并发上传契约还需完成。
- Codex/Cursor 安装及其余三类文件路径的合成原型。**所有真实宿主自动安装仍被版本门禁拒绝**。测试通过不能移除门禁。
- 本轮提交前 `just check-all` 退出码 0；PowerShell pre-PR gate 退出码 0（首次 Bun 子进程被 sandbox EPERM 拒绝，获批后在隔离 HOME 下重跑通过）。包括 workspace all-targets、Clippy `-D warnings`、memory-integrity、Bun fixtures、4 项 Node regressions。日志为本机 `tmp/reboot-check-all.log` 和 `tmp/reboot-pre-pr.log`，不进 Git。这证明阶段代码通过自动检查，不代表全计划/真实宿主已验收。

## 私有运行数据位置（不得加入 Git）

- 数据与配置：Windows 独立 D 盘 `Data/chronicle-native`。配置已引用用户给定的外部密码文件；不要重建密钥、初始化覆盖仓库或输出密码。
- 本工作区 `tmp/cursor-nas-restore-0afca009`：真实 Cursor NAS 恢复副本，含私人聊天；不可发布。
- `tmp/native-tools`：测试用 Restic；`tmp/native-restic-gate.json`：合成 Restic 验收结果。
- 所有 `tmp/`、`target/`、原始 DB/JSONL/快照和真实运行配置均排除于提交。

## 派发顺序

| 批次 | 任务 | 执行与边界 |
|---|---|---|
| 1，可并行 | `trx-nr-sync` 搜索/同步证据；`trx-nr-manifest` 原生源清单与监听 | 不同模块、独立 worktree；先各自验收再整合 |
| 2，可并行 | `trx-nr-install` 安装事务与恢复计划；`trx-nr-components` 可选非凭据组件 | 依赖清单契约；install 独占原生 lib 的安装段，components 只新增独立模块，接线由主线程整合 |
| 3 | `trx-nr-codex`、`trx-nr-cursor`、`trx-nr-filehosts` | 各自独立宿主模块，不修改公共安装框架；公共改动退回 install 负责人 |
| 4 | `trx-nr-uat-codex`、`trx-nr-uat-cursor`、`trx-nr-uat-filehosts` | 隔离客户端真实恢复，记录打开/重启/续聊；共享桌面时串行，不得同时操纵同一客户端 |
| 5 | `trx-nr-deploy` 常驻部署；`trx-nr-release` 最终整合验收 | 基线保护、重启/断网演练后才替换重叠任务；最后提交/发布分别报告 |

每个信封含允许路径、禁止动作、依赖、产物、验收、验证命令和回报格式。不要重复建 GitHub/Multica 台账。恢复后可直接让主线程从批次 1 派发。

## 已知风险，不能因当前测试绿而忽略

- 安装原型依赖源路径当前是否存在推断类型，灾后原路径丢失会映射错误；必须改为清单自足。
- Cursor 合并原型覆盖同 ID 不同内容；需要冲突合同。Codex schema、外键/子表和索引的全保真范围仍未证明。
- 安装回滚会复制目标 SQLite 全库；真实目标可能混凭据，禁止在解决之前启用。不得绕过版本门禁或 `--force` 上生产。
- 版本探测可能执行宿主 `--version`，需超时及测试 home 隔离；一个非空版本字符串不是已验证版本。
- optional 凭据检测仅是启发式防线，不是完整 allowlist；尚未接通全部可选组件，不能承诺任意配置安全。
- 监听持续写入/关机恢复、快照/回执可变性、故障持久状态、增量语义与源依赖完整性都需更强证据。
- 搜索 pending 与 ingest_pending 是不同概念；不得用采集成功标记上传完成。壁钟水位仍需覆盖历史时间戳导入、并发写入和重启。

## 统一任务回报

每个执行者返回：任务 ID、改动文件、测试命令/退出码、边界验证、证据文件、未解决项。未完成不得自改验收标准；不自行 commit/push/merge/deploy，不读密钥，不写真实 profile。主线程核验并整合后才更新任务状态。
