# 执行计划：可配置执行器与 AutoTask 接入

## 1. Plan Header
- Owner: Codex；子代理沿用用户指定 gpt-6-luna/max。
- Design: [design.md](design.md)；Last updated: 2026-10-16。
- Plan status: ACTIVE（T-101～T-103 已完成；T-104 已安装并验证协调器、提交真实开发请求且完成运行中 Dashboard 浏览器检查，仍待发布完成及匹配 receipt）。Review gate: 设计与实现独立复核均 PASS。
- Current checkpoint: [verification.md](verification.md)。user service 已在本机 loopback ready；真实 systemd run 正等待既有 deploy lock，production 保持 disabled。

## 2. Scope Lock
只修改 auto-deliver 工程与 AutoTask 的发布提交入口/文档。首个真实目标本机开发环境；不变更生产、不覆盖其他会话工作、不重写构建系统。新增能力遵守原覆盖、持久化、unknown 和验收不变量。

## 3. Task Breakdown
### T-101 可配置 hook 与真实执行生命周期
- Status: DONE；Owner: backend；Priority: P0；Requirements: FR-101/103/104/105, NFR-101/102/103。
- Range: src/config.rs、executor.rs、新 hook 模块、service.rs、api.rs、migrations（如必要）。
- Dependencies: 技术评审 PASS。
- Details: command/script/webhook 共用 JSON 协议；异步有界调用、独立观察和 verify、重启不盲重放、真实模式不再自动生成模拟回执；提供执行器 metadata。
- Evidence: [hook implementation](../../../src/hooks.rs)、[hook acceptance](hooks-acceptance.json)、[Rust/CI verification](verification.md)。
- Completion:
  - [x] command/script/webhook 协议与隔离子进程验收通过；超时、失败、响应丢失、重启后只读 observe/verify 和 fencing 有证据，见 [hooks-acceptance.json](hooks-acceptance.json)。这些回执均为 synthetic，不是实际部署。
  - [x] 38 个 Rust 测试、fmt/clippy/debug build 通过；GitHub Actions run [38897050033](https://github.com/aak1247/auto-deliver/actions/runs/38897050033)（commit `880c1de`）成功，见 [verification.md](verification.md)。
  - [x] 独立设计 gate 与有界 Rust hooks 实现复核通过，见 [verification.md](verification.md)。

### T-102 环境选择、Dashboard 与常驻包装
- Status: DONE；Owner: root + dashboard；Priority: P1；Requirements: FR-102/106。
- Range: src/main.rs、web、Cargo 清单、部署 unit/config/说明；API/config 改动与 backend 协调。
- Dependencies: T-101 契约。
- Evidence: [dashboard source/tests](../../../web/src)、[verification.md](verification.md)。
- Completion:
  - [x] HTTP/CLI 的 project/environment 选择与 `lane_id` 兼容；未唯一匹配或跨 lane 权限不满足时失败，见 [hooks-acceptance.json](hooks-acceptance.json)。
  - [x] 同源 Dashboard 及前端 10 个测试、typecheck、build 通过，见 [verification.md](verification.md)。
  - [x] 1440px 浏览器中已验证真实服务的鉴权、环境过滤、详情和该真实 run 的日志；最终 receipt 展示随 T-104 完成。

### T-103 AutoTask 桥接和提交入口
- Status: DONE；Owner: root；Priority: P0；Requirements: FR-107, NFR-102。
- Range: integration/autotask/、AutoTask scripts/deploy-coordinated.*、相应说明与测试。
- Dependencies: T-101 协议；本机开发环境已明确。
- Evidence: [bridge implementation/tests](../../../integration/autotask/test_bridge.py)、[AutoTask source commits](verification.md)。
- Completion:
  - [x] Bridge 6 个 Python tests、AutoTask wrapper 4 个 HTTP tests 与 deploy lock/fence Bash suite 通过；systemd task identity、frozen SHA、独立进程/源码/健康证明的实现已复核，见 [verification.md](verification.md)。这是代码与隔离测试证据，不代表已执行真实部署。
  - [x] AutoTask core patches 已推送至 `origin/vibe`；提交映射与共享工作区保护说明见 [verification.md](verification.md)。

### T-104 回归、部署与真实请求验收
- Status: IN_PROGRESS；Owner: root；Priority: P0；Requirements: 所有上述 FR/NFR。
- Range: tests/scripts、docs 验收、用户级 auto-deliver unit 与私有配置/数据。
- Dependencies: T-101~103。
- Completion: 运行必要质量检查、部署服务、真实开发发布完成并有匹配 receipt、Dashboard 实际数据可见、提交推送和状态记录。生产保持禁用。
- Evidence: [M1 regression](m1-regression.json)、[hook protocol acceptance](hooks-acceptance.json)、[current deployment target](service-installation.json)、[verification.md](verification.md)。两份 acceptance JSON 均为本地/合成验证，`real_deployments` 为 0。
- Remaining:
  - [x] Required source checks, synthetic M1 regression, hook acceptance, wrapper/bridge/deploy-lock suites, CI and bounded reviews passed; see [verification.md](verification.md)。
  - [x] User-level `auto-deliver.service` is installed and ready on `127.0.0.1:8787`.
  - [x] Real HTTP request `f6315628-c0cc-434b-96ae-d3d0651b74eb` and CLI request `2496d753-3ef9-45c1-966e-aad5082a11f7` share physical run `b2924ad8-1949-45ab-b712-21dbd07628bc`, generation 1, pinned to `7454e798bff72cf43175e8b565edf7270b98a246`; the real systemd executor is active.
  - [x] Browser at 1440px verified authenticated access, filters, details and live logs for the real run; final receipt display remains pending.
  - [ ] The run is waiting for the legacy deploy lock, currently held by a separate Codex Python validation script with a 900-second hold after acquisition. Do not interrupt it; after release, verify the frozen SHA, service process/source hashes and health, then capture the matching receipt.
  - [ ] Use the completed live receipt in the Dashboard and append its verification evidence here and in [verification.md](verification.md). Seed the trusted mirror from the deployed checkout; later updates remain fast-forward only.
  - [x] Keep service local-only at `127.0.0.1:8787` (remote access through SSH tunnel); production remains disabled and legacy opaque hooks remain non-preemptible.

## 4. TODO Board
- TODO: 无独立后续任务；未完成的本迭代验收均在 T-104 的 release、user service、真实开发请求、receipt 和浏览器 gate 下追踪。
- IN_PROGRESS: T-104。
- BLOCKED: 无；环境已明确。
- DONE: T-101、T-102、T-103。

## 5. Milestones
协议及 isolated tests → 常驻服务+提交入口 → 一次真实开发发布及 Dashboard 验收。

## 6. Feature-Level Completion Conditions
全部必要任务 DONE、有测试及真实运行证据、技术 gate PASS、两仓库推送成功。M1 性能/完整故障矩阵未完成项不因本迭代被关闭；上一轮 NFR-001（50 requests/s × 60s、p95 < 250ms）仍为 NOT PASSED。

## 7. Plan Update Rules
状态与证据同步；越界先修订设计；生产变更需另有明确目标与授权。新执行机制不得虚构安全停止或环境验收。
