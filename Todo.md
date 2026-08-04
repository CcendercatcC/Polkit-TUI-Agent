# Todo

代码审查（2026-08-04）发现的待办项。分「文档与实现不符」「安全修复」「已评估、
暂不处理」三类，每项附验收测试方法。所有真实认证测试的前置条件：

- 停掉同 scope 的其它 agent：`systemctl --user stop 'app-niri-polkit\x2dgnome\x2dauthentication\x2dagent\x2d1-2352.scope'`
- 确认 root helper socket 存在：`ls /run/polkit/agent-helper.socket`
- 用真实 tty / tmux 会话运行，agent 无法在会话内自测

## 文档与实现不符

- [x] **AGENTS.md「构建与验证」**（2026-08-04 已修）：原文声称「仓库无任何
      测试、无 CI/lint 配置」，实际已存在 `.github/workflows/ci.yml`
      （fmt/clippy/build）与 release.yml。已更新为「CI 走 GitHub Actions」。
- [x] **README.md:205 / README_cn.md:176**（2026-08-04 核对已符，条目作废）：
      原以为 README 声称日志走 stderr；核对当前 README.md:204-205 与
      README_cn.md:176-178 均已写 stdout，与 logging.rs 的 `log_line`
      （stdout）/`error_line`（stderr）一致，无需修改。
- [x] **ARCHITECTURE.md:186,496 / ARCHITECTURE_cn.md:164,444 / agent.rs:150**
      （2026-08-04 已修）：`pick_username` 原文「unix-group 跳过」；实际若
      候选全是 unix-group，`identities.first().and_then(identity_uid)?`
      直接返回 None → 整个认证 Failed，并非「跳过」。已改为「仅接受
      unix-user 候选；全是 unix-group 时返回失败」，并同步修了源码注释。
- [x] **AGENTS.md:86 / AGENTS.md:35,130**（2026-08-04 已修）：`## 依赖与
      Cargo` 章节标题紧贴上一句缺换行；`--prompt` 退出码描述只写「0 成功 /
      2 取消」，遗漏等待输入超时的退出码 1（映射 Failed）。

## 安全修复

- [x] **删除 /tmp socket 回退**（2026-08-04 已修）：`default_socket_path()`
      在 `XDG_RUNTIME_DIR` 缺失时不再回退 `/tmp/polkit-tui-agent-{uid}.sock`，
      改为返回错误。原因是 /tmp 1777 下无法安全绑定：其他用户可 unlink 该
      socket 文件（controller 连不上，功能 DoS）、可抢先 bind 同一路径
      （`Daemon::start` 的 stale 探测 connect 成功 → 拒绝启动）、可删文件后
      自 bind 冒充 daemon 窃听认证请求（cookie/用户名）。`XDG_RUNTIME_DIR`
      是 systemd 用户会话的标准位置（0700），daemon 模式本就要求 logind
      session，缺失即报错更合理。
  - 测试：
    ```bash
    unset XDG_RUNTIME_DIR
    ./target/release/polkit-tui-agent --daemon   # 期望报错退出，不再创建 /tmp socket
    ./target/release/polkit-tui-agent --controller  # 同样报错退出
    ```
- [x] **删除 cancel file 的 /tmp 回退**（2026-08-04 已修，controller.rs:219）：
      `cancel_file_path` 在 `XDG_RUNTIME_DIR` 缺失时不再回退 temp_dir。
      所有入口（`--controller`/`--tmux`/`--daemon`）在到达 controller 之前都
      经 `default_socket_path()` 校验过 `XDG_RUNTIME_DIR`，缺失已提前退出，
      此处 fallback 永远不会走到。
  - 测试：同上，`XDG_RUNTIME_DIR` 缺失时各模式均在启动阶段报错。
- [x] **`--prompt` 钓鱼面**（prompt.rs:35，Low，2026-08-04 已修）：`--prompt`
       无调用者校验，任何进程可 `tmux display-popup -E ... --prompt` 弹外观
       一致的伪造 polkit 认证框。**危害边界**：攻击者必须是同 uid 或已入侵
       受害者 tmux 会话；密码明文只发给 root 的 helper，**不会回流攻击者
       进程**；真实危害是「诱导授权」（用从 daemon socket 窃听到的真实 cookie
       + 伪造 UI 文案，诱导用户为攻击者发起的高危提权输密码）与降低社会工程
       门槛。修复方式随「全面走 socket」一并完成：`--prompt` 只认 `POLKIT_SOCK`
       （临时 socket 路径，含 `fnv1a_hex(cookie)` 不可预测），缺失即报错退出，
       手动调用不带 `POLKIT_SOCK` 直接被拒；cookie/user/action/message 不再
       从环境变量读取。
  - 测试（修复前证明存在）：
    ```bash
    tmux new -d -s t
    tmux send-keys -t t 'POLKIT_COOKIE=fake POLKIT_USER=root POLKIT_MESSAGE=phishing ./target/release/polkit-tui-agent --prompt' Enter
    ```
    修复后此手动调用应报 `POLKIT_SOCK not set` 拒绝，controller 正常路径仍可用。
- [x] **cookie 经 `-e POLKIT_COOKIE` 暴露于 environ**（controller.rs:192，
      Medium，2026-08-04 已修）：cookie 写进弹窗进程 environ，同 uid 进程可读
      `/proc/<pid>/environ` 拿到真实 cookie。**危害边界**：cookie 本身无法
      伪造成功认证（root helper 才可调 `AuthenticationAgentResponse2/3`），
      但配合上方「钓鱼面」条目会降低社工门槛；另同 uid 已能从 daemon socket
      （peer_cred 仅校验同 uid）读到 Request 中的 cookie，故属纵深防御。
      修复：随「全面走 socket」一并完成——弹窗只经 `POLKIT_SOCK` 临时 socket
      读请求，所有请求字段（含 cookie）不再进入 environ。
  - 测试（手动）：弹窗进程 environ 中不再有 `POLKIT_COOKIE`
    （只剩 `POLKIT_SOCK` 路径），正常认证仍可用。
- [x] **popup↔controller 通讯全面走 socket，禁止通过 `-e` 环境变量传参**
      （controller.rs:191-201 / prompt.rs:26-29，2026-08-04 已修）：现有设计把
      cookie、user、action、message、cancel_file 全部经 `tmux
      display-popup -e` 塞进弹窗进程 environ，同 uid 可被动枚举
      `/proc/<pid>/environ` 拿到全部请求上下文。修复：controller 在每
      次弹窗前起一个临时 `UnixListener`（`$XDG_RUNTIME_DIR/polkit-tui-
      popup-<fnv1a_hex>`），仅把 socket 路径经 `-e POLKIT_SOCK=<path>` 传入；
      popup 启动后连上该 socket、收到一行 `AuthRequest` NDJSON（cookie、
      user、action、message）后**保持连接**；polkitd 取消时 controller 经
      同一连接写一行 `ServerMsg::Cancel` NDJSON，弹窗读到即退出（退出码 2）。
      controller accept 一次即关 listener。user/action/message 虽不敏感但
      一并迁出 env 可消除 `/proc/environ` 的信息泄露面。与现有 daemon↔
      controller NDJSON socket 完全同模式（socket + 行协议），无新增依赖。
  - 测试（手动）：弹窗期间 `tr '\0' '\n' < /proc/<pid>/environ | head`
    应只含 `POLKIT_SOCK` 路径，cookie/user/action/message 均不可见；
    正常认证仍可用。
- [x] **删除取消文件机制，取消信号走弹窗 socket**（2026-08-04 已修）：
      原设计用 `$XDG_RUNTIME_DIR/polkit-tui-cancel-<hash>` 取消文件 + 弹窗
      200ms 轮询。改造后取消与请求共用同一条弹窗 socket：controller 在
      `ServerMsg::Cancel` 时经共享取消通道通知弹窗连接写任务，往同一条
      socket 写一行 `ServerMsg::Cancel` NDJSON；`--prompt` 的取消读任务读到
      匹配 cookie 即退出码 2。整个取消文件机制（`cancel_file_path`、写/删
      文件、轮询任务、`POLKIT_CANCEL_FILE`）已删除。`tmux display-popup -C`
      兜底保留。
  - 测试：Ctrl-C 终止 pkexec 后弹窗仍即时关闭（走 socket 取消信号）。
- [ ] **剥离 TUI 渲染文本的控制字节**（ui.rs:270,283,299,308，Medium）：
      渲染面所有外来文本（`message`/`action_id`/PAM 状态消息）若含 ANSI/ESC
      会原样输出到 `/dev/tty`（终端注入：清屏、改色、伪造输出）。渲染前过滤
      `\x1b` 等控制字符。
  - **威胁来源分析**：
    - `message`/`action_id` 表面来自 polkitd 的 action 策略 XML（root 所有），
      但 polkitd 按 action 模板展开变量——如 `org.freedesktop.policykit.exec`
      的 `$(program)`/`$(user)` 由**调用方**（pkexec 命令行）注入，恶意构造的
      程序路径/用户名可携带 ESC 序列进入 `message`。
    - PAM 状态文本（`UiEvent::Status`，agent.rs:442-461；prompt.rs 失败消息）
      来自 helper 的 `PAM_ERROR_MSG`/`PAM_TEXT_INFO`，部分 PAM 模块会把用户
      输入回显其中，同样原样进终端。
  - **修复建议**：统一在 `draw_dialog_at`（ui.rs:257）构造 Span 前对
    message/action_id/username/status 调 `sanitize()`（剔除 C0 控制字节，
    保留 `\t`/`\n`），一处收口，勿散落各调用点。
  - 测试（单元测试即可，无需真实 polkit）：
    ```rust
    #[test]
    fn sanitize_strips_esc() {
      assert!(!sanitize("bad\x1b[2Jmessage").contains('\x1b'));
      assert_eq!(sanitize("normal text"), "normal text");
    }
    ```
    手动验证：临时把 `UiEvent::Prompt` 的 message 硬编码为含 `\x1b[2J` 的
    字符串，肉眼观察对话框渲染后终端未被清屏/改色。

## 已评估、暂不处理

- **inline 认证重试无总时长上限**（agent.rs:336，撤回，非缺陷）：每轮等待
  密码有 30s 空闲超时（`sleep_until(last_activity + input_timeout)`），用户
  不操作即超时关闭并返回 Failed，不会无限挂起；仅当用户持续主动输入/输错
  密码才延续，属正常交互，不是程序缺陷。
- exe 路径双引号拼接进 shell（controller.rs:208）：Bash **双引号内** `$`/反引号/
  `\` 仍会被展开（非「可能」），若安装路径含这些符号会破坏 `display-popup`
  命令；但路径来自 `env::current_exe()`，由用户自己的安装位置决定，属自我
  影响而非外部注入，风险低。可选：单引号转义（须同时处理路径中的 `'`）。
- **`COOKIE_LOG_CACHE` 无界增长**（main.rs:199-200，Low）：FNV 哈希缓存只增
  不删，代理长期运行（数周/数月）会累积内存；每条目约几十字节，千次认证仅
  几十 KB，实际影响可忽略。可选：定期清空 / 上限淘汰 / 改为只算不缓存。
- 排队期间取消与 reply 竞态可能返回 `Failed` 而非 `Cancelled`
  （agent.rs:221-245）：仅影响 pkexec 报错文案，概率低。
- daemon socket 显式 chmod 0700（daemon.rs:74）：删除 /tmp 回退后，socket
  始终位于 `$XDG_RUNTIME_DIR`（目录本身 0700），chmod 属纯纵深防御，可选。
