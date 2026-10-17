# 未提交工作收尾记录（2026-10-17）

Status: COMPLETE。实现提交 `7e2b89f`；[验证摘要](pending-workspace-20261017.json)。

## 基线与范围

原工作区 main 为 `a7f0839`，落后当时远端 `0c060a0` 十二个提交；有 12 个 tracked 修改及 24 个 untracked 文件。以 `origin/main=0c060a0` 新建整合 worktree，保存全部 36 个草稿文件的私有副本/摘要和 tracked patch。仅处理 Shipbraid；AutoTask 会话界面工作不属于本轮。

本轮源码版本为 `0.3.2-dev`，正式已发布/运行版本仍为 v0.3.1；不创建新 Release，不切换实际服务。

## 处理结果

| 原文件 | 处理 |
| --- | --- |
| `.github/workflows/check.yml` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `Cargo.lock` | 旧草稿仅包排序变化；采用当前 lockfile，再更新开发版本 |
| `README.md` | 保留通用接入/CLI 说明或示例改动，按当前参数及发行边界整合 |
| `README.zh-CN.md` | 保留通用接入/CLI 说明或示例改动，按当前参数及发行边界整合 |
| `docs/agents.md` | 保留通用接入/CLI 说明或示例改动，按当前参数及发行边界整合 |
| `docs/api.md` | 三方合入新主分支；补齐 request/run 日志与旧 CLI 兼容，统一验证 |
| `docs/design.md` | 保留通用接入/CLI 说明或示例改动，按当前参数及发行边界整合 |
| `docs/hooks.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `examples/local.toml` | 保留通用接入/CLI 说明或示例改动，按当前参数及发行边界整合 |
| `scripts/acceptance.py` | 三方合入新主分支；补齐 request/run 日志与旧 CLI 兼容，统一验证 |
| `src/api.rs` | 三方合入新主分支；补齐 request/run 日志与旧 CLI 兼容，统一验证 |
| `src/main.rs` | 三方合入新主分支；补齐 request/run 日志与旧 CLI 兼容，统一验证 |
| `.github/workflows/release.yml` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `deploy/install-standalone.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `deploy/test_install_standalone.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `docs/distribution.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `docs/features/iter-20261017-autotask-decoupling/design.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `docs/features/iter-20261017-autotask-decoupling/execution-plan.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `docs/features/iter-20261017-distribution/design.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `docs/features/iter-20261017-distribution/execution-plan.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `docs/project-integration.md` | 保留通用接入/CLI 说明或示例改动，按当前参数及发行边界整合 |
| `examples/standalone/github.json` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `examples/standalone/project/README.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `examples/standalone/project/deploy.sh` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `examples/standalone/project/verify.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `examples/standalone/simulation.toml` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `integration/autotask/README.md` | 旧项目专用草稿，仅归档；当前主分支不恢复此目录 |
| `integration/github/README.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `integration/github/receiver.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `integration/github/test_receiver.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `integration/local/README.md` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `integration/local/adapter.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `integration/local/test_adapter.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `scripts/acceptance-distribution.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `scripts/package-release.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |
| `scripts/test_package_release.py` | 采用已发布主分支的完整实现/记录；旧副本保存在归档中 |

## 日志契约与兼容性

- Request 日志优先 current_run、其次 satisfied_by_run、最后查请求关联的历史 run；一次响应只返回同一 snapshot 中一个 run 的有界日志。
- cursor 绑定 request/run；更换 run 从起点读取，跨 request cursor 拒绝；查询仍按 lane/身份授权。
- 旧 `logs RUN_ID --after N` 保留，新增 `logs run RUN_ID` 与 `logs request REQUEST_ID --cursor ...`。
- 既有 run-log API 的 items/numeric cursor 保持；新增 run_id 用于明确关联。
- 不修改数据库 schema、请求/执行状态机、停止证明或调度写入权。

## 验证

Rust fmt/clippy 通过；56 library + 9 CLI + 2 ledger tests（67 项）通过，debug/release 构建通过。四套协议验收分别有 13/4/8/10 项检查通过；Python 7 packager + 10 installer + 19 local + 11 GitHub tests（47 项）通过。Dashboard 类型检查、10 tests、build 通过。实际打包的 0.3.2-dev bundle 通过七项验收，包括新 request 日志、旧 CLI 及游标无重复恢复、独立 systemd 任务重启恢复。全部使用临时合成环境，真实业务部署次数为零；未更新实际服务。

## 工作区归档

逐一确认原工作区 HEAD、36 个文件及 index 未被并发修改后，归档为本地 `codex/archive-pending-20261017-095151`（`3bf073265122`）；已逐文件核对归档 blob 与原快照 SHA256 相同。归档提交仅保存在本地分支，不推送旧草稿到远端。工作树干净后切换分支，将原目录 main 对齐已验证实现，随后合入文档与规划。原稿仍可用 git show 或独立 worktree 查阅；未使用 reset --hard、git clean、整树 stash 或覆盖未提交文件。

## 下一步建议

建议先实施[只读诊断命令](../features/iter-20261017-diagnostics/design.md)；其余候选与任务边界见该设计及执行计划。本轮仅提出规划，D-001～D-005 尚未实现。
