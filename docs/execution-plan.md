# auto-deliver 执行计划

## 1. Plan Header

- Feature name：M1 发布请求协调与安全抢占内核。
- Linked design doc：[design.md](design.md)。
- Linked review report：[设计 §13](design.md#13-review-feedback-tracking)。
- Plan owner：项目所有者；实现负责人待指定。
- Contributors：当前设计由 Codex 编写并按清单自审，没有启动实现子任务。
- Last updated：2026-10-16。
- Plan status：DRAFT（本轮交付设计，尚未开始实现）。
- Review gate status：PASS，仅针对 M1；不是生产接入批准。

## 2. Scope Lock

允许范围：本仓库 Cargo 工程、src、migrations、tests、配置样例、文档及本项目自身质量检查。一个 package、一个服务、SQLite、HTTP/CLI、受控本地 Git 关系证明和 ScriptedExecutor。

排除范围：AutoTask 代码/数据库、真实 GitHub Actions 调度、真实生产部署、集群/SSH/Harbor 凭据配置、Web UI、多实例 HA、通用 DAG、自动 rollback 和多个执行器同时接入。

范围变更规则：改变 coverage、lane 原子边界、状态终态、停止证明、权限或失败重试策略，先同步设计和场景矩阵并重新过评审；不在实现中默默增加“更智能”的默认行为。

评审通过是进入实现的技术前提。M1 是否启动由后续实施指令决定，本计划保持 DRAFT；不把设计文件交付标记成运行时功能完成。

## 3. Task Breakdown

### Task T-001 — 初始化 Rust 工程与持久运行基础

- Status：TODO；Owner：实施负责人待指定；Priority：P0。
- Linked requirements：NFR-002、NFR-003、NFR-005、FR-012。
- Objective：建立可测试的单 package 项目与配置/数据库启动边界。
- Task details：固定 Rust 工具链/MSRV 与 Cargo.lock；接入 Tokio、Axum、SQLx SQLite、Serde、Clap、tracing；建立服务/CLI 分发、TOML 配置、规范化数据目录锁、迁移与健康检查。
- Implementation requirements：WAL/FULL/foreign_keys 检查实际生效；验证所锁定 SQLite 版本的官方已知修复；只允许本地数据库卷；数据库锁先于迁移；第二进程明确拒绝启动。默认监听 loopback，配置 secret 引用而非秘密值。
- Allowed modification range：Cargo.toml、Cargo.lock、rust-toolchain.toml、src/main.rs、src/config.rs、src/store、migrations、tests/startup、配置样例、README。
- Dependencies：无。
- Risks/notes：单实例是明确限制；不为将来 HA 添加备用数据库实现。
- Evidence links：待实现后记录具体测试和提交；当前无实现证据。
- Completion conditions：
  - [ ] serve 能启动并区分 /live 与 /ready；损坏数据库/错误配置返回明确错误。
  - [ ] 真实数据库 pragma、第二进程排斥、迁移失败与数据目录权限测试通过。
  - [ ] 不在配置、日志、测试产物中泄露秘密值。

### Task T-002 — 目标、覆盖与纯调度决策

- Status：TODO；Owner：实施负责人待指定；Priority：P0。
- Linked requirements：FR-003、FR-004、FR-005、NFR-003。
- Objective：在没有执行器副作用时证明哪些请求可共享。
- Task details：实现 Target、CoverageMode、Priority、coverage_reason、LineageResolver；选择实际存在候选版本，处理组件/配置/lane/发布线边界；普通合并窗口与 urgent 绕过；明确同优先级不抢占。
- Implementation requirements：exact 默认；descendant 必须项目开启且请求允许；Git 关系仅在受控 bare mirror 上查询；不在 DB 写事务执行 Git I/O；验证后做 lane/config 版本检查。不能覆盖所有旧 waiters 的紧急候选不得抢占。
- Allowed modification range：src/domain、src/lineage.rs、tests/coverage、tests/scheduler_decisions。
- Dependencies：T-001；领域规则可独立先写，但不提前接生产。
- Risks/notes：ancestor 只保证版本历史；不建设 revert 语义分析器或自动合并器。
- Evidence links：待实现。
- Completion conditions：
  - [ ] 临时 Git DAG 验证祖先/分叉/缺历史/受控发布线，exact 与 descendant 行为不同。
  - [ ] 不同环境、组件缺失、profile 变更、权限范围不兼容不会共享。
  - [ ] 候选选择、合并等待上限、urgent 已覆盖时仅提升优先级均有确定性测试。

### Task T-003 — 请求账本、事务与持久命令

- Status：TODO；Owner：实施负责人待指定；Priority：P0。
- Linked requirements：FR-001、FR-002、FR-009、FR-010、NFR-002、NFR-005。
- Objective：接纳并发调用并建立崩溃后仍可恢复的事实源。
- Task details：实现六张主表及索引/外键；短写事务；幂等正文摘要；同事务创建请求、归属、command、event；支持按 sequence 查询审计事件。
- Implementation requirements：事务内检查 lane/config 版本；同主体同 key 同正文复用 ID，不同正文 409；客户端看到 202 前提交已完成。任何执行器调用都不放进事务。全局 permit 来自 DB 活动状态，不能在重启后清零丢失。
- Allowed modification range：src/store、migrations、src/domain 持久 DTO、tests/store、tests/idempotency。
- Dependencies：T-001、T-002。
- Risks/notes：使用追加审计，不把系统扩展成 event-sourcing 框架。
- Evidence links：待实现。
- Completion conditions：
  - [ ] 100 并发同目标提交只产生一个可执行批次，幂等重试返回原请求。
  - [ ] 在每个写入点注入失败，状态、归属和事件不存在半提交。
  - [ ] API 进程在提交成功/响应返回之间退出后，调用方重试不重复创建请求。

### Task T-004 — 执行器契约、调度循环与安全抢占

- Status：TODO；Owner：实施负责人待指定；Priority：P0。
- Linked requirements：FR-004、FR-005、FR-006、FR-011、NFR-002。
- Objective：完整跑通用户提出的“共享执行”和“紧急替代普通执行”。
- Task details：固定 Executor trait；实现有持久状态的 ScriptedExecutor；调度 tick/通知唤醒；抢占计划、取消 command、停止确认、请求转移、阶段准入与 generation 检查。
- Implementation requirements：preparing/building 可抢占，deploying/verifying 不抢占；停止确认前旧 run 保持 lane 所有权；cancel 与 phase-grant 使用同一事务边界竞争；新目标必须覆盖全部旧未完成请求，替代无环。
- Allowed modification range：src/executor、src/scheduler.rs、src/domain 状态转换、src/store 对应事务、tests/scenarios/preemption。
- Dependencies：T-002、T-003。
- Risks/notes：ScriptedExecutor 的物理计数是测试执行次数，不能写成真实部署证明；禁止加入任意 shell runner。
- Evidence links：待实现。
- Completion conditions：
  - [ ] urgent 已被 normal 覆盖时不重新执行，run 有效优先级提升。
  - [ ] 停止确认前不启动替代写入，确认后转移所有有效 waiters，旧 run 为 superseded。
  - [ ] cancel/准入两种提交顺序，以及 A→B→C 连续替代的确定性测试通过。
  - [ ] 单 lane 1 个活动执行、全局上限 2 的约束在并发下始终成立。

### Task T-005 — 不明投递、重启恢复与有证据的处置

- Status：TODO；Owner：实施负责人待指定；Priority：P0。
- Linked requirements：FR-006、FR-009、FR-011、NFR-002、NFR-005。
- Objective：服务重启或外部响应丢失时不重复上线、不提前解锁。
- Task details：command 发送/确认状态；回执去重、乱序处理；启动 observe；unknown/blocked；drain；管理员证据处置；停止确认超时与已知外部身份恢复。
- Implementation requirements：没有外部唯一身份的 dispatch unknown 不重发；超时/失联不作为已停止证明；迟到可信证据可修正 unknown，但不能推翻已被新 generation 接管的 lane。恢复旧数据库后先观察环境。
- Allowed modification range：src/scheduler.rs、src/store、src/executor、src/domain、tests/scenarios/recovery。
- Dependencies：T-003、T-004。
- Risks/notes：不能通过管理员“确认风险”跳过物理所有权核对；缺证据就继续阻塞。
- Evidence links：待实现。
- Completion conditions：
  - [ ] 进程退出发生在 dispatch/ack/cancel/verify 的各个边界时，恢复结果无重复环境写入。
  - [ ] 停止确认 60 秒超时保留活动所有权，替代任务不被错误放行。
  - [ ] 重复和旧 generation 回执不会修改当前环境证据。
  - [ ] drain/重启/resolve-unknown 都留下可读审计原因。

### Task T-006 — 发布验收、请求结算、失败与撤回

- Status：TODO；Owner：实施负责人待指定；Priority：P0。
- Linked requirements：FR-003、FR-007、FR-008、NFR-002。
- Objective：对每项上线需求给出可解释、可追踪且不会误报成功的结果。
- Task details：receipt 冻结；逐请求 covers 检查与共享完成；当前环境证据新鲜度；failure_paused、needs_attention、显式 retry 与独立的自动/人工持久计数；请求撤回/过期和终态保护。
- Implementation requirements：CI 绿色/命令 0 退出不能代替环境验收；自动预算默认 0、人工 retry 默认最多 3 次，混合批次采用最小剩余自动预算；superseded 不消耗失败重试预算；新请求不能清除失败冻结；重试不改变目标、不重置截止时间，不自动回退旧版本。
- Allowed modification range：src/domain、src/store、src/scheduler.rs、src/executor/verifier 边界、tests/scenarios/settlement。
- Dependencies：T-003、T-004、T-005。
- Risks/notes：历史成功请求不因后续回滚被改写；新请求也不能直接复用过时成功状态。
- Evidence links：待实现。
- Completion conditions：
  - [ ] 被替代 run 仍为 superseded；其原请求在后继真实验收后 satisfied，并有实际 run/receipt 指针。
  - [ ] 后继失败、产物 SHA 不符、组件缺失、验收失败都不会让旧请求成功。
  - [ ] 单请求与最后请求撤回、过期、迟到成功、等待端断线都保持正确归属。
  - [ ] 历史成功后环境变化，再提交目标必须重新观察，不能误复用。

### Task T-007 — HTTP/CLI、权限与可观测性

- Status：TODO；Owner：实施负责人待指定；Priority：P1。
- Linked requirements：FR-001、FR-002、FR-005、FR-008、FR-010、FR-012、NFR-003、NFR-004。
- Objective：让多个调用方使用同一个协调器提交和等待发布需求。
- Task details：实现设计 §7.8 的端点/CLI 与错误码；认证 actor；project/lane 权限；urgent/admin 能力；query/wait；事件分页；关联日志和低基数指标。
- Implementation requirements：请求正文不接收任意脚本/秘密/环境地址；只有受信执行器能提交 phase-grant 和结果；用户撤回请求不直接取消共享 run。wait 本地超时与服务端请求过期有不同状态和退出码。
- Allowed modification range：src/api、src/main.rs、src/config.rs、src/telemetry.rs、tests/api、tests/cli、docs 契约与配置示例。
- Dependencies：T-001～T-006。
- Risks/notes：M1 仅静态主体授权，不实现账号/组织/RBAC 产品；测试环境权限不隐式覆盖生产 lane。
- Evidence links：待实现。
- Completion conditions：
  - [ ] 两个独立 CLI 进程提交重叠目标，得到不同 request_id 并等待同一 run，结果可追溯。
  - [ ] 无权限 urgent、跨 lane 读取、伪造执行器回执和任意命令输入被拒绝。
  - [ ] wait 断开/超时不终止发布；退出码与错误契约一致。
  - [ ] 日志和指标能区分排队、构建、取消确认、部署和验收耗时，且不含秘密值。

### Task T-008 — 完整场景、负载和运维验收

- Status：TODO；Owner：实施负责人待指定；Priority：P1。
- Linked requirements：FR-001～FR-012、NFR-001～NFR-005。
- Objective：证明交付的 M1 是可恢复调度内核，并准确说明它尚未接真实生产。
- Task details：跑通设计 §10 全场景；属性测试和数据库并发；独立进程故障注入；负载采样；在线备份/恢复；drain 与返回旧发布入口演练；整理复现步骤和本项目质量检查。
- Implementation requirements：记录运行提交、硬件/磁盘参数、SQLite 版本、命令及结果；不把单元 mock 通过当作真实 GitHub/生产验收；仅执行本项目相关检查，不构建 AutoTask 三入口。
- Allowed modification range：tests/scenarios、tests/load、配置/运行示例、docs/acceptance、本项目 .github/workflows 质量检查。
- Dependencies：T-001～T-007。
- Risks/notes：性能目标未达需如实记录并评审原因；不能省略失败注入以换取绿色报告。
- Evidence links：待实现；当前设计不能代替运行证据。
- Completion conditions：
  - [ ] 设计 §10 场景矩阵均有可重放证据，覆盖真实 SQLite/临时 Git/多调用者进程。
  - [ ] cargo fmt --check、cargo clippy --all-targets -- -D warnings、cargo test 通过。
  - [ ] 达到 NFR-001 的测量目标，或经评审修改目标并明确原因；不得冒充已达标。
  - [ ] README 明确 M1 已实现的能力、单实例限制及 M2 尚未接通的边界。

## 4. TODO Board

### TODO

- [ ] T-001 Rust 工程与持久启动基础。
- [ ] T-002 覆盖和纯调度决策。
- [ ] T-003 请求账本与持久命令。
- [ ] T-004 执行器契约与安全抢占。
- [ ] T-005 未知投递与重启恢复。
- [ ] T-006 验收、请求结算和失败/撤回。
- [ ] T-007 HTTP/CLI、权限与可观测性。
- [ ] T-008 场景、负载与运维验收。

### IN_PROGRESS

无。当前未开始实现。

### BLOCKED

无 M1 外部阻塞。M2 的环境和可信执行器问题见设计 Q-001～Q-003，不计入 M1 TODO。

### DONE

设计交付已完成，证据为 design.md 和本计划；以上八项实施任务均未完成，不因文档存在而勾选。

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

- [ ] T-001～T-008 全部 DONE，每项有提交/测试/验收证据。
- [ ] 核心两条规则及完整异常场景满足设计 §10。
- [ ] 不存在未解决 P0/P1 实施阻塞；scope lock 没有被暗中扩张。
- [ ] 技术评审仍为 PASS；实际交付状态与文档一致。
- [ ] M1 仅对可控执行器声明完成；不宣称已经生产可用或接通 GitHub Actions。
- [ ] 备份、恢复、drain、失败暂停与 unknown 处置有可执行说明。

## 7. Plan Update Rules

状态只使用 TODO、IN_PROGRESS、BLOCKED、DONE；实施开始后及时更新 owner、依赖、证据与完成条件，TODO Board 同步修改。

增加任务时写明来源需求和范围影响；遇到越界先修订设计，不以“顺手”增加真实生产执行、额外数据库或 Web UI。只有全部必要任务验收后才把计划改为 DONE。M2 必须有单独计划，不能在 M1 收尾时悄悄接入。
