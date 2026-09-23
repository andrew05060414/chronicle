# Fork 演进时间线

这个 fork 每一步是什么时候、为了解决什么问题做的。提交列表以 `git log` 为准，日期是提交日期。
配套阅读：为什么会这样决定，见 [`project-thread.md`](./project-thread.md)；部署合同见 [`archive-model.md`](./archive-model.md)。

仓库事实截至 2026-09-18。

---

## 谱系

| | |
|---|---|
| 上游 | [byteowlz/hstry](https://github.com/byteowlz/hstry)，首次提交 `5498b32`（2026-01-17，OpenCode adapter） |
| 本仓库 | [andrew05060414/chronicle](https://github.com/andrew05060414/chronicle) |
| fork 版本 | `1.0.0`（`0977f3d`，2026-08-20 起） |
| 最后合入上游 | `v0.5.25` / `a4be7c9`，合并提交 `0186b81`（2026-09-17） |
| fork 侧第一笔提交 | `13782bc`（2026-07-24） |

上游在 2026-01-20 就有了 `mmry extract`（`3581113`）。它不是这个 fork 加的功能，这解释了为什么它后来被当成边界问题处理而不是缺陷（见 #20）。

---

## 四个阶段

### 阶段一：把 Windows 变成一等公民（2026-07-24 → 07-30）

起点不是「做一个产品」，是「这台 Windows 上的对话得先收得进来」。

| 提交 | 日期 | 内容 |
|---|---|---|
| `13782bc` | 2026-07-24 | Add Windows support and full Cursor Composer import |
| `8799c90` | 2026-07-24 | docs(ci): finish Windows workflow polish for local use |
| `de3f919` | 2026-07-24 | Merge branch 'feat/windows-cursor-composer-import' |
| `c2391c9` | 2026-07-24 | docs: personal NAS hub and Mac satellite setup |
| `df94445` | 2026-07-25 | feat(adapters): add qclaw, workbuddy, and antigravity sources |
| `8e0fb83` | 2026-07-25 | fix(adapters): normalize path separators for canonical root checks |
| `2f354a3` | 2026-07-25 | **feat(sync): namespace satellite pushes with device_id** |
| `e2d22e7` | 2026-07-25 | docs: update NAS hub push workflow and adapter rollout notes |
| `9e5f832` | 2026-07-25 | fix(remote): quote spaced hub paths so push merges instead of replacing |
| `32a4754` | 2026-07-25 | fix(remote): fix Windows SCP fetch for spaced hub paths |
| `67f7638` | 2026-07-25 | docs: remote sync guide and andrew-nas branch split notes |
| `b7325ab` | 2026-07-25 | docs: link upstream PR numbers in branch notes |
| `4b77288` | 2026-07-30 | fix(remote): refresh hub source metadata on repeat push |

`2f354a3` 是这一阶段的关键：在此之前两台机器 push 到同一个 hub 会互相覆盖。`device_id` 命名空间让 Windows 和 Mac 的归档在 hub 上共存，这条约束后来写进了 [`archive-model.md`](./archive-model.md) 的硬规则。

`9e5f832` 修的是一个具体的坑：hub 路径带空格时 push 变成了整库替换而不是 merge。

### 阶段二：采集补齐，hub 做成真的 hub（2026-08-20 → 08-30）

| 提交 | 日期 | 内容 |
|---|---|---|
| `0977f3d` | 2026-08-20 | chore(release): v1.0.0 personal fork |
| `5d6fe35` | 2026-08-21 | feat: ingest Antigravity stores and dsh; default satellite search to hub |
| `f33b831` | 2026-08-21 | fix: hub search, sqlite timestamps, and Windows debug help stack |
| `d695ca1` | 2026-08-22 | feat: ingest Zcode sessions and document live hub |
| `179aaba` | 2026-08-25 | **feat(sync): ingest satellite deltas on the hub and add rolling checkpoints** |
| `cbfa3f4` | 2026-08-30 | feat(adapters): add Grok Build session importer |
| `9512266` | 2026-08-30 | Merge origin/main into merge/upstream-v0.5.23 |

v1.0.0 之前做的一次本机盘点（记在 [`archive-model.md`](./archive-model.md) 的「本机采集覆盖」一节）发现 Antigravity 实际上是三套独立会话库——2.0 应用、agy CLI、IDE 1——现有 adapter 一个都没读到。`5d6fe35` 把三个 canonical root 一起登记，同时补上 dsh。

`179aaba` 改变了同步的性质：在此之前 push 是把整个 hub 文件 SCP 覆盖掉，之后是导出 delta、在 hub 侧 `hub ingest` 增量 merge，并加上滚动 checkpoint（daily / weekly，压缩上限 10 GiB）。

### 阶段三：名字和边界（2026-09-05 → 09-16）

| 提交 | 日期 | 内容 |
|---|---|---|
| `423d7c4` | 2026-09-05 | fix(cursor): harden adapter against duplicate sources and bun sqlite crash |
| `24218e6` | 2026-09-06 | **feat(cli): add chronicle binary, backup, and skills proxy** |
| `1c62c0c` | 2026-09-06 | Merge origin/main (v0.5.24) |
| `199ccc1` | 2026-09-16 | docs: finalize Chronicle repository transition |
| `4e5210c` | 2026-09-16 | fix: resolve workspace Clippy lints |
| `cae1514` | 2026-09-16 | fix: derive WorkBuddy timestamps from events |
| `c78da18` | 2026-09-16 | fix: make Andrew-Skill integration opt-in |
| `f84f711` | 2026-09-16 | chore: clean test-target Clippy lints |
| `b06df57` | 2026-09-16 | fix: use Chronicle as the default adapter source |
| `d8ba1ce` | 2026-09-16 | fix: package Chronicle release aliases |
| `68a848a` | 2026-09-16 | **docs: remove private deployment notes from public tree** |
| `9d33f18` | 2026-09-16 | docs: point configuration schema to Chronicle |
| `a61f74c` | 2026-09-16 | fix: gate third-party package publishing |
| `90dcb5a` | 2026-09-16 | fix: checkpoint SQLite WAL before close |
| `9b61ddb` | 2026-09-17 | Merge pull request #2 (fix/ci-baseline) |

改名是「只改对人说的那一层」：产品名、CLI 别名、文档指向本仓库。crate 名、`%APPDATA%\hstry\`、数据库路径全部保持 `hstry`，合上游时才不会炸。

`c78da18` 把 Andrew-Skill 集成改成 opt-in，核心 CLI 不再依赖外部 checkout、PowerShell 和 `asm`。`68a848a` 把本机绝对路径和部署细节从公开树里清掉——这条约束现在写在 [`project-thread.md`](./project-thread.md) 的操作纪律里。

**PATH 上装的二进制来自 `24218e6` / `1c62c0c` 这一版**（2026-09-06 构建）。后面 `0186b81` 合进来的代码还没有被 `cargo install` 过。

### 阶段四：合入 v0.5.25，然后审查（2026-09-17 →）

`0186b81` 把上游 v0.5.25 合进 main：59 个文件，约 8.0k 插入 / 1.4k 删除，新增 `backup.rs`、`checkpoint.rs`、`skills.rs`，重写 `remote.rs` 的 hub/delta 同步，以及新增和重写的 TypeScript adapter。

`cargo check --workspace --all-targets` 退出码 0；随后的审查查出的全是运行时或逻辑缺陷，不是编译错误。审查的主题只有一句话：

> 这次合并删掉了 fork 已有的三个安全机制，连同证明它们的测试。

分别是远程路径的 printf 转义（#3）、持久化 UUID 设备身份（#4）、SQLite `quick_check` 校验（#7）。13 条发现记在 #16，全部登记见 [`merge-review-register.md`](./merge-review-register.md)。

修复在 2026-09-18 上午一次性落地：`2829434` 合入 #30、`3b1396e` 合入 #29、`b36cad3` 合入 #31、`c436a65` 合入 #34（记忆完整性门禁，从此 CI 是 `clippy --all-targets -- -D warnings` 加全 target 测试），随后 #28 和本套文档 `fddb223` 也合了进去。main 现在在 `fddb223`。

还在飞的是 #35（修 #17 的 pull 方向）和 #32（文档边界）。

---

## 分支状态

| 分支 | 最后一笔 | 状态 |
|---|---|---|
| `main` | `fddb223`（2026-09-18） | 当前 base |
| `andrew-nas` | `4b77288`（2026-07-30） | **已并入 main**，旧指针 |
| `feat/rip-out-tantivy` | `740bcbb`（2026-04-26） | **已并入 main**，旧指针 |
| `release/1.0` | `179aaba`（2026-08-25） | **已并入 main**，旧指针 |
| `fix/ci-baseline` | `90dcb5a` | 已通过 PR #2 合并 |
| `fix/backup-safety-5-6-7`、`fix/adapter-defects-batch`、`fix/windows-service-process-detection`、`ci/pre-pr-memory-integrity-gate`、`fix/search-scope-and-mcp-surface` | — | 2026-09-18 上午已合并 |
| `docs/chronicle-boundary-20-21`、`claude/project-thread-rgec62` | 见 [`merge-review-register.md`](./merge-review-register.md) | 进行中 |

前三个分支上没有任何未并入 main 的提交。它们是遗留指针，不是搁置的探索——删掉不会丢东西。

---

## 上游合并史

| fork 侧合并提交 | 消费的上游版本 |
|---|---|
| `9512266`（2026-08-30） | v0.5.23 线 |
| `1c62c0c`（2026-09-06） | v0.5.24 |
| `0186b81`（2026-09-17） | v0.5.25 / `a4be7c9` |

合上游的策略是「功能以 fork 为准，作者的更新能吸进来」。#24 提出给 fork 的不变量加合并守卫，正是因为 `0186b81` 这次没有守住——它删掉的三个机制都有测试，测试也一起被删了。下次合上游之前应该先有这道守卫。
