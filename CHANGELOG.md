## v0.4.0 (2026-08-31)

### Feat

- add `--daemon-cfg` pass json to daemon process avoid missing args

### Fix

- add CREATE_NO_WINDOW flag to avoid spawn a terminal window on win
- use 127.0.0.1 instead of localhost resolution to avoid ip binding mismatch
- `serve --daemon --idle-lock-timeout` is not working

### Refactor

- extract main logic into run() for streamline error handling

## v0.3.0 (2026-08-29)

### Feat

- add sshkey to vault item

## v0.2.0 (2026-08-29)

### Feat

- add --wait-port-timeout option in bw serve
- add `--log-file` to getting bw serve daemon output

### Fix

- Skip shutdown request if addr unavailable to avoid test failure

## v0.1.0 (2026-08-24)

### Feat

- support any subcommand of bw
- add bw_serve_url option
- add `unlock --restart --raw` to start daemon
- add mimallc on linux-musl
- add bw unlock
- add tests
- idle shutdown
- build to `bw` bin
- optmize
- **cli**: bw serve 支持 --daemon/--stop/--restart 分发
- **cli**: 抽取 spawn_daemon 并补全 Windows daemon 拉起
- **cli**: daemon 管理函数（stop_daemon/wait_port_free/wait_port_ready）
- **cli**: bwrap serve 新增 --daemon/--stop/--restart 互斥 flag
- **agent_server**: 控制端点 /__bwrap/shutdown 优雅关闭
- add serve cmd impl
- init

### Fix

- Avoid infinite loop by not setting self-path as bw path in bw_external
- not compile on windows
- borrow_interior_mutable_const warn lint
- exit sucess when bw get item is not found
- daemon log file has color code
- **test**: wait_port_ready_when_listening 用端口 0 消除端口复用竞态
- **test**: stop_daemon_no_daemon 用 httpmock 消除端口复用竞态
- **cli**: 移除 windows creation_flags 多余 .0（PROCESS_CREATION_FLAGS 为 u32 别名）
- **agent_server**: shutdown 时记录子进程 kill 失败
- **agent_server**: 优雅关闭时显式回收 bw serve 子进程
