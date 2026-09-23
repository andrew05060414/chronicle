# Chronicle 原生容灾：交接（更新于 2026-09-23）

原生备份基础和恢复原型已进入 PR #51。真实客户端与部署验收仍有边界；不要把合成测试或一次客户端演练当成所有宿主均已验收。

## 从这里恢复

- 工作分支：`codex/chronicle-native-recovery`；GitHub 交付入口：[PR #51](https://github.com/andrew05060414/chronicle/pull/51)。以 PR 的最新提交和 checks 为准。
- 原计划：五类宿主原生保护、Restic 本机和 NAS 副本、本机加 hub 搜索、增量同步状态、原生安装恢复、可选非凭据组件、部署和客户端验收。不得把文件提取成功缩减为完整目标。
- GitHub Issues 是 fork 的任务主账本。#26（异地备份调度）和 #27（异地备份保留）仍 open，属于后续独立工作，不由 PR #51 自动关闭。
- `.trx/` 是继承的本地 JSONL 任务历史；`trx sync` 提交这些文件，不会同步 GitHub Issues。本 fork 不再为新工作创建或更新 TRX 任务，也不把旧条目当作当前状态来源。
- Windows 上同一时间只允许一个跑 Cargo 的执行者，并设 `CARGO_BUILD_JOBS=4`。主线程必须亲自检查每个 diff。

## 当前状态

本机 `just check-all` 在最新本地修复树通过；提交后必须查看 PR #51 的 Linux、macOS、Windows checks。PR 描述中的旧测试计数不能替代最新 checks。

仍需明确区分：一次 Codex CLI NAS 往返演练的报告、各宿主隔离 UAT、重启/断网/磁盘满部署演练，以及正式合并或常驻部署。未知版本继续 fail closed，不得用 `--force` 绕过。

本机 Windows 最新修复树：`just check-all`、`cargo test -p chronicle-backup`（65 项）和 `cargo test -p hstry-cli`（80 项）通过。一次旧的 workspace gate 因 MCP 测试启动默认 `hstry sync` 超时而中断；测试现已隔离到临时配置，单测和后续完整 gate 均通过。以上不替代推送后 PR #51 的 Linux/macOS/Windows checks。

## 私有运行数据（不得加入 Git）

本机数据根、密码文件引用和含私人聊天的恢复副本位置，记录在本机 native 数据根下的 `PRIVATE-NOTES.md`，不在仓库中。所有 `tmp/`、`target/`、原始 DB/JSONL/快照和真实运行配置均排除于提交。

## 下一步

1. 等待 PR #51 最新跨平台 CI；修复后再做独立验收。
2. 在隔离 profile 中按宿主完成 UAT；共享桌面不得同时操纵同一客户端。
3. 重启、NAS 离线、磁盘满等部署演练须使用隔离数据根；常驻任务注册与合并分别需要明确授权。

## 已知风险，不能因测试绿而忽略

- 所有真实宿主自动安装仍被版本门禁拒绝；测试通过不能移除门禁，不得用 `--force` 上生产。
- 可选组件凭据防线基于允许列表与合成陷阱，只有合成证据，不能承诺任意真实配置安全。
- sync：只改来源信息、未改会话时不推送；从 hub 拉回的会话可能被再推一次。
- filehosts 默认额外备份 `~/.gemini/tmp`，依据是仓库 antigravity adapter 的说明，未经真实客户端证实。
- Cursor UAT 脚本会删除隔离 profile 的整个 `state.vscdb`，续聊可能需要重新登录。
- Codex schema、子表和索引的全保真范围只有合成证据。

## 统一任务回报

每个执行者返回：任务 ID、改动文件、测试命令/退出码、边界验证、证据文件、未解决项。未完成不得自改验收标准；不自行 commit/push/merge/deploy，不读密钥，不写真实 profile。主线程核验并整合后才更新任务状态。
