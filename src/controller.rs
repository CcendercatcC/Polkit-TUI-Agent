//! # tmux 侧控制器（桥）
//!
//! 把 daemon 的认证请求变成 tmux 悬浮弹窗。运行在 **tmux 会话内部的窗格**里
//! （这是 `display-popup` 能工作的前提）。
//!
//! 两种使用方式：
//! - `--controller`：独立进程，连远端 daemon 的 socket。
//! - `--tmux` 一体模式：作为本进程内 `tokio::spawn` 的任务，自连本进程
//!   `Daemon::start` 起的 socket——复用同一套逻辑，无需第二个进程。
//!
//! 每次收到请求就调用
//!
//! ```text
//! tmux display-popup -E -T "polkit 认证" -w 70% -h 50% \
//!      -e POLKIT_COOKIE=.. -e POLKIT_USER=.. -e POLKIT_ACTION=.. -e POLKIT_MESSAGE=.. \
//!      '<self> --prompt'
//! ```
//!
//! 在屏幕正中央弹出一个悬浮框运行 `--prompt` 弹窗进程；弹窗退出后把退出码
//! 映射成 `AuthResult` 回报给 daemon。
//!
//! 收到 `Cancel` 时按 cookie 匹配当前弹窗：先写取消文件让弹窗进程自行退出，
//! 再 `tmux display-popup -C` 兜底关闭（弹窗进程被终止后其响应会迟到——daemon
//! 此时已通过本地取消令牌返回，迟到响应无害）。同时把 cookie 记入 `cancelled`
//! 集合：若取消先于弹窗请求到达，后续迟到的 `Request` 会直接回报取消而不弹窗，
//! 避免已取消的认证被重新弹出来。
//!
//! 连接断开会自动重连。`run()` 永不返回（除非异常）。

use std::env;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::protocol::{AuthRequest, AuthResult, ClientMsg, ServerMsg};

pub async fn run(socket_path: &str) -> Result<(), String> {
  if env::var("TMUX").is_err() {
    return Err("polkit-tui-agent: --controller must run inside a tmux session".to_string());
  }
  // 当前正在弹的认证 cookie。Cancel 按 cookie 精确关闭，避免误关别的弹窗。
  let current = Arc::new(std::sync::Mutex::new(None::<String>));
  // 已收到取消但弹窗请求尚未到达（或弹窗已关闭）的 cookie：防止迟到的
  // Request 把已取消的认证又弹出来。Request 消费后移除，避免集合膨胀。
  let cancelled = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
  loop {
    match connect_with_retry(socket_path).await {
      Ok(stream) => {
        crate::logging::log_line("polkit-tui-agent: connected to daemon");
        let (read_half, write_half) = stream.into_split();
        let (rtx, rrx) = mpsc::channel::<ClientMsg>(16);
        let write_task = tokio::spawn(write_loop(write_half, rrx));

        let mut reader = BufReader::new(read_half).lines();
        while let Ok(Some(line)) = reader.next_line().await {
          let Ok(msg) = serde_json::from_str::<ServerMsg>(&line) else {
            continue;
          };
          match msg {
            ServerMsg::Request { id, req } => {
              let cookie = req.cookie.clone();
              // 该 cookie 已被取消（Cancel 先到而弹窗请求还没发出）：直接回报
              // 取消，不弹窗——避免已取消的认证被重新弹出来。
              if cancelled.lock().unwrap().remove(&cookie) {
                crate::logging::log_line(&format!(
                  "polkit-tui-agent: controller skip-cancelled request cookie={}",
                  crate::log_cookie(&cookie)
                ));
                let _ = rtx
                  .send(ClientMsg::Response {
                    id,
                    result: AuthResult::Cancel,
                  })
                  .await;
                continue;
              }
              // 弹窗期间不能阻塞读循环，否则 Cancel 无法及时处理。
              let rtx2 = rtx.clone();
              let cur = current.clone();
              let cancel_file = cancel_file_path(&cookie);
              // 记录当前弹窗；若有更新的请求覆盖，稍后按 cookie 判断再清除。
              *current.lock().unwrap() = Some(cookie.clone());
              crate::logging::log_line(&format!(
                "polkit-tui-agent: controller request cookie={}",
                crate::log_cookie(&cookie)
              ));
              tokio::spawn(async move {
                let result = run_popup(&req, &cancel_file).await;
                // 弹窗结束清除取消文件与当前标记。
                let _ = std::fs::remove_file(&cancel_file);
                {
                  let mut g = cur.lock().unwrap();
                  if g.as_ref().is_some_and(|c| c == &cookie) {
                    *g = None;
                  }
                }
                let _ = rtx2.send(ClientMsg::Response { id, result }).await;
              });
            }
            ServerMsg::Cancel { cookie } => {
              crate::logging::log_line(&format!(
                "polkit-tui-agent: controller cancel cookie={}",
                crate::log_cookie(&cookie)
              ));
              // 记录已取消 cookie：即使弹窗请求尚未到达（current 未匹配），
              // 之后到来的 Request 也会被跳过，不会把已取消的认证再弹出来。
              cancelled.lock().unwrap().insert(cookie.clone());
              // 仅当弹窗正是该 cookie 的认证时才关闭，避免误清新认证的弹窗。
              let matched = current
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|c| c == &cookie);
              if matched {
                // 写取消文件让弹窗进程自行退出；display-popup -C 作兜底双保险。
                let _ = std::fs::write(cancel_file_path(&cookie), "cancel");
                let _ = Command::new("tmux")
                  .args(["display-popup", "-C"])
                  .status()
                  .await;
              }
            }
          }
        }
        write_task.abort();
        crate::logging::log_line("polkit-tui-agent: daemon disconnected, retrying...");
      }
      Err(e) => {
        crate::logging::error_line(&format!("polkit-tui-agent: {e}"));
        tokio::time::sleep(Duration::from_secs(2)).await;
      }
    }
  }
}

/// 连接 daemon socket，失败则打印提示并重试（daemon 可能还没起来）。
async fn connect_with_retry(socket_path: &str) -> Result<UnixStream, String> {
  loop {
    if let Ok(stream) = UnixStream::connect(socket_path).await {
      return Ok(stream);
    }
    crate::logging::log_line(&format!(
      "polkit-tui-agent: waiting for daemon socket {socket_path}..."
    ));
    tokio::time::sleep(Duration::from_secs(2)).await;
  }
}

/// 后台写任务：把响应行写出 socket。
async fn write_loop(
  write_half: tokio::net::unix::OwnedWriteHalf,
  mut rx: mpsc::Receiver<ClientMsg>,
) {
  let mut writer = BufWriter::new(write_half);
  while let Some(msg) = rx.recv().await {
    let Ok(line) = serde_json::to_string(&msg) else {
      continue;
    };
    if writer.write_all(line.as_bytes()).await.is_err()
      || writer.write_all(b"\n").await.is_err()
      || writer.flush().await.is_err()
    {
      break;
    }
  }
}

/// 在一个 tmux 悬浮弹窗里运行 `--prompt`，并映射退出码为认证结果。
///
/// 消息文本经 `-e` 环境变量传入，避免 shell 转义问题；密码不在此路径上。
/// `cancel_file` 是取消文件路径，经 `POLKIT_CANCEL_FILE` 传给弹窗进程，用于
/// polkitd 取消认证时让弹窗自行退出。
async fn run_popup(req: &AuthRequest, cancel_file: &str) -> AuthResult {
  let exe = env::current_exe().unwrap_or_else(|_| "polkit-tui-agent".into());
  // 命令整体作为单个参数交给 tmux，tmux 用 shell 执行；路径加引号防空格。
  let cmd = format!("\"{}\" --prompt", exe.to_string_lossy());
  let status = Command::new("tmux")
    .args([
      "display-popup",
      "-E",
      "-T",
      "polkit 认证",
      "-w",
      "70%",
      "-h",
      "50%",
      "-e",
      &format!("POLKIT_COOKIE={}", req.cookie),
      "-e",
      &format!("POLKIT_USER={}", req.user),
      "-e",
      &format!("POLKIT_ACTION={}", req.action),
      "-e",
      &format!("POLKIT_MESSAGE={}", req.message),
      "-e",
      &format!("POLKIT_CANCEL_FILE={cancel_file}"),
      &cmd,
    ])
    .status()
    .await;
  match status {
    Ok(st) => match st.code() {
      Some(0) => AuthResult::Ok,
      Some(2) => AuthResult::Cancel,
      _ => AuthResult::Failed,
    },
    Err(_) => AuthResult::Failed,
  }
}

/// 取消文件路径：`$XDG_RUNTIME_DIR/polkit-tui-cancel-<cookie-hash>`。
///
/// controller 写它通知弹窗进程取消，弹窗进程轮询到文件即自行退出。
fn cancel_file_path(cookie: &str) -> String {
  let dir = env::var("XDG_RUNTIME_DIR")
    .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
  // cookie 可能含非文件名安全的符号，用 FNV-1a 派生短 hash。
  format!("{dir}/polkit-tui-cancel-{}", crate::fnv1a_hex(cookie))
}
