# auto-deliver

Rust 发布请求协调器，附带 React + TypeScript + Tailwind Dashboard。

它管理的是“上线需求”：已有执行覆盖需求时共享结果；紧急版本可以安全替代尚可停止的普通执行，旧请求继续等待后继验收。执行被替代后保留 `superseded`，请求只有拿到匹配的验收证据才会 `satisfied`。

支持 **ScriptedExecutor 模拟执行器**和管理员配置的 **command / script / webhook 外部执行器**。真实任务通过独立监督器运行，观察与验收分开；首个集成为 AutoTask 本机开发环境，生产保持禁用。见 [执行器协议与常驻安装](docs/hooks.md)。

## 本地模拟运行

需要 Rust ≥ 1.94、Git、Node.js 24、pnpm 11。当前验证环境和结果见 [验收记录](docs/acceptance/m1.md)。

```bash
cargo build --locked
mkdir -p data
# 受信的本地 bare mirror；协调器不会按客户端输入拉取仓库。
git clone --mirror . data/source.git
export AUTO_DELIVER_TOKEN="$(openssl rand -hex 32)"
export AUTO_DELIVER_EXECUTOR_TOKEN="$(openssl rand -hex 32)"
cargo run --locked -- serve --config examples/local.toml
```

另一个终端启动 Dashboard：

```bash
cd web
pnpm install --frozen-lockfile
pnpm dev
```

打开 `http://127.0.0.1:5187`，手动输入第一个终端的 `AUTO_DELIVER_TOKEN`。令牌只保存在当前页面内存中。Vite 将 API 请求代理到 `127.0.0.1:8787`；Dashboard 不内置凭据或示例状态。

Dashboard 可查看通道、请求与实际执行、优先级、阶段、替代关系、审计时间线和游标日志。Dashboard 为只读界面，提交和运维控制使用 API/CLI。

## 提交与等待

在相同令牌环境的终端中：

```bash
REVISION=$(git rev-parse HEAD)
cargo run --locked -- submit --lane demo-test --revision "$REVISION" \
  --components app --idempotency-key example-001
# 使用返回的 id；等待超时不撤回服务端请求。
cargo run --locked -- wait REQUEST_ID --timeout-seconds 120
cargo run --locked -- list request --lane demo-test
cargo run --locked -- inspect run RUN_ID
cargo run --locked -- logs RUN_ID
```

同一个幂等键重试必须保持相同正文。需要允许后代提交覆盖时显式传 `--coverage descendant`；Git 祖先关系只证明版本历史，不能证明功能没有被 revert。紧急请求使用 `--priority urgent`，同时需要令牌与通道授权。

`wait` 退出码：0 已满足、2 需处理、3 过期、4 撤回、124 本地等待超时、1 调用或配置错误。

配置样例使用独立操作者与执行器令牌。默认 loopback；对外访问需要受控 TLS 入口。通道权限、组件、仓库、发布线、配置摘要和抢占开关都由服务端配置，调用方不能提交任意命令或仓库地址。配置修改采用 drain 后重启。

## 检查

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 scripts/acceptance.py
cd web
pnpm typecheck
pnpm test
pnpm build
```

进程验收脚本只使用临时 Git 仓库、SQLite 和随机临时令牌。涵盖并发共享、幂等冲突、紧急替代、已覆盖紧急请求不重启、CLI 超时、SIGKILL 恢复与鉴权。

## 运行边界

单实例、单本地持久卷，SQLite WAL + FULL、启动独占锁；不使用 NFS。停止命令被接受不代表已经停止，未知状态继续占有通道，不能靠超时解锁。Dashboard 轮询不是调度事实源。

数据库备份应使用 SQLite 在线备份 API；不要在活动 WAL 状态下只复制 `.db` 文件。外部 hook 首版整体不可抢占，必须独立验收；只有具备停止证明和阶段准入的执行器才能在后续启用安全抢占。模拟器通过不代表真实部署已验收。当前环境新鲜证据复用、可配置自动重试、公平策略、完整故障矩阵和阶段指标仍待补齐；性能验收结果见 [验收记录](docs/acceptance/m1.md)。具体实现覆盖与待验收项以 [执行计划](docs/execution-plan.md) 为准。

## 设计与实施

- [外部执行器、环境配置与常驻部署](docs/hooks.md)
- [设计文档](docs/design.md)
- [执行计划与任务状态](docs/execution-plan.md)
- [前后端实施契约](docs/implementation-contract.md)
- [Agent 工作约束](AGENTS.md)
