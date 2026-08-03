//! # daemon 与 controller 之间的线协议（NDJSON）
//!
//! 每个消息一行 JSON，走 unix stream socket。`tag` 字段区分消息类型。
//!
//! - daemon → controller：`ServerMsg`
//! - controller → daemon：`ClientMsg`
//!
//! 密码永不出现在这些消息里——弹窗进程（`--prompt`）拿到 cookie 后通过
//! helper 私有通道完成认证，controller 只回报一个最终结果。

use serde::{Deserialize, Serialize};

/// 一次认证请求的全部信息（polkit cookie + 展示信息）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequest {
  pub cookie: String,
  pub user: String,
  pub action: String,
  pub message: String,
}

/// 认证最终结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthResult {
  /// 认证成功（helper 已代调 AuthenticationAgentResponse2/3）。
  Ok,
  /// 用户取消。
  Cancel,
  /// 一般失败。
  Failed,
}

/// daemon → controller。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerMsg {
  /// 发起一次认证：`id` 是 daemon 侧用于关联响应的序号。
  Request { id: u64, req: AuthRequest },
  /// 通知关闭弹窗（polkitd 取消了认证）。`cookie` 标明关闭哪一次认证的弹窗。
  Cancel { cookie: String },
}

/// controller → daemon。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMsg {
  /// 某次请求的结果。
  Response { id: u64, result: AuthResult },
}
