//! # 弹窗单请求认证（`--prompt`）
//!
//! 由 controller 在 tmux `display-popup` 里启动，是**自包含**的单次认证：
//! 从环境变量读请求信息 → 画对话框收密码 → 连 helper 完成 PAM → 按结果退出。
//!
//! 退出码约定（controller 据此映射）：
//! - `0` = 认证成功（helper 已代调 AuthenticationAgentResponse2/3）
//! - `2` = 用户取消（含 polkitd 经取消文件发来的取消）
//! - 其他 = 失败（含等待输入超时，见 `POLKIT_TUI_TIMEOUT`）
//!
//! 认证在独立任务里跑，主事件循环保持响应：验证期间仍能重绘「正在验证…」、
//! 用 Esc/Ctrl-C 取消，不会因一次慢认证冻住整个弹窗。

use std::env;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use tokio::sync::{mpsc, watch};

use crate::helper::{HelperSession, PamMessage};
use crate::ui::{Action, App, PromptState};

pub async fn run() -> Result<i32, String> {
  let cookie = env::var("POLKIT_COOKIE").map_err(|_| "POLKIT_COOKIE not set")?;
  let user = env::var("POLKIT_USER").unwrap_or_default();
  let action = env::var("POLKIT_ACTION").unwrap_or_default();
  let message = env::var("POLKIT_MESSAGE").unwrap_or_default();
  // 等待输入的时长：`POLKIT_TUI_TIMEOUT` 可覆盖，默认 30s。超时视为认证失败。
  let input_timeout = env::var("POLKIT_TUI_TIMEOUT")
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(30u64);

  if !crate::tui::has_controlling_tty() {
    return Err(
      "polkit-tui-agent: --prompt needs a controlling terminal (should run inside a tmux popup)"
        .into(),
    );
  }

  let mut tui = crate::tui::Tui::open()?;
  let mut keys = EventStream::new();
  let mut tick = tokio::time::interval(Duration::from_millis(100));
  tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

  let mut app = App::new();
  app.open_prompt(cookie, action, message, user);

  // 认证结果通道：提交后 spawn 独立任务跑 helper，主循环保持响应——
  // 验证期间仍能重绘「正在验证…」、用 Esc/Ctrl-C 取消，不再被一次慢认证冻住。
  let (auth_tx, mut auth_rx) = mpsc::channel::<Outcome>(1);

  // 取消文件：polkitd 取消认证时由 controller 写入该文件，此处轮询到即退出
  // （exit 2）。让弹窗进程自身具备取消能力，不再单点依赖 `display-popup -C`。
  let (cancel_tx, mut cancel_rx) = watch::channel(false);
  if let Ok(cancel_path) = env::var("POLKIT_CANCEL_FILE") {
    tokio::spawn(async move {
      loop {
        if std::path::Path::new(&cancel_path).exists() {
          let _ = cancel_tx.send(true);
          break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
      }
    });
  }

  // 最后按键时间，用于编辑态无操作超时判定。
  let mut last_activity = Instant::now();

  let exit_code = loop {
    tokio::select! {
        Some(Ok(ev)) = keys.next() => {
            if let Event::Key(key) = ev {
                last_activity = Instant::now();
                // 编辑/验证任一状态下 Esc、Ctrl-C 都视为取消（验证中也可退出）。
                let cancel = key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL));
                if cancel && app.active.is_some() {
                    break 2;
                }
                if let Some(action) = app.handle_key(key) {
                    match action {
                        Action::Submit(pwd) => {
                            // 快照当前认证信息，交给后台任务；App 已进入 Verifying 态。
                            let (user, cookie) = match app.active.as_ref() {
                                Some(a) => (a.username.clone(), a.cookie.clone()),
                                None => break 2,
                            };
                            let tx = auth_tx.clone();
                            tokio::spawn(async move {
                                let _ = tx
                                    .send(authenticate_once(&user, &cookie, &pwd).await)
                                    .await;
                            });
                        }
                        Action::Cancel => break 2,
                    }
                }
            }
        }
        outcome = auth_rx.recv() => {
            match outcome {
                Some(Outcome::Success) => break 0,
                Some(Outcome::Failure) => {
                  app.retry("认证失败，请重试".into());
                  // 验证失败也是一次交互活动：刷新空闲计时，重新获得满额等待时间。
                  last_activity = Instant::now();
                }
                Some(Outcome::Error(e)) => {
                  app.retry(format!("认证失败：{e}"));
                  last_activity = Instant::now();
                }
                None => break 2,
            }
        }
        // polkitd 取消认证：退出码 2（取消）。
        _ = cancel_rx.changed() => break 2,
        _ = tick.tick() => {
            // 编辑态无任何按键超过超时时长 → 认证失败（退出码非 0/2，controller 映射 Failed）。
            let idle = app
                .active
                .as_ref()
                .is_some_and(|a| a.state == PromptState::Editing)
                && last_activity.elapsed() >= Duration::from_secs(input_timeout);
            if idle {
                break 1;
            }
        }
    }
    let _ = tui.terminal.draw(|f| render_prompt_frame(f, &app));
  };

  Ok(exit_code)
}

fn render_prompt_frame(frame: &mut Frame, app: &App) {
  crate::ui::render_full(frame, app);
}

enum Outcome {
  Success,
  Failure,
  Error(String),
}

/// 单次认证：连 helper、应答 PAM、得到最终结果。失败时外层会重试。
///
/// 运行在独立任务里，`app` 的状态（用户名/cookie）以参数快照传入，避免跨
/// 任务借用。
async fn authenticate_once(user: &str, cookie: &str, password: &str) -> Outcome {
  // 连接 helper 加 10s 超时兜底（与 inline 一致），socket 激活挂起时不致弹窗冻死。
  let mut session = match tokio::time::timeout(
    Duration::from_secs(10),
    HelperSession::connect(user, cookie),
  )
  .await
  {
    Ok(Ok(s)) => s,
    Ok(Err(e)) => return Outcome::Error(e),
    Err(_) => return Outcome::Error("连接超时，请重试".into()),
  };
  loop {
    // 单条消息 30s 超时：helper 无响应（PAM 挂起）时判失败，不让验证永久悬着。
    let msg = match tokio::time::timeout(Duration::from_secs(30), session.next_message()).await {
      Ok(r) => r,
      Err(_) => return Outcome::Error("认证超时，请重试".into()),
    };
    match msg {
      Ok(Some(PamMessage::PromptEchoOff(_))) => {
        if let Err(e) = session.respond(password).await {
          return Outcome::Error(e);
        }
      }
      Ok(Some(PamMessage::PromptEchoOn(_))) => {
        let _ = session.respond("").await;
      }
      Ok(Some(PamMessage::Success)) => return Outcome::Success,
      Ok(Some(PamMessage::Failure)) => return Outcome::Failure,
      _ => return Outcome::Failure,
    }
  }
}
