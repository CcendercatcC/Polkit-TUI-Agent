//! # TUI 界面：状态 + 渲染
//!
//! 纯 ratatui + crossterm，无 GUI 依赖。包含两部分：
//!
//! - **状态**：`App` 持有当前对话框（`ActivePrompt`）、密码输入缓冲与光标。
//!   它是纯数据、无副作用，便于被事件循环反复操作。
//! - **渲染**：`render`/`render_full` 把状态画到屏幕；空闲时不画任何东西
//!   （ratatui 每帧 diff，空帧即清屏）。
//!
//! ## 输入缓冲用 `Vec<char>` 而不是 `String`
//!
//! 密码可能含多字节字符（中文等）。`String` 的下标是字节，直接
//! `insert(byte_idx)` 会错位甚至在非字符边界 panic。`Vec<char>` 让「下标 =
//! 字符序号」，插入、删除、掩码计数全部天然正确。

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tokio::sync::oneshot;

/// UI 对 agent 某一次密码请求的答复。
#[derive(Debug)]
pub enum PromptAnswer {
  /// 用户提交的密码（已输入非空才产生）。
  Submit(String),
  /// 用户取消（Esc / Ctrl-C）。
  Cancel,
}

/// agent → UI 的认证事件。
pub enum UiEvent {
  /// 请求输入密码。`reply` 是本次请求的应答通道：
  /// UI 提交/取消时把 `PromptAnswer` 发回阻塞中的 `begin_authentication`。
  /// 随 `status` 带来上一轮失败的提示（首次为空）。
  Prompt {
    cookie: String,
    action_id: String,
    message: String,
    username: String,
    status: String,
    reply: oneshot::Sender<PromptAnswer>,
  },
  /// polkitd 通过 CancelAuthentication 要求关掉对话框。
  Cancel { cookie: String },
  /// 认证过程中的状态信息（PAM 提示 / 错误），实时刷到对话框。
  Status { cookie: String, text: String },
  /// 认证结束（成功或整个流程被取消），关闭对话框。
  Dismiss { cookie: String },
}

/// `handle_key` 返回的动作：表示用户对当前对话框做了一个决定。
pub enum Action {
  Submit(String),
  Cancel,
}

/// 对话框的两种显示状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptState {
  /// 等待输入密码（可编辑）。
  Editing,
  /// 已提交、helper 正在验证（只读，显示「正在验证…」）。
  Verifying,
}

/// 当前活动对话框的全部信息。
pub struct ActivePrompt {
  pub cookie: String,
  pub username: String,
  pub message: String,
  pub action_id: String,
  pub status: String,
  pub state: PromptState,
  /// `Option` 便于 `take()` 移走 oneshot Sender（只能发一次）。
  pub reply: Option<oneshot::Sender<PromptAnswer>>,
}

/// 应用状态：当前对话框 + 密码输入缓冲 + 光标（字符下标）。
pub struct App {
  pub active: Option<ActivePrompt>,
  input: Vec<char>,
  cursor: usize,
}

impl App {
  pub fn new() -> Self {
    Self {
      active: None,
      input: Vec::new(),
      cursor: 0,
    }
  }

  /// 处理键盘事件。返回 `Some(Action)` 表示用户做了决定（提交/取消），
  /// 此时调用方应把结果经 `reply` 通道发给 agent。
  ///
  /// - 无对话框时返回 `None`（唯一例外是 Ctrl-C 返回 `Some(Action::Cancel)`，
  ///   事件循环在调用本方法前先拦截它作为退出信号，见 main.rs 的 `run_tui`）。
  /// - `Verifying` 态锁死输入，只接受 Esc/Ctrl-C 取消。
  pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
      return Some(Action::Cancel);
    }
    let active = self.active.as_mut()?;
    if active.state == PromptState::Verifying {
      return None;
    }
    match key.code {
      KeyCode::Esc => Some(Action::Cancel),
      KeyCode::Enter => {
        // 空密码不允许提交。
        if !self.input.is_empty() {
          let pwd: String = std::mem::take(&mut self.input).into_iter().collect();
          self.cursor = 0;
          active.state = PromptState::Verifying;
          Some(Action::Submit(pwd))
        } else {
          None
        }
      }
      KeyCode::Backspace => {
        if self.cursor > 0 {
          self.input.remove(self.cursor - 1);
          self.cursor -= 1;
        }
        None
      }
      KeyCode::Left => {
        self.cursor = self.cursor.saturating_sub(1);
        None
      }
      KeyCode::Right => {
        self.cursor = (self.cursor + 1).min(self.input.len());
        None
      }
      KeyCode::Home => {
        self.cursor = 0;
        None
      }
      KeyCode::End => {
        self.cursor = self.input.len();
        None
      }
      KeyCode::Char(c) => {
        // 过滤 Ctrl+字母（避免把 Ctrl-C 当字符输入）。
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
          self.input.insert(self.cursor, c);
          self.cursor += 1;
        }
        None
      }
      _ => None,
    }
  }

  /// 直接打开一个对话框（无 oneshot 应答通道）。
  ///
  /// 供 `--prompt` 弹窗模式用：本进程自己处理回车/取消，不需要回传 agent。
  pub fn open_prompt(
    &mut self,
    cookie: String,
    action_id: String,
    message: String,
    username: String,
  ) {
    self.active = Some(ActivePrompt {
      cookie,
      username,
      message,
      action_id,
      status: String::new(),
      state: PromptState::Editing,
      reply: None,
    });
    self.input.clear();
    self.cursor = 0;
  }

  /// 认证失败后重新进入可编辑态（清空输入、更新状态行）。
  pub fn retry(&mut self, status: String) {
    if let Some(a) = self.active.as_mut() {
      a.state = PromptState::Editing;
      a.status = status;
    }
    self.input.clear();
    self.cursor = 0;
  }

  /// 消费一条 agent 发来的认证事件，更新状态。
  ///
  /// 所有事件都带 `cookie` 用于校验是否匹配当前对话框，避免错配
  /// （理论上同一时间只有一次认证，这里是防御性检查）。
  pub fn on_event(&mut self, ev: UiEvent) {
    match ev {
      UiEvent::Prompt {
        cookie,
        action_id,
        message,
        username,
        status,
        reply,
      } => {
        self.active = Some(ActivePrompt {
          cookie,
          username,
          message,
          action_id,
          status,
          state: PromptState::Editing,
          reply: Some(reply),
        });
        // 新对话框总是清空旧输入。
        self.input.clear();
        self.cursor = 0;
      }
      UiEvent::Cancel { cookie } => {
        if self.active.as_ref().is_some_and(|a| a.cookie == cookie) {
          self.active = None;
        }
      }
      UiEvent::Status { cookie, text } => {
        if let Some(a) = self.active.as_mut()
          && a.cookie == cookie
        {
          a.status = text;
        }
      }
      UiEvent::Dismiss { cookie } => {
        if self.active.as_ref().is_some_and(|a| a.cookie == cookie) {
          self.active = None;
        }
      }
    }
  }
}

/// 每帧入口：有对话框才画（inline 模式：居中 60%×40%）。
pub fn render(frame: &mut Frame, app: &App) {
  if let Some(a) = &app.active {
    let area = centered_rect(60, 40, frame.area());
    draw_dialog_at(frame, app, a, area);
  }
}

/// 每帧入口：对话框铺满整个屏幕（`--prompt` 弹窗模式）。
pub fn render_full(frame: &mut Frame, app: &App) {
  if let Some(a) = &app.active {
    draw_dialog_at(frame, app, a, frame.area());
  }
}

/// 在指定区域绘制密码对话框。
#[allow(clippy::vec_init_then_push)]
fn draw_dialog_at(frame: &mut Frame, app: &App, a: &ActivePrompt, area: Rect) {
  // 先 Clear 覆盖旧内容再画框，避免残留。
  frame.render_widget(Clear, area);

  // 标题行：「polkit」+ action 标识（如 org.freedesktop.policykit.exec）。
  let title = Line::from(vec![
    Span::styled(
      "polkit",
      Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD),
    ),
    Span::raw("  "),
    Span::styled(a.action_id.clone(), Style::default().fg(Color::DarkGray)),
  ]);

  // 内容行：用户 / 空行 / 认证消息 / 空行，随后按状态追加。
  let mut lines: Vec<Line> = Vec::new();
  lines.push(Line::from(vec![
    Span::styled("用户: ", Style::default().fg(Color::Gray)),
    Span::styled(
      a.username.clone(),
      Style::default().add_modifier(Modifier::BOLD),
    ),
  ]));
  lines.push(Line::raw(""));
  lines.push(Line::from(vec![Span::raw(a.message.clone())]));
  lines.push(Line::raw(""));

  // 状态行颜色：含「失败/错误」标红，其余标黄。
  let status_style = if a.status.contains("失败") || a.status.contains("错误") {
    Style::default().fg(Color::Red)
  } else {
    Style::default().fg(Color::Yellow)
  };

  if a.state == PromptState::Verifying {
    lines.push(Line::from(vec![Span::styled(
      "正在验证…",
      Style::default().fg(Color::Yellow),
    )]));
    if !a.status.is_empty() {
      lines.push(Line::from(Span::styled(a.status.clone(), status_style)));
    }
  } else {
    // 密码行：标签 + 掩码（每字符一个「•」），不显示明文。
    lines.push(Line::from(vec![
      Span::styled(PASSWORD_LABEL, Style::default().fg(Color::Gray)),
      Span::raw(mask(&app.input)),
    ]));
    if !a.status.is_empty() {
      lines.push(Line::from(Span::styled(a.status.clone(), status_style)));
    }
    lines.push(Line::from(vec![
      Span::styled("回车 确认", Style::default().fg(Color::DarkGray)),
      Span::raw("   "),
      Span::styled("Esc 取消", Style::default().fg(Color::DarkGray)),
    ]));
  }

  // 画边框 + 内文。title 渲染在边框行，inner 是去掉边框后的区域。
  let block = Block::default()
    .borders(Borders::ALL)
    .title(title)
    .title_alignment(Alignment::Center);
  let inner = block.inner(area);
  frame.render_widget(block, area);
  frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

  // 编辑态才放光标：落在「密码: 」后第 cursor 个掩码字符的右侧。
  // PASSWORD_LABEL_W 是标签的显示列宽（"密码: " = 2+2+1+1 = 6）。
  if a.state == PromptState::Editing {
    let row = password_row(inner, &a.message);
    let x = (inner.x + PASSWORD_LABEL_W + app.cursor as u16).min(inner.right().saturating_sub(1));
    frame.set_cursor_position((x, row));
  }
}

/// 密码输入行的固定标签与它的显示列宽。
const PASSWORD_LABEL: &str = "密码: ";
const PASSWORD_LABEL_W: u16 = 6;

/// 把每个输入字符替换为掩码字符「•」（1 列宽）。
fn mask(input: &[char]) -> String {
  input.iter().map(|_| '•').collect()
}

/// 估算文本在给定宽度下会折成几行（用于定位密码行的 y 坐标）。
///
/// 按实际显示列宽计算（CJK/全角字符计 2 列、组合字符计 0 列），对任意
/// 消息文本都足够精确。
fn wrapped_line_count(text: &str, width: usize) -> usize {
  use unicode_width::UnicodeWidthStr;
  if width == 0 {
    return 0;
  }
  text
    .lines()
    .map(|l| l.width().div_ceil(width))
    .sum::<usize>()
    .max(1)
}

/// 密码输入行的 y 坐标。
///
/// 内容行结构：`0=用户`、`1=空`、`2=消息(可能折多行)`、`3=空`、`4+=密码`。
/// 所以密码行 y = 对话框内区顶 + 3 + 消息折行数。
fn password_row(inner: Rect, message: &str) -> u16 {
  let msg_rows = wrapped_line_count(message, inner.width.saturating_sub(2) as usize);
  inner.y + 3 + msg_rows as u16
}

/// 生成屏幕中央、指定百分比大小的矩形。
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
  // ratatui 0.30 的 Flex::Center 布局：先垂直再水平居中。
  let [area] = Layout::vertical([Constraint::Percentage(percent_y)])
    .flex(Flex::Center)
    .areas(r);
  let [area] = Layout::horizontal([Constraint::Percentage(percent_x)])
    .flex(Flex::Center)
    .areas(area);
  area
}
