# 黑箱发行与项目接入执行计划

## 1. Plan Header

- Feature: distribution / project adapter / GitHub push ingress
- Design and review: [design.md](design.md)（§13）
- Owner / Contributors: Codex
- Updated: 2026-10-17
- Status: DONE; Review gate: PASS

## 2. Scope Lock

允许：新增 scripts/package-release.py、deploy/install-standalone.py、integration/local、integration/github、examples/standalone、release/check workflow、用户接入文档与本 feature 两份文档。README/hooks/project-integration 仅追加相应入口，保留其他会话改动。排除：调度核心、数据库 schema、已有 AutoTask 安装、真实部署/凭据/集群权限。扩展平台或生产接入需重审。

## 3. Task Breakdown

### T-001 — 发行打包与 draft Release

- Status: DONE; Owner: Codex; Priority: P0
- Requirements: FR-001, NFR-004
- Objective/details: 版本核对、清单摘要、二进制+静态资源+工具/示例/文档打包，tag workflow 验收后产出 draft。
- Change range: scripts/package-release.py, .github/workflows/release.yml
- Dependencies: 现有 Rust/前端构建；Risks: 发行平台 ABI，草稿不等于已公开。
- Evidence: 见 §8 本轮验证记录。
- Completion: [x] 包清单正确、包内二进制烟测；[x] workflow 不在 PR 发布。

### T-002 — 通用本机适配器

- Status: DONE; Owner: Codex; Priority: P0
- Requirements: FR-003/004, NFR-001/002/003
- Objective/details: 固定配置和 argv、独立 systemd worker、幂等交接、停止证明、新鲜环境验收；支持协议兼容 HTTP 服务入口。
- Change range: integration/local, examples/standalone
- Dependencies: systemd user/cgroup v2，Python 3.11+；Risks: 单元证据丢失保持 unknown。
- Evidence: 见 §8 本轮验证记录。
- Completion: [x] 重复/丢响应不重复部署；[x] 失败和非空进程树不误释放；[x] 错误验收不产生 receipt。

### T-003 — 新安装工具

- Status: DONE; Owner: Codex; Priority: P0
- Requirements: FR-002/006, NFR-002/004
- Objective/details: 只写新 prefix，私有凭据、loopback API、禁生产、固定 release 路径、服务 unit；不启动不发布、不 clone/fetch。
- Change range: deploy/install-standalone.py, examples/standalone
- Dependencies: T-001/002；Risks: 保留所有存量配置/所有权。
- Evidence: `deploy/test_install_standalone.py`（10/10）和 `integration/github/test_receiver.py`（11/11）；包级闭环见 §8。
- Completion: [x] 新目录可运行；[x] 再安装和包篡改拒绝；[x] 单元文件和秘密权限正确。

### T-004 — 可选 GitHub push 接收器

- Status: DONE; Owner: Codex; Priority: P1
- Requirements: FR-005, NFR-002
- Objective/details: HMAC-SHA256、固定仓库/分支到 lane 映射、服务端幂等、loopback 监听、请求大小/超时/重定向限制。
- Change range: integration/github, examples/standalone
- Dependencies: 现有 HTTP API；Risks: 镜像同步及公网代理由管理员管理。
- Evidence: `deploy/test_install_standalone.py`（10/10）和 `integration/github/test_receiver.py`（11/11）；包级闭环见 §8。
- Completion: [x] 错误签名/未授权仓库不能提交；[x] 重投同请求；[x] 上游失败返回非成功可重投。

### T-005 — 包验收与用户路径

- Status: DONE; Owner: Codex; Priority: P0
- Requirements: FR-006, 全部 NFR
- Objective/details: 从包安装临时服务、合成项目与监督器完整验收，文档给出离线/HTTP/GitHub 三种接入及升级步骤；运行 Rust 和前端必需检查。
- Change range: scripts/acceptance-distribution.py, 新工具测试, check/release workflow, README, docs/hooks.md, docs/project-integration.md, docs/distribution.md, 本计划
- Dependencies: T-001–004；Risks: 不把合成环境验收或未运行的 CI 标为真实项目/平台已验证。
- Evidence: 见 §8 本轮验证记录。
- Completion: [x] fmt/clippy/test 和相关验收通过；[x] 包内闭环无需源码工具链；[x] 证据与限制写入计划。

## 4. TODO Board

- [x] DONE T-001 发行
- [x] DONE T-002 适配器
- [x] DONE T-003 安装器
- [x] DONE T-004 GitHub
- [x] DONE T-005 端到端与文档

## 5. Milestones

- M1: 打包与协议工具；退出条件 T-001–004。
- M2: 用户可运行闭环；退出条件 T-005 与全量检查。

## 6. Feature-Level Completion Conditions

全部必需任务 DONE，review 保持 PASS；隔离包安装闭环和错误路径通过；无越界真实部署。GitHub 公开发布、官方 CI 平台运行和用户项目真实验收另记外部状态，不声称完成。

## 7. Plan Update Rules

实现/检查后即时更新状态与证据。发现需改内核/生产/存量配置时先重审范围；不掩盖跳过或失败的检查。


## 8. 本轮验证记录（2026-10-17）

交付文件：[`scripts/package-release.py`](../../../scripts/package-release.py)、[release workflow](../../../.github/workflows/release.yml)、[`deploy/install-standalone.py`](../../../deploy/install-standalone.py)、[本机适配器](../../../integration/local/README.md)、[GitHub 接收器](../../../integration/github/README.md)、[二进制用户指南](../../distribution.md)。README 中英文、Agent、hooks 和项目接入文档均使用 `shipbraid` 二进制；Cargo/Node 只出现在开发/源码构建说明中，历史验收证据保持原样。修正了旧 `examples/local.toml` 的环境变量前缀。

### 已通过

- `cargo fmt --all --check`。
- `CARGO_INCREMENTAL=0 cargo clippy --locked --all-targets -- -D warnings`。
- `CARGO_INCREMENTAL=0 cargo test --locked`：重新编译当前源码，47 library + 6 CLI + 2 ledger，共 55 tests 通过。
- `CARGO_INCREMENTAL=0 cargo build --release --locked`：当前源码 release 构建通过。
- 前端 `typecheck`、`test`（10 tests）、`build` 通过；用户运行使用打包静态资源。
- Python：发行打包 7/7、新安装器 10/10、本机适配器 19/19、GitHub 接收器 11/11，共 47 tests。单测之外，真实临时 systemd user unit / cgroup v2 的包级闭环已通过。
- `python3 scripts/acceptance-distribution.py --bundle target/distribution-final/shipbraid-0.2.0-x86_64-unknown-linux-gnu.tar.gz --report target/distribution-final/acceptance.json`：安装后 CLI/API/Dashboard、协调器重启且只交接一次、篡改/不健康环境拒绝 receipt、Bearer HTTP hook、新鲜验收、GitHub 错签拒绝与重复事件去重、非零部署由独立停止证明结算并暂停通道。报告只含合成 run/receipt 身份，不含凭据。工件/报告位于 gitignored `target/distribution-final/`。
- 按发行包的 `examples/standalone/simulation.toml` 从零初始化模拟仓库，直接使用包内二进制提交并等待 satisfied；无需 executor token、Rust/Node 或 systemd 执行器。
- 已有模拟与 Agent 验收通过；hooks 验收首次与其他脚本并行运行时 verify_count 断言失败，单独重跑通过。没有修改其合成夹具或把第一次失败隐去。
- `git diff --check`；Python AST、bash 语法和发行 workflow YAML/最小权限检查通过。

### 实际发现并修复

- verify 可以携带持久 command；不能误要求所有 verify.command 为 null。
- systemd 249 的正常退出 `ExecMainCode=1`；RemainAfterExit 的已退出任务可以有空 ControlGroup。只在持久结果、完整目标、InvocationID 和前后稳定监督器终态一致时接受已销毁 cgroup；不能用 unit 不存在或 main PID 为 0 单独证明停止。
- allowed_revisions 是会增长的授权缓存，不能将其动态全集作为旧执行的固定配置指纹。
- 验证器超时后保留 flock，避免无限重启重叠验证器；输出有界读取，不将超时视为停止证明。
- 部署日志通过专用 reader 截断存储但继续消费；不使用会限制所有构建产物并可能发送 SIGXFSZ 的全局 LimitFSIZE。
- 新安装拒绝脚本包隐藏/私密文件，release 文件和目录只读；systemd 对 ExecStart 和普通路径采用不同美元符转义；IPv6 地址必须有括号。

### 原有问题与外部状态

- 额外运行 `scripts/acceptance-cli.py --binary target/release/shipbraid` 失败：原有测试要求 `--param`，当前 HEAD 的 `src/main.rs` 没有该选项，领域/API 也没有相应参数字段；`9c37407` 仅提交了该功能的文档/生产适配器/测试。该不一致在本轮前已存在，未通过修改用户会话的 Rust 改动或删除测试来掩盖。它不在本轮 dev/test 接入契约内，相关未实现参数说明不随本次黑箱包发行；完整项目 CI 和公开发行就绪度仍需单独解决该缺口。
- GitHub workflow 尚未在远端运行，也未创建 tag 或公开 Release；本轮仅生成并本机验收工件。workflow 在相符版本 tag 上重新构建并创建 draft，公开发布需维护者检查。不要将本地构建称为官方 Ubuntu runner/MSRV 验收。
- 本机实际工具链为 Rust 1.98 / Python 3.14 / systemd 249；Python 3.11、Rust 1.94 和 Ubuntu 22.04 runner 验收由 workflow 配置承担，未在此主机独立执行这些具体版本。
- 真实 GitHub HTTPS 投递、具体业务项目和生产环境未接入、未验收；所有外部执行均限于本轮临时合成文件和独立测试 units。未知状态不触发自动强杀或生产操作。
- 磁盘满曾中断隔离重复依赖构建；仅删除本轮失败的 `target/distribution-check` 与本轮已运行完的三个测试可执行文件后，用现有依赖缓存重新编译当前源码完成上面的验证。失败包验收保留私有临时证据，仅对独立确认终态的测试 unit 执行清理；未清理其他会话改动。


后续整合说明：`iter-20261017-autotask-decoupling` 从最新已提交的 origin/main 整合发行工具，`646a557` 已提供参数能力。0.3.0 的 CLI 参数验收通过；本节前述缺口是当时落后主工作树的实际验证结果，不代表 0.3.0 仍缺此能力。
