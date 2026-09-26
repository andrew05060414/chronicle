# Chronicle 项目脉络

给后续 AI 的可携带简报。口语叫 **Chronicle**；CLI 是 `chronicle`（兼容名 `hstry`）。

- 仓库事实写到 **2026-09-26**（`main` = `ff4d449` / #54）。
- 2026-09-18 以前的本机对话补遗来自对 live archive 的只读检索；09-25 / 09-26 的部分来自云端 Claude Code 会话。
- 部署合同以 [`archive-model.md`](./archive-model.md) 为准；恢复以 [`restore.md`](./restore.md) 为准。本页补的是「为什么会变成这样」和「Andrew 在对话里拍过什么板」。

---

## 30 秒版（可整段粘贴给其他 AI）

Chronicle 是 Andrew 的个人 AI 系统 **Antiochus** 里「历史档案」这一类的部件。Antiochus 是系统总代号（前身叫 Personal AI Assistant）；语音输入常把它识别成「Antigravity」，但 Antigravity 是 Google 的编码 IDE，在这里只是被采集的一个来源。

Chronicle 是 Andrew 的 **AI 对话原文档案**，不是聊天 App，也不是记忆系统。它把 Cursor / Codex / Claude Code / Antigravity / Pi 等十几个本地工具的对话收进本机 SQLite，再按 `device_id` 推到 NAS hub。抽取、总结、打分是下游的事。

上游是 `byteowlz/hstry`。Andrew 的 fork 是 `andrew05060414/chronicle`。对人说 Chronicle；crate、`%APPDATA%\hstry\`、数据库路径继续叫 hstry，方便合上游。

不要碰：CTX、AMS / `agent-memory-system`、Mem0、Agent Memory、jobs tracker、knowledge-hub。技能安装走 `chronicle skills`（opt-in），不要 clone AMS。GitHub Issues 才是看板；`.trx/` 是上游习惯，当只读历史。

本机卫星：`sync.mode = satellite`，`device_id` 见本地 `config.toml`，`hub_remote = nas-lan`。禁止把带设备前缀的 hub 全库覆盖回 staging 再 push。搜索在 Windows 上用 `HSTRY_NO_SERVICE=1` 加 `--scope local`。未授权不要 `sync` / `backup` / `remote sync`。

当前工程焦点：[#61](https://github.com/andrew05060414/chronicle/issues/61)。检索、索引、界面和多机汇总逐步交给 AgentsView；Chronicle 保留名字、入口（`chronicle search` 以后转发给 AgentsView）、浏览器网页对话采集和备份配置；现有 hstry 库冻结为只读档案兜底。不 fork AgentsView。第 1–3 步完成之前不删任何 hstry 代码或数据。两条审查轴基本收口，只剩 #25 还开着。

---

## 这份文件怎么用

| 你要做什么 | 读哪里 |
|---|---|
| 判断该不该改 Chronicle | 本文「是什么 / 不是什么」+「已定决策」 |
| 改采集、同步、备份 | [`archive-model.md`](./archive-model.md)、[`remote-sync.md`](./remote-sync.md)、[`restore.md`](./restore.md) |
| 查「Andrew 当时怎么说的」 | 本文「本机对话补遗」里的短 ID，本地 `chronicle peek <id>` |
| 查仓库时间线、issue、PR | 本文「怎么走到今天」；提交级时间线见 [`fork-timeline.md`](./fork-timeline.md)，issue↔PR 登记见 [`merge-review-register.md`](./merge-review-register.md)；细节以 GitHub 为准 |
| 本机路径、remote host、device_id | `%APPDATA%\hstry\config.toml`。不要把里面的 host / 绝对路径写进公开 PR 或测试 |

**来源分层（不要混）：**

1. **仓库与 GitHub**：docs、提交、issue #3–#34、PR。2026-09-18 有一份云端整理，仓库部分可用；它明确写了读不到本机档案。
2. **本机 Chronicle 档案**：2026-09-18 检索。当时库内约 5800 会话 / 35 万条消息 / 40 个 source；时间范围 2025-03-14 → 2026-09-17。关键词命中约 127 个不重复会话，其中大量是 history skill 被注入到无关 Claude Code 会话，或 AMS / 3-2-1 的误伤。下面只收 **Andrew 本人在拍板或追问** 的线程。
3. **仍缺**：claude.ai 网页会话、未入库的手机端、以及 adapter 还没覆盖的源。网页 ChatGPT / Gemini 直播抓取是后置的。

---

## 01 这是什么，不是什么

项目自己在 [`archive-model.md`](./archive-model.md) 里把边界写死：

> 不是聊天 App，不是记忆系统。记忆（Agent Memory）是这份档案的客户。

档案层只负责逐字存下对话和提供检索。抽取、总结、打分、归纳，全都是别人的事。这条线直接决定了 #20（`mmry extract` 名不副实）和 #21（仓库里不该有第二个任务板）。

README 的清单：

> Not on this bus: CTX, AMS, Mem0, the jobs tracker, knowledge-hub.

技术形态：Rust workspace，6 个 crate（core / runtime / cli / tui / mcp / api）。适配器是 TypeScript，由 Rust 运行时用 node / bun 执行，走 JSON。`chronicle` 和 `hstry` 是同一个二进制。

`hstry-mcp` 在 Andrew 的日常技能里是空壳占位：**不要**把它配进 MCP 当搜索接口。要用 CLI：`chronicle search` / `peek` / `show`。

---

## 02 四层，不要揉

```
  IDE / CLI / harness / 网页导出
              │  adapters（只解析，不改原工具）
              ▼
     本机 staging.db  ──push device_id──►  NAS hub
                                              │
                                              ├── rclone / NAS Cloud Sync ──► 云盘冷快照
                                              └── CLI / Search API / MCP ──► 人 + 其他 agent
```

| 层 | 职责 | 硬规则 |
|---|---|---|
| 采集 | 各工具对话 → 规范化会话 | 一个工具一个 canonical root。本机没有数据的工具不预做 |
| 档案 | 一台机一份 staging，NAS 一份合并 hub | 按 `device_id` 命名空间 merge。保持 dumb：不抽记忆 |
| 备份 | hub 库的时间点副本 | push 是 merge，不是备份。云盘不是第二个 live hub |
| 检索 | 跨工具、跨设备搜 | satellite 默认问 hub；`--scope local` 才只搜本机 |

硬规则：禁止把带设备前缀的 hub 全库覆盖回 staging 再 push。恢复下来的副本只给本机搜索。违反这条的后果就是 #17。

当前拓扑（角色名，具体 host 在本地 config）：

- Windows satellite（`device_id` 见 config，现网是 `arknights`）
- Mac satellite
- NAS hub（`hub_remote = nas-lan`；Tailscale 备用 remote 名 `nas`）
- 云盘 / Oracle：冷副本

---

## 03 在 Antiochus 里的位置

Andrew 近两个月高强度做的「个人 AI 定制」，2026-09-20 起有了总代号 **Antiochus**，09-26 冻结了 v0.2 设计。Antiochus 分三部分：前台（跟人说话，现任是 ChatGPT 网页）、后台、基础设施。后台分 9 类：1 历史档案、2 长期记忆、3 记忆检索、4 记忆整理、5 任务编排、6 工具箱、7 模型网关、8 安全与权限、9 触发与通知。

只有自己写的或 fork 的部件才起代号，目前只有 **Chronicle**（1 历史档案）和 **Agent Factory**（5 任务编排）。代号只指那个部件，不指整个类别。框架篇、现状篇和 ADR 在 Andrew 的 antiochus 仓库里，以那边为准。

对话里反复出现、后来被拆开的东西：

| 名字 | 现在的定位 | 不要当成 |
|---|---|---|
| **Chronicle / hstry** | 1 历史档案。对人说的名字和日常入口 | 记忆、日记、任务板 |
| **AgentsView** | 3 记忆检索的现任（#61 迁移中）。不 fork，只用它公开的 CLI、MCP 和导入格式 | Chronicle 的替代名 |
| **OpenCodex**（`ocx`） | 7 模型网关，唯一网关；也是额度数据的来源 | 档案层 |
| **9Router** | 被 OpenCodex 替代，逐步下线 | 现行网关 |
| **Multica** + MCPX | 5 任务编排：MCPX 管计划与授权，Multica 管派发 | 档案层 |
| **Andrew-Skill / ASM** | 6 工具箱。`chronicle skills` 是 opt-in 代理 | 记忆图；不要 clone AMS 来装 skill |
| **worklog** | 人类日记与凭证。Chronicle 转交 | 不要把日报写进档案库 |
| **Agent Memory**（`agentmemory`） | 不再使用（「效果非常差」） | Chronicle 的替代品 |
| **CTX** | 排除 | 记忆总线 |
| **AMS / `agent-memory-system`** | **已冻结**（2026-09-05） | 不要装 Junction，不要跑 `ams rollup` |
| **Mem0** | 不用；不引入会话注入式记忆框架 | 记忆层 |
| **knowledge-hub / collection-demo** | 收藏与阅读沉淀，另一条线 | Chronicle 功能 |

2026-09-11 的原话（`late-lets`，Cursor）：健康检查的知识库「现在已经建好了，就是 Chronicle，这是基础」；智能体集合中心是 Multica；token 是 9Router（现在已换成 OpenCodex）。

2026-09-05 的原话（`pass-yawn`，Cursor）：求职仓库和星际 setup 都不是记忆系统，最多是档案里的一类记录；真正要进包装层的是对话档案、备份手段，以及 Andrew-Skill。

---|---|---|
| **Chronicle / hstry** | 对话原文档案。日常入口 | 记忆、日记、任务板 |
| **Andrew-Skill / ASM** | 技能安装与审计。`chronicle skills` 是 opt-in 代理 | 记忆图；不要 clone AMS 来装 skill |
| **worklog** | 人类日记与凭证。Chronicle 转交 | 不要把日报写进档案库 |
| **Agent Memory**（`agentmemory`） | 闲置。不要当日常入口 | Chronicle 的替代品 |
| **CTX** | 闲置。对话里被明确排除；有一次还被误说成 token 统计 | 记忆总线 |
| **AMS / `agent-memory-system`** | **已冻结 / 逐渐废弃**（2026-09-05 拍板） | 不要装 Junction，不要跑 `ams rollup` |
| **Multica** | 多 agent 派发看板 | 档案层 |
| **9Router** | token / 线路 | 档案层 |
| **knowledge-hub / collection-demo** | 收藏与阅读沉淀，另一条线 | Chronicle 功能 |

2026-09-11 的原话（`late-lets`，Cursor）：健康检查的知识库「现在已经建好了，就是 Chronicle，这是基础」；智能体集合中心是 Multica；token 是 9Router。这是栈的分工，不是要把它们重新包成一个巨石仓库。

2026-09-05 的原话（`pass-yawn`，Cursor）：求职仓库和星际 setup 都不是记忆系统，最多是档案里的一类记录；真正要进包装层的是对话档案、备份手段，以及 Andrew-Skill。

---

## 04 怎么走到今天

上游 2026-01 起步。Andrew 的改动从 **2026-07-24** 开始。主线是：先把本机工具收全 → 多机同步和备份做稳 → 最后才谈品牌和工程纪律。

### 2026-07-24：Windows 变成一等公民

本机档案里这一天非常密，几乎全在 Cursor，仓库当时还叫 `Github\hstry`。

| 短 ID | 在聊什么 |
|---|---|
| `eyed-even` | 第一次把仓库当「hustle」在 Windows 上跑。要编译、要周期性同步，不要一次性导入。明确接受「采集必须在 Win/Mac 本机做，主库在 NAS」。要求 setup 简单到交给 AI 就能处理，并且日常用的 OpenCode / Pi 也要能同步 |
| `camp-sham` / `pied-cove` / `loco-knee` | Cursor adapter v2 从 `globalStorage` 拉全量：dry-run 约 610 会话 / 1.5 万消息，明显多于只扫 workspace 的 v1 |
| `shut-mayo` / `puff-cali` | Windows 构建缺 `protoc`；配置和 adapters 在 `%APPDATA%\hstry` |
| `tall-site` | 按 `nas-hub-handoff.md` 选 NAS 落点。push 成功后一度写进默认路径而不是预定目录，后来纠正。当天 NAS 侧已能搜到五千级会话 |
| `awry-flat` | Cursor 多 workspace（SSH IP 在变）≠ hstry 登记了三个 Cursor source。两层问题不要混：cursaves 修的是 Cursor 自己的 SQLite；hstry 修的是 source 注册 / dedup / push |

同一周还补了 qclaw / workbuddy / antigravity、路径分隔符、以及用 `device_id` 给 satellite push 分命名空间。

### 2026-08：采集补齐，hub 做成真的 hub

仓库侧：Antigravity 其实是三套独立会话库（2.0 应用、agy CLI、IDE 1），旧 adapter 一个都没读到；加上 dsh、zcode、Grok Build。hub 侧 delta ingest + 滚动 checkpoint，push 改成增量 merge。

本机对话里 8 月底仍在推 nas-lan、处理 Mac 重置后留在 hub 上的 `nas-lan:macbook:*` 源。

### 2026-09-03：先做过一套「记忆系统」，随后否定

| 短 ID | 在聊什么 |
|---|---|
| `punk-smog` | 「用 HSTRY 追溯我们说过的整套记忆管理，结论是什么，HSTRY 正不正常」——当时答案还指向 `agent-memory-system` |
| `dear-turf` | 在 `agent-memory-system` 上做 AMS 1.0：消融、CTX→worklog、3-2-1、一键 Skill 部署。后来这套被冻结，**不要把那次结项当成现行架构** |

### 2026-09-05 / 09-06：名字和边界定下来

这是整段个人 AI 定制里对 Chronicle 最关键的两天。

`amok-then`（Antigravity）：Andrew 喜欢 Chronicle，也想过 AMS = Agent / Angel / Andrew Memory System，问能不能两个名字都留。

`pass-yawn`（Cursor，65 条，约 25 小时）：拍板。

Andrew 原话，不要再重新辩论：

1. AMS 仓库「可以冻结掉，或者逐渐废弃掉」。
2. 不要再做一个「把这几个整合在一起重新包装」的新仓库。
3. 仍要做的三件事：HSTRY 的 3-2-1 备份；新环境一键 setup（这才是当初想用 ASM 做的事）；改成更好听的名字，例如 Chronicle。
4. 口语和命令入口用 **Chronicle**，不要用 AMS（和 ASM 技能安装器撞车，语音识别也差）。
5. CTX 排除。求职仓库不是记忆系统。
6. 技能系统可以挂到 Chronicle 边上代管，但主线仍是对话档案。
7. 支线：把作者上游和自己的 fork 逐步合并，「功能以我的为主，作者的更新能吸进来」。

当天代码侧：`chronicle` 二进制、`backup`、`skills` 代理进仓库，合入上游 v0.5.24。rclone 接到个人 Google Drive，冷备目录 `gdrive:chronicle-cold-backup`。

### 2026-09-07：备份的真正目标是「能解析也能放回去」

`lame-mule`（Antigravity，从 C 盘爆满聊到备份）：3-2-1 已经有了，但换机器后路径会变，物理拷 Cursor 目录会失效。结论方向是——**最主要的是把对话记录保存下来**；理想中 hstry 既能解析也能按新路径放回去。这和 [`restore.md`](./restore.md) 的优先级一致：档案恢复 > 会话 resume，resume 只在同类环境做。

### 2026-09-09：第一次把 Claude 会话修回去，并跑通 3-2-1

`sign-tote`（Claude Code，在 pxread 工作区里顺手做的）：Documents 里一堆 `claude-3p-repair-backup-*`。两件事都做了：31 个 jsonl 放回 `~/.claude/projects/`（会话 299→330）；Chronicle 侧 31/31 peek 通过，当时统计 1985 会话 / 136318 消息；checkpoint + nas-lan push + gdrive 冷备跑通。NAS 端首次建表会跑 embedded migration，属正常。

注意：这证明 **3-2-1 管道能跑**，不证明它在排程里一直跑。现网 `config.toml` 里 `[checkpoint] enabled = false`，和 #25 / #26 对得上。

### 2026-09-10：Chronicle 已经是跨会话的检索层

`rude-yolk`：Andrew 问「你是原生 Claude 能 refer 另一个对话，还是通过 Chronicle？」答案是原生不能；跨会话靠 Chronicle 档案 + 偶尔的 MEMORY.md 摘要。这是产品被真正用起来的标志，不是功能讨论。

同日 `male-robe` 用 Chronicle 挖实习/简历材料——档案的客户出现了，客户不是档案自己。

### 2026-09-14：日常用它整理别的工具

`deaf-raft`：用 Chronicle 看 Antigravity 里乱七八糟的 Projects，怎么整理、怎么隐藏。结论：整理 Projects 书签不会破坏已经入库的历史。

`blue-frey`：Grok `/learn` 扫 traces，Chronicle 用来核对「人坐着打字」的习惯。Grok 侧样本少，没写成新技能。

### 2026-09-16 / 09-17：品牌落地，然后合上游炸了

仓库：11 笔提交做完改名、schema、清掉公开树里的私有部署路径、Andrew-Skill 改 opt-in、第三方发包加闸。PR #1 合并。随后 `0186b81` 合入上游 v0.5.25。

本机对话：

| 短 ID | 在聊什么 |
|---|---|
| `even-king` | 云端 fork 已改名 Chronicle；本地 `Github\hstry` → `Github\chronicle`，旧连接层 → `chronicle-old` / `chronicle-legacy` |
| `ripe-lots` | 要不要对 pxread 或 Chronicle 做一次「总审核」。判断：不要空泛总审核；如果只能选一个，选 Chronicle，因为它守着 1.7GB 级对话史，且刚并完上游 |
| `like-gray` | `/code-review` 后把 13 条回归开到 GitHub（#16 追踪）。**没有**报到上游 `byteowlz/hstry` |

一个容易踩的时间差：v0.5.25 合进 git 了，PATH 上的 `chronicle 1.0.0` 是更早的安装。#3–#15 这类回归主要在代码里，重新 `cargo install` 之后才会进日常使用。

### 2026-09-18 → 09-24：两条审查轴收口

#35（修 #17 回流）、#32、#36 在 09-18 合入；09-20 合了 #38 #39 #40 #43 #44 #45（远程注入、设备 UUID、备份 stderr、加密备份清理、Windows 服务启动）；09-23 / 09-24 合了 #47（agent 只读打开数据库，#19）、#48（fork 不变量守卫，#24）、#49（checkpoint 健康检查，#25）、#52（远端快照清理，#27）、#53（Issues 是唯一看板，#21）。

09-24 审查 #51（原生备份与恢复）：结论 FAIL，6 个阻塞问题。Andrew：「先把 A 到 D 修了……我比较在乎 1 到 5，第 6 个倒没有那么重要」。于是拆成 #56（零碎修复）、#57（原生备份只读部分）、#58（同步确认、本地加 hub 搜索、HTTP，默认关闭）；写回客户端拆成 #59，更多客户端和非会话数据拆成 #60。#51 保持打开，只当代码来源。

### 2026-09-25 / 09-26：清掉回流副本，检索交给 AgentsView

诊断：检索差的主因是重复和自我污染，中文分词是次要的。7911 个会话里 4067 个是 `nas-lan:<device>:*` 回流副本；#35 的修复 09-18 就合了，但一直没部署；结果返回的是碎片不是会话；当前会话会搜到自己。

Andrew：「先用 main 重新编译安装，副本清理的方案给我看看（最好是先做一下数据备份，然后再清理）」。先做 2.4 GB 完整备份，再删掉 3958 个会话、约 21 万条消息，剩 3953 个。随后合入 #54 #56 #57 #58，`main` 到 `ff4d449`。

20 道真实题的对比（排除评测会话本身）：AgentsView v0.44 加中文分词，前 5 命中 13/20、中位 0.8 秒；Chronicle 9/20、2.3 秒。两边数据互补：Chronicle 独有约 1430 个网页对话，AgentsView 的 Codex 等来源更全。

Andrew 拍板：「我觉得记忆检索总体就叫 Chronicle……相当于只保留同步这一部分」「可以把这个步骤方案挂到 issue 里面去，相当于逐渐地把 HSTRY 给推移掉」。这就是 #61。他也不想自己包一层 restic 带来双重维护，所以备份以后改用 restic 或 Kopia 的现成配置，#57 会被替换。

向量不是必需的：Andrew「个人是不太喜欢碰向量重排这些东西的」，主路径是关键词加中文分词，不做 rerank。

---

## 05 已经定下来的决策

这些有对话或文档证据。新讨论不必从头吵。

| 决策 | 依据 |
|---|---|
| 叫 Chronicle，底层留 hstry | `pass-yawn`、`amok-then`、README、09-16 品牌提交 |
| 档案层不抽记忆 | `archive-model.md`；AMS 冻结于 `pass-yawn` |
| 不要再造一个「整合包装层」仓库 | `pass-yawn`：「现在是不是没有必要整合在一起重新去包装」 |
| 一键 setup 属于技能/ASM 效果，不是档案核心 | `pass-yawn`、`eyed-even`；实现上是 `chronicle skills bootstrap`，opt-in |
| GitHub Issues 是看板，`.trx/` 只读 | #21、PR #32 |
| 恢复：档案 > resume，且在要用的那台机器上做 | `lame-mule`、[`restore.md`](./restore.md) |
| 网页直播抓取后置 | archive-model；takeout adapter 已有 |
| 云盘只做冷快照，仓库不写 Drive SDK | rclone；`pass-yawn` 当天接通 gdrive |
| 只 merge「某种格式怎么解析」，不整仓合 agy-reader / cass | archive-model |
| 日常合上游：功能以 fork 为准，作者更新要能吸进来 | `pass-yawn` 最后一轮 |
| 未授权不把 issue 报到上游 | `like-gray` 明确没发到 `byteowlz/hstry` |
| Chronicle 是 Antiochus「1 历史档案」的部件；只有自研或 fork 的部件起代号 | Antiochus ADR-0004（2026-09-26） |
| 检索、索引、界面、多机汇总交给 AgentsView；Chronicle 保留名字、入口、网页采集、备份配置 | #61（2026-09-26） |
| 不 fork AgentsView，不写自有备份代码 | #61 |
| 模型网关用 OpenCodex，9Router 下线 | Antiochus ADR-0005（Proposed） |

---

## 06 悬而未决

以 GitHub 为准。2026-09-26 还开着的 issue：

| # | 问题 | 现状 |
|---|---|---|
| #61 | 逐步退役 hstry 核心，检索交给 AgentsView | 第 1 步评测进行中 |
| #25 | 服务停了，checkpoint 陈旧 | #49 已加健康检查；服务要管理员重启一次 |
| #59 | 原生安装：把快照安全写回 agent 目录 | #51 的第 6 块，优先级低 |
| #60 | 原生备份覆盖更多 agent 和非会话数据 | 可能被 #61 第 4 步的现成备份工具替代 |
| #55 | 合并队列 | 看 GitHub |
| #50 | Grok 网页采集 | #54 已合 |

### 还没想清楚

- 向量检索要不要保留。Andrew 倾向不用。
- AgentsView 升级后换成安装 ID，`pg push` 会不会在 NAS 的 PG 里产生重复会话；PG 汇总库的中文检索效果（中文分词只作用于 SQLite）。
- Grok 网页对话没有 AgentsView 的导入路径，要单独处理。
- #3 要不要报给上游（代码已修）。

### 对话里提出、档案层故意不做的

- 「既能解析也能放回去」的跨机 resume（`lame-mule`）：拆成 #59，不是主线。
- 个人 AI 收藏夹 / 链接分享（`east-firm`、`brag-poll`，2026-09-17）：在 knowledge-hub / `collection-demo`，**不是 Chronicle 的 issue**。
- 记忆提炼、决策摘要：属于 Antiochus 的「长期记忆 / 记忆整理」，不进 Chronicle。

---

## 07 现在手上正在进行的

- #61 第 1 步：向量建完后测整句提问；查 NAS 上 PG 的 pgvector 版本和中文检索；确认 `pg push` 不重复。
- 第 2 步准备：`chronicle search` 改为转发给 AgentsView；整句提问先拆成关键词再搜。hstry 服务并行 2–4 周。
- 开着的 PR 只有 #51（不合，只当代码来源）。

`andrew-nas`、`feat/rip-out-tantivy`、`release/1.0` 已并入 main，只是没删的旧指针。

---

## 08 给后续 AI 的操作纪律

1. 先读本文和 [`archive-model.md`](./archive-model.md)，再改代码。碰检索相关的改动先看 #61，别再往 hstry 的检索上加功能。
2. 检索用 `--scope local` 加 `HSTRY_NO_SERVICE=1`。未授权不要 `sync` / `remote sync` / `backup` / `reseed`。
3. 不要把 live archive 路径写进测试或 CI。#34 的门就是为这个而开。
4. 不要把 CTX / AMS / Agent Memory / Mem0 接回来「增强」Chronicle。
5. 不要把 `.trx/`、`.pi/todos/`、`.octo/` 当成现行看板。
6. 合上游时保留 fork 已有的安全机制和证明它们的测试（#16 就是因为这次没守住）。
7. Windows / Linux / macOS 行为对等，不为旧系统写 shim。
8. 公开树里不要写本机绝对路径、NAS hostname、rclone token。09-16 专门清过一次。
9. 项目总名写 Antiochus。只有指 Google 那个 IDE（Chronicle 的采集来源）时才写 Antigravity。

---

## 09 本机对话索引

本地：`chronicle peek <短ID>`。这些是补云端空白用的主线程，不是完整清单。

| 短 ID | 日期 | 源 | 为何重要 |
|---|---|---|---|
| `eyed-even` | 07-24 | Cursor | Windows 接手、setup 要简单、多工具同步 |
| `tall-site` | 07-24 | Cursor | NAS hub 落点；push ≠ 备份 |
| `awry-flat` | 07-24 | Cursor | 三个 Cursor source vs Cursor 3.0 header 坑 |
| `punk-smog` | 09-03 | Antigravity | 用 HSTRY 追溯「记忆系统」讨论 |
| `dear-turf` | 09-03 | Antigravity | AMS 1.0 结项（此后冻结） |
| `amok-then` | 09-05 | Antigravity | 起名 Chronicle / AMS |
| `pass-yawn` | 09-05 | Cursor | 冻结 AMS、3-2-1、一键 setup、合上游策略 |
| `lame-mule` | 09-07 | Antigravity | 换机后路径失效；档案优先于物理拷贝 |
| `sign-tote` | 09-09 | Claude Code | Claude 会话修复 + 第一次 3-2-1 跑通 |
| `rude-yolk` | 09-10 | Claude Code | 跨会话检索靠 Chronicle，不靠原生客户端 |
| `late-lets` | 09-11 | Cursor | 个人 AI 栈：Chronicle 底座，Multica / 9Router 旁路 |
| `deaf-raft` | 09-14 | Antigravity | 用档案整理 Antigravity Projects |
| `blue-frey` | 09-14 | Grok | 用档案核对 Grok 使用痕迹 |
| `even-king` | 09-17 | Cursor | 本地目录随仓库改名 |
| `ripe-lots` | 09-17 | Claude Code | 不要空泛总审核；优先守档案完整性 |
| `like-gray` | 09-17 | Claude Code | v0.5.25 回归写成 GitHub issues |

检索备忘（Windows）：

```powershell
$env:HSTRY_NO_SERVICE = '1'
chronicle search "chronicle" --scope local --limit 20 --compact
chronicle peek pass-yawn
```

---

整理于 2026-09-18，2026-09-26 更新。仓库依据：`andrew05060414/chronicle` 在 `ff4d449` 上的文档、提交、issue 与 PR。本机依据（09-18 以前）：live archive 只读检索 + 上表会话的 `peek`。09-24 到 09-26 的部分来自云端 Claude Code 会话（#51 审查、Chronicle 架构评估与演进、Antiochus v0.2 冻结）。未包含 claude.ai 网页会话正文，也未把 `config.toml` 里的 host 与绝对路径抄进本页。
