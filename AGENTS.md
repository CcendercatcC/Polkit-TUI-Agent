# AGENTS.md

终端 polkit 认证代理（Rust，零 GTK/glib）。使用方式、手动测试见 README.md；
程序内架构详解（模块协同、数据流、并发模型、超时与安全边界）见
ARCHITECTURE.md。本文件是改代码时的事实手册：架构、模块职责、核心设计、
依赖、红线。

## 架构总览（inline 模式）

- 进程经 **system bus**（`org.freedesktop.PolicyKit1`）与 polkitd 双向通信：
  polkitd 调 `agent.rs` 实现的 `AuthenticationAgent` 接口（如
  `BeginAuthentication`、`CancelAuthentication`），认证完成后由 root 的 helper
  调 `AuthenticationAgentResponse2/3` 回报 polkitd，agent 不经 D-Bus 传密码。
- `agent.rs` 通过 `mpsc<UiEvent>` 把认证请求推给 `ui.rs`（ratatui TUI 状态机，
  对话框渲染与密码掩码输入）；UI 用 `oneshot<PromptAnswer>` 回传密码答案；
  `watch<bool>` 作取消令牌（取消时通知 UI 关框）。
- 密码经 `helper.rs` 用 **Unix socket / pipe** 传给 `polkit-agent-helper-1`
  （root 进程，由 systemd socket 激活拉起，PAM 认证）。
- 并发认证请求经 `Agent` 的 `Semaphore(1)` FIFO 串行化：同一时刻只弹一个框，
  逐个验证——inline 单对话框、tmux 单 popup 无法并发展示，否则后续请求会
  覆盖对话框 / 顶掉弹窗，导致旧请求失败、取消链断裂（弹窗残留）。

## 架构总览（tmux 模式）

`--tmux` 一体模式 = daemon 角色 + controller 角色，**同一个进程**：

- `Agent::daemon` 注册 system bus（接收 `BeginAuthentication`）；`Daemon::start`
  监听本地回环 socket；controller 任务自连该 socket。
- controller 收到请求后用 `tmux display-popup -E -T "polkit 认证" -w 70% -h 50%`
  起一个弹窗进程（`--prompt`，经 `-e POLKIT_*` 环境变量传参）。
- `--prompt` 弹窗进程自包含：ratatui 画对话框 + helper 认证，错密码框内重试，
  退出码 0 成功 / 2 取消。

分离部署（`--daemon` + `--controller`）时多一个 socket 转发层，适合 daemon 做
systemd 用户服务、controller 单独在 tmux 里跑；`--tmux` 一体模式把两者合并在
一个进程，controller 任务自连自己的 socket，完全复用分离部署的
Request/Response 与 `display-popup` 逻辑。

## 一次认证的完整时序

一次 pkexec 认证的完整调用链：

1. 发起方（pkexec / pkcheck 等）向 polkitd 发 `CheckAuthorization`（带
   `AllowUserInteraction`），polkitd 据请求方所在 session 找到 agent，调
   `BeginAuthentication` 并传入 `action_id`、`message`、`cookie`、`identities`。
2. agent 弹 TUI 对话框或 tmux 弹窗，经 socket 把 **用户名 + cookie** 发给 root
   的 `polkit-agent-helper-1`（systemd socket 激活拉起）。
3. helper 回 `PAM_PROMPT_ECHO_OFF` 提示，用户输入密码，密码经私有通道传回
   helper（不经过 D-Bus / socket NDJSON）。
4. helper 完成 PAM 认证后，以 root 身份调 `AuthenticationAgentResponse2/3`
   回报 polkitd。
5. polkitd 把授权结果返回发起方，pkexec 据此执行目标程序。

核心设计：

1. **`BeginAuthentication` 必须阻塞**到认证结束。它返回前，polkitd 的
   `CheckAuthorization` 一直挂起，`pkexec` 则等待授权结果。
2. **认证成功由 root 的 helper 通知 polkitd**（调用
   `AuthenticationAgentResponse2/3`），agent 自身无权也不需调用——这是密码
   不必经过 D-Bus 的原因，也是 setuid/socket helper 存在的意义。
3. **用户取消**时 agent 返回 `org.freedesktop.PolicyKit1.Error.Cancelled`，
   `pkexec` 会报 `Request dismissed`。
4. **发起方（如 pkexec）被终止**时，polkitd 通过 `NameOwnerChanged` 检测到后
   调用 agent 的 `CancelAuthentication`：agent 置位本地取消令牌，并通知
   controller 关弹窗。弹窗进程经取消文件（`$XDG_RUNTIME_DIR/polkit-tui-cancel-<hash>`）
   自行退出（退出码 2），`display-popup -C` 作兜底。

## 模块职责

| 文件 | 职责 |
|---|---|
| `src/main.rs` | 入口：四种模式分发、注册、inline TUI 事件循环 |
| `src/agent.rs` | D-Bus 服务端接口 `AuthenticationAgent`；`Backend` 抽象 Inline/Daemon 两种收集密码方式；并发认证经信号量 FIFO 串行逐个验证 |
| `src/daemon.rs` | socket 服务端：请求/响应队列、取消广播、Drop 清理守卫 |
| `src/controller.rs` | tmux 桥：读请求 → `display-popup -E` 起弹窗 → 映射退出码回报 |
| `src/prompt.rs` | 弹窗单请求认证（`--prompt`，自包含收密码 + helper 认证） |
| `src/protocol.rs` | daemon↔controller 的 NDJSON 线协议 |
| `src/helper.rs` | `polkit-agent-helper-1` 会话客户端：socket/setuid 双路径 + 行协议 |
| `src/ui.rs` | ratatui 状态（`App`）与对话框渲染，密码掩码输入 |

tmux 数据流：`AuthRequest` 经 socket NDJSON 传给 controller → 弹窗进程 → 结果
经 socket 回报。密码只存在于弹窗进程与 helper 的私有通道，socket 上只有结果。

## 依赖与 Cargo

| crate | 用途 |
|---|---|
| `ratatui` + `crossterm` | TUI 渲染与终端事件 |
| `tokio` | 异步运行时 |
| `zbus` / `zbus_polkit` | D-Bus（必须 `default-features=false, features=["tokio"]`，见红线） |
| `serde` / `serde_json` | daemon↔controller 的 NDJSON 线协议 |
| `uzers` | uid → 用户名 |

## 构建与验证
- 构建：`cargo build --release`（edition 2024，需 rustc ≥ 1.85）
- 仓库无任何测试、无 CI/lint 配置；`cargo test` 无有效用例
- **运行验证交给用户**：inline 需真实 tty、tmux 需 tmux 会话、真实认证依赖系统 polkit 与 root helper，agent 无法在会话内自测。改完只做编译级验证（`cargo build` / `cargo clippy` 若有），功能验证由用户手动执行

## 运行前置条件
- inline/`--prompt` 必须有可控终端（`tui.rs::has_controlling_tty()`，检查 `/dev/tty` 可打开）；TUI 输出走 `/dev/tty`，stdout/stderr 可任意重定向不影响界面
- tmux 相关模式（`--tmux`/`--controller`/`--prompt`）必须有 `$TMUX`
- 同一 session scope 只能注册一个 agent：niri 的 gnome agent 在跑时注册必失败（`An authentication agent already exists`），测试前先 `systemctl --user stop 'app-niri-polkit\x2dgnome\x2dauthentication\x2dagent\x2d1-2352.scope'`
- 真实认证依赖系统 polkit 与 root helper（`/run/polkit/agent-helper.socket`，socket 激活优先 / setuid 回退）
- daemon 模式需 logind session：session 解析链 = 进程直属 session（`GetSessionByPID`）→ uid 图形会话（`GetUser(uid).Display`，即 polkit 的 `sd_uid_get_display`）→ `XDG_SESSION_ID` 兜底，全取不到注册失败
- `--uid-session` 是「强制第二级」不是开关：进程无直属 session（tmux 窗格 / systemd 用户服务 / 桌面终端）时默认逻辑本就落到 uid 图形会话，带不带结果相同；仅当进程**有**直属 session（如 daemon 跑在 SSH 直属终端）时才强制注册到桌面图形会话
- 注册 session 必须与 polkit 判定一致，只信 `XDG_SESSION_ID` 会报 `Passed session and the session the caller is in differs`；注册日志打印 `session=<id>` 可核对
- `--prompt` 为内部模式：controller 以 `display-popup -E` 拉起，读 `POLKIT_COOKIE/USER/ACTION/MESSAGE`（另有 `POLKIT_CANCEL_FILE` 指向取消文件），勿手动运行

## 日志与调试选项
- 日志里 cookie 默认以 FNV-1a 哈希（16 位 hex）表示（`main.rs::log_cookie`）：
  能区分同一 agent 下的不同请求，且不泄露完整会话标识
- 排查时可加 `--full-cookie-log` 打印完整 cookie（与 polkitd/helper 日志对应）；
  全局开关由 `main()` 统一设置（`LOG_FULL_COOKIE`），各模块直接读
- 哈希有全局缓存（`COOKIE_LOG_CACHE`）：同一 cookie 的 begin/queued/cancel
  多行日志只算一次；full 模式不走缓存
- 取消文件路径复用同一 `fnv1a_hex`（controller.rs::cancel_file_path），
  勿另写一份 FNV

## 改代码时的红线
- zbus 必须保持 `default-features=false, features=["tokio"]`，不可换回 `async-io`
- agent.rs 的 D-Bus 接口方法用 `&self` + 内部 `Mutex`，勿改 `&mut self`（zbus 串行化会致 `CancelAuthentication` 排队饿死）
- 密码输入用 `Vec<char>`（`String` 下标 insert 多字节会 panic）；界面列宽按 CJK 计（`"密码: "` 是 6 列）
- `--prompt` 退出码即协议：0 成功 / 2 取消，controller 据此映射 `AuthResult`；勿在 `--prompt` 外套吞退出码的 shell
- 密码只走 agent↔root helper 私有通道；D-Bus 与 socket NDJSON（protocol.rs）上只有结果
- `zbus_polkit::Identity` 不可作 D-Bus 入参：只实现 `Serialize` 无 `Deserialize`，用元组 `(String, HashMap<String, OwnedValue>)` 手动解析（agent.rs:125）
- `Subject::subject_details` 的值是 `OwnedValue`，无 `From<String>`，需 `OwnedValue::from(Str::from(...))`（main.rs:436）
- logind 属性查询必须走 `PropertiesProxy::get`：`Properties.Get` 返回 variant（`v`），`Proxy::call` 直接反序列化具体类型报 `Signature mismatch`；且 `User.Display` 是 `(so)` 不是 `s`（见 `find_session_via_logind` / `find_uid_display_session`）
- TUI 输出走 `/dev/tty`（`tui.rs::Tui` = `ratatui::Terminal<CrosstermBackend<File>>`）：进 raw mode + alternate screen、panic hook、退出还原（`Drop`）都在 `Tui::open`/`Drop` 里；勿再用 `ratatui::init()/restore()`（绑 stdout），TUI 不占 stdout/stderr
- 日志走 stdout、报错走 stderr：`logging.rs::log_line` / `error_line`；TUI 活跃且 stdout 是终端时日志写屏幕左上角安全区（`log_line_to_corner`），勿向 stderr 直接 eprintln 日志（会落在 alternate screen 光标处污染输入框）
- `Terminal::clear()` 内部查询光标位置（CPR `\x1b[6n`），`EventStream` 在跑时占用 crossterm 全局事件读取器锁必然超时；强制整屏重绘用「空帧 + 正常帧」两段 draw（`run_tui` 的 `needs_full_redraw`）
- `--tmux` 一体模式 = daemon 自连：`Daemon::start` 起 socket 后再 `tokio::spawn(controller::run(socket))` 自连，复用分离部署的 Request/Response 与 `display-popup` 逻辑，勿另写一套
- `tmux display-popup` 只能从 tmux 会话内调用：后台 daemon 没有 tmux 客户端上下文，必须有 controller 进程（或 `--tmux` 内嵌控制器）作为桥

## 约定
- 注释与模块文档一律中文，沿用现有风格
- 缩进 2 空格（由 rustfmt.toml 的 tab_spaces=2 配置）
- 非 git 仓库，无分支/提交规范
