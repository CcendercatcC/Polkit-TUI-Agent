//! # polkit-tui-agent 入口模块
//!
//! 程序启动入口，负责按命令行旗标分发到四种运行形态（见 `main`），外加
//! `--prompt` 内部弹窗模式：
//!
//! | 模式 | 旗标 | 说明 |
//! |---|---|---|
//! | inline TUI | （默认） | 进程内自带 TUI，本文件实现事件循环 |
//! | tmux 一体 | `--tmux` | 单进程：注册 + 弹 tmux popup（daemon+controller 自连） |
//! | 后台服务 | `--daemon` | headless，systemd user 服务，socket 转发请求 |
//! | tmux 控制器 | `--controller` | tmux 窗格内，把请求变成悬浮弹窗 |
//! | 弹窗 | `--prompt` | 内部模式，由 controller 在 popup 里启动，单次认证 |
//!
//! 注册相关逻辑（`register`/`build_subject`）对所有需要连接 system bus 的
//! 模式共享。session 解析对齐 polkit 的判定口径，见 `find_session_id`。
//!
//! ## inline 模式的数据流（三个异步任务的桥接）
//!
//! ```text
//!   polkitd ──BeginAuthentication──▶ Agent 任务 (agent.rs)
//!                                         │ mpsc<UiEvent>（携带 oneshot 应答通道）
//!                                         ▼
//!   TUI 事件循环 ──键盘事件──▶ App (ui.rs) ──oneshot<PromptAnswer>──▶ Agent 任务
//!   （main.rs run_tui）          ▲                 │
//!                               └─ redraw ◀────────┘
//! ```
//!
//! `mpsc<UiEvent>` 是 agent→UI 的单向队列；每次要求输入密码时随
//! `UiEvent::Prompt` 送出一个 `oneshot::Sender`，用户按下回车/取消后通过它把
//! 答案送回正在阻塞的 `begin_authentication`。三路事件（键盘、agent 认证事件、
//! tick）在 `tokio::select!` 中并行等待。
//!
//! ## tmux 模式的数据流
//!
//! `--tmux`/`--daemon` 下 `begin_authentication` 把 `AuthRequest` 经 socket
//! 交给 controller（`--tmux` 时是本进程自连），controller 用
//! `tmux display-popup -E` 起一个 `--prompt` 弹窗进程完成密码收集与 helper
//! 认证，再把结果经 socket 送回。

mod agent;
mod controller;
mod daemon;
mod helper;
mod logging;
mod prompt;
mod protocol;
mod tui;
mod ui;

use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;
use zbus::fdo::PropertiesProxy;
use zbus::zvariant::{OwnedValue, Str, Value};
use zbus::{Connection, Proxy};
use zbus_polkit::policykit1::{AuthorityProxy, Subject};

use crate::agent::Agent;
use crate::ui::{Action, App, PromptAnswer, UiEvent};

/// Agent 对象在调用者 unique name 上导出的路径。
///
/// 注册时传给 polkitd 的 `object_path` 是「我们的对象在 *unique bus name*
/// 上的路径」，不是 well-known name 的路径。libpolkit-agent 默认用它，
/// 我们保持约定：`/org/<厂商>/PolicyKit1/AuthenticationAgent`。
const OBJECT_PATH: &str = "/org/EMeow/PolicyKit1/AuthenticationAgent";

/// 命令行选项。
struct Options {
  /// 传给 polkitd 的 locale，用于本地化 action 的消息文本。
  locale: String,
  /// 注册到 uid 的图形会话而非进程直属 session，见 `build_subject`。
  uid_session: bool,
}

/// 打印帮助信息（`--help`）。
///
/// 用 `concat!` 逐行拼接：`\` 续行会吞掉缩进，逐字面量才能保留对齐。
fn print_help() {
  print!(
    "{}",
    concat!(
      "polkit-tui-agent - terminal polkit authentication agent (inline TUI / tmux popup)\n",
      "\n",
      "USAGE:\n",
      "    polkit-tui-agent [OPTIONS]            inline TUI mode (default)\n",
      "    polkit-tui-agent --tmux [OPTIONS]     all-in-one tmux mode (run inside a tmux pane)\n",
      "    polkit-tui-agent --daemon [OPTIONS]   headless service (systemd user service)\n",
      "    polkit-tui-agent --controller         tmux bridge (run inside a tmux pane)\n",
      "    polkit-tui-agent --help\n",
      "\n",
      "MODES:\n",
      "    (default)     run the TUI in the current terminal\n",
      "    --tmux        register and show floating tmux popups from a single process;\n",
      "                  this is the primary tmux usage\n",
      "    --daemon      headless background service; forwards authentication requests\n",
      "                  to a controller over a unix socket (for split deployment)\n",
      "    --controller  run inside a tmux pane; shows floating popups for requests\n",
      "                  from the daemon (used together with --daemon)\n",
      "    --prompt      internal: one-shot auth dialog spawned inside a tmux popup by\n",
      "                  the controller (not for direct use)\n",
      "\n",
      "OPTIONS:\n",
      "    --locale <LOCALE>    locale passed to polkit (default: $LANG)\n",
      "    --uid-session         register against the uid's graphical (display)\n",
      "                          session instead of this process's own session;\n",
      "                          use this when auth requests come from processes\n",
      "                          outside any logind session (e.g. tmux panes\n",
      "                          attached over ssh) - same behavior as a desktop\n",
      "                          polkit agent\n",
      "    --full-cookie-log     print full polkit cookies in logs instead of their\n",
      "                          FNV-1a hash (debugging; default is the hash)\n",
      "\n",
      "    -h, --help           print this help\n",
      "\n",
      "TMUX MODE:\n",
      "    1. run `polkit-tui-agent --tmux` inside a tmux pane (or use --daemon as a\n",
      "       systemd user service plus --controller in a tmux pane)\n",
      "    2. authentication requests (e.g. `pkexec ...`) then pop up as a floating\n",
      "       tmux window\n",
    )
  );
}

/// 构造注册时用的 `Subject`：`unix-session`。默认 session-id 由 `find_session_id`
/// 解析（对齐 polkit 的判定口径，见该函数）；`--uid-session` 时强制用 uid 的
/// 图形会话（桌面 polkit agent 的行为），适用于认证请求来自无 logind session
/// 的进程（如从 SSH attach 的 tmux 窗格）的场景。polkitd 用注册的 subject 对
/// 请求方进程做匹配。
///
/// 注意 `Subject::subject_details` 的值是 `OwnedValue`，从字符串转换要用
/// `OwnedValue::from(Str::from(...))`——`OwnedValue` 没有直接 `From<String>`。
async fn build_subject(conn: &Connection, opts: &Options) -> Result<Subject, String> {
  let session_id = if opts.uid_session {
    find_uid_display_session(conn).await
  } else {
    find_session_id(conn).await
  }
  .ok_or_else(|| {
    "cannot determine current logind session; run inside a logind session or set XDG_SESSION_ID"
      .to_string()
  })?;
  session_subject(&session_id)
}

/// 向 polkitd 注册认证代理，返回一个 `AuthorityProxy` 供退出时注销用。
///
/// 同一 scope 上只能注册一个 agent：已有其他 agent 时
/// `RegisterAuthenticationAgent` 会失败（"An authentication agent already
/// exists"）。polkit 不允许两个 agent 共用同一 scope，无法共存。
async fn register<'a>(
  conn: &'a Connection,
  subject: &Subject,
  opts: &Options,
) -> zbus::Result<AuthorityProxy<'a>> {
  let authority = AuthorityProxy::new(conn).await?;
  authority
    .register_authentication_agent(subject, &opts.locale, OBJECT_PATH)
    .await?;
  // 打印实际注册的 session-id，方便核对与 polkit 判定的 session 是否一致。
  logging::log_line(&format!(
    "polkit-tui-agent: registered on {OBJECT_PATH} (subject={}, session={}, locale={})",
    subject.subject_kind,
    subject_session_id(subject),
    opts.locale
  ));
  Ok(authority)
}

/// 取出注册 subject 里的 session-id（`subject_details["session-id"]`）。
fn subject_session_id(subject: &Subject) -> String {
  subject
    .subject_details
    .get("session-id")
    .map(|v| match v.deref() {
      Value::Str(s) => s.to_string(),
      other => format!("{other:?}"),
    })
    .unwrap_or_default()
}

/// 日志模式：`true` 打印完整 cookie；`false`（默认）打印 FNV-1a 哈希。
///
/// 由 `--full-cookie-log` 开关决定，`main()` 在模式分发前统一设置，各模块
/// 只读此全局即可（含 `--controller` 这种不走 `parse_args_from` 的模式）。
static LOG_FULL_COOKIE: OnceLock<bool> = OnceLock::new();

/// cookie → FNV-1a 哈希 的缓存：同一认证的 begin/queued/cancel 多行日志只算
/// 一次 hash，避免每输出一行就重复计算。cookie 生命周期内查询次数很少
/// （2-4 次），无界 map 的内存增长（每次认证几十字节）可忽略。
static COOKIE_LOG_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

/// 日志里的 cookie 表示：默认 FNV-1a 哈希（能区分同一 agent 下的不同请求），
/// `--full-cookie-log` 时打印完整 cookie 用于排查。
pub(crate) fn log_cookie(cookie: &str) -> String {
  if *LOG_FULL_COOKIE.get_or_init(|| false) {
    return cookie.to_string();
  }
  let mut cache = COOKIE_LOG_CACHE.lock().unwrap();
  cache
    .entry(cookie.to_string())
    .or_insert_with(|| fnv1a_hex(cookie))
    .clone()
}

/// FNV-1a（64 位），输出 16 位 hex。
pub(crate) fn fnv1a_hex(s: &str) -> String {
  let mut h: u64 = 0xcbf29ce484222325;
  for b in s.as_bytes() {
    h ^= *b as u64;
    h = h.wrapping_mul(0x100000001b3);
  }
  format!("{h:x}")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args: Vec<String> = std::env::args().skip(1).collect();

  // 任意模式下 --help 都优先打印帮助。
  if args.iter().any(|a| a == "-h" || a == "--help") {
    print_help();
    return Ok(());
  }

  // 日志里的 cookie 表示：默认 FNV-1a 哈希，--full-cookie-log 时打印完整值。
  let _ = LOG_FULL_COOKIE.set(args.iter().any(|a| a == "--full-cookie-log"));

  if args.iter().any(|a| a == "--prompt") {
    // 弹窗模式：由 controller 在 tmux popup 里启动，无 D-Bus。
    let code = prompt::run()
      .await
      .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    std::process::exit(code);
  }
  if args.iter().any(|a| a == "--controller") {
    // tmux 控制器：桥接 daemon 与 display-popup。
    controller::run(&default_socket_path()?).await?;
    return Ok(());
  }
  if args.iter().any(|a| a == "--daemon") {
    // 后台服务：headless 注册 + socket 服务端。
    daemon_main(&args).await?;
    return Ok(());
  }
  if args.iter().any(|a| a == "--tmux") {
    // 一体模式：单进程同时扮演 daemon 与 controller。
    tmux_main(&args).await?;
    return Ok(());
  }
  // 默认：inline TUI。
  inline_main(&args).await
}

/// daemon 与 controller 的 socket 路径：`$XDG_RUNTIME_DIR/polkit-tui-agent.sock`。
///
/// 只接受 `XDG_RUNTIME_DIR`（systemd 用户会话的标准 0700 位置），缺失时返回
/// 错误而非回退 /tmp：/tmp 是 1777 的共享目录，无法安全承载 socket——其他
/// 用户可删除该 socket 文件（controller 连不上，功能 DoS）、可抢先 bind 同一
/// 可预测路径（`Daemon::start` 的 stale 探测 connect 成功 → 误判「另一 daemon
/// 已存在」拒绝启动）、可删文件后自 bind 冒充 daemon 窃听认证请求。daemon
/// 模式本就要求 logind session，取不到 XDG_RUNTIME_DIR 即报错退出更合理。
fn default_socket_path() -> Result<String, &'static str> {
  std::env::var("XDG_RUNTIME_DIR")
    .map(|d| format!("{d}/polkit-tui-agent.sock"))
    .map_err(|_| {
      "XDG_RUNTIME_DIR is not set; cannot place the socket securely (run inside a systemd user session or set XDG_RUNTIME_DIR)"
    })
}

/// tmux 一体模式：一个进程同时做 daemon 与 controller 的事。
///
/// - 本进程注册认证代理（同 daemon）。
/// - 起本地 socket 服务端，再 `tokio::spawn` 一个 controller 任务**自连**
///   自己的 socket——于是 popup 弹窗逻辑被原样复用，无需第二个进程。
/// - 必须运行在 tmux 会话内部（`display-popup` 需要 tmux 客户端上下文）。
async fn tmux_main(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
  if std::env::var("TMUX").is_err() {
    return Err("polkit-tui-agent: --tmux must run inside a tmux session".into());
  }
  let opts = parse_args_from(args).map_err(|e| format!("{e}\nTry --help"))?;
  // 尽早校验 XDG_RUNTIME_DIR（socket 与一次性弹窗 socket 的宿主目录），缺失时先报错，
  // 不无谓连接 system bus。
  let socket = default_socket_path()?;

  let conn = Connection::system().await?;
  let subject = build_subject(&conn, &opts)
    .await
    .map_err(|e| e.to_string())?;
  register(&conn, &subject, &opts).await?;

  let daemon = daemon::Daemon::start(PathBuf::from(&socket))
    .await
    .map_err(|e| e.to_string())?;
  let socket2 = socket.clone();
  // 内嵌 controller：连回自己的 socket，请求时弹 tmux popup。
  tokio::spawn(async move {
    let _ = controller::run(&socket2).await;
  });
  conn
    .object_server()
    .at(OBJECT_PATH, Agent::daemon(daemon))
    .await?;
  logging::log_line(&format!(
    "polkit-tui-agent: tmux all-in-one listening on {socket}"
  ));

  // 常驻等待 D-Bus 调用；进程退出时 polkitd 自动清理。
  std::future::pending::<()>().await;

  #[allow(unreachable_code)]
  Ok(())
}

/// 后台 daemon 模式：注册认证代理 + 启动 socket 服务端，然后常驻。
async fn daemon_main(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
  let opts = parse_args_from(args).map_err(|e| format!("{e}\nTry --help"))?;
  // 尽早校验 XDG_RUNTIME_DIR（socket 与一次性弹窗 socket 的宿主目录），缺失时先报错，
  // 不无谓连接 system bus。
  let socket = default_socket_path()?;

  let conn = Connection::system().await?;
  let subject = build_subject(&conn, &opts)
    .await
    .map_err(|e| e.to_string())?;
  register(&conn, &subject, &opts).await?;

  let daemon = daemon::Daemon::start(PathBuf::from(&socket))
    .await
    .map_err(|e| e.to_string())?;
  conn
    .object_server()
    .at(OBJECT_PATH, Agent::daemon(daemon.clone()))
    .await?;
  logging::log_line(&format!("polkit-tui-agent: daemon listening on {socket}"));

  // 常驻等待 D-Bus 调用；进程被 SIGTERM 时连接随进程退出，polkitd 自动清理。
  std::future::pending::<()>().await;

  #[allow(unreachable_code)]
  Ok(())
}

/// 解析参数（从显式 args 切片读取，供各模式共用）。
fn parse_args_from(args: &[String]) -> Result<Options, String> {
  let mut opts = Options {
    locale: std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_string()),
    uid_session: false,
  };
  let mut iter = args.iter();
  while let Some(arg) = iter.next() {
    // 支持 `--locale=xx` 与 `--locale xx` 两种写法。
    if let Some(v) = arg.strip_prefix("--locale=") {
      opts.locale = v.to_string();
      continue;
    }
    match arg.as_str() {
      "--locale" => {
        opts.locale = iter.next().ok_or("--locale requires a value")?.clone();
      }
      "--uid-session" => {
        opts.uid_session = true;
      }
      "-h" | "--help" => {
        print_help();
        std::process::exit(0);
      }
      "--full-cookie-log" => {}
      "--daemon" | "--controller" | "--prompt" | "--tmux" => {}
      other => return Err(format!("unknown argument: {other}")),
    }
  }
  Ok(opts)
}

/// 解析「本进程所在 session」，与 polkit 判定 agent 注册合法性的口径保持一致
/// （见 polkit 的 `polkit_backend_session_monitor_get_session_for_subject`）：
///
/// 1. logind `GetSessionByPID`：进程直属 session（SSH 会话内运行时命中）。
/// 2. 失败 → logind `GetUser(uid)` 的 `Display` 属性：图形显示会话。桌面
///    kitty/tmux 等派生进程不在任何 session 的 cgroup 里，polkit 正是靠这一步
///    把它们算进桌面 session；必须复刻，否则传入的 session 与 polkit 判定
///    不一致，注册会报 "Passed session and the session the caller is in
///    differs"。
/// 3. 仍失败 → 环境变量 `XDG_SESSION_ID` 兜底。
async fn find_session_id(conn: &Connection) -> Option<String> {
  if let Some(id) = find_session_via_logind(conn).await {
    return Some(id);
  }
  if let Some(id) = find_uid_display_session(conn).await {
    return Some(id);
  }
  std::env::var("XDG_SESSION_ID")
    .ok()
    .filter(|s| !s.is_empty())
}

/// logind `GetUser(uid)` 的 `Display` 属性：该 uid 的图形显示会话。
/// 对应 polkit 的 `sd_uid_get_display` fallback。
///
/// 注意 `Display` 的类型是结构体 `(so)`——`(session_id, session_object_path)`，
/// 不能按 `String` 反序列化。
async fn find_uid_display_session(conn: &Connection) -> Option<String> {
  let mgr = Proxy::new(
    conn,
    "org.freedesktop.login1",
    "/org/freedesktop/login1",
    "org.freedesktop.login1.Manager",
  )
  .await
  .ok()?;
  let uid = uzers::get_current_uid() as u32;
  let path: zbus::zvariant::OwnedObjectPath = match mgr.call("GetUser", &(uid)).await {
    Ok(p) => p,
    Err(e) => {
      logging::error_line(&format!("DBG GetUser: {e}"));
      return None;
    }
  };
  let props = PropertiesProxy::new(conn, "org.freedesktop.login1", path.as_str())
    .await
    .ok()?;
  let val = props
    .get(
      zbus::names::InterfaceName::try_from("org.freedesktop.login1.User").ok()?,
      "Display",
    )
    .await
    .ok()?;
  let (display, _): (String, zbus::zvariant::OwnedObjectPath) = val.try_into().ok()?;
  if display.is_empty() {
    None
  } else {
    Some(display)
  }
}

fn session_subject(session_id: &str) -> Result<Subject, String> {
  let mut details = HashMap::new();
  details.insert(
    "session-id".to_string(),
    OwnedValue::from(Str::from(session_id.to_string())),
  );
  Ok(Subject {
    subject_kind: "unix-session".to_string(),
    subject_details: details,
  })
}

/// 通过 logind D-Bus 查询本进程所属会话 id。
///
/// 注意 `Properties.Get` 的返回是 variant（`v`），不能用 `Proxy::call` 直接
/// 反序列化为具体类型（会签名不匹配），必须经 `PropertiesProxy::get`。
async fn find_session_via_logind(conn: &Connection) -> Option<String> {
  let mgr = Proxy::new(
    conn,
    "org.freedesktop.login1",
    "/org/freedesktop/login1",
    "org.freedesktop.login1.Manager",
  )
  .await
  .ok()?;
  let path: zbus::zvariant::OwnedObjectPath = mgr
    .call("GetSessionByPID", &(std::process::id() as u32))
    .await
    .ok()?;
  let props = PropertiesProxy::new(conn, "org.freedesktop.login1", path.as_str())
    .await
    .ok()?;
  let val = props
    .get(
      zbus::names::InterfaceName::try_from("org.freedesktop.login1.Session").ok()?,
      "Id",
    )
    .await
    .ok()?;
  let id: String = val.try_into().ok()?;
  if id.is_empty() { None } else { Some(id) }
}

async fn inline_main(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
  let opts = parse_args_from(args).map_err(|e| format!("{e}\nTry --help"))?;

  // TUI 需要可控终端（/dev/tty）。ssh/tmux 里没问题；被 `no> cmd` 或 daemon
  // 化时没有控制终端，TUI 无法渲染，提前报错更友好。stdout/stderr 可任意
  // 重定向（日志走 stdout、报错走 stderr，界面不受影响）。
  if !crate::tui::has_controlling_tty() {
    return Err("no controlling terminal (/dev/tty); run inside a real tty (e.g. tmux/ssh)".into());
  }

  // 连接 system bus。zbus 开了 `tokio` feature（见 Cargo.toml），
  // 连接与后续所有 handler 都跑在 #[tokio::main] 的运行时上。
  let conn = Connection::system().await?;
  // 认证事件队列：agent.rs 往这发，run_tui 里消费。容量 16 足够。
  let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(16);
  // 键盘活动通道：UI 每按键推送时间戳，agent 据此实现「输入算活动」的空闲超时。
  let (activity_tx, _activity_rx) = watch::channel(Instant::now());
  // 把 Agent 导出为 D-Bus 对象。此时接口可被调用，但还没在 polkitd 注册。
  conn
    .object_server()
    .at(
      OBJECT_PATH,
      Agent::inline(ui_tx.clone(), activity_tx.clone()),
    )
    .await?;

  let subject = build_subject(&conn, &opts)
    .await
    .map_err(|e| format!("{e}\nTry --help"))?;
  let authority = register(&conn, &subject, &opts).await?;
  logging::log_line("polkit-tui-agent: running, press q to quit");

  // 进入 TUI 事件循环；退出后注销认证代理（否则 scope 会残留一个死 agent）。
  let result = run_tui(&conn, ui_rx, ui_tx.clone(), activity_tx).await;

  let _ = authority
    .unregister_authentication_agent(&subject, OBJECT_PATH)
    .await;
  result
}

/// TUI 事件循环：用 `tokio::select!` 同时等待三路事件。
///
/// 1. `keys`（crossterm `EventStream`）：键盘输入，由 `App::handle_key` 处理；
///    空闲时按 `q`/`Ctrl-C` 退出。
/// 2. `ui_events`（认证事件 mpsc）：agent 发来的 Prompt/Status/Cancel/Dismiss。
/// 3. `tick`：固定周期重绘（ratatui 需要周期性 `draw` 刷新光标等）。
///
/// 每次循环末尾统一 `tui.terminal.draw`，保证一次只重绘一帧、状态一致。
///
/// TUI 输出走 `/dev/tty`（见 tui.rs），stdout/stderr 不归界面用。raw mode 与
/// alternate screen 的还原由 `Tui` 的 Drop 保证（任何退出路径含 panic 展开）。
///
/// `_conn` 参数只是借来保证 Connection 在本函数期间存活（zbus 的 D-Bus 派发
/// 依赖它）；`ui_tx` 同理，持有它保证通道不因发送端全部 drop 而关闭。
async fn run_tui(
  _conn: &Connection,
  ui_rx: mpsc::Receiver<UiEvent>,
  ui_tx: mpsc::Sender<UiEvent>,
  activity_tx: watch::Sender<Instant>,
) -> Result<(), Box<dyn std::error::Error>> {
  let mut tui = crate::tui::Tui::open().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
  let mut keys = EventStream::new();
  let mut ui_events = ReceiverStream::new(ui_rx);
  let mut tick = tokio::time::interval(Duration::from_millis(100));
  tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
  let mut app = App::new();
  let _ = ui_tx; // keep sender alive so agent can always send

  // 每次弹窗都强制整屏重绘（抹掉一切绕开 diff 的外部残留）。
  //
  // 不用 `Terminal::clear()`：它内部会查询光标位置（CPR 查询），而本程序运行
  // 的 `EventStream` 后台线程无限占用 crossterm 全局事件读取器锁，查询必然
  // 超时报错。改为「空帧 + 正常帧」两段 draw：空帧把上一帧内容 diff 成空格
  // （物理清屏）并重置 ratatui 的 previous buffer，下一帧即全量重画对话框，
  // 全程不触发任何光标查询。
  let mut needs_full_redraw = false;

  loop {
    tokio::select! {
        Some(Ok(ev)) = keys.next() => {
            if let Event::Key(key) = ev {
                // 任何按键都算活动：刷新 agent 侧空闲超时的活动基准。
                let _ = activity_tx.send(Instant::now());
                // 仅当没有活动对话框时才允许退出，避免误触。
                let quit = app.active.is_none()
                    && (key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)));
                if quit {
                    break;
                }
                // 返回的 Action 表示用户对当前对话框做了决定（提交/取消）。
                if let Some(action) = app.handle_key(key) {
                    send_answer(&mut app, action);
                }
            }
        }
        Some(ev) = ui_events.next() => {
            // 新对话框（含重试替换）到来时置位，下面先清屏再重绘。
            let is_prompt = matches!(ev, UiEvent::Prompt { .. });
            app.on_event(ev);
            if is_prompt {
                needs_full_redraw = true;
            }
        }
        _ = tick.tick() => {}
    }
    if needs_full_redraw {
      tui.terminal.draw(|_| {})?;
      needs_full_redraw = false;
    }
    tui.terminal.draw(|frame| ui::render(frame, &app))?;
  }
  Ok(())
}

/// 把 `App::handle_key` 产生的 `Action` 通过当前对话框的 oneshot 通道发回 agent。
///
/// `reply` 是 `Option`：`take()` 移走 Sender（oneshot 只能发一次），发完即失。
/// 提交后对话框会留在「正在验证」态，等 agent 发来下一个 Prompt（重试）或
/// Dismiss（结束）。
fn send_answer(app: &mut App, action: Action) {
  if let Some(a) = app.active.as_mut()
    && let Some(reply) = a.reply.take()
  {
    let answer = match action {
      Action::Submit(p) => PromptAnswer::Submit(p),
      Action::Cancel => PromptAnswer::Cancel,
    };
    let _ = reply.send(answer);
  }
}
