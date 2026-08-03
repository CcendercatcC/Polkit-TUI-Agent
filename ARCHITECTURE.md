# polkit-tui-agent 程序内架构

本文档描述**本程序内部**的架构：模块之间的依赖、进程与任务怎么组成、数据
怎么流、并发与同步点、超时与安全边界。不涉及 polkit 协议本身的细节（那部分
见 AGENTS.md）。面向要改代码的开发者；阅读前提是先对四种运行形态有印象
（见 README.md）。

## 1. 总览

单个 Rust binary（`src/main.rs` 一个 crate），10 个模块，零外部二进制依赖
（只调 `tmux` 与系统 `polkit-agent-helper-1`）。程序没有"库 crate 加多个
二进制"的划分——**所有模块都被 `main.rs` 以 `mod` 声明进同一个 crate**，
各运行形态靠 `main` 按命令行旗标分派，复用同一批组件。

五种运行形态（四种用户可见 + 一个内部）：

| 形态 | 旗标 | 进程数 | 组件装配 |
|---|---|---|---|
| inline TUI | （默认） | 1 | `Agent::inline` + `run_tui` 事件循环 |
| 后台 daemon | `--daemon` | 1 | `Agent::daemon` + `Daemon::start` + socket 服务端 |
| tmux 控制器 | `--controller` | 1（独立） | `controller::run`，连 daemon 的 socket |
| tmux 一体 | `--tmux` | 1 | daemon 组件 + controller 任务自连，同一进程 |
| 弹窗（内部） | `--prompt` | 1（短命） | 事件循环 + helper 认证，由 controller 拉起 |

关键洞察：**`--tmux` 就是 `--daemon` 加一个 `controller::run` 任务**；
`--prompt` 是唯一"自带完整认证循环"的进程，其它形态要么借 UI 事件循环
（inline）要么借弹窗进程（daemon/controller）来完成收密码。

## 2. 模块依赖

编译期依赖（`mod` 引用）用文字描述：`main` 声明全部 10 个模块；`agent` 依赖
`daemon`/`helper`/`protocol`/`ui`；`daemon`、`controller` 依赖 `protocol`；
`prompt` 依赖 `helper`/`ui`/`tui`；`tui` 依赖 `logging`。`helper`/`protocol`/
`ui`/`logging` 不依赖任何 crate 内模块（`ui` 用 `tokio::sync::oneshot`，
`logging` 用 `crossterm`/`unicode-width`）。

```
            ┌──────────────────────────────────────────┐
            │                 main.rs                  │
            │  分派 / 参数 / session 解析 / run_tui     │
            └───┬─────┬──────┬──────┬──────┬───────────┘
                ▼     ▼      ▼      ▼      ▼
             agent  daemon controller prompt  tui
               │      │       │       │      │
               │      └───┬───┘       │      ▼
               │          ▼           │   logging
               │        protocol      │
               │          ▲           │
               ├───────► helper ◄─────┤   （prompt 另依赖 tui，
               └───────► ui ◄─────────┘     agent 依赖 protocol/helper/ui）
```

箭头表示 `use` 依赖方向。运行期协作见下表：

| 提供方 | 消费方 | 协作内容 |
|---|---|---|
| `agent::Agent` | `main` | 导出为 D-Bus 对象（`object_server().at(OBJECT_PATH, ...)`） |
| `agent` | `ui` | 经 `mpsc<UiEvent>` 推送认证事件；`UiEvent::Prompt` 携带 `oneshot::Sender` |
| `ui` | `agent` | 经 oneshot 回传 `PromptAnswer`（Submit/Cancel） |
| `agent` | `helper` | `HelperSession`：socket/二进制双路径连 root helper，行协议应答 |
| `agent` | `daemon` | `daemon.request(AuthRequest) -> AuthResult`、`daemon.cancel(cookie)` |
| `daemon` | `controller` | socket NDJSON：`ServerMsg`（Request/Cancel）/ `ClientMsg`（Response） |
| `controller` | `prompt` | 经 `tmux display-popup -E` + `POLKIT_*` 环境变量拉起 `--prompt` |
| `main`/`prompt` | `tui` | `Tui::open` 拿 `/dev/tty` 终端句柄，`Terminal::draw` 渲染 |
| `main`/`prompt` | `ui` | `App` 状态机 + `render`/`render_full` |
| `main`/`daemon`/`tui` | `logging` | `log_line`（stdout）/`error_line`（stderr）/`set_tui_active` |

## 3. 进程与任务装配

### 3.1 inline（默认）

一个进程，两类并发工作：

- **zbus D-Bus 派发**：`Connection::system()` + 把 `Agent::inline` 挂到
  `OBJECT_PATH`。zbus 开着 `tokio` feature，接口方法（`begin_authentication`
  等）跑在 `#[tokio::main]` 的运行时上；`begin_authentication` 阻塞等待认证，
  `cancel_authentication` 并发插入。
- **`run_tui` 事件循环**（main.rs）：一个 `tokio::select!` 同时等三路——
  crossterm `EventStream`（键盘）、`mpsc<UiEvent>`（agent 推送）、100ms tick
  （周期性重绘）。每次 select 结束统一 `terminal.draw` 一帧。

两路认证事件经 `mpsc<UiEvent>` + `oneshot<PromptAnswer>` 桥接，键盘活动经
`watch<Instant>` 回流供空闲超时判定；TUI 输出走 `/dev/tty`（`tui.rs`），
`ui_tx` 由 `run_tui` 持有一份，防止通道因发送端全部 drop 而关闭。并发认证
请求由 agent 的 `slot: Semaphore(1)` FIFO 串行，同一时刻只弹一个对话框。

### 3.2 后台 daemon（`--daemon`）

headless，无任何 TUI。任务拓扑：

- 主流程：注册 agent（`Agent::daemon`）→ `Daemon::start` → `object_server().at`
  → `std::future::pending::<()>()` 常驻。
- `Daemon::start` 绑定 socket 后 spawn 一个 `accept_loop`。
- `accept_loop` 每接一个连接 spawn 一个 `handle_connection`。
- 每个 `handle_connection`：校验对端 uid → 注册为「当前 controller」→ spawn
  一个写任务（把 `ServerMsg` 从 mpsc 写成 NDJSON 行）→ 本任务循环读 `ClientMsg`。

daemon 自己不做任何密码收集，`begin_authentication` 把 `AuthRequest` 交给
`daemon.request` 后阻塞，等 controller 回报。并发认证在 agent 层排队
（`slot: Semaphore(1)`），同一时刻只向 controller 发一个请求，controller 的
单弹窗不被顶掉。

### 3.3 tmux 控制器（`--controller`）

独立进程，运行在 tmux 会话内。`run` 永不返回（除非异常）：外层循环
`connect_with_retry` 连 daemon socket，连上后：

- spawn `write_loop` 任务（`ClientMsg` → socket）。
- 读循环解析 `ServerMsg`：`Request` 时 spawn 一个 `run_popup` 任务（弹窗期间
  读循环不被阻塞，`Cancel` 能及时处理）；`Cancel` 时按 cookie 关闭弹窗并记入
  `cancelled` 集合（防「取消先到、弹窗请求后到」竞态）。
- 断线后 `write_task.abort()`，回到外层循环重连。

每个弹窗由 `run_popup` 用 `tmux display-popup -E` 起一个 `--prompt` 子进程，
等它退出后把退出码映射成 `AuthResult` 经 mpsc→`write_loop`→socket 回报。

### 3.4 tmux 一体（`--tmux`）

即 3.2 与 3.3 合并在一个进程：`tmux_main` 先做 daemon 的全部初始化，然后
`tokio::spawn(controller::run(socket))` 让 controller **自连自己进程起的
socket**。内嵌 controller 任务与远程 controller 行为完全一致，只是对端是本
进程的 daemon。`--tmux` 是用户推荐用法，但对实现而言没有引入任何新逻辑。

### 3.5 弹窗进程（`--prompt`）

由 controller 在 `display-popup` 内启动的短命进程，自包含一次认证。任务：

- 主事件循环（`select!` 四路）：键盘、`mpsc<Outcome>`（后台认证任务回报）、
  `watch<bool>`（取消文件）、100ms tick。
- 每提交一次密码 spawn 一个 `authenticate_once` 任务连 helper 做 PAM 认证，
  结果经 `Outcome` 送回主循环（成功 break 0、失败 `app.retry` 回编辑态）。
- 若设置了 `POLKIT_CANCEL_FILE`，额外 spawn 一个 200ms 轮询任务，发现文件即
  置位取消 watch。

## 4. 模块内部结构

### 4.1 `main.rs`（入口 + 装配 + inline 事件循环）

| 项 | 内容 |
|---|---|
| `OBJECT_PATH` | `/org/EMeow/PolicyKit1/AuthenticationAgent`，注册时传给 polkitd 的 object path |
| `Options` | `locale`（默认 `$LANG`）、`uid_session` |
| 分派 | `main` 依次查 `--prompt`/`--controller`/`--daemon`/`--tmux`，默认 inline；`-h/--help` 全程优先 |
| `inline_main` | 检查控制终端 → 连 system bus → 建 `mpsc<UiEvent>` → 挂 Agent → `build_subject` → `register` → `run_tui` → 退出后注销 agent |
| `tmux_main`/`daemon_main` | 注册 + `Daemon::start` + 挂 Agent + `pending()` 常驻；`--tmux` 额外 spawn controller 自连 |
| `run_tui` | 三路 `select!`；空对话框时才允许 `q`/Ctrl-C 退出；`needs_full_redraw` 用「空帧+正常帧」两段 draw |
| `send_answer` | 从 `app.active` 的 `reply` 字段 `take()` 出 oneshot，发 `PromptAnswer` |
| `build_subject` | `--uid-session` 时 `find_uid_display_session`，否则 `find_session_id` |
| session 解析 | `find_session_id` = 直属 session（`GetSessionByPID`）→ uid 图形会话（`GetUser.Display`，`(so)` 结构体）→ `XDG_SESSION_ID` 兜底 |
| `register` | `RegisterAuthenticationAgent`，日志打印实际 session-id 供核对 |
| `log_cookie` | 日志里的 cookie 表示：默认 FNV-1a 哈希（`fnv1a_hex`，能区分同一 agent 下不同请求）；`--full-cookie-log` 时打印完整值；哈希有全局缓存 |

### 4.2 `agent.rs`（D-Bus 接口 + 认证状态机）

| 项 | 内容 |
|---|---|
| `PolkitError` | `#[zbus(prefix="org.freedesktop.PolicyKit1.Error")]`，`Cancelled`/`Failed` 两个 D-Bus 错误名 |
| `Backend` | `Inline { events, activity }` / `Daemon { daemon: Arc<Daemon> }`，决定密码在哪收集；`activity` 是键盘活动回流通道 |
| `Agent` | `backend` + `pending: Mutex<HashMap<String, watch::Sender<bool>>>`（cookie→取消令牌）+ `slot: Semaphore`（容量 1，并发认证 FIFO 串行） |
| `begin_authentication` | 选用户名 → 注册取消令牌 → `select!`（排队中取消 / 获取 slot）→ 按后端分发；结束清理令牌表并释放名额 |
| `cancel_authentication` | 置位 watch + 按后端发 `UiEvent::Cancel` 或 `daemon.cancel` |
| `authenticate_inline` | 认证主循环：发 Prompt → `select!`（取消/答复/超时/键盘活动刷新）→ 连 helper → PAM 行协议循环 → SUCCESS/Dismiss 或失败重试 |
| `pick_username` | 候选 identity 偏好：当前用户 → root → 第一个候选；`unix-group` 跳过 |
| `identity_uid` | 手动解析 `(String, HashMap<String, OwnedValue>)`，取 `unix-user` 的 uid |

### 4.3 `daemon.rs`（socket 服务端）

| 项 | 内容 |
|---|---|
| `Daemon` | `pending: Arc<Mutex<HashMap<u64, oneshot::Sender<AuthResult>>>>`（请求 id→应答）、`active: Arc<Mutex<ActiveController>>`、`conn_seq`（generation）、`next_id` |
| `ActiveController` | `Option<(u64, mpsc::Sender<ServerMsg>)>`，同一时间只认一个当前 controller |
| `PendingGuard` | Drop 时移除 pending 表项，保证请求被放弃/超时后表不泄漏 |
| `start` | 清残留 socket（可连上则报"another daemon"）、bind、spawn `accept_loop` |
| `request` | 分配 id → 插表 → 取当前 controller → 发 `ServerMsg::Request` → 120s 超时等应答；超时主动 `cancel` |
| `cancel` | 按 cookie 发 `ServerMsg::Cancel` |
| `accept_loop` | 每连接 spawn `handle_connection` |
| `handle_connection` | peer_cred 校验 uid → generation 递增并覆盖 active → spawn 写任务 → 读循环（`ClientMsg::Response` 唤醒对应请求）→ 断开时仅当仍是当前 generation 才清 active，并 drain `pending` 全部判 Failed（不等 120s 超时） |

### 4.4 `controller.rs`（tmux 桥）

| 项 | 内容 |
|---|---|
| `run` | 外层重连循环 + 读循环；`current: Arc<Mutex<Option<String>>>` 记录当前弹窗 cookie、`cancelled: Arc<Mutex<HashSet<String>>>` 记录已取消 cookie（防「取消先到、弹窗请求后到」竞态） |
| `connect_with_retry` | 2s 间隔重连，daemon 未起时持续等待 |
| `write_loop` | 把 `ClientMsg` 序列化为 NDJSON 行写 socket |
| `run_popup` | `tmux display-popup -E -T "polkit 认证" -w 70% -h 50% -e POLKIT_* <exe> --prompt`；退出码 0→Ok / 2→Cancel / 其他→Failed |
| `cancel_file_path` | `$XDG_RUNTIME_DIR/polkit-tui-cancel-<FNV-1a hash>`，cookie 含非文件名安全符号时用 hash 派生 |

`Request` 处理不阻塞读循环：先记录 current cookie，spawn `run_popup` 任务，
弹窗结束清理取消文件与 current 标记后回报。`Cancel` 到达即记入 `cancelled`
集合；若 cookie 匹配 current 则写取消文件 + `tmux display-popup -C` 兜底关闭；
此后若同 cookie 的 `Request` 才到，直接回报取消不弹窗。

### 4.5 `prompt.rs`（弹窗单请求认证）

| 项 | 内容 |
|---|---|
| `run` | 读 `POLKIT_*` 环境变量 → `Tui::open` → `App::open_prompt` → 四路 `select!` 事件循环 → 退出码 |
| `Outcome` | `Success` / `Failure` / `Error(String)`，后台认证任务的回报 |
| `authenticate_once` | 快照用户名/cookie/密码，10s 连 helper，30s 单消息 PAM 循环，返回 `Outcome` |
| 取消文件轮询 | `POLKIT_CANCEL_FILE` 存在即置位 watch → 主循环 break 2 |
| 空闲超时 | 编辑态且 `last_activity.elapsed() >= input_timeout` → break 1（非 0/2，映射 Failed） |

### 4.6 `helper.rs`（polkit-agent-helper-1 客户端）

| 项 | 内容 |
|---|---|
| `SOCKET_PATH`/`HELPER_BIN` | `/run/polkit/agent-helper.socket`（socket 激活优先）/ `/usr/lib/polkit-1/polkit-agent-helper-1`（setuid 回退） |
| `Inner` | `Socket { reader, writer }`（`Box<dyn AsyncRead/Write>`）或 `Binary { reader, writer, _child, _stderr }`（持有 Child 防被 drop 杀进程） |
| `connect` | socket 存在则连并按协议写「用户名、cookie」两行；否则 spawn 二进制、用户名走 argv、stdin 写 cookie |
| `write_line` | 分两次写避免拼串分配 |
| `respond` | 把密码/文本写回 helper |
| `next_message` | 逐行解析：`PAM_PROMPT_ECHO_OFF/ON`、`PAM_ERROR_MSG`、`PAM_TEXT_INFO`、`SUCCESS`、`FAILURE`；未知命令按 `Info` 容错 |

### 4.7 `protocol.rs`（NDJSON 线协议）

`AuthRequest`（cookie/user/action/message）、`AuthResult`（Ok/Cancel/Failed）、
`ServerMsg`（`Request{id,req}` / `Cancel{cookie}`）、`ClientMsg`
（`Response{id,result}`）。都用 `#[serde(tag="type", rename_all="lowercase")]`
做标签化。密码永不进入这些消息。

### 4.8 `ui.rs`（状态 + 渲染）

| 项 | 内容 |
|---|---|
| `PromptAnswer` | `Submit(String)` / `Cancel` |
| `UiEvent` | `Prompt`（携带 oneshot 应答通道与上一轮 status）/ `Cancel` / `Status` / `Dismiss`，都带 cookie |
| `PromptState` | `Editing` / `Verifying`，渲染与输入行为据此分支 |
| `App` | `active: Option<ActivePrompt>` + `input: Vec<char>` + `cursor` |
| `ActivePrompt` | cookie/username/message/action_id/status/state + `reply: Option<oneshot::Sender>`（`Option` 便于 `take()`） |
| `handle_key` | Esc/Ctrl-C 取消、Enter 提交（空密码拒绝、切 Verifying）、Backspace/方向键/Home/End 编辑；Verifying 态只认取消 |
| `on_event` | 消费 `UiEvent`，所有事件按 cookie 校验匹配当前对话框 |
| `open_prompt` | `--prompt` 用，无 oneshot 应答通道 |
| `retry` | 失败后回 Editing、清输入、更新状态行 |
| `render`/`render_full` | inline 居中 60%×40% / 弹窗铺满全屏；无对话框时不画（空帧即清屏） |
| `draw_dialog_at` | Clear → 标题/用户/消息/状态/掩码行 → 边框 → 光标定位（`PASSWORD_LABEL_W`=6 列，按 CJK 计） |

### 4.9 `tui.rs`（`/dev/tty` 终端封装）

| 项 | 内容 |
|---|---|
| `has_controlling_tty` | 启动守卫：`/dev/tty` 能否以读写打开 |
| `Tui` | `tty: File`（Drop 还原）+ `terminal: Terminal<CrosstermBackend<File>>`（绘制目标） |
| `Tui::open` | 开 tty → raw mode → alternate screen → panic hook → 构建 Terminal；任一步失败都还原 |
| `install_panic_hook` | 保存前一 hook，panic 时先 `disable_raw_mode` 再 `LeaveAlternateScreen` |
| `Drop` | 同样序还原，任何退出路径（含 panic 展开）都保证终端不被破坏 |
| `PANIC_TTY` | 全局 `Mutex<Option<File>>`，panic hook 用它操作终端 |

### 4.10 `logging.rs`（日志通道）

| 项 | 内容 |
|---|---|
| `TUI_ACTIVE` | `AtomicBool`，TUI 进入/还原时置位/复位 |
| `log_line` | 日志走 stdout；TUI 活跃且 stdout 是终端时改写屏幕左上角安全区 |
| `log_line_to_corner` | `SavePosition → MoveTo(0,0) → Clear(CurrentLine) → Print(按列宽截断) → RestorePosition` 一段字节一次写 |
| `error_line` | 报错走 stderr，始终原文 |

## 5. 数据流追踪

### 5.1 inline：认证请求到密码回传

```
 polkitd ──BeginAuthentication──▶ begin_authentication（agent.rs）
                                     │ ① pick_username → 注册 cookie→watch
                                     │ ② mpsc<UiEvent>::Prompt{oneshot,status}
                                     ▼
                                 App 对话框（ui.rs）◀── 键盘 EventStream（run_tui）
                                     │ ③ Action → oneshot.send(PromptAnswer)
                                     ▼
                             authenticate_inline 拿到密码
                                     │ ④ HelperSession::connect（10s 超时）
                                     ▼
                         polkit-agent-helper-1（root · PAM）
                                     │ ⑤ 行协议循环（30s/消息）
                                     │    PAM_ERROR/TEXT_INFO → UiEvent::Status
                                     │ ⑥ SUCCESS → UiEvent::Dismiss
                                     ▼
                             返回 Ok → polkitd 放行（root helper 代调 Response2/3）
```

1. polkitd 调 `begin_authentication` → `pick_username` 选身份 → 往
   `pending` 表插入 `(cookie, watch::Sender)`。
2. `authenticate_inline` 每轮新建 oneshot，发 `UiEvent::Prompt`（带
   `reply_tx`、上一轮 status）到 `mpsc<UiEvent>`。
3. `run_tui` 的 `ui_events` 分支收到后置 `needs_full_redraw`，`app.on_event`
   打开对话框。
4. 键盘事件经 `app.handle_key` 产生 `Action` → `send_answer` `take()` 出
   oneshot → `reply.send(PromptAnswer)`。
5. agent 的 `select!` 收到密码 → 连 helper → PAM 循环；过程中的
   `PAM_ERROR_MSG`/`PAM_TEXT_INFO` 经 `UiEvent::Status` 实时刷到对话框状态行。
6. `SUCCESS` → 发 `UiEvent::Dismiss` 关框 → `begin_authentication` 返回 `Ok`；
   失败 → 更新 status 回到第 2 步重试。

### 5.2 daemon 链路：请求转发、弹窗、结果回报

```
 begin_authentication（Daemon 后端）
       │ ① daemon.request(req)
       ▼
 ┌────────────────────────┐  ② ServerMsg::Request{id,req}  ┌───────────────────┐
 │  daemon（socket 服务端） │ ─────────────────────────────▶ │ controller（读循环） │
 │  id → pending 表        │         （socket NDJSON）       │ current=cookie     │
 └────────────────────────┘                                └─────────┬─────────┘
       ▲                                                           │ ③ spawn run_popup
       │ ⑥ ClientMsg::Response{id,result}                           │    tmux display-popup -E
       │    pending.remove(id) → oneshot                            │    -e POLKIT_COOKIE/...
       └────────────────────────────────────────────────────────────┘
                                                                     ▼
                                                           ┌──────────────────┐
                                                           │ --prompt 弹窗进程  │
                                                           │ ④ App + helper PAM│
                                                           │ ⑤ 退出码 0/2/其他   │
                                                           └──────────────────┘
```

1. `begin_authentication`（Daemon 后端）→ `daemon.request(AuthRequest)`。
2. `request` 分配 `id`，把 oneshot 插入 `pending` 表，向当前 controller 发
   `ServerMsg::Request{id, req}`，`select!` 等取消令牌或应答。
3. controller 读循环收到 → 记录 current cookie → spawn `run_popup`。
4. `run_popup` 用 `tmux display-popup -E` 起 `--prompt`，经 `-e POLKIT_*`
   传 cookie/user/action/message/cancel_file，命令体是 `"<exe>" --prompt`。
5. `--prompt` 进程内：`App::open_prompt` 画框 → 提交后 spawn
   `authenticate_once` → helper PAM → 退出码。
6. controller 把退出码映射 `AuthResult` → `ClientMsg::Response{id, result}` →
   socket → daemon 读循环 → `pending.remove(id)` → oneshot 送进 `request`。
7. `begin_authentication` 按 `AuthResult` 返回 Ok/Cancelled/Failed。

### 5.3 取消链路（polkitd 主动取消）

```
 polkitd ──CancelAuthentication(cookie)──▶ cancel_authentication（agent.rs）
                                             │ pending[cookie].watch.send(true)
                                             ▼
                                  begin_authentication 的 select! 命中 → 返回 Cancelled
                                             │
                     ┌───────────────────────┴────────────────────────┐
                     ▼ inline                                      ▼ daemon
              UiEvent::Cancel 关框                          daemon.cancel(cookie)
                                                                    │ ServerMsg::Cancel
                                                                    ▼
                                                             controller 匹配 current
                                                                     │ 写取消文件 + display-popup -C
                                                                     ▼
                                                             --prompt 轮询到文件 → exit 2
```

1. polkitd 调 `cancel_authentication(cookie)`。
2. agent 置位 `pending[cookie]` 的 watch；inline 后端再发 `UiEvent::Cancel`，
   daemon 后端调 `daemon.cancel(cookie)`。
3. inline：`authenticate_inline` 的 `select!` 命中 `cancel_rx.changed()` → 发
   `UiEvent::Dismiss` → 返回 `Cancelled`。
4. daemon 链路：`daemon.cancel` 发 `ServerMsg::Cancel`；controller 匹配
   current cookie → 写取消文件 + `tmux display-popup -C`；`--prompt` 轮询到
   文件退出 2；controller 回报的迟到 `Response` 无害（daemon 已通过本地
   取消令牌返回，`pending` 表项已被 `PendingGuard` 清理）。

### 5.4 关键对象生命周期

- **cookie**：从 polkitd 传入起贯穿 agent→daemon→controller→`--prompt`→
  helper，是取消文件、`UiEvent`、`ServerMsg::Cancel` 的对齐键。
- **请求 id**：`next_id` 递增，仅存在于 daemon 的 `pending` 表与 socket 消息，
  用于关联 controller 的回报。
- **取消令牌（watch）**：`begin_authentication` 插入、结束清理；`cancel` 只
  查表置位，两者互不阻塞。
- **`PendingGuard`**：Drop 清理保证 `request` 被 `select!` 放弃或超时时
  `pending` 表不残留。
- **socket 文件**：`Daemon::start` 时若残留且可连接则拒启，否则删除重绑。

## 6. 并发与同步模型

所有异步都跑在同一个 `#[tokio::main]` 多线程运行时；`select!` 决定谁能
抢先，`Mutex` 只保护小片共享状态，不存在长时间持锁。

`tokio::spawn` 点：

| 位置 | 任务 | 说明 |
|---|---|---|
| `daemon.rs start` | `accept_loop` | socket 接受循环 |
| `daemon.rs accept_loop` | `handle_connection` | 每连接一个 |
| `daemon.rs handle_connection` | 写任务 | 把 `ServerMsg` mpsc → NDJSON 写 socket |
| `controller.rs run` | `write_loop` | `ClientMsg` → socket |
| `controller.rs run` | `run_popup` 任务 | 每请求一个，弹窗期间读循环不被阻塞 |
| `main.rs tmux_main` | `controller::run` | 自连内嵌控制器 |
| `prompt.rs run` | 取消文件轮询 | 200ms 查文件 |
| `prompt.rs run` | `authenticate_once` | 每次提交一个，主循环保持响应 |

`tokio::select!` 点：`main.rs run_tui`（keys/ui_events/tick）、
`agent.rs begin_authentication`（排队：cancel/acquire；认证：cancel/request）、
`agent.rs authenticate_inline`（cancel/reply/timeout/activity）、`prompt.rs run`
（keys/outcome/cancel/tick）。

`Mutex` 保护点：`agent.pending`（取消令牌表）、`Daemon.pending`（请求应答表）、
`Daemon.active`（当前 controller）、`controller.current`（当前弹窗 cookie）、
`controller.cancelled`（已取消 cookie 集合）、`PANIC_TTY`。`AtomicBool`：
`TUI_ACTIVE`；`AtomicU64`：`conn_seq`（连接 generation）、`next_id`（请求序号）。

## 7. 超时体系

| 超时 | 位置 | 默认 | 触发后果 |
|---|---|---|---|
| helper 连接 | `agent.rs`/`prompt.rs` | 10s | 判失败，回到重试 |
| PAM 单条消息 | `agent.rs`/`prompt.rs` | 30s | 判失败，回到重试（防 PAM 挂起） |
| 输入空闲 | `agent.rs`/`prompt.rs` | 30s（`POLKIT_TUI_TIMEOUT` 覆盖） | inline：整轮失败；prompt：退出码 1 |
| daemon 响应 | `daemon.rs request` | 120s | 主动 `cancel` 关弹窗，返回超时错误 |

inline 与弹窗的实现路径不同但语义一致——**键盘输入、提交、验证失败都算活动**：

- **inline（`agent.rs`）**：UI 每次按键经 `activity` watch 通道回流刷新
  `last_activity`；提交、验证失败回到循环顶部同样刷新。只有持续无操作才超时。
- **弹窗（`prompt.rs`）**：`last_activity` 在键盘事件与验证失败回报时刷新，
  仅编辑态计入超时判定（验证中不计）。

## 8. 安全边界

- **密码只在私有通道**：agent ↔ root helper 之间走 Unix socket 或匿名
  stdin/stdout 管道；D-Bus、socket NDJSON、环境变量、日志里都没有密码。
- **daemon socket 只收同用户**：`handle_connection` 校验 `peer_cred().uid()`
  等于当前 uid；socket 位于 `$XDG_RUNTIME_DIR`（0700）内，peer_cred 是纵深
  防御。
- **防双 daemon**：`Daemon::start` 对残留 socket 先探测能否连接，能连即拒启。
- **取消文件**：仅作取消信号（内容固定 `"cancel"`），路径含 FNV-1a hash，
  不泄露 cookie 明文。
- **弹窗传参用环境变量**：`run_popup` 经 `-e` 传请求字段，命令体只拼
  当前 exe 路径（加引号防空格），避免把消息内容插进 shell。
- **身份解析**：手动解 `(String, HashMap)` 元组，跳过 `unix-group` 等不支持
  的 identity；未知 PAM 命令按 `Info` 容错不误伤。
- **诊断脱敏**：日志里 cookie 默认以 FNV-1a 哈希（16 位 hex）表示
  （`log_cookie`），完整会话标识不外泄；排查时加 `--full-cookie-log` 打印全量。

## 9. 已知行为差异（inline vs 弹窗）

文档如实记录、代码未强求一致的差异点：

- **PAM 消息展示**：inline 把 `PAM_ERROR_MSG`/`PAM_TEXT_INFO` 经
  `UiEvent::Status` 显示到对话框状态行；弹窗的 `authenticate_once` 不展示，
  直接按 FAILURE/EOF 判失败（密码错误通常由 helper 回 `FAILURE`，影响有限）。
- **验证中取消**：弹窗在 Verifying 态仍接受 Esc/Ctrl-C 取消（`run` 的键盘
  分支先于 `handle_key` 判断）；inline 的 `App::handle_key` 在 Verifying 态
  返回 `None`，且 `run_tui` 的退出判断要求 `active.is_none()`，故 inline
  验证中不可取消，只能等结果。
