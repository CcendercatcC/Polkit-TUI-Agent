//! # daemon 的 socket 服务端
//!
//! 提供 socket 服务端：接受 controller 连接、转发认证请求并等待结果。
//!
//! 两种使用方式：
//! - `--daemon`：作为 systemd user 服务独立运行（headless、无 tty）。
//! - `--tmux`：一体模式，本进程起 socket 后由内嵌 controller 任务自连，
//!   复用同一套请求/响应逻辑。
//!
//! 职责：
//!
//! - 监听 `$XDG_RUNTIME_DIR/polkit-tui-agent.sock`，接受 controller 连接。
//! - 提供 `request()`：把一次认证请求发给 controller，等待其回报结果
//!   （带超时与 Drop 清理）。
//! - 提供 `cancel()`：polkitd 取消认证时按 cookie 通知 controller 关闭弹窗。
//!
//! 同一时间只认一个「当前 controller」（新连接顶掉旧的）；`pending` 表用
//! 请求序号关联 oneshot 应答通道。`PendingGuard` 用 Drop 保证请求被取消
//! （比如超时或被 select 放弃）时表项一定被清理，不会泄漏。
//!
//! controller 断开（含进程退出）时，若它仍是「当前 controller」，`pending`
//! 里所有进行中的请求立即判失败（`AuthResult::Failed`），不等 120s 超时。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::protocol::{AuthRequest, AuthResult, ClientMsg, ServerMsg};

/// 当前 controller 连接：generation 号 + 发送端。
type ActiveController = Option<(u64, mpsc::Sender<ServerMsg>)>;

/// socket 服务端句柄，可跨任务共享（Arc）。
pub struct Daemon {
  /// id → 应答通道。controller 回报结果时按 id 唤醒对应 `request()`。
  pending: Arc<Mutex<HashMap<u64, oneshot::Sender<AuthResult>>>>,
  /// 当前 controller 连接。新连接递增 generation。
  active: Arc<Mutex<ActiveController>>,
  conn_seq: Arc<AtomicU64>,
  next_id: AtomicU64,
}

/// 请求注册的清理守卫：无论函数正常返回、报错还是被取消，都会移除表项。
struct PendingGuard {
  map: Arc<Mutex<HashMap<u64, oneshot::Sender<AuthResult>>>>,
  id: u64,
}

impl Drop for PendingGuard {
  fn drop(&mut self) {
    self.map.lock().unwrap().remove(&self.id);
  }
}

impl Daemon {
  /// 绑定监听 socket 并启动 accept 循环。返回共享句柄。
  pub async fn start(socket_path: PathBuf) -> Result<Arc<Self>, String> {
    // 清理残留 socket：若还能连上说明有别的 daemon 在跑，报错退出。
    if Path::new(&socket_path).exists() {
      if UnixStream::connect(&socket_path).await.is_ok() {
        return Err(format!(
          "another daemon is already listening on {}",
          socket_path.display()
        ));
      }
      let _ = std::fs::remove_file(&socket_path);
    }
    let listener = UnixListener::bind(&socket_path)
      .map_err(|e| format!("failed to bind {}: {e}", socket_path.display()))?;
    let daemon = Arc::new(Daemon {
      pending: Arc::new(Mutex::new(HashMap::new())),
      active: Arc::new(Mutex::new(None)),
      conn_seq: Arc::new(AtomicU64::new(0)),
      next_id: AtomicU64::new(1),
    });
    let d = daemon.clone();
    tokio::spawn(async move { accept_loop(listener, d).await });
    Ok(daemon)
  }

  /// 向 controller 发一次认证请求并等待结果。
  ///
  /// 无 controller 连接时立即返回 `Err`（调用方把它映射成 Failed，绝不挂起）。
  /// 超过 120s 仍无回报时视为超时：通知 controller 关闭该请求的弹窗，避免
  /// 用户对着一个已经失效的对话框白输入。
  pub async fn request(&self, req: AuthRequest) -> Result<AuthResult, String> {
    let id = self.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    self.pending.lock().unwrap().insert(id, tx);
    // 守卫在函数结束（含被 select 放弃）时移除表项。
    let _guard = PendingGuard {
      map: self.pending.clone(),
      id,
    };

    let sender = {
      let active = self.active.lock().unwrap();
      active.as_ref().map(|(_, s)| s.clone())
    };
    let Some(sender) = sender else {
      return Err("no controller connected".to_string());
    };
    sender
      .send(ServerMsg::Request { id, req: req.clone() })
      .await
      .map_err(|_| "controller gone".to_string())?;

    match tokio::time::timeout(Duration::from_secs(120), rx).await {
      Ok(Ok(r)) => Ok(r),
      Ok(Err(_)) => Err("controller dropped the response".to_string()),
      Err(_) => {
        // 超时：主动通知 controller 关掉这个弹窗，让用户立即感知认证已结束。
        self.cancel(&req.cookie).await;
        Err("authentication timed out".to_string())
      }
    }
  }

  /// 通知 controller 关闭指定 cookie 的弹窗（polkitd 取消了认证）。
  pub async fn cancel(&self, cookie: &str) {
    crate::logging::log_line(&format!(
      "polkit-tui-agent: daemon cancel cookie={}",
      crate::log_cookie(cookie)
    ));
    let sender = {
      let active = self.active.lock().unwrap();
      active.as_ref().map(|(_, s)| s.clone())
    };
    if let Some(s) = sender {
      let _ = s
        .send(ServerMsg::Cancel {
          cookie: cookie.to_string(),
        })
        .await;
    }
  }
}

async fn accept_loop(listener: UnixListener, daemon: Arc<Daemon>) {
  loop {
    match listener.accept().await {
      Ok((stream, _)) => {
        let d = daemon.clone();
        tokio::spawn(async move { handle_connection(stream, d).await });
      }
      Err(e) => {
        crate::logging::error_line(&format!("polkit-tui-agent: accept error: {e}"));
        tokio::time::sleep(Duration::from_millis(200)).await;
      }
    }
  }
}

async fn handle_connection(stream: UnixStream, daemon: Arc<Daemon>) {
  // 仅接受同用户连接（daemon 与 controller 同属一个用户，socket 文件权限在
  // XDG_RUNTIME_DIR 下已受目录 0700 保护，这里是 peer credential 的纵深防御）。
  let uid = match stream.peer_cred() {
    Ok(cred) => cred.uid(),
    Err(_) => {
      crate::logging::error_line("polkit-tui-agent: rejected connection (no peer cred)");
      return;
    }
  };
  if uid != uzers::get_current_uid() {
    crate::logging::error_line(&format!("polkit-tui-agent: rejected connection from uid {uid}"));
    return;
  }

  let my_gen = daemon.conn_seq.fetch_add(1, Ordering::SeqCst);
  let (read_half, write_half) = stream.into_split();

  // 注册为当前 controller。写端由独立任务负责转发 ServerMsg。
  let (tx, mut rx) = mpsc::channel::<ServerMsg>(16);
  {
    let mut active = daemon.active.lock().unwrap();
    *active = Some((my_gen, tx));
  }

  let mut writer = BufWriter::new(write_half);
  let write_task = tokio::spawn(async move {
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
  });

  // 读端：接收 controller 的响应，唤醒对应请求。
  let mut reader = BufReader::new(read_half).lines();
  while let Ok(Some(line)) = reader.next_line().await {
    let Ok(msg) = serde_json::from_str::<ClientMsg>(&line) else {
      continue;
    };
    match msg {
      ClientMsg::Response { id, result } => {
        if let Some(tx) = daemon.pending.lock().unwrap().remove(&id) {
          let _ = tx.send(result);
        }
      }
    }
  }

  // 断开：仅当仍是「当前」controller 时才清空，避免误清新连接。
  {
    let mut active = daemon.active.lock().unwrap();
    if let Some((g, _)) = active.as_ref()
      && *g == my_gen
    {
      *active = None;
      // 进行中的请求已无人回报，立即全部判失败，避免调用方空等 120s 超时。
      let mut pending = daemon.pending.lock().unwrap();
      for (_, tx) in pending.drain() {
        let _ = tx.send(AuthResult::Failed);
      }
    }
  }
  write_task.abort();
  crate::logging::log_line("polkit-tui-agent: controller disconnected");
}
