# shipbraid 执行计划

## 1. Plan Header

- Feature name：M1 发布请求协调与安全抢占内核。
- Linked design doc：[design.md](design.md)。
- Linked review report：[设计 §13](design.md#13-review-feedback-tracking)。
- Plan owner：项目所有者；Codex 集成负责人。
- Contributors：Codex 主代理负责集成与 CLI；gpt-6-luna/max 三个子代理分别负责领域内核、持久化/API/执行器、Dashboard。
- Last updated：2026-10-16。
- Plan status：ACTIVE（T-001～T-008 核心实现已落地但验收仍有未闭项；T-009 Dashboard 已完成）。
- Review gate status：设计评审 PASS；Dashboard 增量独立 review 的三项修正均已解决，ship 结论限于该 review 范围；不代表 M1 验收完成或生产接入批准。

新增独立迭代：[同团队 Agent 发布闭环](features/iter-20261017-agent-workflow/execution-plan.md)（2026-10-17，本轮 DONE 不关闭本计划剩余项）。

## 2. Scope Lock

允许范围：本仓库 Cargo 工程、src、migrations、tests、配置样例、文档及本项目自身质量检查。一个 package、一个服务、SQLite、HTTP/CLI、受控本地 Git 关系证明、ScriptedExecutor 和 React/TypeScript/Tailwind 只读 Dashboard。

排除范围：接入项目代码/数据库、真实 GitHub Actions 调度、真实生产部署、集群/SSH/镜像仓库凭据配置、多实例 HA、通用 DAG、自动 rollback 和多个执行器同时接入。

范围变更规则：改变 coverage、lane 原子边界、状态终态、停止证明、权限或失败重试策略，先同步设计和场景矩阵并重新过评审；不在实现中默默增加“更智能”的默认行为。

评审通过是进入实现的技术前提。用户于 2026-10-16 明确启动实施；以运行证据更新状态。新增范围见 design.md §15 与 implementation-contract.md。

## 3. Task Breakdown

### Task T-001 — 初始化 Rust 工程与持久运行基础

- Status：IN_PROGRESS；Owner：Codex 主代理 / backend；Priority：P0。
- Linked requirements：NFR-002、NFR-003、NFR-005、FR-012。
- Objective：建立可测试的单 package 项目与配置/数据库启动边界。
- Task details：固定 Rust 工具链/MSRV 与 Cargo.lock；接入 Tokio、Axum、SQLx SQLite、Serde、Clap、tracing；建立服务/CLI 分发、TOML 配置、规范化数据目录锁、迁移与健康检查。
- Implementation requirements：WAL/FULL/foreign_keys 检查实际生效；验证所锁定 SQLite 版本的官方已知修复；只允许本地数据库卷；数据库锁先于迁移；第二进程明确拒绝启动。默认监听 loopback，配置 secret 引用而非秘密值。
- Allowed modification range：Cargo.toml、Cargo.lock、rust-toolchain.toml、src/main.rs、src/config.rs、src/store、migrations、tests/startup、配置样例、README。
- Dependencies：无。
- Risks/notes：单实例是明确限制；不为将来 HA 添加备用数据库实现。
- Evidence links：[配置验证](../src/config.rs)、[SQLite store/restart test](../src/store.rs)、[数据库锁与恢复](../src/service.rs)、[事务与 pragma 不变量](../tests/ledger_invariants.rs)、[已记录场景](acceptance/scenarios.json)。
- Completion conditions：
  - [x] Store 实际启用 WAL、synchronous=FULL、foreign_keys；事务失败时实体和审计回滚，见 [ledger_invariants.rs](../tests/ledger_invariants.rs)。
  - [x] 持久化重启后保留事件序号和幂等记录，见 [store.rs](../src/store.rs)；规范路径与符号链接共享数据库锁，见 [service.rs](../src/service.rs)。
  - [x] 进程独占数据库所有权列入 [场景记录](acceptance/scenarios.json)；实现锁在 [service.rs](../src/service.rs)。
  - [ ] 补齐损坏数据库、迁移失败、配置错误、目录权限和真实 `/live`/`/ready` 启动矩阵；当前配置单测只覆盖容量/auth 基本校验，见 [config.rs](../src/config.rs)。
  - [ ] 补齐配置/日志/测试产物的秘密泄漏扫描证据。
  - Remaining acceptance：启动损坏/权限测试与对应报告仍未记录。

### Task T-002 — 目标、覆盖与纯调度决策

- Status：IN_PROGRESS；Owner：domain（gpt-6-luna/max）；Priority：P0。
- Linked requirements：FR-003、FR-004、FR-005、NFR-003。
- Objective：在没有执行器副作用时证明哪些请求可共享。
- Task details：实现 Target、CoverageMode、Priority、coverage_reason、LineageResolver；选择实际存在候选版本，处理组件/配置/lane/发布线边界；普通合并窗口与 urgent 绕过；明确同优先级不抢占。
- Implementation requirements：exact 默认；descendant 必须项目开启且请求允许；Git 关系仅在受控 bare mirror 上查询；不在 DB 写事务执行 Git I/O；验证后做 lane/config 版本检查。不能覆盖所有旧 waiters 的紧急候选不得抢占。
- Allowed modification range：src/domain、src/lineage.rs、tests/coverage、tests/scheduler_decisions。
- Dependencies：T-001；领域规则可独立先写，但不提前接生产。
- Risks/notes：ancestor 只保证版本历史；不建设 revert 语义分析器或自动合并器。
- Evidence links：[覆盖与候选选择测试](../src/engine.rs)、[临时 bare mirror 证明](../src/lineage.rs)、[未知仓库 fail closed](../src/lineage.rs)。
- Completion conditions：
  - [x] 临时 bare mirror 验证可达关系和 descendant 方向，未知仓库 fail closed；测试覆盖 exact 与 descendant 的区分，见 [lineage.rs](../src/lineage.rs) 和 [engine.rs](../src/engine.rs)。
  - [x] 覆盖要求同 lane/profile/components/mode 与 Git proof，见 [engine.rs](../src/engine.rs)。
  - [x] 已存在覆盖目标的候选批次与期限排序、urgent 已覆盖时复用并提升优先级有确定性测试/场景记录，见 [engine.rs](../src/engine.rs)、[engine.rs](../src/engine.rs) 和 [scenarios.json](acceptance/scenarios.json)。
  - [ ] 补齐最大等待公平性与覆盖/回滚 barrier 的端到端验收；当前候选排序测试不证明服务对最大等待时间提供公平性保证。
  - Remaining acceptance：max-wait fairness 与 barrier API 未验收。

### Task T-003 — 请求账本、事务与持久命令

- Status：IN_PROGRESS；Owner：backend（gpt-6-luna/max）；Priority：P0。
- Linked requirements：FR-001、FR-002、FR-009、FR-010、NFR-002、NFR-005。
- Objective：接纳并发调用并建立崩溃后仍可恢复的事实源。
- Task details：实现六张主表及索引/外键；短写事务；幂等正文摘要；同事务创建请求、归属、command、event；支持按 sequence 查询审计事件。
- Implementation requirements：事务内检查 lane/config 版本；同主体同 key 同正文复用 ID，不同正文 409；客户端看到 202 前提交已完成。任何执行器调用都不放进事务。全局 permit 来自 DB 活动状态，不能在重启后清零丢失。
- Allowed modification range：src/store、migrations、src/domain 持久 DTO、tests/store、tests/idempotency。
- Dependencies：T-001、T-002。
- Risks/notes：使用追加审计，不把系统扩展成 event-sourcing 框架。
- Evidence links：[ledger restart/idempotency test](../src/store.rs)、[transaction fault invariant](../tests/ledger_invariants.rs)、[100-concurrent scenario](acceptance/scenarios.json)。
- Completion conditions：
  - [x] 100 并发同目标调用共享一个 verified run，且幂等 replay/conflict 行为正确，见 [场景记录](acceptance/scenarios.json)。
  - [x] 延迟外键失败证明实体和追加审计事件同事务回滚，见 [ledger_invariants.rs](../tests/ledger_invariants.rs)；重启后事件序号和幂等记录保留，见 [store.rs](../src/store.rs)。
  - [ ] 覆盖每个写入点的故障注入，以及 API 提交完成到响应返回之间进程退出后的重试行为。
  - Remaining acceptance：全写点注入与提交/响应崩溃窗口仍待验收。

### Task T-004 — 执行器契约、调度循环与安全抢占

- Status：IN_PROGRESS；Owner：domain + backend（gpt-6-luna/max）；Priority：P0。
- Linked requirements：FR-004、FR-005、FR-006、FR-011、NFR-002。
- Objective：完整跑通用户提出的“共享执行”和“紧急替代普通执行”。
- Task details：固定 Executor trait；实现有持久状态的 ScriptedExecutor；调度 tick/通知唤醒；抢占计划、取消 command、停止确认、请求转移、阶段准入与 generation 检查。
- Implementation requirements：preparing/building 可抢占，deploying/verifying 不抢占；停止确认前旧 run 保持 lane 所有权；cancel 与 phase-grant 使用同一事务边界竞争；新目标必须覆盖全部旧未完成请求，替代无环。
- Allowed modification range：src/executor、src/scheduler.rs、src/domain 状态转换、src/store 对应事务、tests/scenarios/preemption。
- Dependencies：T-002、T-003。
- Risks/notes：ScriptedExecutor 的物理计数是测试执行次数，不能写成真实部署证明；禁止加入任意 shell runner。
- Evidence links：[urgent preemption and handoff](../src/engine.rs)、[phase-gate/drain/deadline checks](../src/engine.rs)、[service command processing](../src/service.rs)、[scenario record](acceptance/scenarios.json)。
- Completion conditions：
  - [x] 已被 normal run 覆盖的 urgent 请求复用当前 run 并提升有效优先级，见 [engine.rs](../src/engine.rs) 与 [scenarios.json](acceptance/scenarios.json)。
  - [x] 停止确认前旧 run 保留 lane 所有权；确认后转移 generation 与有效 waiter，旧 run 标记 superseded，见 [engine.rs](../src/engine.rs)。
  - [x] drain、禁用抢占或过期截止时间时不启动 handoff；in-flight phase gate 优先，见 [engine.rs](../src/engine.rs) 和 [engine.rs](../src/engine.rs)。
  - [ ] 补齐 cancel 与准入并发的两种提交顺序、A→B→C 连续替代及单 lane/global 并发上限的完整确定性验收。
  - Remaining acceptance：完整 phase/cancel 竞争与替代链矩阵尚未完成。

### Task T-005 — 不明投递、重启恢复与有证据的处置

- Status：IN_PROGRESS；Owner：domain + backend（gpt-6-luna/max）；Priority：P0。
- Linked requirements：FR-006、FR-009、FR-011、NFR-002、NFR-005。
- Objective：服务重启或外部响应丢失时不重复上线、不提前解锁。
- Task details：command 发送/确认状态；回执去重、乱序处理；启动 observe；unknown/blocked；drain；管理员证据处置；停止确认超时与已知外部身份恢复。
- Implementation requirements：没有外部唯一身份的 dispatch unknown 不重发；超时/失联不作为已停止证明；迟到可信证据可修正 unknown，但不能推翻已被新 generation 接管的 lane。恢复旧数据库后先观察环境。
- Allowed modification range：src/scheduler.rs、src/store、src/executor、src/domain、tests/scenarios/recovery。
- Dependencies：T-003、T-004。
- Risks/notes：不能通过管理员“确认风险”跳过物理所有权核对；缺证据就继续阻塞。
- Evidence links：[unknown observation recovery test](../src/service.rs)、[unknown ownership transition](../src/engine.rs)、[restart/unknown scenarios](acceptance/scenarios.json)。
- Completion conditions：
  - [x] 已记录 SIGKILL/restart 后观察 durable scripted execution、不新建替代 run；见 [scenarios.json](acceptance/scenarios.json)。
  - [x] 确认响应丢失时 lane 保持 blocked，只有独立 observation 才恢复 running，见 [service.rs](../src/service.rs) 和 [engine.rs](../src/engine.rs)。
  - [x] drain/resume 与待派发命令、重复 started event 的行为有测试，见 [service.rs](../src/service.rs) 和 [service.rs](../src/service.rs)。
  - [ ] 补齐 dispatch/ack/cancel/verify 各崩溃窗口、停止确认超时和重复/旧 generation 回执的完整矩阵，并核对每条处置审计原因。
  - Remaining acceptance：完整 crash/cancel 矩阵尚未跑完。

### Task T-006 — 发布验收、请求结算、失败与撤回

- Status：IN_PROGRESS；Owner：domain（gpt-6-luna/max）；Priority：P0。
- Linked requirements：FR-003、FR-007、FR-008、NFR-002。
- Objective：对每项上线需求给出可解释、可追踪且不会误报成功的结果。
- Task details：receipt 冻结；逐请求 covers 检查与共享完成；当前环境证据新鲜度；failure_paused、needs_attention、显式 retry 与独立的自动/人工持久计数；请求撤回/过期和终态保护。
- Implementation requirements：CI 绿色/命令 0 退出不能代替环境验收；自动预算默认 0、人工 retry 默认最多 3 次，混合批次采用最小剩余自动预算；superseded 不消耗失败重试预算；新请求不能清除失败冻结；重试不改变目标、不重置截止时间，不自动回退旧版本。
- Allowed modification range：src/domain、src/store、src/scheduler.rs、src/executor/verifier 边界、tests/scenarios/settlement。
- Dependencies：T-003、T-004、T-005。
- Risks/notes：历史成功请求不因后续回滚被改写；新请求也不能直接复用过时成功状态。
- Evidence links：[receipt settlement](../src/engine.rs)、[failure and retry behavior](../src/engine.rs)、[request/run relationship tests](../web/src/api.test.ts)、[scenario record](acceptance/scenarios.json)。
- Completion conditions：
  - [x] 被替代 run 保留 superseded；receipt 满足匹配的 covered requests，截止时间到达的请求保持 terminal，见 [engine.rs](../src/engine.rs)。
  - [x] run failure 不会把旧请求标为 satisfied，也不会自动重试，见 [engine.rs](../src/engine.rs)。
  - [x] Dashboard/API relationship tests retain the receipt-producing `satisfied_by_run` link and original run association, 见 [api.test.ts](../web/src/api.test.ts)。
  - [ ] 加入当前环境证据复用；现有行为保守地新建执行，不凭历史 receipt 直接满足请求。
  - [ ] 完成自动 retry policy 与 admin allowance；验证共享失败预算、显式 retry、撤回/截止/迟到 receipt 全组合。
  - Remaining acceptance：current-environment receipt reuse、自动 retry/admin allowance 与完整终态组合尚未验收。

### Task T-007 — HTTP/CLI、权限与可观测性

- Status：IN_PROGRESS；Owner：backend + Codex 主代理；Priority：P1。
- Linked requirements：FR-001、FR-002、FR-005、FR-008、FR-010、FR-012、NFR-003、NFR-004。
- Objective：让多个调用方使用同一个协调器提交和等待发布需求。
- Task details：实现设计 §7.8 的端点/CLI 与错误码；认证 actor；project/lane 权限；urgent/admin 能力；query/wait；事件分页；关联日志和低基数指标。
- Implementation requirements：请求正文不接收任意脚本/秘密/环境地址；只有受信执行器能提交 phase-grant 和结果；用户撤回请求不直接取消共享 run。wait 本地超时与服务端请求过期有不同状态和退出码。
- Allowed modification range：src/api、src/main.rs、src/config.rs、src/telemetry.rs、tests/api、tests/cli、docs 契约与配置示例。
- Dependencies：T-001～T-006。
- Risks/notes：M1 仅静态主体授权，不实现账号/组织/RBAC 产品；测试环境权限不隐式覆盖生产 lane。
- Evidence links：[API actor and lane authorization test](../src/api.rs)、[CLI URL/route guards](../src/main.rs)、[scenario record](acceptance/scenarios.json)。
- Completion conditions：
  - [x] API bearer 身份、lane scope 和 urgent 权限有针对性测试；场景记录覆盖未授权 urgent、跨 lane 与未认证读取，见 [api.rs](../src/api.rs) 和 [scenarios.json](acceptance/scenarios.json)。
  - [x] CLI route identifier 与嵌入式 URL credentials 有拒绝测试，见 [main.rs](../src/main.rs)；场景记录确认本地 wait timeout 不撤回请求。
  - [ ] 补齐两个独立 CLI 进程共享同一 run、伪造 executor event/任意命令输入的端到端拒绝证据。
  - [ ] 提供 max-wait fairness 与 rollback barrier API，并导出各阶段耗时指标；当前没有对应完整验收证据。
  - [ ] 补齐日志秘密过滤的扫描和报告证据。
  - Remaining acceptance：max-wait fairness、rollback barrier API、phase metrics 与独立 CLI 端到端场景仍未验收。

### Task T-008 — 完整场景、负载和运维验收

- Status：IN_PROGRESS；Owner：Codex 主代理；Priority：P1。
- Linked requirements：FR-001～FR-012、NFR-001～NFR-005。
- Objective：证明交付的 M1 是可恢复调度内核，并准确说明它尚未接真实生产。
- Task details：跑通设计 §10 全场景；属性测试和数据库并发；独立进程故障注入；负载采样；在线备份/恢复；drain 与返回旧发布入口演练；整理复现步骤和本项目质量检查。
- Implementation requirements：记录运行提交、硬件/磁盘参数、SQLite 版本、命令及结果；不把单元 mock 通过当作真实 GitHub/生产验收；仅执行本项目相关检查，不构建下游应用的发布入口。
- Allowed modification range：tests/scenarios、tests/load、配置/运行示例、docs/acceptance、本项目 .github/workflows 质量检查。
- Dependencies：T-001～T-007。
- Risks/notes：性能目标未达需如实记录并评审原因；不能省略失败注入以换取绿色报告。
- Evidence links：[M1 scenario summary](acceptance/scenarios.json)、[aborted baseline](acceptance/baseline.json)、[aborted optimized baseline](acceptance/load-optimized.json)、[implementation and acceptance report](acceptance/m1.md)、[current load measurement](acceptance/load-delta.json)。
- Completion conditions：
  - [x] `scenarios.json` 已记录 10 项 ScriptedExecutor 检查，包括 100 并发共享、抢占与 unknown 恢复、drain/backup；这不是完整的设计 §10 矩阵。
  - [ ] 完成设计 §10 全场景可重放证据，特别是事务/执行边界 crash、cancel/phase-grant 竞争、current-environment receipt、自动 retry 和安全权限矩阵。
  - [x] `cargo fmt`、`clippy`、`test`、debug/release build、真实子进程场景，以及前端 typecheck/test/build 均通过，命令与结果见 [m1.md](acceptance/m1.md)；启动损坏/权限矩阵仍未覆盖。
  - [ ] 达到 NFR-001：50 requests/s 持续 60s 且 p95 < 250ms，或经评审修改目标。[baseline](acceptance/baseline.json) 与 [load-optimized](acceptance/load-optimized.json) 因 backlog 中止，不是达标证据；最终增量序列化版本的完整测量见 [load-delta.json](acceptance/load-delta.json)：3000 个 202，总耗时 162.323s、p95 14265.768ms（含 Git 校验），未达目标。已解析版本的纯入账性能尚未单列测量，详见 [m1.md](acceptance/m1.md)。
  - [x] README 准确说明 ScriptedExecutor、单实例限制和 M2/真实部署边界，见 [README](../README.md)。
  - Remaining acceptance：完整异常矩阵、启动权限/损坏测试和 NFR-001 性能测量未闭合。

### Task T-009 — 只读 Dashboard

- Status：DONE；Owner：dashboard（gpt-6-luna/max）；Priority：P1。
- Linked requirements：FR-010、FR-013、NFR-003。
- Objective：直接查看真实服务中的任务状态、替代关系和日志。
- Allowed modification range：web/、PRODUCT.md、DESIGN.md、.impeccable/ 前端设计记录。
- Dependencies：T-007 契约；可并行开发。
- Completion conditions：
  - [x] React + TypeScript + Tailwind 的 typecheck、build 通过；7 个前端测试通过，见 [web/package.json](../web/package.json) 和 [api.test.ts](../web/src/api.test.ts)。
  - [x] 已对真实 API 浏览器验收：lane/request/run 列表与详情、阶段时间线、游标日志、空态和错误态可用；M1 仅只读，见 [App.tsx](../web/src/App.tsx) 与 [api.ts](../web/src/api.ts)。
  - [x] bearer token 仅保存在页面内存，页面可见时轮询；桌面 1440px 与移动 390px 本地验收截图：`.impeccable/review/desktop.png`、`.impeccable/review/mobile.png`（忽略提交）。
  - Independent review：三项修正均已解决；结论为 ship，限于该 Dashboard fix-list 范围。
  - Evidence links：[浏览器验收说明](acceptance/m1.md)、[frontend tests](../web/src/api.test.ts)、[dashboard source](../web/src/App.tsx)。

## 4. TODO Board

### TODO

- [ ] T-001：startup 的损坏数据库、迁移失败、目录权限和配置错误测试。
- [ ] T-002/T-007：最大等待公平性与 rollback barrier API 的实现验收。
- [ ] T-003：逐写点故障注入及 commit 成功到 HTTP response 前进程退出的验证。
- [ ] T-004/T-005：完整 cancel/crash/phase-grant 竞争矩阵、A→B→C 替代和旧 generation 回执验证。
- [ ] T-006：current-environment receipt reuse、自动 retry policy/admin allowance，以及撤回/截止/迟到回执组合。
- [ ] T-007：两个独立 CLI 进程验收、秘密扫描和 exported phase metrics。
- [ ] T-008：设计 §10 全矩阵、启动权限/损坏测试和 NFR-001 性能报告。

### IN_PROGRESS

T-001～T-008 核心实现已落地，剩余验收见上方具体条目；T-009 Dashboard 已 DONE。计划继续保持 ACTIVE。

### BLOCKED

无 M1 外部阻塞。M2 的环境和可信执行器问题见设计 Q-001～Q-003，不计入 M1 TODO。

### DONE

设计交付已完成，证据为 [design.md](design.md) 和本计划。T-009 只读 Dashboard 已通过实现、测试、真实 API 桌面/移动浏览器验收及限定范围的独立 review；T-001～T-008 仍有验收项，不能据此关闭 M1。

## 5. Milestones

日期不在未确认实施资源时虚构；以下为依赖顺序与退出条件。

| 里程碑 | 必需任务 | 可见价值 / 退出条件 |
| --- | --- | --- |
| M1-A：可持久提交并共享执行 | T-001～T-003 | 目标/覆盖规则明确，100 并发请求可复用同一执行计划，事务/幂等测试通过 |
| M1-B：可安全抢占、恢复、结算 | T-004～T-006 | 两条用户核心场景和 unknown/崩溃/后继失败场景通过 |
| M1-C：可由调用者验收的调度内核 | T-007～T-008 | 独立 CLI/API 闭环、权限、性能和恢复报告完成；执行器仍是 ScriptedExecutor |

### 后续 M2，不在本次 M1 完成条件中

开始前：解决设计 Q-001～Q-003，提交新的执行器设计和评审。

- 确认独立 GitHub 发布仓库、受控 workflow/ref、完整源 SHA 和 artifact provenance。
- 使用 GitHub App 和可信回调身份；实现持久关联、重复 run 防护、查询与停止确认。
- 将部署准入接入受控 workflow，验证旧实例不会绕过授权写环境；不能验证时禁用抢占。
- 在真实测试 lane 验收 dispatch 响应丢失、取消与部署竞争、产物错配和未知恢复。
- 生产部署作为后续明确授权动作，保留备份、diff、来源校验及发布后检查；不与创建仓库或部署调度服务捆绑。

M2 退出条件：用户的核心场景在真实测试工作流中成立；M1 场景证据不可替代它。首版不同时接 Dagger、Argo CD 或多个 CI 供应商。

## 6. Feature-Level Completion Conditions

- [ ] T-001～T-009 全部 DONE，每项有提交/测试/验收证据。
- [ ] 核心两条规则及完整异常场景满足设计 §10。
- [ ] 不存在未解决 P0/P1 实施阻塞；scope lock 没有被暗中扩张。
- [ ] 技术评审仍为 PASS；实际交付状态与文档一致。
- [ ] M1 仅对可控执行器声明完成；不宣称已经生产可用或接通 GitHub Actions。
- [ ] 备份、恢复、drain、失败暂停与 unknown 处置有可执行说明。

## 7. Plan Update Rules

状态只使用 TODO、IN_PROGRESS、BLOCKED、DONE；实施开始后及时更新 owner、依赖、证据与完成条件，TODO Board 同步修改。

增加任务时写明来源需求和范围影响；遇到越界先修订设计，不以“顺手”增加真实生产执行、额外数据库或危险 Web 运维操作。只有全部必要任务验收后才把计划改为 DONE。M2 必须有单独计划，不能在 M1 收尾时悄悄接入。
