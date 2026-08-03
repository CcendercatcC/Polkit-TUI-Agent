//! # 日志与报错通道
//!
//! 通道约定（TUI 输出走 `/dev/tty` 后，stdout/stderr 与界面彻底解耦）：
//!
//! - **日志走 stdout**（`log_line`）：正常流程的状态信息。TUI 活跃且 stdout
//!   是终端时，直接写 stdout 会落在 alternate screen 的光标处——而光标被
//!   ratatui 停在密码输入框（见 `ui.rs` 的 `set_cursor_position`），日志会污染
//!   输入框且绕开 ratatui 的 diff 无法被重绘抹掉；因此这时改写到**屏幕左上角**
//!   （对话框居中，其顶边在约 30% 屏高处，第一行始终在绘制区上方），写完恢复
//!   光标位置。
//! - **报错走 stderr**（`error_line`）：真正的错误/告警，始终原文。inline TUI
//!   存活期间不产生报错（认证失败走对话框状态行，致命错误在 TUI 已 Drop 后由
//!   `main` 返回 Err 打印），故报错写 stderr 不会破坏界面。

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use unicode_width::UnicodeWidthChar;

/// inline TUI 是否正在屏幕上显示。进入 TUI 前置 true、restore 前置 false。
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 标记 inline TUI 是否活跃，见模块文档。
pub(crate) fn set_tui_active(active: bool) {
  TUI_ACTIVE.store(active, Ordering::Relaxed);
}

/// 输出一条日志（stdout）：TUI 活跃且 stdout 是终端时写到屏幕左上角安全区，
/// 否则 `println!` 原文到 stdout（重定向到文件时是干净原文，不掺转义字节）。
pub(crate) fn log_line(msg: &str) {
  if TUI_ACTIVE.load(Ordering::Relaxed) && std::io::stdout().is_terminal() {
    log_line_to_corner(msg);
  } else {
    println!("{msg}");
  }
}

/// 输出一条报错（stderr）：始终 `eprintln!` 原文。
pub(crate) fn error_line(msg: &str) {
  eprintln!("{msg}");
}

/// 把日志写到屏幕左上角（第一行），不干扰对话框绘制。
///
/// 对话框居中，inline 模式下其顶边在约 30% 屏高处，第一行始终在对话框上方，
/// 不会与界面重叠；弹窗全量重绘（`run_tui` 的空帧）也不触碰第一行，日志可
/// 跨重绘留存。
///
/// 用 `queue!` 把 `SavePosition → MoveTo(0, 0) → Clear(CurrentLine) →
/// Print(截断到列宽的原文) → RestorePosition` 拼成一段字节，一次 `write_all`
/// 到 stdout，尽量与 ratatui 的 `/dev/tty` 刷新保持原子；写完恢复光标位置
/// （ratatui 把光标停在输入框），下一帧照常放回输入框。
fn log_line_to_corner(msg: &str) {
  let (cols, _) = crossterm::terminal::size().unwrap_or((80, 24));
  let row = 0;
  let max_w = cols.saturating_sub(1) as usize;
  // 超宽截断，防止折行顶掉上一行内容。
  let text: String = msg
    .chars()
    .scan(0usize, |w, c| {
      let cw = UnicodeWidthChar::width(c).unwrap_or(0);
      if *w + cw > max_w {
        None
      } else {
        *w += cw;
        Some(c)
      }
    })
    .collect();
  let mut buf: Vec<u8> = Vec::new();
  let _ = queue!(
    &mut buf,
    SavePosition,
    MoveTo(0, row),
    Clear(ClearType::CurrentLine),
    Print(text),
    RestorePosition
  );
  let mut out = std::io::stdout();
  let _ = out.write_all(&buf);
  let _ = out.flush();
}
