# Shipbraid 改名验收

日期：2026-10-17。结果：PASS。验证对象是基线 `9e2dad7` 加本轮改名补丁的隔离 worktree；其他会话正在进行的执行器恢复修改没有并入该验证副本或本轮提交。

## 标识

- Cargo package、Rust library、CLI 为 `shipbraid`，Dashboard package 为 `shipbraid-dashboard`。
- CLI 环境变量为 `SHIPBRAID_URL`、`SHIPBRAID_TOKEN`、`SHIPBRAID_TOKEN_FILE`；示例/脚本/安装器统一使用 `SHIPBRAID_*`。
- 新服务和 LAN unit 模板为 `shipbraid.service`、`shipbraid-lan.service`、`shipbraid-lan.socket`。
- 新安装目录和新任务 unit 使用 shipbraid；Dashboard 标题、页脚、开发文档与许可证项目名称同步。
- `target/debug/shipbraid --version` 实际输出 `shipbraid 0.2.0`；help 显示 `Usage: shipbraid` 和新环境变量名。

## 检查

以下均在隔离改名副本通过：

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 scripts/acceptance.py
python3 scripts/acceptance-hooks.py
python3 scripts/acceptance-agents.py
python3 scripts/acceptance-cli.py
python3 integration/autotask/test_bridge.py
python3 deploy/test_install_local.py
pnpm --dir web install --frozen-lockfile --offline
pnpm --dir web typecheck
pnpm --dir web test
pnpm --dir web build
```

Rust 49 个测试、前端 10 个测试、Bridge 7 个测试、安装器预检 5 个测试通过；原模拟 10 组、hooks 4 组、多 Agent 8 组及 CLI 9 组隔离场景通过。测试使用新二进制和新环境变量，未执行真实部署（各验收报告 `real_deployments=0`）。依赖版本未更改，Cargo.lock 仅修改本包名称，pnpm lock 无变更。

初次隔离前端准备尝试共享 node_modules 软链被 pnpm 安全校验拒绝；改为独立 offline frozen 安装后，上述全部前端检查通过。没有删除共享工作区的依赖目录。

当前文档链接与 diff 空白检查通过；旧名扫描在当前代码中仅保留旧安装检测、历史执行身份兼容及相应测试。README clone URL 使用仍真实配置的远端地址，历史验收文件未改写。

## 兼容与运行边界

安装器拒绝活动的新/旧协调器，无法确认 systemd 状态时拒绝继续，检测旧数据/配置/unit 时也拒绝自动创建新账本。测试证明原数据保持不变。

Bridge 的新任务使用 shipbraid 前缀；已有 run/generation 的旧 executor_ref 保持原值继续观察，错误身份仍拒绝。此兼容不绕过冻结配置和环境验收。

本轮不重启现有服务、不迁移真实数据库、不改 token、不改变已有执行/请求/receipt，不移动远端仓库或 checkout 目录。旧环境变量没有隐式别名。维护升级要求见[命名与升级说明](../naming.md)。
