# 审查登记：v0.5.25 合并回归与边界审查

一张表把 issue 和 PR 对起来，省得每次重新翻。**GitHub Issues 是这个仓库唯一的看板**——`.trx/`、`.pi/todos/`、`.octo/` 都是要冻结的历史（#21）。

状态截至 2026-09-18 06:45 UTC，main 在 `fddb223`。严重度以 GitHub 标签为准。下面两张表里的 issue↔PR 对应关系不随合并变化；PR 的实时状态见「PR 面板」。

背景见 [`fork-timeline.md`](./fork-timeline.md)，决策依据见 [`project-thread.md`](./project-thread.md)。

---

## 两条轴

- **轴一：合并回归**。`0186b81` 合入上游 v0.5.25 之后的代码审查，13 条发现记在 **#16**，加上 #18 共 14 条，标签 `merge-regression`。
- **轴二：设计边界审查**。2026-09-17 另开的一轮，#17、#19–#27、#33，标签 `design-review` / `product-boundary`。这些不是合并带来的，是本来就存在、这次才被看见的。

`verdict` 一列来自 #16：`CONFIRMED` 是对着源码复核过的，`PLAUSIBLE` 只做了静态分析、**没有对 fixture 复现过**，动手修之前先确认。

---

## 轴一：合并回归

| # | 严重度 | verdict | 问题 | 归属 PR |
|---|---|---|---|---|
| [#3](https://github.com/andrew05060414/chronicle/issues/3) | critical · security | CONFIRMED | 远程命令注入：`expand_remote_path` 用 `eval echo` | 无 |
| [#4](https://github.com/andrew05060414/chronicle/issues/4) | high | CONFIRMED | `device_namespace()` 回落到 `"unknown"`，静默把两台设备的归档合成一份 | 无 |
| [#5](https://github.com/andrew05060414/chronicle/issues/5) | high | CONFIRMED | `checkpoint restore --live` 覆盖打开着的 SQLite 文件，且忽略安全 checkpoint 的失败 | [#30](https://github.com/andrew05060414/chronicle/pull/30) |
| [#6](https://github.com/andrew05060414/chronicle/issues/6) | high | CONFIRMED | 一旦 weekly 占满容量上限，`plan_prune` 先删最新的 checkpoint | [#30](https://github.com/andrew05060414/chronicle/pull/30) |
| [#7](https://github.com/andrew05060414/chronicle/issues/7) | high | CONFIRMED | 取回的 hub 校验从 SQLite `quick_check` 降级成 1 KiB 大小检查 | [#30](https://github.com/andrew05060414/chronicle/pull/30) |
| [#8](https://github.com/andrew05060414/chronicle/issues/8) | medium | CONFIRMED | `backup --json` 可能死锁在 NAS 步骤的 stderr 管道上 | 无 |
| [#9](https://github.com/andrew05060414/chronicle/issues/9) | medium | CONFIRMED | 加密的 `.enc` 备份无限增长，从不清理 | 无 |
| [#10](https://github.com/andrew05060414/chronicle/issues/10) | medium | PLAUSIBLE | 旧版 Grok 转录（`chat_format_version 0`）不再解析，从归档里消失 | [#29](https://github.com/andrew05060414/chronicle/pull/29) |
| [#11](https://github.com/andrew05060414/chronicle/issues/11) | medium | PLAUSIBLE | antigravity `readVarint` 截断到 32 位，毫秒时间戳被破坏 | [#29](https://github.com/andrew05060414/chronicle/pull/29) |
| [#12](https://github.com/andrew05060414/chronicle/issues/12) | medium | PLAUSIBLE | `isSystemContext` 用 `includes()`，真实用户消息被跳过不参与取标题 | [#29](https://github.com/andrew05060414/chronicle/pull/29) |
| [#13](https://github.com/andrew05060414/chronicle/issues/13) | medium | PLAUSIBLE | `is_process_running` 放松成对 tasklist 输出的裸子串匹配 | [#31](https://github.com/andrew05060414/chronicle/pull/31) |
| [#14](https://github.com/andrew05060414/chronicle/issues/14) | medium | CONFIRMED | 两条 CLI 回归：`--no-color` 被删；satellite 搜索不再读本地库 | 无 |
| [#15](https://github.com/andrew05060414/chronicle/issues/15) | low | PLAUSIBLE | Grok/Cursor 的 `--limit` 返回字母序最前的会话而不是最新的 | [#29](https://github.com/andrew05060414/chronicle/pull/29) |
| [#18](https://github.com/andrew05060414/chronicle/issues/18) | low | — | 没有启用的 remote 时 `search --scope all` 直接放弃，把本地结果也丢了 | [#28](https://github.com/andrew05060414/chronicle/pull/28) |

**建议顺序**（来自 #16）：

1. 在下一次同步或备份之前修掉 **#3、#4、#5** —— 它们能执行远程代码、静默合并两台设备的归档、或损坏实时数据库。
2. 在依赖备份之前修掉 **#6、#7、#8、#9** —— checkpoint 和灾难恢复路径目前交付的东西和它报告的不一致。
3. 先复现再修 **#10、#11、#12、#13、#15**。
4. 用户可见的回归 **#14**。

**上游说明**：#3 描述的代码来自 `origin/main`，byteowlz/hstry 很可能同样受影响。目前没有上报，这是一个单独的决定。

### 已核查、确认无误

这些在审查里逐条读过，是好的。不要重复审计：

- `db.rs` 的 `updated_after` SQL —— 谓词追加顺序和绑定顺序一致，`COALESCE(updated_at, created_at) >= ?` 对 delta 导出是正确的。
- `export_delta` 的 source 选择 —— 拥有 delta 会话的 source 总会被一起复制，所以 hub 侧 `merge_databases` 不会遇到缺失外键。
- push 水位 —— `export_started` 在导出之前取，`updated_after` 是闭区间，导出与水位提交之间不会丢窗口。
- `acquire_ingest_lock` 绑到 `_lock` 而不是 `_`，所以在 `create_checkpoint` 和 `ingest_into_hub` 里都覆盖整个临界区。
- `hub ingest` 的 JSON 解析 —— `tracing` 写 stderr，`SshTransport::exec` 把 stderr 折进错误串，所以「hub 太旧 → 退回全量 push」这条回退确实会触发。
- `zcode` adapter 的 `ORDER BY COALESCE(sequence, time_created)` —— 测试 fixture 里 `message` 和 `part` 上都有 `sequence`。
- `isUnderAnyCanonicalRoot` 从 `adapters/types/index.ts` 导出，继承了新的反斜杠归一化。
- `which`、`zstd`、`fs4`、`tempfile` 都在 workspace manifest 里声明了。

---

## 轴二：设计边界审查

| # | 严重度 | 问题 | 归属 PR |
|---|---|---|---|
| [#17](https://github.com/andrew05060414/chronicle/issues/17) | high | hub 往返把本机自己的归档重新导入，重复一半以上 | 无 |
| [#19](https://github.com/andrew05060414/chronicle/issues/19) | high | 没有只读模式：面向 agent 的路径以读写方式打开实时归档，还可能静默建出一个空库 | 无 |
| [#25](https://github.com/andrew05060414/chronicle/issues/25) | high | 服务停止：checkpoint 静默陈旧 8 天，搜索硬失败，没有健康信号 | 无 |
| [#26](https://github.com/andrew05060414/chronicle/issues/26) | high | 3-2-1 备份并不是 3-2-1：异地目标从来没有被排程 | 无 |
| [#27](https://github.com/andrew05060414/chronicle/issues/27) | medium | 异地备份从不清理：Oracle 和 Google Drive 副本无界增长 | 无 |
| [#20](https://github.com/andrew05060414/chronicle/issues/20) | low · product-boundary | `mmry extract` 不做任何抽取，而 README 把它列在功能里，与连接层排除清单自相矛盾 | [#32](https://github.com/andrew05060414/chronicle/pull/32) |
| [#21](https://github.com/andrew05060414/chronicle/issues/21) | — · product-boundary | 仓库把贡献者指向 `.trx/`，而 issue 在 GitHub；`.pi/todos` 和 `.octo` 是并行看板 | [#32](https://github.com/andrew05060414/chronicle/pull/32) |
| [#22](https://github.com/andrew05060414/chronicle/issues/22) | low | MCP 服务端界面：去掉 echo、修 `get_profile`、替换模板字符串、写清它在记忆层里的角色 | [#28](https://github.com/andrew05060414/chronicle/pull/28) |
| [#23](https://github.com/andrew05060414/chronicle/issues/23) | enhancement | agent 读取面不完整：MCP 上没有暴露渐进式读取工具 | 无 |
| [#24](https://github.com/andrew05060414/chronicle/issues/24) | enhancement | fork 不变量没有合并守卫：用 fork-only 测试把它们钉住 | 无 |
| [#33](https://github.com/andrew05060414/chronicle/issues/33) | — | Pre-PR 记忆完整性门禁 | [#34](https://github.com/andrew05060414/chronicle/pull/34) |

### #17 的根因（已定位，写在 issue 评论里）

`remote sync` 的 `--direction` 默认值是 `pull`。一条不带参数的 `chronicle remote sync -r <hub>` 会把 hub 拉下来 merge 进本地库，而 `merge_databases` 给每个进来的 source id 都加上 remote 名前缀——于是本机自己 push 上去的 `<device>:<source>` 再回来就变成 `<remote>:<device>:<source>`。双前缀就是「satellite 把已经含有自己命名空间的 hub 拉回来」的特征。

排查过程中有两次自我纠错，记下来避免重走：先误判成 v0.5.25 合并导致（错，那份代码根本没装），再误判成 09:30 有一次来路不明的 pull（错，SQLite 存的是 UTC，09:30 UTC 就是本地 05:30 的既有计划任务）。

修法有三步，第一步是核心：pull 时跳过 id 以自己的 `device_namespace` 开头的 source；satellite 模式下把默认方向改成 `push` 或强制显式 `--direction`；help 文本要写明 pull 会写入本地归档。清理已有的双前缀行是第四件事，取决于保留策略——hub 上那份可能留着本机 adapter 已经轮转掉的历史。

[#35](https://github.com/andrew05060414/chronicle/pull/35) 正在做这件事，目前 open 待评审。

### #25 有两个原因，别只修一个

服务停了是一个，另一个是配置里 `[checkpoint] enabled = false`。只把服务拉起来不会让 checkpoint 恢复。

---

## PR 面板

2026-09-18 上午一次性合掉了五个：#30（06:15）、#29（06:16）、#31（06:16）、#34（06:33）、#28（06:40）。main 从 `0186b81` 走到 `fddb223`。

| PR | 覆盖 | 状态 |
|---|---|---|
| [#30](https://github.com/andrew05060414/chronicle/pull/30) | #5 #6 #7 | 已合并 |
| [#29](https://github.com/andrew05060414/chronicle/pull/29) | #10 #11 #12 #15 | 已合并 |
| [#31](https://github.com/andrew05060414/chronicle/pull/31) | #13 | 已合并 |
| [#34](https://github.com/andrew05060414/chronicle/pull/34) | #33 | 已合并，门禁已生效 |
| [#28](https://github.com/andrew05060414/chronicle/pull/28) | #18 #22 | 已合并 |
| [#35](https://github.com/andrew05060414/chronicle/pull/35) | #17 | open，待评审 |
| [#32](https://github.com/andrew05060414/chronicle/pull/32) | #20 #21 | draft |

**合并没有自动关掉对应的 issue。** 只有 #10、#13、#18、#22 关了；#5、#6、#7、#11、#12、#15 仍然 open，尽管修它们的代码已经在 main 上。读这张表之前先确认 issue 的真实状态，不要把 open 当成「还没修」。

#34 作为前置这件事已经过去了：它先落地，CI 现在跑 `clippy --all-targets -- -D warnings` 和全 target 测试，后面合进去的 PR 都是在这道门禁下过的。#33 本身仍然 open——门禁的最后一步（给 `main` 配 ruleset / 分支保护，让检查成为硬闸）还没做。

## 还没有 PR 的

#3、#4、#8、#9、#14、#19、#23、#24、#25、#26、#27。#17 现在有 [#35](https://github.com/andrew05060414/chronicle/pull/35)。

按「会不会弄坏数据」排，#3 最靠前——唯一一个标 `security` 的。#4 和 #17 同属同步命名空间这一族，#35 落地之后应该紧接着做 #4。#24 应该在下一次合上游**之前**做：`0186b81` 炸出 13 条回归正是因为没有它，而 #34 的门禁只覆盖记忆完整性，不覆盖 fork 不变量。
