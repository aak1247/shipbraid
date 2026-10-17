# 项目适配层迁出与 Release 切换计划

## 1. Plan Header

- Design/review: [design.md](design.md) §13
- Owner: Codex；子任务按用户指定使用 gpt-6-luna max
- Date: 2026-10-17; Status: ACTIVE; Review gate: PASS

## 2. Scope Lock

Shipbraid 删除专属适配/安装/LAN unit，更新相关文档和检查；通用发行工具整合至独立 worktree。AutoTask 仅 scripts/shipbraid、部署指南与必要入口说明，保留现有前端 WIP。线上仅协调器/UI切换和维护备份，不部署业务、不改执行器配置/数据库身份/凭据。扩大到业务部署或 schema 迁移需重审。

## 3. Task Breakdown

### T-001 — 迁入 AutoTask

- Status: DONE; Owner: gpt-6-luna max; Priority: P0; Requirements: FR-002,NFR-002
- Details: 从最新已提交版本迁入专属适配器、恢复器和测试；维护指南，显式提交并 push vibe。
- Range: AutoTask scripts/shipbraid、docs/guides/auto-deliver.md、必要 AGENTS 部署段与 package.json 测试入口。
- Dependencies: 已核对两个仓库状态；Risk: 不触碰前端 WIP，不部署。
- Evidence: AutoTask `4b7ea9140`（迁入）、`3b778ff21`（文档），最终 `vibe=3cf03a7c2` 已推送且远端一致；51 个适配器测试 + 8 个客户端测试通过。
- Completion: [x] 测试通过；[x] vibe推送且其他会话改动保留。

### T-002 — Shipbraid 去耦与发行整合

- Status: DONE; Owner: gpt-6-luna max / Codex; Priority: P0; Requirements: FR-001/003,NFR-002
- Details: 独立 worktree 基于最新 origin/main，清理当前源码及活跃文档/CI、整合上一轮发行工具；历史验收不删。
- Range: integration/autotask、deploy项目专属文件、README/文档/CI，发行工具与 Cargo 版本。
- Dependencies: T-001安全落地后删除源；Risk: 线上冻结路径不可删。
- Evidence: `e5ae304` 迁出，`c5959c8` 通用发行整合，`8d35bd3` 修复验收 fixture；均已 push main。当前 integration 只有 local/github，deploy 只有通用安装器。Rust 62 tests、fmt/clippy，Python 47 tests、Dashboard 10 tests 与构建、四套协议验收通过。
- Completion: [x] 当前 Git 无专属执行代码；[x] 通用检查通过；[x] main源提交可追溯。

### T-003 — 发布 GitHub Release

- Status: DONE; Owner: Codex; Priority: P0; Requirements: FR-003/004
- Details: 版本/tag一致，干净已提交源码、远端检查和包级验证后发布；下载工件进行二次校验。
- Range: GitHub shipbraid main/tag/Release，私有临时工件。
- Dependencies: T-002；Risk: 本地源码包不是已发布工件，不混淆。
- Evidence: [v0.3.1](https://github.com/aak1247/shipbraid/releases/tag/v0.3.1) 已公开发布；tag 指向 `8d35bd326f8c629e38ade4444110110bd4fb6f51`。checks runs `39014512604`/`39014515075` 和 release run `39014515065` 全通过；GitHub Ubuntu 22.04 完成六项包级真实 systemd 合成验收。公开后重新下载，sidecar 与完整 manifest 24 files 通过；tar SHA256 `f6edd8b4060873fcc72c77407bedd41e6384ea7c621368bc4e43f600756b2c10`，binary SHA256 `252bd33920e55abb3489c7c972f04f972ac689e992c1c3065055c758bd5933cd`。
- Completion: [x] 实际 GitHub Release URL/资产；[x] 下载checksum/manifest通过。

### T-004 — 协调器安全切换

- Status: IN_PROGRESS; Owner: Codex; Priority: P0; Requirements: FR-004/005,NFR-001/003
- Details: 现场再次检查，drain、备份，私有副本禁调度验证兼容，切 ExecStart/Dashboard、重启并只读核对；保留原配置和回退工件。
- Range: 原 auto-deliver 用户服务、配置/dashboard路径、私有版本目录和CLI链接；保留hook/账本/令牌值。
- Dependencies: T-003；Risk: 新活动/unknown必须保留所有权，不强杀/换空库。
- Evidence: 待补（只存无秘密摘要）。
- Completion: [ ] 实际进程hash等于Release资产；[ ] 原账本/参数/指纹/请求结果保持；[ ] 无新增业务部署。

## 4. TODO Board

- [x] T-001 DONE AutoTask迁入
- [x] T-002 DONE 通用仓库清理
- [x] T-003 DONE GitHub发行
- [ ] T-004 IN_PROGRESS 现场切换

## 5. Milestones

M1 项目归属与通用发行；M2 已发布Release工件；M3 实际协调器切换。各任务条件为退出门槛。

## 6. Feature-Level Completion Conditions

四项任务DONE、review PASS、两个仓库提交明确、GitHub公开资产可下载、现场身份/数据比对通过；不以有代码/有草稿冒充已使用Release。

## 7. Update Rules

按事实即时更新；不记录秘密/私有DB。不把CI未运行写成通过，不把业务环境未验收写成已发布。


## 8. 发行前检查

基线为已提交 `origin/main=3d5d497`，包含 `646a557` 的通用参数能力及 `073ccaf` 的受控未交接恢复；没有从共享主工作树暂存其他会话的 API/CLI 改动。首轮候选版本为 `0.3.0`。Rust release 模式 54 library + 6 CLI + 2 ledger tests、fmt、clippy 通过；通用 Python 47 tests、既有模拟/hooks/Agent/CLI 四套验收通过；前端类型检查、10 tests、build 通过。前端复用已安装依赖，直接执行相同 tsc/vitest/vite 工具；pnpm 因跨 worktree 依赖目录保护拒绝自动安装后未删除原依赖。

GitHub tag `v0.3.0` 的 main/tag checks 均通过，但 release 验收在 HTTP fixture 的重启计数竞争上失败（run `39001960834`）。遗留 handler 持有旧 record，释放 gate 后覆盖新 verify_count；修复 fixture 的共享临界区，再发布 `0.3.1`，不移动既有 tag、不跳过验收。

现场准备：已下载并冻结官方 v0.3.1 新目录；维护 drain 已启用，保留原值以便结束后恢复。现有生产任务 `98d90d06-77da-469f-9af7-a913ac92db8a` 继续执行，未被取消；开发 Unknown 保留。待生产任务结束再做隔离副本与实际切换，尚不声称现场已升级。
