//! # AuthenticationAgent D-Bus 接口与认证状态机
//!
//! 这是整个项目的核心：实现 polkit 的 **服务端** 接口
//! `org.freedesktop.PolicyKit1.AuthenticationAgent`。
//!
//! ## D-Bus 角色
//!
//! polkit 要求每个用户会话有一个「认证代理」：polkitd 判断某操作需要认证时，
//! 会调用该接口：
//!
//! ```text
//! BeginAuthentication(action_id, message, icon_name, details, cookie, identities)
//! CancelAuthentication(cookie)
//! ```
//!
//! - `BeginAuthentication` **必须阻塞直到认证结束**（成功后返回 `Ok`，用户取消
//!   返回 `org.freedesktop.PolicyKit1.Error.Cancelled`）。polkitd 的
//!   `CheckAuthorization` 会一直等着它返回。
//! - 认证成功后的通知由 **root 的 helper 进程** 调用
//!   `Authority.AuthenticationAgentResponse2/3` 完成，本 agent 不参与——因此
//!   **密码永远不经过 D-Bus**。
//!
//! ## 两种认证后端（`Backend`）
//!
//! - `Inline`：传统模式，agent 进程内自带 TUI。认证事件经 `mpsc<UiEvent>` 推给
//!   UI，密码答案经 `oneshot` 回传，键盘活动经 `watch<Instant>` 回流供空闲超时
//!   判定。
//! - `Daemon`：tmux 模式，agent 在后台（systemd 服务）headless 运行，把认证
//!   请求转发给 tmux 侧的 controller，由弹出的 `--prompt` 进程完成收集密码。
//!
//! ## 并发认证串行化
//!
//! 并发的 `BeginAuthentication`（同一 session 内多个 pkexec 同时提权）经
//! `Agent::slot`（`Semaphore(1)`）FIFO 排队，逐个弹框验证。inline 单对话框、
//! tmux 单 popup 无法并发展示，若不排队会互相覆盖/顶掉，导致旧请求失败、取消
//! 链断裂。排队期间仍可被 `CancelAuthentication` 取消（不弹框、不占位）。
//!
//! ## 为什么接口方法用 `&self` + 内部 Mutex？
//!
//! zbus 对 `&mut self` 的方法调用会**串行化**执行。而 `BeginAuthentication`
//! 会长时间 await；若用 `&mut self`，这段时间内 `CancelAuthentication`
//! 永远得不到执行 → 取消功能失效（死锁）。用 `&self` + `Mutex<HashMap>` 让
//! 两者能并发执行：一个阻塞等认证，另一个随时能中断它。

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use zbus::DBusError;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

use crate::daemon::Daemon;
use crate::helper::{HelperSession, PamMessage};
use crate::protocol::AuthRequest;
use crate::ui::{PromptAnswer, UiEvent};

/// 方法返回给 polkitd 的 D-Bus 错误类型。
///
/// `#[zbus(prefix = "...")]` 把错误名固定为
/// `org.freedesktop.PolicyKit1.Error.Cancelled`，正是 polkitd 期望的取消信号。
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum PolkitError {
  /// 用户取消认证。
  Cancelled,
  /// 一般性失败。
  Failed,
}

/// 认证后端：决定「收集密码」这一动作在哪里发生。
enum Backend {
  /// 进程内 TUI（inline 模式），经 mpsc 与 UI 交互。
  Inline {
    events: mpsc::Sender<UiEvent>,
    /// UI 键盘活动通道：每次按键推送新时间戳，供空闲超时判定（输入算活动）。
    activity: watch::Sender<Instant>,
  },
  /// 转发给 tmux controller（daemon 模式）。
  Daemon { daemon: Arc<Daemon> },
}

/// D-Bus 对象：`org.freedesktop.PolicyKit1.AuthenticationAgent` 的实现。
///
/// - `backend`：Inline 或 Daemon 两种收集密码的方式。
/// - `pending`：cookie → 取消令牌。`begin_authentication` 开始时插入、结束时
///   移除；`cancel_authentication` 只查表并置位，两者互不阻塞。
/// - `slot`：认证名额信号量（容量 1）。并发的 `begin_authentication` 经它
///   FIFO 串行化，逐个弹框验证——inline 单对话框、tmux 单 popup 无法并发展示，
///   否则新请求会覆盖/顶掉旧请求导致互相失败。
pub struct Agent {
  backend: Backend,
  pending: Mutex<HashMap<String, watch::Sender<bool>>>,
  slot: Semaphore,
}

impl Agent {
  /// inline 模式：UI 事件经 `events` 通道发给 TUI 事件循环；键盘活动经
  /// `activity` 通道回流，供空闲超时判定。
  pub fn inline(events: mpsc::Sender<UiEvent>, activity: watch::Sender<Instant>) -> Self {
    Self {
      backend: Backend::Inline { events, activity },
      pending: Mutex::new(HashMap::new()),
      slot: Semaphore::new(1),
    }
  }

  /// daemon 模式：认证请求经 socket 转发给 tmux controller。
  pub fn daemon(daemon: Arc<Daemon>) -> Self {
    Self {
      backend: Backend::Daemon { daemon },
      pending: Mutex::new(HashMap::new()),
      slot: Semaphore::new(1),
    }
  }
}

/// D-Bus 的 `Identity` 结构 `(sa{sv})`：`("unix-user", {"uid": <u32>})`。
///
/// zbus_polkit 的 `Identity` 只实现了 `Serialize` 没有 `Deserialize`，无法直接
/// 作为方法参数接收，所以这里用等价的元组类型手动解析。
type Identity = (String, HashMap<String, OwnedValue>);

/// 从 Identity 里取出 `unix-user` 的 uid。
fn identity_uid(id: &Identity) -> Option<u32> {
  if id.0 != "unix-user" {
    return None;
  }
  // OwnedValue 通过 Deref 可当 Value 用，U32 变体里就是 uid。
  match id.1.get("uid") {
    Some(v) => match v.deref() {
      Value::U32(u) => Some(*u),
      _ => None,
    },
    None => None,
  }
}

/// uid → 用户名（读 `/etc/passwd`，uzers 是跨平台封装）。
fn uid_to_username(uid: u32) -> Option<String> {
  uzers::get_user_by_uid(uid).map(|u| u.name().to_string_lossy().into_owned())
}

/// 从候选 identities 里选一个用户名。
///
/// 偏好顺序：当前用户 → root → 第一个候选。桌面 agent 常这么选：优先让用户
/// 认证自己，其次管理员；`unix-group` 之类不支持的 identity 会被跳过。
fn pick_username(identities: &[Identity]) -> Option<String> {
  let current = uzers::get_current_uid();
  for id in identities {
    if identity_uid(id) == Some(current)
      && let Some(name) = uid_to_username(current)
    {
      return Some(name);
    }
  }
  for id in identities {
    if identity_uid(id) == Some(0)
      && let Some(name) = uid_to_username(0)
    {
      return Some(name);
    }
  }
  let uid = identities.first().and_then(identity_uid)?;
  uid_to_username(uid)
}

/// 导出为 D-Bus 接口。方法名 Rust snake_case ↔ D-Bus CamelCase 自动对应。
#[interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
  /// polkitd 要求用户认证时调用。**必须阻塞到认证结束。**
  ///
  /// 参数语义：
  /// - `action_id`：action 标识（如 `org.freedesktop.policykit.exec`）。
  /// - `message`：polkitd 按本 agent 注册的 locale 翻译好的提示文案。
  /// - `cookie`：本次认证的会话标识，后面 helper 和
  ///   `AuthenticationAgentResponse2/3` 都要用到。
  /// - `identities`：候选身份（unix-user / unix-group）。
  async fn begin_authentication(
    &self,
    action_id: String,
    message: String,
    _icon_name: String,
    _details: HashMap<String, String>,
    cookie: String,
    identities: Vec<Identity>,
  ) -> Result<(), PolkitError> {
    crate::logging::log_line(&format!(
      "polkit-tui-agent: begin_authentication cookie={}",
      crate::log_cookie(&cookie)
    ));
    let username = pick_username(&identities).ok_or(PolkitError::Failed)?;
    // 注册本 cookie 的取消令牌。cancel_authentication 可并发访问；排队期间
    // 也能被取消（select 感知），不弹框、不占位。
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    self
      .pending
      .lock()
      .unwrap()
      .insert(cookie.clone(), cancel_tx);

    let req = AuthRequest {
      cookie: cookie.clone(),
      user: username,
      action: action_id,
      message,
    };

    // 并发认证串行化：容量 1 的信号量让同时到达的请求 FIFO 排队，逐个弹框
    // 验证。inline 单对话框、tmux 单 popup 无法并发展示，若不排队，新请求
    // 会覆盖 current/顶掉弹窗，导致旧请求失败、取消链断裂（弹窗残留）。
    if self.slot.available_permits() == 0 {
      crate::logging::log_line(&format!(
        "polkit-tui-agent: authentication queued cookie={}",
        crate::log_cookie(&cookie)
      ));
    }
    let permit = match tokio::select! {
        // 排队中被取消（polkitd CancelAuthentication / 发起方 Ctrl-C）。
        _ = cancel_rx.changed() => {
            crate::logging::log_line(&format!(
              "polkit-tui-agent: queued auth cancelled cookie={}",
              crate::log_cookie(&cookie)
            ));
            self.pending.lock().unwrap().remove(&cookie);
            return Err(PolkitError::Cancelled);
        }
        p = self.slot.acquire() => p,
    } {
        Ok(p) => p,
        Err(_) => {
            // 信号量被关闭（Agent 生命周期内不会发生），按失败处理。
            self.pending.lock().unwrap().remove(&cookie);
            return Err(PolkitError::Failed);
        }
    };

    // 获得名额瞬间的兜底：select 可能选了 acquire 而取消值恰好已置位。
    if *cancel_rx.borrow() {
      self.pending.lock().unwrap().remove(&cookie);
      return Err(PolkitError::Cancelled);
    }

    let result = match &self.backend {
      Backend::Inline { events, activity } => {
        self
          .authenticate_inline(req, events.clone(), activity.clone(), cancel_rx)
          .await
      }
      Backend::Daemon { daemon } => {
        // daemon 模式不在此进程做 helper 认证，只把请求转发出去。
        tokio::select! {
            _ = cancel_rx.changed() => Err(PolkitError::Cancelled),
            r = daemon.request(req) => match r {
                Ok(crate::protocol::AuthResult::Ok) => Ok(()),
                Ok(crate::protocol::AuthResult::Cancel) => Err(PolkitError::Cancelled),
                Ok(crate::protocol::AuthResult::Failed) => Err(PolkitError::Failed),
                Err(e) => {
                    crate::logging::error_line(&format!("polkit-tui-agent: {e}"));
                    Err(PolkitError::Failed)
                }
            },
        }
      }
    };

    // 无论成功失败，清理令牌表并释放验证名额，让排队的下一个请求开始。
    self.pending.lock().unwrap().remove(&cookie);
    drop(permit);
    result
  }

  /// polkitd 取消某次认证时调用（如 pkexec 进程被 Ctrl-C）。
  ///
  /// 置位对应的取消令牌，让阻塞中的 `begin_authentication` 通过 `select!`
  /// 退出；同时按后端通知 UI/controller 关闭对话框。
  async fn cancel_authentication(&self, cookie: String) -> Result<(), PolkitError> {
    crate::logging::log_line(&format!(
      "polkit-tui-agent: cancel_authentication cookie={}",
      crate::log_cookie(&cookie)
    ));
    if let Some(tx) = self.pending.lock().unwrap().get(&cookie) {
      let _ = tx.send(true);
    }
    match &self.backend {
      Backend::Inline { events, .. } => {
        let _ = events.send(UiEvent::Cancel { cookie }).await;
      }
      Backend::Daemon { daemon } => {
        daemon.cancel(&cookie).await;
      }
    }
    Ok(())
  }
}

impl Agent {
  /// 认证主循环：`提示密码 → 验证 → 失败则重试`，直到成功或取消。
  ///
  /// 循环的每一轮：
  /// 1. 通过 oneshot 向 UI 请求一个密码（`select!` 同时监听取消令牌）。
  /// 2. 连上 polkit-agent-helper-1（socket 或 setuid 二进制），喂用户名+cookie。
  /// 3. 逐行解析 helper 的输出协议：`PAM_PROMPT_ECHO_OFF` 时回密码、
  ///    `PAM_ERROR_MSG`/`PAM_TEXT_INFO` 显示给用户、`SUCCESS`/`FAILURE`
  ///    决定这一轮结果。
  /// 4. 成功 → 发 Dismiss 关对话框并返回；失败 → 状态行报错，继续下一轮。
  ///
  /// 密码只在 UI 的 oneshot 与 helper 的私有流里传递，从不经 D-Bus。
  async fn authenticate_inline(
    &self,
    req: AuthRequest,
    events: mpsc::Sender<UiEvent>,
    activity: watch::Sender<Instant>,
    mut cancel_rx: watch::Receiver<bool>,
  ) -> Result<(), PolkitError> {
    let AuthRequest {
      cookie,
      user,
      action,
      message,
    } = req;
    let mut status = String::new();
    // 单次输入机会的最长等待：`POLKIT_TUI_TIMEOUT` 可覆盖，默认 30s。
    // 超时判认证失败，防止用户一直不输入时认证永久挂起。
    // 语义是「空闲超时」：从最后一次活动起算，UI 每按键都刷新活动时间。
    let input_timeout = std::env::var("POLKIT_TUI_TIMEOUT")
      .ok()
      .and_then(|s| s.parse().ok())
      .map(Duration::from_secs)
      .unwrap_or(Duration::from_secs(30));
    // 空闲超时的活动基准：输入（UI 键盘事件）、提交、验证失败返回都算活动，
    // 因此只有用户持续无操作（不按键、不提交）才超时。
    loop {
      // 监听 UI 键盘活动：每次按键 activity 通道推送新时间戳，刷新等待基准。
      let mut activity_rx = activity.subscribe();
      let mut last_activity = Instant::now();
      // 每轮新建 oneshot；它随 Prompt 事件一起送进 UI 队列。
      let (reply_tx, mut reply_rx) = oneshot::channel();
      let ev = UiEvent::Prompt {
        cookie: cookie.clone(),
        action_id: action.clone(),
        message: message.clone(),
        username: user.clone(),
        status: status.clone(),
        reply: reply_tx,
      };
      // mpsc 关闭（UI 已退出）说明 agent 该结束了，返回 Failed。
      events.send(ev).await.map_err(|_| PolkitError::Failed)?;

      // 阻塞等四件事：取消令牌、用户提交/取消、输入空闲超时、键盘活动刷新。
      let password = loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                let _ = events.send(UiEvent::Dismiss { cookie: cookie.clone() }).await;
                return Err(PolkitError::Cancelled);
            }
            ans = &mut reply_rx => match ans {
                Ok(PromptAnswer::Submit(p)) => break p,
                Ok(PromptAnswer::Cancel) => {
                    let _ = events.send(UiEvent::Dismiss { cookie: cookie.clone() }).await;
                    return Err(PolkitError::Cancelled);
                }
                Err(_) => return Err(PolkitError::Failed),
            },
            _ = tokio::time::sleep_until(
              last_activity
                .checked_add(input_timeout)
                .unwrap_or_else(|| Instant::now() + input_timeout),
            ) => {
              let _ = events.send(UiEvent::Dismiss { cookie: cookie.clone() }).await;
              return Err(PolkitError::Failed);
            },
            changed = activity_rx.changed() => {
                // 有键盘活动：刷新空闲基准，继续等待输入。
                if changed.is_ok() {
                    last_activity = *activity_rx.borrow();
                }
            }
        }
      };

      // 连 helper，加 10s 超时兜底：任何情况都能回到重试，不卡死 UI。
      let mut session = match tokio::time::timeout(
        Duration::from_secs(10),
        HelperSession::connect(&user, &cookie),
      )
      .await
      {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
          status = format!("认证失败：{e}");
          continue;
        }
        Err(_) => {
          status = "认证超时，请重试".to_string();
          continue;
        }
      };

      // PAM 会话循环：按 helper 的提示应答，直到 SUCCESS / FAILURE / EOF。
      // 单条消息 30s 超时：helper 长时间无响应（PAM 挂起）时判失败重试，不卡死认证。
      let mut succeeded = false;
      loop {
        let msg =
          match tokio::time::timeout(Duration::from_secs(30), session.next_message()).await {
            Ok(r) => r,
            Err(_) => {
              status = "认证超时，请重试".to_string();
              break;
            }
          };
        match msg {
          // 不回显的提示：这里就是密码，回传本轮输入的密码。
          Ok(Some(PamMessage::PromptEchoOff(t))) => {
            if !t.is_empty() {
              let _ = events
                .send(UiEvent::Status {
                  cookie: cookie.clone(),
                  text: t,
                })
                .await;
            }
            if let Err(e) = session.respond(&password).await {
              status = format!("认证失败：{e}");
              break;
            }
          }
          // 回显提示（如 OTP）：本项目 UI 不采集，回空串占位。
          Ok(Some(PamMessage::PromptEchoOn(t))) => {
            let _ = events
              .send(UiEvent::Status {
                cookie: cookie.clone(),
                text: t,
              })
              .await;
            let _ = session.respond("").await;
          }
          // PAM 错误/信息，显示到对话框状态行。
          Ok(Some(PamMessage::Error(t))) => {
            status = t.clone();
            let _ = events
              .send(UiEvent::Status {
                cookie: cookie.clone(),
                text: t,
              })
              .await;
          }
          Ok(Some(PamMessage::Info(t))) => {
            let _ = events
              .send(UiEvent::Status {
                cookie: cookie.clone(),
                text: t.clone(),
              })
              .await;
            if status.is_empty() {
              status = t;
            }
          }
          // 认证成功：helper 已以 root 调 AuthenticationAgentResponse2/3。
          Ok(Some(PamMessage::Success)) => {
            succeeded = true;
            break;
          }
          // FAILURE / EOF / IO 错误都视为本轮失败。
          _ => {
            break;
          }
        }
      }

      if succeeded {
        let _ = events
          .send(UiEvent::Dismiss {
            cookie: cookie.clone(),
          })
          .await;
        return Ok(());
      }

      // 失败后给个明确提示，进入下一轮重新要密码。
      if status.is_empty() {
        status = "认证失败，请重试".to_string();
      }
    }
  }
}
