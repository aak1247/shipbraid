# 项目适配层迁出与 GitHub Release 切换

## 1. Document Control

- Version: 3; Author / Reviewer: Codex（同会话自审）
- Date: 2026-10-17; Status: APPROVED; Review gate: PASS（§13）
- Plan: [execution-plan.md](execution-plan.md)

## 2. Context and Problem

用户要求清除 Shipbraid Git 中 AutoTask 专用代码，并将 AutoTask 实际使用的协调器切换到 GitHub Release。现有服务为本机 `auto-deliver.service`，同一账本包含 dev/prod，生产已显式配置且使用必填参数；不能把不支持该配置的旧 main 构建替换上线。

## 3. Goals and Non-Goals

目标：项目代码归项目维护；发布版本可追溯到干净已提交源码；下载校验 GitHub Release 后切换实际协调器和 UI。非目标：重写 Git 历史、删除历史验收、重命名存量数据库/unit、部署 AutoTask 业务版本、取消/重试生产请求、变更集群/网络权限。

## 4. Scope

允许 Shipbraid 的 AutoTask 专用 integration/安装器/LAN unit 删除、相关活跃文档与 CI 去耦；通用发行工具进入已更新的独立发布 worktree。AutoTask 仓库接收专用适配器/测试/配置说明，按该仓库规则显式提交并 push vibe。现场仅更新协调器二进制与 Dashboard 路径、可选 CLI 链接；保持账本、身份、lane、项目 hook 路径、参数约束不变。共享主工作树的其他会话改动不暂存或覆盖。

## 5. Users and Use Cases

Shipbraid 用户只安装二进制并配置通用 hooks。AutoTask 维护者在自己仓库维护项目发布工具；已有实例升级保持历史执行可观察。需要 unknown 的实例不得因升级强行释放所有权。

## 6. Requirements

- FR-001: Shipbraid 当前 Git 快照不含 AutoTask 专用可执行代码或主机绑定 LAN units；历史记录保留。
- FR-002: AutoTask 仓库保存当前已审查的开发/生产/未交接恢复适配器及测试，源代码归属清晰；不改写运行中的冻结副本。
- FR-003: 正式发行包含线上现有通用参数能力、所有权与协议兼容；基于已提交远端 main 整合上一轮发行工具，发行 tag 与 package version 一致。
- FR-004: GitHub Release 工件下载后校验 sidecar 与 manifest；先 drain、确认没有未确认命令；活动部署只有在独立执行器身份可验证且调度写入权完整保留时允许维护；在线备份账本和私有配置；已有 Unknown 必须保留原所有权，原子切换 unit override/UI 路径，保留回退记录。
- FR-005: 启动后请求/run/receipt 数量与状态、lane 指纹与参数、凭据作用域相符；已知只读入口可用；不触发业务部署。
- NFR-001: 不记录秘密，不提交 DB 或私有配置；至少一次交付，unknown 不因超时变为 stopped。
- NFR-002: main 和 AutoTask vibe 只整合本任务变更；发布在干净独立 worktree，保护共享 WIP。
- NFR-003: 失败回退到旧二进制/UI/原配置；不以回退数据库抹去新请求。若 schema 变化则在切换前阻断，本轮预期无 schema 变化。

## 7. Proposed Solution

以 `origin/main`（已含通用参数与生产交接修复）建立 `codex/release-decoupling` worktree。迁走项目集成文件，通用 API/调度器不加入业务特例。更新用户文档与 CI，适配器保存在 AutoTask `scripts/shipbraid/`，安装配置不携带秘密。发布新 SemVer 版本，经本地和远端检查后发布 GitHub Release。

线上继续使用现有 `~/.config/auto-deliver/` 和 `~/.local/share/auto-deliver/`，不启第二协调器。旧 hook 路径作为冻结运行资产保留；新源码位置不会改变旧 run 的配置。下载的发行二进制和 web 安装到新私有版本目录，systemd drop-in 只变更 ExecStart，服务 TOML 只变更 dashboard_dir。首次快照无 active run，但准备发行期间其他会话启动了生产部署，开发 lane 进入 Unknown；切换前重新检查，不据旧快照推断后续状态。

## 8. Alternatives and Tradeoffs

不选择新安装器重建实例：会丢历史请求和所有权。不重写历史以消除所有字符串：会破坏审计及 Git 共享历史。不用 cargo --release 本机构建冒充 GitHub Release；以实际下载资产为切换来源。

## 9. Risks

共享工作树 main 落后远端，直接 pull/覆盖会损坏其他工作：隔离整合。旧 main 缺必填参数，不兼容当前配置：使用已经提交的最新实现并以私有在线备份做禁调度启动验证。并发新请求：drain 后重新统计 accepted/waiting/active，切换过程无新副作用；恢复原 drain 状态须现场检查队列，不提交/重试业务请求。旧 hook 仍可读是恢复必要条件，不删除安装目录。

## 10. Validation Strategy

Rust fmt/clippy/test；Python 通用与迁入适配器测试；CLI 参数与 hooks/Agent/包验收；前端构建。GitHub 远端 tag 检查与工件 checksum、manifest核对。使用在线备份的私有副本禁调度验证新二进制能读原 schema、保留参数和指纹；实际切换后只读比对账本快照、请求历史、lane、Dashboard 与进程 executable hash。

## 11. Rollout Plan

先代码迁出和测试/提交，再发行；准备新工件与回退，最后按已授权范围切换本机协调器。服务短暂停机不代表执行器停止。生产应用版本不变，不向集群发送部署命令。若真实任务启动或出现 unknown，等待并核对，不擅自取消。

## 12. Open Questions

用户已明确选择 GitHub 已发布 Release（不是本机 release 构建）。新版本号由本轮发布整合决定。无其他阻塞问题。

## 13. Review Feedback Tracking

1. Verdict: PASS；confidence MEDIUM，运行前必须再次检查现场。
2. Overdesign: 不拆服务/新数据库/重做执行器；只迁归属与发行切换。
3. Feasibility: 一轮可交付，CI/网络为外部等待；已有通用兼容实现可复用。
4. Completeness: MAJOR—main落后导致配置不兼容，已通过 origin/main 基线与副本禁调度验证解决设计；MAJOR—删除冻结 hook 会失去证据，只删除 Git 源，不动已安装副本；MAJOR—跨仓库 WIP，只显式提交本任务文件。
5. Clarifying questions: release来源已回答；没有业务部署授权，也不需要业务部署来验证协调器升级。

## 14. Definition of Design Complete

范围、授权、失败处理和验证明确，review PASS；执行中的状态与最终证据见计划。

## 15. 现场变化后的设计补充与复审

准备期间出现已有开发 Unknown 与其他会话触发的生产部署。等待正在进行的部署/验证结束，不取消、不重试、不清除 Unknown。Unknown 可能长期存在；它不等于协调器不能维护，而是其执行权必须完整保留。切换 gate 调整为：没有非 Unknown 的活动执行、没有 Pending/Delivered/Unknown 命令，保留既有 Unknown 的 active_run_id/generation、原 DB、hook 路径及 lane 指纹。

代码复核：Service::open 对已有 Unknown 不做恢复重放，register_lane 保留 active_run_id/generation，reconcile 对 Unknown 返回 Blocked。scheduler 在 drain 时仍会 Observe，并可能调用 failure_observe/Verify，因此只做 drain 的私有副本并非隔离演练。副本验证使用 bubblewrap：宿主根只读、只有维护目录可写、独立网络、屏蔽 user/system bus；新生成的副本令牌，禁止接触真实部署控制面。允许副本 Unknown 的观察错误描述变化，不允许状态/owner/generation/target、历史 request/receipt/command 或 schema 变化。

复审 PASS：这项调整保留同一所有权和冻结执行器，既不把 Unknown 当停止，也不为了维护人工释放 lane。实际切换只恢复原协调器的正常观察行为，并逐项核对；启动后 Unknown 仍须由独立证据解决。本轮不解决该开发部署问题。

## 16. 独立任务持续运行时的协调器维护复审

本节取代 §15 对非 Unknown 活动执行一律等待的维护 gate，其余所有权和隔离要求仍适用。

生产构建持续超过二十分钟。等待整轮构建并不是所有权不变量的要求；GitHub 和本机对下载的正式包均已验证“独立 systemd 部署期间重启协调器、只有一次 dispatch、任务继续并最终验证”。现场生产 run 处于 Running/Deploying，dispatch 已 acknowledged；实际 worker 与协调器属于两个独立 user units/cgroups。

复审 PASS，允许使用相同机制维护：必须先 drain；所有命令 acknowledged，无 Dispatching/CancelRequested/Verifying 操作；每个 Running/Deploying run 的 executor_ref 是已存在独立 systemd 单元，MainPID/InvocationID/ControlGroup/ActiveState/SubState 在停协调器后和新协调器就绪后相同。新进程必须保留同 DB、active_run_id/generation、配置指纹、冻结 target、原 hooks；请求/receipt/command 和 schema 比对仍通过。Unknown 所有权继续保留。任何检查失败恢复旧协调器，不动执行器或回退数据库。

隔离副本已使用正式 binary、只读宿主根、独立 network/PID/IPC、私有 /run、清空环境及合成令牌；仅 shadow 子目录可写。因真实 bus 不可访问，副本中的 Running 可以变 Unknown，但必须保留 owner/phase/generation/target，历史请求/receipt/command不变；这不修改真实账本。实际服务恢复原有 Observe，不触发新的 dispatch；本次维护不提交发布、重试、取消或 resolve 请求。
