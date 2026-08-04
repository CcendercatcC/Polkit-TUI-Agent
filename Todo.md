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
- [ ] **README.md:205 / README_cn.md:176**：声称 daemon/controller 的
      `begin_authentication/cancel_...` 日志打印到 stderr；实现中 `log_line`
      走 stdout（logging.rs:34-40），仅 `error_line` 走 stderr。改为 stdout。
  - 测试：`cargo run -- --daemon` 后观察日志落在 stdout（`2>/dev/null` 仍可见
    日志、`1>/dev/null` 不可见）；与 logging.rs 声明一致。
- [ ] **ARCHITECTURE.md:184,470 / ARCHITECTURE_cn.md:164,426**：声称
      `pick_username` 「unix-group 跳过」；实现中若首个候选是 unix-group 且
      未命中当前用户/root，`identities.first().and_then(identity_uid)?`
      直接返回 None → 整个认证 Failed，并非「跳过」。措辞改为
      「仅接受 unix-user 候选；全是 unix-group 时返回失败」。
  - 测试：纯文档措辞修正，人工比对 agent.rs:151-169 即可。

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
- [ ] **`--prompt` 钓鱼面**（prompt.rs:25，Low）：`--prompt` 无调用者校验，
      任何进程可 `tmux display-popup -E ... --prompt` 弹外观一致的伪造
      polkit 认证框。**危害边界**：攻击者必须是同 uid 或已入侵受害者 tmux
      会话；密码明文只发给 root 的 helper，**不会回流攻击者进程**；真实危害
      是「诱导授权」（用从 daemon socket 窃听到的真实 cookie + 伪造 UI 文案，
      诱导用户为攻击者发起的高危提权输密码）与降低社会工程门槛。最小缓解：
      controller 生成一次性令牌经 `-e` 传给 prompt、prompt 校验；收益有限
      （挡不住同 uid 攻击者自写假 TUI）。
  - 测试（修复前证明存在）：
    ```bash
    tmux new -d -s t
    tmux send-keys -t t 'POLKIT_COOKIE=fake POLKIT_USER=root POLKIT_MESSAGE=phishing ./target/release/polkit-tui-agent --prompt' Enter
    ```
    修复后此手动调用应被拒绝（报令牌缺失），controller 正常路径仍可用。
- [ ] **剥离 TUI 渲染文本的控制字节**（ui.rs:270,283 / controller.rs:198）：
      `message`/`action_id` 来自 polkitd，若含 ANSI/ESC 会原样输出到
      `/dev/tty`（终端注入）。渲染前过滤 `\x1b` 等控制字符。
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
- exe 路径双引号拼接进 shell（controller.rs:180）：路径含 `$`/反引号时会被
  展开，但路径由用户安装位置决定，风险低。可选：单引号转义。
- cookie 经 `-e POLKIT_COOKIE` 暴露于 argv/environ（controller.rs:192）：
  同 uid 进程可读，但 cookie 泄露无法伪造成功认证（root helper 才可调
  `AuthenticationAgentResponse2/3`）。设计权衡，保留。
- 排队期间取消与 reply 竞态可能返回 `Failed` 而非 `Cancelled`
  （agent.rs:221-245）：仅影响 pkexec 报错文案，概率低。
- daemon socket 显式 chmod 0700（daemon.rs:74）：删除 /tmp 回退后，socket
  始终位于 `$XDG_RUNTIME_DIR`（目录本身 0700），chmod 属纯纵深防御，可选。
