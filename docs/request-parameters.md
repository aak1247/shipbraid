# 项目请求参数

项目/环境通过对应 lane 声明执行参数。协调器负责校验、持久化、去重和传递，不推断备份等业务策略，也不把参数当命令执行。

```toml
[lanes.request_parameters.change_window]
required = true
allowed_values = ["scheduled", "emergency"]

[lanes.request_parameters.release_note]
required = false
```

v1 参数值为字符串；`required` 默认 true，未配置 `allowed_values` 时允许有界的非空字符串。最多 16 个参数，名称最长 64 字节，值最长 1024 字节。声明中不应包含秘密；参数作为发布意图进入审计。未知参数、缺少必填参数和非法枚举在受理前被拒绝。开发环境可以没有这些声明，其他环境可以要求额外参数；具体参数名和值由项目管理员定义。

HTTP 提交示例：

```json
{
  "project": "myapp",
  "environment": "dev",
  "revision": "<full-commit-sha>",
  "components": ["app"],
  "parameters": {"change_window": "scheduled"}
}
```

通用 CLI 使用可重复参数；同名键重复会拒绝：

```bash
shipbraid submit --project myapp --environment dev \
  --revision <full-commit-sha> --components app \
  --param change_window=scheduled
```

项目客户端可以用自己的发布命令传入相同参数。通过 `GET /v1/lanes` 或 `inspect lane` 可以查询 lane 的 `request_parameters` 声明；请求和执行详情返回实际 `parameters`。

命令和脚本从标准输入读取原有 hook 信封，webhook 接收相同 JSON。冻结的参数位于 `run.target.parameters`：

```python
request = json.load(sys.stdin)
parameters = request["run"]["target"]["parameters"]
change_window = parameters["change_window"]
# 具体脚本自行将已验证的枚举映射为受信 argv；不要拼成 shell 命令。
```

参数进入请求正文的幂等哈希，修改参数却复用原 key 返回冲突。同一版本、不同参数属于不同执行目标，即使允许后代版本覆盖也不共享执行或成功回执；重试保留原参数。旧请求缺失该字段按空参数读取，不补写推断值。项目声明发生变化也会改变 profile 指纹，让新要求不能悄悄套用到旧执行。
