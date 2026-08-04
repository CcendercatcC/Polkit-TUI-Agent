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
//! 每次收到请求先 bind 一个临时 `UnixListener`
//! （`$XDG_RUNTIME_DIR/polkit-tui-popup-<fnv1a_hex>`），仅把 socket 路径经
//!
//! ```text
//! tmux display-popup -E -T "polkit 认证" -w 70% -h 50% \
//!      -e POLKIT_SOCK=<path> '<self> --prompt'
//! ```
//!
//! 传给弹窗进程。弹窗连上 socket 后 controller 写一行 `AuthRequest` NDJSON 并保持
//! 连接；polkitd 取消时 controller 经该连接写一行 `ServerMsg::Cancel` NDJSON 通知
//! 弹窗自行退出，`display-popup -C` 作兜底。弹窗退出后把退出码映射成
//! `AuthResult` 回报给 daemon。
//!
//! 收到 `Cancel` 时按 cookie 匹配当前弹窗：经共享取消通道把取消信号发给弹窗连接
//! 上的写任务（弹窗进程被终止后其响应会迟到——daemon 此时已通过本地取消令牌返回，
//! 迟到响应无害）。同时把 cookie 记入 `cancelled` 集合：若取消先于弹窗请求到达，
//! 后续迟到的 `Request` 会直接回报取消而不弹窗，避免已取消的认证被重新弹出来。
//!
//! 连接断开会自动重连。`run()` 永不返回（除非异常）。

use std::collections::HashSet;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::protocol::{AuthRequest, AuthResult, ClientMsg, ServerMsg};

pub async fn run(socket_path: &str) -> Result<(), String> {
  if env::var("TMUX").is_err() {
    return Err("polkit-tui-agent: --controller must run inside a tmux session".to_string());
  }
  // 当前正在弹的认证 cookie。Cancel 按 cookie 精确关闭，避免误关别的弹窗。
  let current = Arc::new(Mutex::new(None::<String>));
  // 已收到取消但弹窗请求尚未到达（或弹窗已关闭）的 cookie：防止迟到的
  // Request 把已取消的认证又弹出来。Request 消费后移除，避免集合膨胀。
  let cancelled = Arc::new(Mutex::new(HashSet::new()));
  // 共享的当前弹窗取消发送端：run_popup 建立弹窗连接后存入，弹窗退出置 None。
  // run() 的 Cancel 分支从中克隆出发送端通知弹窗取消。
  let cancel_sig = Arc::new(Mutex::new(None::<mpsc::Sender<()>>));
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
              let csig = cancel_sig.clone();
              let canc = cancelled.clone();
              // 记录当前弹窗；若有更新的请求覆盖，稍后按 cookie 判断再清除。
              *current.lock().unwrap() = Some(cookie.clone());
              crate::logging::log_line(&format!(
                "polkit-tui-agent: controller request cookie={}",
                crate::log_cookie(&cookie)
              ));
              tokio::spawn(async move {
                let result = run_popup(&req, csig, canc).await;
                // 弹窗结束清除当前标记（仅当仍是对应 cookie，避免误清新弹窗）。
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
                // 经共享取消通道通知弹窗进程取消（写任务在 popup socket 上写一行
                // ServerMsg::Cancel）；display-popup -C 作兜底双保险。
                // 先 clone 出发送端并结束锁作用域，再 await（避免持 MutexGuard 跨 await）。
                let sig = { cancel_sig.lock().unwrap().clone() };
                if let Some(tx) = sig {
                  let _ = tx.send(()).await;
                }
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
async fn write_loop(write_half: OwnedWriteHalf, mut rx: mpsc::Receiver<ClientMsg>) {
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
/// 请求信息与取消信号都经一条临时 unix socket 传递：本函数先 bind 一个一次性
/// `UnixListener`，仅把 socket 路径经 `-e POLKIT_SOCK=<path>` 传给弹窗进程；
/// 弹窗连上后写一行 `AuthRequest` NDJSON。polkitd 取消时 `cancel_sig` 通知写任务
/// 在同一条连接上写一行 `ServerMsg::Cancel`，弹窗据此自行退出；`display-popup -C`
/// 由 run() 作兜底。弹窗退出后按退出码映射 `AuthResult`。
///
/// `cancelled` 用于覆盖「取消先到、连接后到」竞态：连接建立后若发现该 cookie 已被
/// 取消，立即向取消通道发一个信号，不让已取消的认证继续弹着。
async fn run_popup(
  req: &AuthRequest,
  cancel_sig: Arc<Mutex<Option<mpsc::Sender<()>>>>,
  cancelled: Arc<Mutex<HashSet<String>>>,
) -> AuthResult {
  let sock_path = popup_socket_path(&req.cookie);
  // 一次性监听 socket：弹窗进程连上来即完成请求传递，随后关闭监听。
  let listener = match UnixListener::bind(&sock_path) {
    Ok(l) => l,
    Err(e) => {
      crate::logging::error_line(&format!(
        "polkit-tui-agent: bind popup socket {sock_path}: {e}"
      ));
      return AuthResult::Failed;
    }
  };
  let exe = env::current_exe().unwrap_or_else(|_| "polkit-tui-agent".into());
  // 命令整体作为单个参数交给 tmux，tmux 用 shell 执行；路径加引号防空格。
  let cmd = format!("\"{}\" --prompt", exe.to_string_lossy());
  let mut child = match Command::new("tmux")
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
      &format!("POLKIT_SOCK={sock_path}"),
      &cmd,
    ])
    .spawn()
  {
    Ok(c) => c,
    Err(e) => {
      crate::logging::error_line(&format!("polkit-tui-agent: spawn tmux display-popup: {e}"));
      let _ = std::fs::remove_file(&sock_path);
      return AuthResult::Failed;
    }
  };
  // 等弹窗进程连上临时 socket；10s 没连上视为失败。
  let stream = match tokio::time::timeout(Duration::from_secs(10), listener.accept()).await {
    Ok(Ok((stream, _))) => stream,
    Ok(Err(e)) => {
      crate::logging::error_line(&format!("polkit-tui-agent: accept popup socket: {e}"));
      let _ = child.kill().await;
      let _ = std::fs::remove_file(&sock_path);
      return AuthResult::Failed;
    }
    Err(_) => {
      crate::logging::error_line("polkit-tui-agent: popup did not connect within 10s");
      let _ = child.kill().await;
      let _ = std::fs::remove_file(&sock_path);
      return AuthResult::Failed;
    }
  };
  // 纵深防御：校验连接方 uid 是本用户，防止其它进程抢先连上窃取认证请求。
  let uid_ok = stream
    .peer_cred()
    .map(|cred| cred.uid() == uzers::get_current_uid())
    .unwrap_or(false);
  if !uid_ok {
    crate::logging::error_line("polkit-tui-agent: popup connection rejected: uid mismatch");
    let _ = child.kill().await;
    let _ = std::fs::remove_file(&sock_path);
    return AuthResult::Failed;
  }
  // 单次 accept 后关闭监听，不再接受新连接。
  drop(listener);
  let (_read_half, mut write_half) = stream.into_split();
  // 写一行 AuthRequest NDJSON 给弹窗；弹窗据此发起认证。
  let line = match serde_json::to_string(req) {
    Ok(l) => l,
    Err(e) => {
      crate::logging::error_line(&format!("polkit-tui-agent: serialize popup request: {e}"));
      let _ = child.kill().await;
      let _ = std::fs::remove_file(&sock_path);
      return AuthResult::Failed;
    }
  };
  if write_half.write_all(line.as_bytes()).await.is_err()
    || write_half.write_all(b"\n").await.is_err()
    || write_half.flush().await.is_err()
  {
    let _ = child.kill().await;
    let _ = std::fs::remove_file(&sock_path);
    return AuthResult::Failed;
  }
  // 取消写任务：收到取消信号即往同一条连接写一行 ServerMsg::Cancel 通知弹窗。
  let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(1);
  let cookie = req.cookie.clone();
  let cancel_task = tokio::spawn(async move {
    while cancel_rx.recv().await.is_some() {
      let msg = ServerMsg::Cancel {
        cookie: cookie.clone(),
      };
      let Ok(line) = serde_json::to_string(&msg) else {
        continue;
      };
      if write_half.write_all(line.as_bytes()).await.is_err()
        || write_half.write_all(b"\n").await.is_err()
        || write_half.flush().await.is_err()
      {
        break;
      }
    }
  });
  // 发送端存入共享取消通道，供 run() 的 Cancel 分支取用。
  *cancel_sig.lock().unwrap() = Some(cancel_tx);
  // 覆盖「取消先到、连接后到」竞态：该请求已被取消，弹窗连上后立即通知取消。
  // 先 clone 出发送端并结束锁作用域，再 await（避免持 MutexGuard 跨 await）。
  let sig = { cancel_sig.lock().unwrap().clone() };
  if cancelled.lock().unwrap().contains(&req.cookie)
    && let Some(tx) = sig
  {
    let _ = tx.send(()).await;
  }
  // 等弹窗进程退出，按退出码映射认证结果。
  let status = child.wait().await;
  // 清理：共享取消发送端置 None、中止写任务、删除临时 socket。
  *cancel_sig.lock().unwrap() = None;
  cancel_task.abort();
  let _ = std::fs::remove_file(&sock_path);
  match status {
    Ok(st) => match st.code() {
      Some(0) => AuthResult::Ok,
      Some(2) => AuthResult::Cancel,
      _ => AuthResult::Failed,
    },
    Err(_) => AuthResult::Failed,
  }
}

/// 一次性弹窗 socket 路径：`$XDG_RUNTIME_DIR/polkit-tui-popup-<cookie-hash>`。
///
/// 请求信息与取消信号都经此 socket 在 controller 与弹窗进程间传递。
///
/// `XDG_RUNTIME_DIR` 缺失时不回退 /tmp：/tmp 是 1777 的共享目录，无法安全
/// 放 socket。所有入口（`--controller`/`--tmux`/`--daemon`）在到达 controller
/// 之前都经 `default_socket_path()` 校验过 `XDG_RUNTIME_DIR`，缺失已提前退出，
/// 这里 expect 不会触发。
fn popup_socket_path(cookie: &str) -> String {
  let dir = env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR required (validated at startup)");
  // cookie 可能含非文件名安全的符号，用 FNV-1a 派生短 hash。
  format!("{dir}/polkit-tui-popup-{}", crate::fnv1a_hex(cookie))
}
