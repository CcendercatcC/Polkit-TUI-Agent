//! # TUI 终端封装：输出走 `/dev/tty`
//!
//! `ratatui::init()`/`restore()` 把绘制、alternate screen 都绑死在 **stdout** 上：
//! 一旦用户重定向 stdout/stderr，TUI 就跟着跑偏，日志还会和界面抢同一个终端
//! （本项目早期"日志飘进输入框"的根因）。
//!
//! 本模块改为把 TUI 输出到 **`/dev/tty`**（进程的控制终端），与 stdio 彻底解耦：
//! stdout 留给日志、stderr 留给报错，任意重定向都不影响界面渲染。
//!
//! - `has_controlling_tty`：启动前检查 `/dev/tty` 是否可打开。
//! - `Tui::open`：开 `/dev/tty` → raw mode → alternate screen → 构建全屏 Terminal，
//!   并安装恢复终端的 panic hook。
//! - `Drop`：离开 alternate screen + 关 raw mode，任何退出路径（含主循环 panic
//!   展开）都会还原终端。
//!
//! 注意：**不能用 `Terminal::clear()`** 做整屏重绘——它内部会查询光标位置
//! （CPR 查询），而本程序运行的 `EventStream` 后台线程无限占用 crossterm 全局
//! 事件读取器锁，查询必然超时。强制全量重绘用「空帧 + 正常帧」两段 draw
//! （见 main.rs 的 `run_tui`）。

use std::fs::{File, OpenOptions};
use std::sync::{Mutex, OnceLock};

use crossterm::execute;
use crossterm::terminal::{
  disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::logging;

/// 供 panic hook 使用的 `/dev/tty` 句柄（`Tui` 存活期间持有克隆）。
static PANIC_TTY: Mutex<Option<File>> = Mutex::new(None);

/// TUI 句柄。`terminal` 绘制到 `/dev/tty`；`tty` 留给 `Drop` 还原屏幕。
pub struct Tui {
  tty: File,
  pub terminal: Terminal<CrosstermBackend<File>>,
}

/// 是否有可控终端（`/dev/tty` 可打开）。TUI 模式的启动守卫，替代原先的
/// `stdin().is_terminal()`：TUI 走 `/dev/tty` 后 stdin 是否终端已无关紧要。
pub fn has_controlling_tty() -> bool {
  OpenOptions::new()
    .read(true)
    .write(true)
    .open("/dev/tty")
    .is_ok()
}

/// 还原终端状态：清全局句柄、离开 alternate screen、关 raw mode。
fn restore_now(tty: &mut File) {
  logging::set_tui_active(false);
  *PANIC_TTY.lock().unwrap() = None;
  let _ = execute!(tty, LeaveAlternateScreen);
  let _ = disable_raw_mode();
}

/// 安装恢复终端的 panic hook（等价于 `ratatui::init()` 提供的 hook，但恢复
/// 目标是 `/dev/tty` 而非 stdout）。只安装一次。
fn install_panic_hook() {
  static HOOK: OnceLock<()> = OnceLock::new();
  HOOK.get_or_init(|| {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
      // 先关 raw mode 再离开 alternate screen（与 ratatui::restore 同序）。
      let _ = disable_raw_mode();
      if let Some(f) = PANIC_TTY.lock().unwrap().as_mut() {
        let _ = execute!(f, LeaveAlternateScreen);
      }
      prev(info);
    }));
  });
}

impl Tui {
  /// 打开 `/dev/tty`、进 raw mode 与 alternate screen，构建全屏 TUI。
  ///
  /// 中途任一步失败都会把已改的终端状态还原，不留裸 raw mode。
  pub fn open() -> Result<Self, String> {
    let mut tty = OpenOptions::new()
      .read(true)
      .write(true)
      .open("/dev/tty")
      .map_err(|e| format!("cannot open /dev/tty: {e}"))?;
    enable_raw_mode().map_err(|e| e.to_string())?;
    if let Err(e) = execute!(&mut tty, EnterAlternateScreen) {
      let _ = disable_raw_mode();
      return Err(format!("enter alternate screen failed: {e}"));
    }
    logging::set_tui_active(true);
    install_panic_hook();
    let clone = tty.try_clone().map_err(|e| {
      restore_now(&mut tty);
      format!("duplicate /dev/tty failed: {e}")
    })?;
    *PANIC_TTY.lock().unwrap() = Some(clone);
    let backend = CrosstermBackend::new(tty.try_clone().map_err(|e| {
      restore_now(&mut tty);
      format!("duplicate /dev/tty failed: {e}")
    })?);
    let terminal = Terminal::new(backend).map_err(|e| {
      restore_now(&mut tty);
      format!("terminal init failed: {e}")
    })?;
    Ok(Self { tty, terminal })
  }
}

impl Drop for Tui {
  fn drop(&mut self) {
    // 先关 raw mode 再离开 alternate screen（与 ratatui::restore 同序）。
    let _ = disable_raw_mode();
    let _ = execute!(&mut self.tty, LeaveAlternateScreen);
    logging::set_tui_active(false);
    *PANIC_TTY.lock().unwrap() = None;
  }
}
