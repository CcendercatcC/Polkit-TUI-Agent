# polkit-tui-agent

一个用 Rust 编写的 **终端 polkit 认证代理**（authentication agent），为
ssh / tmux 等无图形界面场景设计。

> This project is developed with AI assistance.

当 `pkexec`、`pkcheck` 等机制发起权限提升请求时，polkit 需要一个「认证代理」
来弹出密码输入框。本程序提供两种 UI：

- **inline TUI**：在运行它的终端里画对话框（ratatui）。
- **tmux popup**：在 tmux 会话内注册并常驻，认证请求出现时用 `tmux display-popup`
  在屏幕正中央悬浮弹窗（`--tmux` 一体模式，或 `--daemon` + `--controller` 分离部署）。

纯 Rust 实现，零 GTK/glib 依赖。

## 功能特性

- 纯终端渲染（ratatui + crossterm），无任何图形依赖
- 只使用 **system bus**，不依赖 `DBUS_SESSION_BUS_ADDRESS`（ssh 会话可用）
- tmux 悬浮弹窗（`display-popup`），不占用你的布局
- `--tmux` 一体模式：单进程搞定注册 + 弹窗，无需两个进程
- helper 双路径：systemd socket 激活优先，setuid 二进制回退
- 密码全程不经过 D-Bus，只在 agent ↔ root helper 的私有通道传递
- 错误密码自动重试、Esc/Ctrl-C 取消、10s 连接超时兜底（helper 连接，inline 与弹窗模式均有）
- 并发认证请求 FIFO 排队：同时触发多个 pkexec 时逐个弹框验证，不互相挤掉；排队期间取消的请求不弹框、不占位
- PAM 会话单条消息 30s 超时：helper 长时间无响应（PAM 挂起）即判失败重试，不卡死认证
- 等待输入密码超时（默认 30s，`POLKIT_TUI_TIMEOUT` 环境变量可覆盖）：空闲超时语义——输入、提交、验证失败反馈都算活动并刷新计时，只有持续无操作才超时
- daemon 侧 120s 认证超时兜底：controller 迟迟不回报结果时自动失败，避免认证永久挂起
- daemon socket 校验对端 uid（`SO_PEERCRED`），只接受同用户 controller 连接
- 取消认证双通道关弹窗：取消文件让弹窗进程自行退出 + `display-popup -C` 兜底

## 编译与运行

```bash
cargo build --release
```

四种运行形态（外加 `--prompt` 内部弹窗模式）：

| 模式 | 命令 | 场景 |
|---|---|---|
| inline TUI | `./target/release/polkit-tui-agent` | 直接在当前终端弹框 |
| **tmux 一体（推荐）** | `./target/release/polkit-tui-agent --tmux` | 在 tmux 窗格内跑，请求时悬浮弹窗 |
| 后台服务 | `./target/release/polkit-tui-agent --daemon` | headless，配 systemd user 服务 |
| tmux 控制器 | `./target/release/polkit-tui-agent --controller` | 配 `--daemon` 分离部署，必须在 tmux 窗格内 |
| 弹窗（内部） | `./target/release/polkit-tui-agent --prompt` | 由 controller 自动拉起，勿手动运行 |

选项：

| 参数 | 说明 |
|---|---|
| `--locale <LOCALE>` | 传给 polkitd 的 locale，默认取 `$LANG` |
| `--full-cookie-log` | 日志里打印完整 polkit cookie 而非 FNV-1a 哈希（排查/调试用） |
| `-h, --help` | 帮助 |

### tmux 一体模式（推荐用法）

```bash
# 在 tmux 的某个窗格里启动（可放一个专门的小窗格/窗口）
./target/release/polkit-tui-agent --tmux
```

之后任何 `pkexec` / `pkcheck` 请求都会在你屏幕正中央弹出认证框。

### 分离部署（systemd user 服务 + tmux 控制器）

`~/.config/systemd/user/polkit-tui-agent.service`：

```ini
[Unit]
Description=polkit-tui-agent daemon
After=dbus.service

[Service]
Type=simple
# --uid-session：注册到 uid 图形会话，服务 SSH attach 的 tmux 窗格/桌面进程
# 发起的认证请求（桌面 polkit agent 的行为）；若只在本地桌面会话内使用可去掉
ExecStart=/home/EMeowSystem/Documents/Rust/polkit-tui-agent/target/release/polkit-tui-agent --daemon --uid-session
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now polkit-tui-agent
# tmux 窗格里再跑一个控制器
./target/release/polkit-tui-agent --controller
```

### 与其他认证代理共存

同一 session scope 同时只能有一个 agent——这是 polkit 的限制：
`RegisterAuthenticationAgent` 对同一 subject 只接受一个注册，`fallback` 选项
也不会改变这一点（它只在匹配时生效）。若本机已运行
polkit-gnome-authentication-agent 等，本程序注册会失败：

```
An authentication agent already exists for the given subject
```

**共存不可行**，只能停掉现有 agent 改用本程序（例如 niri 的 gnome agent）：

```bash
systemctl --user stop 'app-niri-polkit\x2dgnome\x2dauthentication\x2dagent\x2d1-2352.scope'
```

> 注：polkit 的进程 scope（`unix-process`）注册只能服务「该进程自身」发起的
> 认证请求，无法服务其他进程（如 `pkexec`），因此没有可用的共存方案。

### SSH 下使用

polkit 只按「请求进程与 agent 是否**同一 session**」匹配，所以 agent 注册的
session 必须等于**请求被 polkit 算到的 session**。SSH 登录本身就是一个 logind
session（`XDG_SESSION_ID` 由 pam_systemd 注入），SSH 终端里直接跑 inline 或
`--tmux` 即可：

```bash
# 方案一：SSH 终端里直接跑 inline（agent 注册到当前 SSH 会话）
./target/release/polkit-tui-agent

# 方案二：SSH 会话内启动 tmux，在窗格里跑 --tmux
tmux new -As main
# 窗格内：
./target/release/polkit-tui-agent --tmux
```

**从 tmux 窗格发起提权**时另有讲究：如果 tmux server 是从桌面终端启动的
（SSH 只是 attach），窗格进程不在任何 logind session 的 cgroup 里，polkit 会
按 uid 把它算进**桌面图形会话**——这种情况下只有注册在桌面会话的 agent
才能收到请求，桌面 gnome agent 正是这么工作的。要让本程序获得同样行为，
注册时加 `--uid-session`：

```bash
# 桌面侧：注册到 uid 图形会话（桌面 polkit agent 的行为）
./target/release/polkit-tui-agent --daemon --uid-session
# SSH 的 tmux 窗格里：连 daemon 弹框
./target/release/polkit-tui-agent --controller
```

之后从 SSH attach 的 tmux 窗格（乃至桌面环境）发起的 `pkexec` 都会弹框。
注意 `--uid-session` 注册的是桌面会话，与桌面上的 gnome agent 冲突，需先停掉
后者（见上节）。

## 测试

inline 模式：

```bash
# 终端 A：启动 agent
./target/release/polkit-tui-agent
# 终端 B（同会话）：触发一次认证
pkexec echo ok
```

tmux 模式：

```bash
# tmux 窗格 A：一体模式
./target/release/polkit-tui-agent --tmux
# tmux 窗格 B：触发认证
pkexec echo ok
```

验证点：弹框 → 错误密码显示「认证失败，请重试」→ Esc 取消（pkexec 报
`Request dismissed`）→ 正确密码执行成功。

附加验证点：
- 弹窗出现后不操作，30s 后自动关闭、pkexec 报认证失败（时长可经
  `POLKIT_TUI_TIMEOUT` 调整）
- 弹窗出现后 Ctrl-C 终止发起提权的 pkexec，弹窗应立即自动关闭；daemon 与
  controller 的 stderr 会打印 `begin_authentication/cancel_authentication/
  daemon cancel/controller cancel` 日志便于核对取消链路。

## 许可

GPL-3.0-or-later，见 [LICENSE](LICENSE)。

> 开发者文档：程序内架构见 [ARCHITECTURE.md](ARCHITECTURE.md)，改代码注意事项见 [AGENTS.md](AGENTS.md)。
