//! # polkit-agent-helper-1 会话客户端
//!
//! 负责与 **polkit 的 PAM 认证 helper** 通信。helper 是 polkit 提供的特权进程
//! （由 systemd 以 root 运行），它完成真正的密码验证（PAM）并在成功后以 root
//! 身份回调 polkitd 的 `AuthenticationAgentResponse2/3`。
//!
//! ## 为什么需要 helper？安全边界
//!
//! 普通 agent 进程没有权限直接告诉 polkitd「认证通过」——否则任何进程都能伪造。
//! 因此密码验证必须由特权 helper 完成：agent 只负责收集密码，并通过私有通道
//! （socket / 匿名管道）传给 helper，**密码从不经过 D-Bus**。
//!
//! ## 两种连接方式（双路径回退）
//!
//! 1. **socket 激活**（polkit ≥ 123，本机 127 即此模式）：
//!    连 `/run/polkit/agent-helper.socket`，依次写入 `<用户名>\n`、`<cookie>\n`，
//!    systemd 按需拉起 helper。helper 无 setuid 位（靠 socket 鉴权拿调用方 uid）。
//! 2. **setuid 二进制回退**（socket 不存在时）：
//!    `spawn <helper> <用户名>`，用户名走 argv[1]，cookie 写 stdin。
//!
//! ## 行协议（helper stdout，逐行）
//!
//! ```text
//! PAM_PROMPT_ECHO_OFF <提示>  → 请求不回显输入（密码），agent 回 `密码\n`
//! PAM_PROMPT_ECHO_ON  <提示>  → 请求回显输入，agent 回普通文本
//! PAM_ERROR_MSG <文本>        → 错误信息，展示给用户
//! PAM_TEXT_INFO <文本>        → 一般信息，展示给用户
//! SUCCESS                    → 认证成功（helper 已调 Response2/3）
//! FAILURE                    → 认证失败
//! ```

use std::path::Path;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

/// helper 的 systemd socket 激活路径（Arch 上由 `polkit-agent-helper.socket` 提供）。
const SOCKET_PATH: &str = "/run/polkit/agent-helper.socket";
/// setuid helper 二进制（回退路径；本机 helper 已非 setuid，仅兜底兼容老系统）。
const HELPER_BIN: &str = "/usr/lib/polkit-1/polkit-agent-helper-1";

/// helper 行协议解析后的消息类型。
#[derive(Debug)]
pub enum PamMessage {
  /// 请求不回显输入（密码）。携带 PAM 提示文本。
  PromptEchoOff(String),
  /// 请求回显输入（如 OTP）。携带 PAM 提示文本。
  PromptEchoOn(String),
  /// PAM 错误消息。
  Error(String),
  /// PAM 信息消息。
  Info(String),
  /// 认证成功。
  Success,
  /// 认证失败（如密码错误）。
  Failure,
}

/// 底层连接的具体形态（socket 还是子进程）。
#[allow(clippy::large_enum_variant)]
enum Inner {
  Socket {
    // Box<dyn AsyncRead> 同时容纳 UnixStream 的读半段和子进程 stdout。
    reader: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
  },
  Binary {
    reader: BufReader<ChildStdout>,
    writer: ChildStdin,
    // 持有 Child 防止被 drop（drop 会杀进程），保证 PAM 会话期间存活。
    _child: Child,
    _stderr: Option<ChildStderr>,
  },
}

/// helper 会话：读/写 helper 的私有流，封装行协议。
pub struct HelperSession {
  inner: Inner,
}

impl HelperSession {
  /// 建立与 helper 的连接并完成握手（用户名 + cookie）。
  ///
  /// 优先走 socket；socket 文件不存在时回退 setuid 二进制。
  /// 握手写入顺序是协议规定：
  /// - socket 模式：先 `用户名`，再 `cookie`（用户名在 socket 模式下
  ///   无法通过 argv 传，必须走首行）。
  /// - 二进制模式：用户名是 argv[1]，stdin 只写 `cookie`。
  pub async fn connect(username: &str, cookie: &str) -> Result<Self, String> {
    if Path::new(SOCKET_PATH).exists() {
      let stream = UnixStream::connect(SOCKET_PATH)
        .await
        .map_err(|e| format!("failed to connect to {SOCKET_PATH}: {e}"))?;
      let (r, w) = stream.into_split();
      let mut session = HelperSession {
        inner: Inner::Socket {
          reader: BufReader::new(Box::new(r)),
          writer: Box::new(w),
        },
      };
      session.write_line(username).await?;
      session.write_line(cookie).await?;
      Ok(session)
    } else {
      let mut child = Command::new(HELPER_BIN)
        .arg(username)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {HELPER_BIN}: {e}"))?;
      let writer = child.stdin.take().ok_or("helper stdin unavailable")?;
      let reader = child.stdout.take().ok_or("helper stdout unavailable")?;
      let stderr = child.stderr.take();
      let mut session = HelperSession {
        inner: Inner::Binary {
          reader: BufReader::new(reader),
          writer,
          _child: child,
          _stderr: stderr,
        },
      };
      session.write_line(cookie).await?;
      Ok(session)
    }
  }

  /// 写一行到 helper（自动补 `\n`）。两种连接统一走 `&mut dyn AsyncWrite`。
  async fn write_line(&mut self, line: &str) -> Result<(), String> {
    let w: &mut (dyn AsyncWrite + Unpin + Send) = match &mut self.inner {
      Inner::Socket { writer, .. } => writer.as_mut(),
      Inner::Binary { writer, .. } => writer,
    };
    // 分两次写避免为拼接换行做一次字符串分配。
    w.write_all(line.as_bytes())
      .await
      .map_err(|e| format!("failed to write to helper: {e}"))?;
    w.write_all(b"\n")
      .await
      .map_err(|e| format!("failed to write to helper: {e}"))
  }

  /// 应答 PAM 提示：把用户输入（密码/文本）写回 helper。
  pub async fn respond(&mut self, text: &str) -> Result<(), String> {
    self.write_line(text).await
  }

  /// 读取下一条 PAM 消息；连接关闭（EOF）返回 `Ok(None)`。
  ///
  /// 解析规则：一行以 `<命令>` 开头，可选地接空格 + 参数。未知命令按
  /// `Info` 对待，避免协议演进时误伤。
  pub async fn next_message(&mut self) -> Result<Option<PamMessage>, String> {
    let r: &mut (dyn AsyncBufRead + Unpin + Send) = match &mut self.inner {
      Inner::Socket { reader, .. } => reader as &mut (dyn AsyncBufRead + Unpin + Send),
      Inner::Binary { reader, .. } => reader,
    };
    let mut line = String::new();
    let n = r
      .read_line(&mut line)
      .await
      .map_err(|e| format!("failed to read from helper: {e}"))?;
    if n == 0 {
      return Ok(None);
    }
    // 去掉行尾换行（helper 用 \n，兼容 \r\n）。
    let line = line.trim_end_matches(['\n', '\r']);
    // 命令与参数以空格分隔；无空格则参数为空。
    let (cmd, rest) = match line.split_once(' ') {
      Some((c, r)) => (c, r),
      None => (line, ""),
    };
    let msg = match cmd {
      "PAM_PROMPT_ECHO_OFF" => PamMessage::PromptEchoOff(rest.to_string()),
      "PAM_PROMPT_ECHO_ON" => PamMessage::PromptEchoOn(rest.to_string()),
      "PAM_ERROR_MSG" => PamMessage::Error(rest.to_string()),
      "PAM_TEXT_INFO" => PamMessage::Info(rest.to_string()),
      "SUCCESS" => PamMessage::Success,
      "FAILURE" => PamMessage::Failure,
      _ => PamMessage::Info(line.to_string()),
    };
    Ok(Some(msg))
  }
}
