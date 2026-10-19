# shipbraid 设计文档

## 1. Document Control

- Feature name：发布请求协调与安全抢占。
- Version：0.2，2026-10-16。
- Author：Codex；产品方向来自项目所有者。
- Reviewers：Codex 按 design-review-gate 清单自审；不是独立评审或用户确认记录。
- Status：APPROVED（技术自审范围为下述 M1；M2 为后续边界）。
- Review gate status：PASS。
- Linked review report：[§13 设计评审与反馈](#13-review-feedback-tracking)。
- Linked execution plan：[execution-plan.md](execution-plan.md)。
- Historical integration record：[外部执行器接入迭代](features/iter-20261016-executor-integration/design.md)（当时的项目环境验收记录；不改变本设计的 M1 验收状态）。
- Agent iteration：[同团队 Agent 发布闭环](features/iter-20261017-agent-workflow/design.md)（2026-10-17，用户确认固定 local 租户；[验收记录](features/iter-20261017-agent-workflow/verification.md)）。
- Implementation checkpoint：[首批实现与未完成项](acceptance/m1.md)；设计批准不代表全部 M1 已实现。
- Usage direction：[单项目前台 CLI 与共享 Server](features/iter-20261017-local-cli/design.md)（2026-10-17，单项目默认 CLI，跨项目或 Dashboard 使用 Server；实施状态以独立计划为准）。

## 2. Context and Problem

频繁提交、多个调用者和紧急上线会触发大量重复构建与部署。每个调用者单独执行流水线，会出现同一版本重复运行、旧版本占用资源、旧执行覆盖新环境、以及被取消的 CI 把实际已满足的上线需求误报失败等问题。

需要统一回答：这个上线需求是否已被满足？是否已有执行会覆盖它？是否值得抢占当前任务？请求最终由哪个产物和哪次环境验收满足？

设计来源是两个明确场景：① 已有上线任务覆盖需求，直接等待同一任务；② 紧急请求抢占可打断的普通执行，被替代请求继续等待新的发布结果。问题重点是调度和结果归属，编译命令可以继续使用现有工具。

项目独立于所有接入方。仓库不复制下游项目的生产凭据、环境清单或整套发布脚本。

## 3. Goals and Non-Goals

**Goals**：持久保存每个发布请求；复用覆盖执行；合并兼容的待处理请求；支持紧急优先与安全抢占；共享已验收的结果；在重复投递、服务重启和取消不明时保留正确归属。

**Non-goals**：自研编译器、通用 DAG 平台、代码合并服务、语义代码分析、自动数据库回滚、多租户 SaaS、跨机房调度或“任意脚本可随时强杀”。

不承诺替代后的每个旧 CI run 都显示绿色；请求层显示成功及 satisfied_by_run，执行器原始取消记录仍保留。

## 4. Scope

| 阶段 | 包含 | 不包含 |
| --- | --- | --- |
| M1：调度内核 | Rust 单服务；SQLite；API/CLI；React + TypeScript + Tailwind 只读 Dashboard；完整覆盖/合并/抢占/恢复；ScriptedExecutor 可控执行器与故障注入 | 真实构建、生产写入、GitHub 凭据接入、任意命令执行 |
| M2：首个真实接入 | GitHub Actions 关联、查询、取消、阶段准入和独立环境验收；先测试环境 | 多执行器矩阵、自动生产授权、Argo CD/Dagger 同时接入 |

M1 必须完整验收用户的两条核心规则，但只对可控执行器声明通过。可运行的真实发布闭环要到 M2 才成立。

约束：首版单实例、单个本地持久卷，多个调用者并发访问；每条发布通道最多一个活动执行。SQLite 不放在 NFS/共享网络文件系统上。不在设计阶段改造调用方的既有发布入口或触发真实部署。

## 5. Users and Use Cases

- 开发者和 Agent：提交不可变版本与目标环境，获取 request_id，重复查询或等待最终结果。
- 发布负责人：提交 urgent 请求，查看合并/抢占原因，对未知投递和环境状态作有证据的处置。
- 执行器：接收执行命令，报告阶段和证据，申请进入不可抢占阶段。

典型行为：同一请求重试不重复创建；不同请求同一版本共享执行；新版本覆盖旧普通请求；紧急请求如果已经被覆盖则提升现有批次；不同分支或配置不兼容时排队；控制器重启后先核对旧执行，不重新盲发部署。

## 6. Requirements

### 6.1 Functional Requirements

- **FR-001 请求账本**：每个调用者保留独立请求、原始目标、优先级、截止时间、当前归属和最终证据。
- **FR-002 幂等提交**：相同主体、项目、幂等键和请求正文返回原请求；正文不同返回 409，不静默替换目标。
- **FR-003 覆盖复用**：满足覆盖谓词时共享活动执行；新请求复用已上线状态前必须重新取得符合新鲜度要求的当前环境证据。
- **FR-004 待处理合并**：只能选择实际存在、已授权的版本；不能创建 merge commit 或把不兼容请求吞掉。
- **FR-005 优先级**：urgent 优先；已覆盖的紧急请求提升现有批次，避免无收益重跑；优先级与原始截止时间随请求转移。
- **FR-006 安全抢占**：普通、可抢占阶段的执行可以被 urgent 替代；取消确认、归属转移与下一代写权限必须明确分开。
- **FR-007 共享完成**：请求只在验收成功后完成，记录实际版本、run、receipt、覆盖证明；superseded 执行永不伪装为成功。
- **FR-008 失败与撤回**：确定失败、投递不明、用户撤回、请求过期分别处理；后继失败不自动满足旧请求。
- **FR-009 持久恢复**：重启、命令重复、回执重复/乱序、取消与完成竞争，不能重复上线或丢请求。
- **FR-010 查询与审计**：API/CLI 可查看请求、执行、决策理由和顺序事件；等待端断开不取消发布。
- **FR-011 执行器契约**：显式声明阶段、可停止能力、稳定身份、观察和验收能力；unknown 不自动重放。
- **FR-013 Dashboard**：React + TypeScript + Tailwind 操作台通过相同鉴权 API 查看通道、请求、执行、替代关系、审计时间线与有界日志；明确模拟执行器，处理空态、加载、断线、移动布局；令牌仅驻留内存。M1 不提供浏览器内危险运维操作。
- **FR-012 接入与权限**：主体按项目授权；urgent/admin 分权；所有环境写入必须走配置的执行器阶段准入。

### 6.2 Non-Functional Requirements

- **NFR-001 性能与资源**：目标是在记录 CPU/磁盘/SQLite 参数的测试机上，50 次提交/秒、持续 60 秒时，已解析版本的提交响应 p95 小于 250 ms。外部 Git/CI/验收延迟单列，不藏入调度耗时；全局活动执行默认 2，单 lane 为 1。它们是待验证目标，不是已有指标。
- **NFR-002 一致性**：状态、请求归属、持久命令和审计事件同事务提交；100 个并发同目标请求只产生一次实际执行；不承诺外部恰好一次。
- **NFR-003 安全**：不接收任意 shell、回调 URL、秘密值或可任意切换的仓库地址；目标与执行器由管理员配置。
- **NFR-004 可观测性**：记录 request_id/run_id/lane_id/command_id/generation；区分等待、构建、取消确认、部署和验收耗时；指标不以 SHA/request_id 作标签。
- **NFR-005 可运维性**：一个服务、一个本地数据库卷；重启可恢复；提供 drain 和显式 unknown 处置，不依赖内存队列作为事实源。

## 7. Proposed Solution

### 7.1 技术与部署形态

采用 Rust stable、edition 2024，一个 Cargo package。HTTP 用 Axum/Tokio，持久化用 SQLx + SQLite，序列化用 Serde，CLI 用 Clap，结构化日志用 tracing；具体依赖版本和 MSRV 在实现 T-001 时核对并锁定，不在设计中声明已经构建通过。

SQLite 使用 WAL、synchronous=FULL、foreign_keys=ON 和有界 busy_timeout。写操作用短 BEGIN IMMEDIATE 事务；不在事务内等待外部系统。使用规范化数据库路径的操作系统独占锁，第二个协调器进程（run/serve）直接拒绝启动，不以锁文件存在与否判断所有权。[SQLite 事务](https://www.sqlite.org/lang_transaction.html)、[WAL 约束](https://www.sqlite.org/wal.html)。

0.3.2-dev 的[入口迭代](features/iter-20261017-local-cli/design.md)复用上述内核：单项目由 `run` 前台持有账本、驱动调度并返回请求结果，不监听 HTTP；跨项目、并发提交或 Dashboard 使用 `serve`。两种入口共享数据库独占锁、持久恢复和验收规则，不建立第二套调度状态机。旧配置默认 server，新增 local 配置显式选择单项目信任边界。

不同时支持 PostgreSQL/SQLite 两套实现。需要多实例时再单独迁移数据库和调度所有权协议；单实例不等于仅允许单个调用者。

~~~mermaid
flowchart LR
    L[单项目前台 CLI] --> S
    C[Server CLI / Agent / Webhook] --> A[HTTP API · server]
    A --> DB[(SQLite: 请求、执行、命令、事件、证据)]
    DB --> S[Scheduler: 覆盖、合并、抢占]
    S --> E[Executor Adapter]
    E --> R[可控执行器 M1 / GitHub Actions M2]
    R --> O[观察、停止确认、环境验收]
    O --> DB
    DB --> W[查询 / 等待结果]
~~~

### 7.2 领域对象

| 对象 | 核心字段 | 规则 |
| --- | --- | --- |
| Lane | id、project、environment、profile_digest、active_run_id、generation、blocked_reason | 代表一个共享部署写入边界，如 myapp/prod/app；组件子集仍共用该 lane |
| ReleaseRequest | id、principal、idempotency_key/hash、target、priority、deadline、status、current_run_id、satisfied_by_receipt、auto_retries_used、manual_retries_used | 需求是不可变快照；紧急提升是显式审计操作，不修改原目标 |
| DeliveryRun | id、lane、target、effective_priority、state、phase、generation、executor_ref、replacement_run_id、cancel_reason | 一次尝试；目标启动后不可改变；下一次尝试有新 run_id |
| DeploymentReceipt | id、run、observed_at、source_revision、component_digests、profile_digest、verification_report、result | 冻结实际产物和目标环境验收证据；CI exit 0 不等于 receipt 成功 |
| Command / Event | id、run/generation、kind、payload_digest、delivery_state、sequence、created_at | 持久命令驱动外部调用；事件记录决策与状态变化 |

目标 Target 包括受信任的 repository_id、完整 source revision、component 集合、profile_digest 和 coverage_mode。profile_digest 覆盖目标环境映射、部署/验收逻辑版本、相关非秘密配置和秘密版本引用，不包含秘密内容。

Lane、请求、run、receipt、commands、events 共六张主表，不另建待办队列、结果镜像表或事件溯源重建系统。events 是追加式审计，业务当前状态仍由关系表给出。

关键约束：主体/项目/幂等键唯一；command 幂等键唯一；event 在 lane 内有持久单调 sequence；当前活动执行由 lane 外键唯一指向；改变 active_run 和请求归属都在同一事务中完成。所有时间由服务端生成，优先级不信任调用者自报权限。

### 7.3 覆盖谓词

定义 covers(candidate, request)，只有以下条件全部成立才能共享结果：

1. repository、lane、profile_digest 相同，candidate 的组件范围包含请求范围。
2. 候选版本是配置发布线上的已授权不可变提交；不能因普通请求拼接而扩大仓库、组件权限或生产授权。
3. exact 请求要求 source revision 完全相同；请求侧不能自行把 exact 放宽。
4. descendant 请求要求请求 revision 等于或为候选 revision 的祖先，且属于同一配置发布线。Git 关系来自可信镜像/提供方；网络失败、浅历史缺失、force-push 导致无法证明时，结果为 unknown，不能猜测覆盖。
5. 匹配的是实际构建的版本和组件证据，不能用运行时最新 branch HEAD 替代冻结的目标。

默认 exact。项目可显式启用 descendant，用于“部署至少达到 R 的发布线版本”；它只保证版本历史语义，不推断业务功能是否仍存在。revert 可以破坏功能而保留祖先关系，这个限制必须出现在配置说明和请求回显中。已知回滚/不兼容配置切换必须使用 exact 并设置发布线 barrier；不承诺自动识别未声明的语义回退。

M1 的 LineageResolver 使用管理员导入的不可变本地 Git bare mirror；只执行受控 Git 参数，不提供任意路径和网络 fetch 接口。覆盖证明绑定完整 SHA 和发布线配置版本，可缓存；Git 外部操作在事务外执行，提交决策时检查 lane/config 版本，否则重新决策。

先看当前已验收环境，再看活动 run，再看排队请求。历史 receipt 只能说明当时成功；新请求需要在当前 lane generation 下重新观察环境（建议证据最大年龄 30 秒，可配置）。当前状态不明时返回 accepted/waiting，而不是立即成功。请求过去的成功是历史事实，后续回滚不会篡改该记录。

### 7.4 调度与合并

一次 reconcile 的顺序固定：

1. 处理已持久化回执、过期和撤回；恢复异常执行；unknown lane 不发新部署。
2. 取得当前环境证据，满足已达到目标的请求。
3. 把可被活动 run 覆盖的请求挂接；有效优先级等于其未完成请求的最高优先级。urgent 已被覆盖时只提升当前 run，不取消重建。
4. 对尚未覆盖的请求，从这些请求已有的目标中选择候选，按 urgent、最早截止时间、创建顺序排序。队列不存在的“最新 HEAD”不得自动成为目标。
5. 将候选能覆盖的请求组成批次；不能覆盖的原样留队。不自动 cherry-pick 或 merge。普通请求可设 2 秒合并窗口，首个请求起最多等待 10 秒；urgent 绕过合并等待。
6. lane 空闲且全局 permit 可用则创建不可变 run；有普通活动 run 时，只有 urgent 且满足 §7.5 才抢占。

默认每 lane 最多一个活动 run（包含构建阶段），全局最多两个；permit 在数据库中由活动 run 计数，不只依靠内存 semaphore。M1 不做跨 lane 抢占，也不承诺 urgent 的绝对开始时延。对普通请求建议最大等待 30 分钟，达到截止时间返回 expired；持续紧急流量的饥饿是可见结果，不隐藏为永久等待。deadline 是请求自动结算的截止时间：已 expired 后的迟到发布证据仍可更新实际环境记录，但不把该请求改成成功。

同优先级新版本不抢占正在进行的普通执行，可替换尚未启动的兼容批次目标。紧急候选不覆盖当前 run 的所有未完成请求时，M1 不抢占当前 run；保留等待和 not_covering_all_waiters 原因，避免为了抢占而偷偷丢掉旧需求。后续可设计暂停/恢复策略，首版不做。

### 7.5 抢占与阶段交接

阶段分为 preparing、building、deploying、verifying。首版仅 preparing/building 可抢占；deploying/verifying 默认不可抢占，必须等结果或进入人工核对。执行器未显式声明并实现可停止能力时，整次运行按不可抢占处理。

抢占流程：

1. 在事务内确认候选覆盖全部旧 waiters、旧 run 为 normal 且处在可抢占阶段；冻结替代目标，创建 planned 新 run，旧 run 进入 cancel_requested，写取消 command 和事件。此时 lane 仍归旧 run，原请求仍等待。
2. 事务外请求取消；cancel API 返回成功只表示请求已受理。
3. 收到可信 stopped 确认，证明旧执行及其部署申请均已关闭；事务内将旧 run 标记 superseded、递增 lane generation、转移仍有效的请求和唯一活动指针，写启动新 run 的命令。
4. 原截止时间、重试预算、权限和订阅关系保留。重复替代只改变请求的直接归属，历史用事件保存；replacement 边只能指向创建序号更大的 run，不能成环。
5. 启动前再次检查请求是否仍有效、授权和配置是否仍匹配；全部已撤回、过期或满足时取消 planned 批次，不发送空执行。新 run 验收成功后逐请求检查 covers，满足的才成功；未覆盖者留队。

取消与完成竞争必须有确定次序：进入 deploying 前，执行器调用阶段准入；服务端在同一个 lane 写事务中检查 active_run、generation、cancel_requested，再决定许可。准入先提交则该 run 已不可抢占；取消先提交则拒绝旧 run 的部署准入。若取消期间旧 run 已取得真实发布成功证据，先用该证据满足请求，再重新判断是否还需要替代。

generation 只保护协调器决策和准入，不能撤回已发给外部系统的命令。安全性依赖受控执行器遵守准入协议：可抢占阶段无生产写入，获准后才开始部署；直接持有凭据、可绕过准入自行部署的脚本不具备安全抢占能力。M2 不满足此契约时必须 capabilities.preempt=false，不能靠取消 API 装作安全。

停止确认超时默认 60 秒后将 lane 标记 blocked、run 进入 unknown；不释放活动所有权、不自动启动替代任务。CLI 明确显示“停止尚未确认”。可恢复证据到达后继续交接。

### 7.6 请求与执行状态机

请求状态：

~~~text
accepted -> waiting -> satisfied
                    -> needs_attention -> waiting（显式 retry，目标不变）
accepted / waiting / needs_attention -> withdrawn | expired
~~~

请求不使用 superseded；它是一项仍待满足的需求。satisfied、withdrawn、expired 为请求终态。needs_attention 是有明确原因的非终态，不把构建被替代解释为需求失败。

执行状态：

~~~text
planned -> dispatching -> running -> succeeded | failed
                            |
                            +-> cancel_requested -> superseded | canceled | unknown
dispatching / running -> unknown -> running | succeeded | failed | superseded | canceled
~~~

phase 是 run 的独立字段；running/succeeded 等不是业务阶段的替代物。succeeded 要求成功 receipt 已持久化；failed 是已确认执行失败。unknown 是交付或外部状态不确定，不等同于失败，可由可信观察修正。未能找到运行实例并不自动等于它从未执行。

替代执行确定失败后，原请求不跟着成功；默认暂停本 lane 的自动尝试，相关请求进入 needs_attention，保留它们各自的原始目标。已确认停止且没有环境不确定性时释放执行 permit，但 lane 保留 failure_paused 标记；显式 retry 核对权限与目标后恢复，或当该 lane 上的所有 needs_attention 失败请求均已撤回/过期时自动解除，亦可由管理员调用 unpause 显式解除暂停；不由新请求到达自动清除。显式 retry 再选择一个能覆盖这些请求的目标，不自动降级上线旧版本。

自动重试默认 0 次，管理员最多可配置 2 次；仅用于已证实没有环境写入的 transient 错误。人工 retry 与自动计数分开，默认每个请求允许 3 次显式 retry，须有 retry 权限且未过截止时间；恢复前仍需核对实际环境。混合批次的自动重试受所有 waiters 最小剩余额度约束。替代转移不重置计数，superseded 也不消耗失败重试额度；额度耗尽由管理员显式追加并审计。unknown、部署失败和验收失败不自动重放。

一个请求撤回不影响其他等待者；最后一个等待者撤回后，只可停止处在可抢占阶段的执行。不可抢占阶段继续核对环境并记录结果，但不复活已经撤回/过期的请求。CLI wait 的本地超时不是发布取消或请求过期。

### 7.7 持久命令、回执和重启恢复

- 外部 dispatch/cancel/observe/verify 都由持久 commands 驱动；业务事务先提交 intent，执行器调用在事务外进行。
- 命令按 command_id、run_id、generation 幂等；受理返回、执行开始、执行停止和验收完成分别记录。
- 即使执行器 API 不支持幂等，重复恢复也不能盲发：dispatch 在途进程崩溃时进入 unknown，先通过关联标识查询；无法唯一定位则冻结 lane，管理员核对后操作。
- 回执校验执行器身份、run/generation、command/event ID、阶段和证据版本；相同回执重复为 no-op，矛盾终态为审计异常。旧 run 在被替代后的迟到成功不能覆盖新 lane 的环境状态。
- 启动先取得数据库独占锁；对所有 dispatching/running/cancel_requested/unknown 逐一 observe，完成所有权核对前不启动它们 lane 的新任务。
- drain 停止接纳新执行，已有不可抢占执行继续记录；进程退出不宣称外部已经停止。
- 管理员 resolve-unknown 必须提供可审计的观察证据和处置理由，动作限定为确认停止/接管已知执行/登记已验收状态。没有“忽略未知并直接解锁”按钮。

SQLite 事务保证本地状态原子性，不能包住 GitHub/SSH/K8s 的副作用；设计不声明分布式 exactly-once。

### 7.8 API 与 CLI 拟议契约

| API | 含义 |
| --- | --- |
| POST /v1/requests | 需 Idempotency-Key；202 表示请求已持久化，不代表已派发或已上线 |
| GET /v1/requests/{id} | 状态、原目标、当前 run、最终 receipt、等待/阻塞原因 |
| POST /v1/requests/{id}/withdraw | 幂等撤回自己的非终态请求，不直接取消共享 run |
| POST /v1/requests/{id}/retry | 操作需权限；仅 needs_attention，目标不可变，重新核对权限/截止时间 |
| POST /v1/requests/{id}/priority | 需 urgent 权限；禁止通过修改正文绕过幂等 |
| GET /v1/runs/{id} | 阶段、替代关系、原始执行器状态、脱敏日志链接和 receipt |
| GET /v1/lanes/{id} | 唯一活动 run、待处理数量、当前环境证据、blocked_reason |
| GET /v1/events?lane_id=&after=&limit= | 按持久 sequence 分页；客户端断开后可续读 |
| POST /v1/runs/{id}/phase-grants | 执行器专用；与取消事务竞争，准入 deploying 等不可抢占阶段 |
| POST /v1/executor-events | 执行器专用；验证身份与 fencing；M1 仅 ScriptedExecutor |
| POST /v1/lanes/{id}/resolve-unknown | 管理员提供证据后恢复，不接受无条件解锁 |
| POST /v1/lanes/{id}/unpause | 管理员显式清除 failure_paused 标记，保留其它 blocked_reason |
| GET /live、GET /ready | 存活、数据库及调度器准备状态 |

请求示例（版本仅为格式示例，不对应真实仓库提交）：

~~~json
{
  "lane_id": "myapp-test-app",
  "revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "components": ["app"],
  "coverage_mode": "descendant",
  "priority": "urgent",
  "deadline_at": "2026-10-16T12:00:00Z",
  "reason": "修复阻断用户发送消息的问题"
}
~~~

repository/profile/环境由 lane 配置派生，客户端不能替换。JSON 时间为 UTC，CLI 按调用者配置显示本地时间。成功结果必须含 satisfied_by_run、receipt_id、actual_revision、component_digests、observed_at 和 coverage_reason；共享执行时每个请求仍有自己的 request_id。

统一错误包含 code、message、retryable、request_id（若已创建）。主要错误：idempotency_conflict、target_not_allowed、lineage_unproven、forbidden、lane_blocked、not_retryable、deadline_elapsed。提交时遇到当前 lane 被阻塞仍可持久接纳请求并回显 blocked_reason；非法目标/无权限不创建请求。默认请求正文上限 16 KiB、reason 上限 2 KiB、每 lane 非终态请求上限 1000；幂等重试优先查已有请求，新增超限返回 429 和 Retry-After，不创建丢失归属的半请求。

CLI 通过服务端 API 提交和查询：submit、wait、inspect/list request/run/lane、logs run/request、events、system、health、withdraw、retry、priority、resolve-unknown、drain/resume、unpause；serve 用于启动服务。请求日志绑定所选 run，换 run 时重置游标，不合并不同执行的日志；原有 logs RUN_ID 调用仍兼容。完整用法见 [Agent 指南](agents.md)。M1 wait 使用有界轮询与退避，事件接口用于审计；不额外建设 SSE/WebSocket/回调通知系统。wait satisfied 返回 0；needs_attention/expired/withdrawn/客户端超时分别给不同退出码（2/3/4/124）。

### 7.9 Rust 模块与执行器边界

~~~text
src/
  main.rs                 # serve/CLI 入口
  domain/                 # Request/Run/Target、covers、状态转移、调度决定；纯函数
  scheduler.rs            # reconcile、优先级和抢占流程
  store/                  # SQLite 事务、migrations、命令和事件存储
  api/                    # Axum DTO、鉴权、HTTP 映射
  executor/               # 固定 trait、ScriptedExecutor；M2 GitHubActionsExecutor
  lineage.rs              # 可信 Git 关系证明
  config.rs               # TOML、lane/profile/auth 规则
  telemetry.rs            # tracing、低基数指标
migrations/
tests/scenarios/          # 并发、取消竞争、故障恢复
~~~

只保留一个实际需要的 Executor 边界，不建立动态插件系统。拟议操作为 dispatch、observe、cancel、verify；结果必须区分 KnownRunning、KnownStopped、VerifiedSuccess、KnownFailure、Unknown。capabilities 描述可恢复身份与可抢占阶段，不能用一个 cancelable=true 概括全部阶段。

ScriptedExecutor 是数据库可恢复的可控状态 fixture，不只是内存 mock：能够暂停确认、制造响应丢失、重排回执、记录物理启动次数和模拟环境状态。其测试证明调度协议，不构成真实部署安全证明。

M2 的 GitHub Actions 工作流通过显式请求输入传入 run_id/generation/correlation_id；不假设 workflow_dispatch 的每一种响应都能提供唯一执行身份，也不把取消接口返回当成停止证明。[工作流触发 API](https://docs.github.com/en/rest/actions/workflows#create-a-workflow-dispatch-event)、[工作流执行 API](https://docs.github.com/en/rest/actions/workflow-runs#cancel-a-workflow-run)。

GitHub run 关联需由受信任的 GitHub API 查询校验仓库、workflow 身份、受控 ref、关联 ID、源版本和唯一 run；回调参数不能自证身份。分发响应丢失先查询同 correlation_id，发现多个实例时不得任取一个继续部署。执行前准入只允许一个实例获取有效阶段许可，其他实例退出；绑定不明期间 lane 为 unknown。

M2 认证采用 GitHub App 查询/调度权限，加已验证 GitHub 身份的执行器回调（优先 OIDC；具体 claim/audience/重放校验需在 M2 单独评审）；未经身份校验的公网 webhook 不能变更状态。GitHub Actions 原始 run 可以 canceled，而请求经后继 receipt 满足，两个状态分别展示。

### 7.10 配置、鉴权与证据

配置由管理员维护的 TOML 加秘密文件/环境引用提供；运行中配置变更通过受控重载，活动 run 固定旧 profile 快照，新 profile_digest 不与旧请求合并。首版可在 drain 后重启完成重载，不开发热配置中心。

内部 HTTP API 放在 TLS 反向代理后；默认仅 loopback 监听。静态客户端令牌使用高熵秘密文件，服务端仅保存/比较摘要，按 client_id 配置 project/lane allowlist、urgent/admin 能力；测试 lane 的权限不自动扩展到同项目生产 lane，actor 从认证派生。CLI 不把 token 放命令行参数；日志不记录授权头。M1 不做用户账号系统。

receipt 的环境/组件/产物摘要必须由可信 verifier 读取实际目标得到，不能仅接受请求里的 revision 或 CI 成功通知。实际 SHA 不符、组件缺失、验收失败均不得满足请求。日志和产物保留在执行器/对象存储，SQLite 存结构化摘要与脱敏链接，避免把全量日志塞入数据库。

### 7.11 与现有系统的关系

调用方的发布入口可提交源 SHA 和目标 lane，保存 request_id，然后等待结果。独立项目不引入接入方的 Session/Task 数据表或内部工具接口。

独立发布仓库中的 GitHub Actions 负责可信的构建/发布步骤；shipbraid 负责统一决策和状态。现有备份、审批、镜像来源校验、环境 diff 和部署验收继续由受控 profile 执行，调度合并不等于授权扩大。M2 先接测试 lane，真实生产接入有单独的审批与资源变更范围。

## 8. Alternatives and Tradeoffs

| 选项 | 优点 | 当前不选的原因/保留方式 |
| --- | --- | --- |
| 仅 GitHub Actions concurrency | 配置少，能限制并发/取消部分任务 | 没有项目所需的请求覆盖、等待者转移与结果结算模型；作为执行器使用 |
| Temporal | 持久执行、消息和恢复基础完善 | 首版为少量固定状态、单实例；引入额外服务与工作流运维成本。后续需求扩大可重新评估 |
| Rust + PostgreSQL 多实例 | 行锁、多个协调器和独立数据库运维 | 当前没有 HA 要求；比单机 SQLite 更重，未来独立迁移而非首版双存储 |
| 在既有业务运行时内实现 | 可复用已有用户/任务界面 | 与独立项目目标冲突，发布调度不应绑定业务运行时 |
| Dagger/BuildKit | 构建编排和缓存有价值 | 不提供本项目的请求成功归属；可在执行器里使用 |

选择单实例 Rust + SQLite，是为了先把调度语义和外部交付边界做正确；性能收益来自少做重复任务，而不是换语言自动加速编译。

## 9. Risk Assessment

| 风险 | 影响 | 缓解与回退 |
| --- | --- | --- |
| 祖先关系被误当业务语义覆盖 | revert 后旧需求误判已满足 | exact 默认；descendant 显式 opt-in；发布线 barrier；不宣称语义分析 |
| 取消受理但执行未停 | 新旧部署互相覆盖 | cancel_requested 与 stopped 分开；阶段准入原子化；不明冻结 lane |
| 副作用已发生但回执丢失 | 重试重复上线 | 持久 intent + 唯一关联 + observe；unknown 不重放 |
| 控制器重启/磁盘丢失 | 归属丢失、旧执行游离 | 本地持久卷、FULL、在线备份；恢复旧数据库后先观察环境，不按旧队列直接重放 |
| 旧版本晚到/外部人工上线 | 新请求复用过期证据 | lane generation、实际产物观察、证据新鲜度；来源不明要求核对 |
| urgent 持续到达 | 普通请求饥饿 | 原截止时间不重置；等待年龄指标、expired、明确受影响请求 |
| 权限或配置在等待中变化 | 越权部署/合并 | 计划时和不可逆阶段前都重新核对授权，profile 快照不跨版本合并；撤权使请求 needs_attention |

不提供自动数据库业务回滚。部署失败后由 profile 和发布负责人执行既有恢复流程；shipbraid 保留事实，不把 rollback 解释为原请求成功。

## 10. Validation Strategy

M1 的发布账本、调度和恢复必须有以下可重复场景，测试使用隔离 Git DAG、临时 SQLite 和可控时钟，不触碰真实环境：

| 场景 | 必须看到的结果 |
| --- | --- |
| 100 个并发同目标请求；其中同一主体幂等重试 | 每个独立请求可查，幂等重试返回原 ID，物理执行计数为 1 |
| R1 是 R2 的祖先，同 lane/config，显式 descendant | 可以共享 R2；exact R1 不能共享 R2；分叉/不同配置/缺历史不能误覆盖 |
| urgent 已被 normal 活动执行覆盖 | 有效优先级提升，不新增物理执行 |
| normal 构建被覆盖它的 urgent 目标抢占 | 先 cancel_requested；确认 stopped 后仅一个新 run；旧请求等待，新 receipt 验收后共享成功 |
| cancel API 已受理但无停止确认 | 超时进入 unknown/blocked，不启动新环境写入 |
| 部署准入与取消同时到达 | 两种提交次序均只有一个合法结果；不得同时准入旧部署和新抢占 |
| 新执行失败/验收失败 | 旧请求没有误成功，预算与 needs_attention 可见，不自动回退旧版本 |
| 替代链 A -> B -> C，重复/迟到回执 | 所有仍有效请求直接等待 C，链无环，旧回执不覆盖新状态 |
| 每个事务/外部调用边界杀进程再启动 | 不丢请求、不盲发未知命令、不提前解锁；恢复后可继续核对 |
| wait 断线、单请求撤回、最后请求撤回、截止时间到达 | 共享执行和请求终态分别正确；不重置截止时间，不复活已撤回请求 |
| 历史发布成功后环境被改变，再提交相同目标 | 重新观察；不能凭旧 receipt 立即成功 |
| 未授权 urgent、回执伪造、ID 探测、日志秘密 | 403/404 按契约处理；不产生部署命令；报告无秘密值 |

纯领域属性测试验证覆盖自反性（相同已解析目标）、不兼容隔离、归属唯一性、终态单调性和替代无环；SQLite 并发/重启测试验证约束真的生效。Git ancestry 使用临时仓库验证，不只 mock 布尔值。

M2 增加真实测试工作流：dispatch 响应丢失、重复 run、取消受理但未停止、旧 run 晚到准入、产物身份不一致、实际环境验收失败。必须证明构建阶段无法绕过准入写生产，才能开启抢占能力。

拟议指标：request_wait_seconds、run_phase_seconds、preempt_stop_seconds、unknown_runs、lane_queue_depth、shared_requests_total、physical_runs_total；标签仅 lane/profile、phase、reason。unknown 出现、等待超期、停止确认超时必须可告警。

## 11. Rollout Plan

1. M1：仅 ScriptedExecutor，场景矩阵和 API/CLI 完整验收；没有真实环境写权限。
2. M2a：GitHub Actions 测试环境接入，先只排队/同目标复用，descendant 和抢占分别由配置显式开启。
3. M2b：通过取消竞争和恢复验证后开启测试抢占；沿用项目既有备份、diff 和验收。
4. 生产：单独确认 lane/profile、执行器权限、停止证明和回退程序，再接入生产请求。开启调度服务不代表允许自主上线。

每 lane 配置 enabled、allow_descendant、allow_preemption；初始 enabled=true、allow_descendant=false、allow_preemption=false。配置关闭只停止新派发，已有执行继续观察与结算。存量手动部署不自动伪造成 receipt；首次接管需验证当前环境并建立基线。

使用 SQLite 前向迁移与 schema_version；启动迁移在独占锁后执行。回退服务版本前检查 schema 兼容；备份采用 SQLite 在线备份机制，不在活动 WAL 状态下只复制 db 单文件。异常时先 drain/冻结新执行，核对活动部署，再切回原发布入口；两个入口不能同时成为同环境的写入方。

## 12. Open Questions

以下是接入阶段的配置问题，不阻塞 M1 调度内核设计；均不以未知值自动填生产默认。

- **Q-001**：第一个真实 lane 的组件原子边界是什么？Owner：项目所有者；M2 开始前确认。建议先接单一测试应用整体，不做多仓库原子发布。
- **Q-002**：哪些发布线允许 descendant，如何声明业务回退 barrier？Owner：发布负责人；M2 接入评审确认。默认 exact，不代替用户作语义兼容判断。
- **Q-003**：GitHub Actions 的可信回调身份、Runner 网络和阶段准入如何接入？Owner：M2 执行器实现者；生产开启前必须通过真实取消/身份验证，不能以模拟测试替代。

已决定：Rust；独立项目；首版单实例 SQLite；HTTP + CLI；两种优先级；M1 不提供生产执行；未知状态保留所有权；后继失败不自动降级旧版本。

## 13. Review Feedback Tracking

### 13.1 Review verdict

PASS（MEDIUM）。同一代理自审，结论仅表示 M1 技术设计满足当前清单，不声称独立评审或用户批准。完整真实发布接入必须按 M2 单独验收。

### 13.2 Overdesign findings and simplification

- **OD-001 / MAJOR，已处理**：同时建设通用执行器插件、Web UI、双数据库和 GitOps 平台会偏离调度核心。简化为一个 Rust package、一个 Executor trait、SQLite、HTTP/CLI。
- **OD-002 / MINOR，已处理**：完整 event sourcing、消息总线和多种通知通道不服务于首版验收。关系表为事实源，追加 events 审计，CLI 轮询等待。

### 13.3 One-iteration feasibility and split suggestion

完整范围一次迭代不可交付，已按 SPLIT_REQUIRED 的方向收敛：M1 是可独立验收的调度内核，两条核心规则均可在可控执行器中真实经历；M2 才承诺外部 CI 与真实环境交付。M1 不把 GitHub OAuth/OIDC、生产凭据和物理部署停止证明混入内核实施完成条件。

### 13.4 Missing/unreasonable design considerations

- **DG-001 / BLOCKER，已处理**：原始覆盖描述容易将 Git 祖先关系等同功能包含。§7.3 明确 exact 默认和 descendant 的版本历史语义，回滚隔离。
- **DG-002 / BLOCKER，已处理**：外部取消响应不是停止证明。§7.5/7.7 加入取消受理、停止确认、unknown 和阶段准入竞争；未停止不移交 lane。
- **DG-003 / MAJOR，已处理**：后继失败、等待端断线与最后一个请求撤回必须有明确归属。§7.6 固定 needs_attention、预算和不自动回退策略。
- **DG-004 / MAJOR，已处理**：历史成功不能证明当前环境仍满足新请求。§7.3/7.10 规定新鲜实际环境证据和产物校验。
- **DG-005 / MAJOR，已处理**：DB fencing 不能制止持有长期凭据的外部脚本。§7.5/7.9 限定执行器能力与信任边界，不合约执行器关闭抢占。

### 13.5 Clarifying questions

仅保留 §12 的 Q-001～Q-003，已指定 owner、决策时间和安全默认；没有必须先问用户才能完成 M1 设计的未决项。

### 13.6 Spec update actions

- **A-001 / P0 / DONE**：设计 §4 和执行计划 M1/M2 边界一致，M2 不计入 M1 完成。
- **A-002 / P0 / DONE**：FR-003/FR-006 的覆盖与抢占语义分别映射 T-002/T-004，并有失败场景验收。
- **A-003 / P0 / DONE**：FR-009/FR-011 的未知投递和恢复映射 T-003/T-005，禁止仅靠心跳超时解锁。
- **A-004 / P1 / DONE**：FR-007/FR-008 的共享完成、失败预算与撤回映射 T-006。

### 13.7 Required actions before next review

- [x] 三项 mandatory checks 已完成，初始范围按 M1/M2 切分。
- [x] 所有 BLOCKER/MAJOR 均有具体设计修订与实施任务。
- [ ] M2 开始前补充真实 GitHub 身份/关联/停止证明与测试环境 profile，并重新评审。

## 14. Definition of Design Complete

- [x] 模板各节完整；需求有 FR/NFR ID，可追踪到执行任务。
- [x] M1 最新技术自审 PASS；明确其非独立评审/用户批准。
- [x] 覆盖、合并、抢占、共享完成、失败、撤回和 unknown 的语义已明确。
- [x] 核心风险、恢复和回退路径已记录。
- [x] 场景验收可测量；M1 与真实部署验收没有混淆。
- [x] 开放问题已指定 owner、时间和安全默认。

参考依据：本设计的业务策略是本项目决策；第三方文档仅支持框架/外部接口事实。[Axum](https://docs.rs/axum/latest/axum/)、[SQLx](https://docs.rs/sqlx/latest/sqlx/)、[GitHub concurrency](https://docs.github.com/en/actions/concepts/workflows-and-actions/concurrency)、[Temporal 消息模型](https://docs.temporal.io/encyclopedia/workflow-message-passing)。

## 15. 2026-10-16 实施增量评审

用户明确要求开始实现，并新增 React/TypeScript/Tailwind Dashboard。范围按 [实施契约](implementation-contract.md) 锁定。技术自审结论 PASS：保留单服务/SQLite，不加入账号系统、WebSocket 服务或编译引擎；Dashboard 仅消费有鉴权的列表、详情、游标日志接口。M1 仍限定 ScriptedExecutor，真实 GitHub Actions 为 M2。新增前端工作可独立并行；领域状态和前后端字段先固定契约。不是独立评审或生产接入授权。
