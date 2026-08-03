# polkit-tui-agent internal architecture

> 中文版本: [ARCHITECTURE_cn.md](ARCHITECTURE_cn.md)

This document describes the architecture **inside this program**: module
dependencies, how processes and tasks are composed, how data flows, the
concurrency and synchronization points, and the timeout and security
boundaries. It does not cover details of the polkit protocol itself (that part
is in AGENTS.md). It targets developers modifying the code.

## 1. Overview

A single Rust binary (`src/main.rs`, one crate) with 10 modules and zero
external binary dependencies (it only invokes `tmux` and the system
`polkit-agent-helper-1`). There is no "library crate plus multiple binaries"
split — **all modules are declared into the same crate via `mod` in
`main.rs`**, and each run mode is dispatched by `main` based on command-line
flags, reusing the same set of components.

Five run modes (four user-visible + one internal):

| Mode | Flag | Processes | Assembly |
|---|---|---|---|
| inline TUI | (default) | 1 | `Agent::inline` + `run_tui` event loop |
| Background daemon | `--daemon` | 1 | `Agent::daemon` + `Daemon::start` + socket server |
| tmux controller | `--controller` | 1 (standalone) | `controller::run`, connects to the daemon's socket |
| tmux all-in-one | `--tmux` | 1 | daemon components + controller task self-connecting, same process |
| Popup (internal) | `--prompt` | 1 (short-lived) | event loop + helper auth, spawned by the controller |

Key insight: **`--tmux` is `--daemon` plus a `controller::run` task**;
`--prompt` is the only process that carries its own complete auth loop — every
other mode either borrows the UI event loop (inline) or the popup process
(daemon/controller) to collect the password.

## 2. Module dependencies

Compile-time dependencies (`mod` references) in prose: `main` declares all 10
modules; `agent` depends on `daemon`/`helper`/`protocol`/`ui`; `daemon` and
`controller` depend on `protocol`; `prompt` depends on `helper`/`ui`/`tui`;
`tui` depends on `logging`. `helper`/`protocol`/`ui`/`logging` depend on no
crate-internal module (`ui` uses `tokio::sync::oneshot`, `logging` uses
`crossterm`/`unicode-width`).

```
            ┌──────────────────────────────────────────┐
            │                 main.rs                  │
            │   dispatch / args / session / run_tui    │
            └───┬─────┬──────┬──────┬──────┬───────────┘
                ▼     ▼      ▼      ▼      ▼
             agent  daemon controller prompt  tui
               │      │       │       │      │
               │      └───┬───┘       │      ▼
               │          ▼           │   logging
               │        protocol      │
               │          ▲           │
               ├───────► helper ◄─────┤   (prompt also depends on tui,
               └───────► ui ◄─────────┘     agent depends on protocol/helper/ui)
```

Arrows show the `use` dependency direction. Runtime collaboration:

| Provider | Consumer | Collaboration |
|---|---|---|
| `agent::Agent` | `main` | Exported as a D-Bus object (`object_server().at(OBJECT_PATH, ...)`) |
| `agent` | `ui` | Pushes auth events via `mpsc<UiEvent>`; `UiEvent::Prompt` carries an `oneshot::Sender` |
| `ui` | `agent` | Returns `PromptAnswer` (Submit/Cancel) over the oneshot |
| `agent` | `helper` | `HelperSession`: dual socket/binary paths to the root helper, line-protocol responses |
| `agent` | `daemon` | `daemon.request(AuthRequest) -> AuthResult`, `daemon.cancel(cookie)` |
| `daemon` | `controller` | socket NDJSON: `ServerMsg` (Request/Cancel) / `ClientMsg` (Response) |
| `controller` | `prompt` | Spawns `--prompt` via `tmux display-popup -E` + `POLKIT_*` env vars |
| `main`/`prompt` | `tui` | `Tui::open` obtains the `/dev/tty` handle, `Terminal::draw` renders |
| `main`/`prompt` | `ui` | `App` state machine + `render`/`render_full` |
| `main`/`daemon`/`tui` | `logging` | `log_line` (stdout) / `error_line` (stderr) / `set_tui_active` |

## 3. Process and task assembly

### 3.1 inline (default)

One process, two kinds of concurrent work:

- **zbus D-Bus dispatch**: `Connection::system()` + mounting `Agent::inline` at
  `OBJECT_PATH`. zbus runs with the `tokio` feature, so interface methods
  (`begin_authentication` etc.) run on the `#[tokio::main]` runtime;
  `begin_authentication` blocks awaiting auth while `cancel_authentication`
  interleaves concurrently.
- **`run_tui` event loop** (main.rs): a single `tokio::select!` waits on three
  sources — the crossterm `EventStream` (keyboard), `mpsc<UiEvent>` (agent
  pushes), and a 100ms tick (periodic redraw). Each select iteration finishes
  with one `terminal.draw` frame.

The two auth event paths are bridged via `mpsc<UiEvent>` +
`oneshot<PromptAnswer>`; keyboard activity flows back through `watch<Instant>`
for idle-timeout detection. TUI output goes to `/dev/tty` (`tui.rs`); `run_tui`
keeps its own `ui_tx` so the channel is never closed because all senders were
dropped. Concurrent auth requests are FIFO-serialized by the agent's
`slot: Semaphore(1)`, so only one dialog shows at a time.

### 3.2 Background daemon (`--daemon`)

Headless, no TUI at all. Task topology:

- Main flow: register the agent (`Agent::daemon`) → `Daemon::start` →
  `object_server().at` → `std::future::pending::<()>()` to stay resident.
- `Daemon::start` binds the socket, then spawns an `accept_loop`.
- `accept_loop` spawns a `handle_connection` per accepted connection.
- Each `handle_connection`: validates the peer uid → registers itself as the
  "current controller" → spawns a writer task (serializing `ServerMsg` from an
  mpsc into NDJSON lines) → this task loops reading `ClientMsg`.

The daemon never collects a password itself; `begin_authentication` hands the
`AuthRequest` to `daemon.request` and blocks until the controller reports back.
Concurrent auth is queued at the agent layer (`slot: Semaphore(1)`), so only one
request reaches the controller at a time and its single popup is never
clobbered.

### 3.3 tmux controller (`--controller`)

A standalone process running inside a tmux session. `run` never returns (except
on error): the outer loop `connect_with_retry` connects to the daemon socket,
then:

- spawns a `write_loop` task (`ClientMsg` → socket).
- The read loop parses `ServerMsg`: on `Request` it spawns a `run_popup` task
  (so the read loop is not blocked while a popup is open and `Cancel` is handled
  promptly); on `Cancel` it closes the popup by cookie and records it in the
  `cancelled` set (guarding against the "cancel arrives before the popup
  request" race).
- On disconnect, `write_task.abort()` and the outer loop reconnects.

Each popup is started by `run_popup` via `tmux display-popup -E` running a
`--prompt` child process; when it exits, the exit code is mapped to an
`AuthResult` and reported through mpsc → `write_loop` → socket.

### 3.4 tmux all-in-one (`--tmux`)

Sections 3.2 and 3.3 merged into one process: `tmux_main` does all the daemon
initialization first, then `tokio::spawn(controller::run(socket))` lets the
controller **self-connect to the socket started by its own process**. The
embedded controller task behaves exactly like a remote controller, except its
peer is this process's daemon. `--tmux` is the recommended user-facing mode but
introduces no new logic to the implementation.

### 3.5 Popup process (`--prompt`)

A short-lived process started by the controller inside `display-popup`,
self-contained for a single auth. Tasks:

- Main event loop (`select!` on four sources): keyboard, `mpsc<Outcome>`
  (background auth task reports), `watch<bool>` (cancel file), 100ms tick.
- Each password submission spawns an `authenticate_once` task that connects to
  the helper for PAM auth; the result returns via `Outcome` to the main loop
  (success → `break 0`, failure → `app.retry` back to editing).
- If `POLKIT_CANCEL_FILE` is set, an extra 200ms polling task is spawned that
  sets the cancel watch when the file appears.

## 4. Module internals

### 4.1 `main.rs` (entry + assembly + inline event loop)

| Item | Content |
|---|---|
| `OBJECT_PATH` | `/org/EMeow/PolicyKit1/AuthenticationAgent`, object path passed to polkitd on registration |
| `Options` | `locale` (default `$LANG`), `uid_session` |
| Dispatch | `main` checks `--prompt`/`--controller`/`--daemon`/`--tmux` in order, inline by default; `-h/--help` wins throughout |
| `inline_main` | check controlling terminal → connect system bus → create `mpsc<UiEvent>` → mount Agent → `build_subject` → `register` → `run_tui` → unregister on exit |
| `tmux_main`/`daemon_main` | register + `Daemon::start` + mount Agent + `pending()` resident; `--tmux` additionally spawns a self-connecting controller |
| `run_tui` | three-way `select!`; only allows `q`/Ctrl-C exit when no dialog is open; `needs_full_redraw` uses a two-phase "empty frame + normal frame" draw |
| `send_answer` | `take()`s the oneshot out of `app.active`'s `reply` field and sends `PromptAnswer` |
| `build_subject` | `find_uid_display_session` when `--uid-session`, otherwise `find_session_id` |
| Session resolution | `find_session_id` = own session (`GetSessionByPID`) → uid graphical session (`GetUser.Display`, `(so)` struct) → `XDG_SESSION_ID` fallback |
| `register` | `RegisterAuthenticationAgent`, logs the actual session-id for verification |
| `log_cookie` | cookie representation in logs: FNV-1a hash by default (`fnv1a_hex`, distinguishes requests under one agent); full value with `--full-cookie-log`; hashes have a global cache |

### 4.2 `agent.rs` (D-Bus interface + auth state machine)

| Item | Content |
|---|---|
| `PolkitError` | `#[zbus(prefix="org.freedesktop.PolicyKit1.Error")]`, two D-Bus error names: `Cancelled`/`Failed` |
| `Backend` | `Inline { events, activity }` / `Daemon { daemon: Arc<Daemon> }`, decides where the password is collected; `activity` is the keyboard-activity feedback channel |
| `Agent` | `backend` + `pending: Mutex<HashMap<String, watch::Sender<bool>>>` (cookie→cancel token) + `slot: Semaphore` (capacity 1, FIFO-serializes concurrent auth) |
| `begin_authentication` | pick username → register cancel token → `select!` (cancel while queued / acquire slot) → dispatch by backend; cleans up the token table and releases the slot on exit |
| `cancel_authentication` | set the watch + dispatch `UiEvent::Cancel` (inline) or `daemon.cancel` (daemon) by backend |
| `authenticate_inline` | auth main loop: send Prompt → `select!` (cancel/reply/timeout/keyboard activity refresh) → connect helper → PAM line-protocol loop → SUCCESS/Dismiss or retry on failure |
| `pick_username` | identity preference: current user → root → first candidate; `unix-group` skipped |
| `identity_uid` | manually parses `(String, HashMap<String, OwnedValue>)`, takes the `unix-user` uid |

### 4.3 `daemon.rs` (socket server)

| Item | Content |
|---|---|
| `Daemon` | `pending: Arc<Mutex<HashMap<u64, oneshot::Sender<AuthResult>>>>` (request id→reply), `active: Arc<Mutex<ActiveController>>`, `conn_seq` (generation), `next_id` |
| `ActiveController` | `Option<(u64, mpsc::Sender<ServerMsg>)>`, only one current controller at a time |
| `PendingGuard` | removes the pending entry on Drop, so the table never leaks after a request is abandoned/timed out |
| `start` | clears a stale socket (if connectable, reports "another daemon"), binds, spawns `accept_loop` |
| `request` | allocates an id → inserts into the table → takes the current controller → sends `ServerMsg::Request` → waits up to 120s for a reply; on timeout actively `cancel`s |
| `cancel` | sends `ServerMsg::Cancel` by cookie |
| `accept_loop` | spawns `handle_connection` per connection |
| `handle_connection` | peer_cred uid check → bumps generation and overwrites active → spawns writer task → read loop (`ClientMsg::Response` wakes the matching request) → on disconnect clears active only if still the current generation, and drains all `pending` as Failed (no 120s wait) |

### 4.4 `controller.rs` (tmux bridge)

| Item | Content |
|---|---|
| `run` | outer reconnect loop + read loop; `current: Arc<Mutex<Option<String>>>` tracks the popup's cookie, `cancelled: Arc<Mutex<HashSet<String>>>` records cancelled cookies (guard against "cancel before popup request" race) |
| `connect_with_retry` | reconnects every 2s, keeps waiting while the daemon is down |
| `write_loop` | serializes `ClientMsg` into NDJSON lines written to the socket |
| `run_popup` | `tmux display-popup -E -T "polkit 认证" -w 70% -h 50% -e POLKIT_* <exe> --prompt`; exit code 0→Ok / 2→Cancel / other→Failed |
| `cancel_file_path` | `$XDG_RUNTIME_DIR/polkit-tui-cancel-<FNV-1a hash>`, hash-derived when the cookie contains characters unsafe in filenames |

`Request` handling does not block the read loop: it records the current cookie
first, spawns a `run_popup` task, and reports back after the popup finishes
(cleaning up the cancel file and the current marker). When a `Cancel` arrives it
is recorded in `cancelled`; if the cookie matches `current`, it writes the
cancel file + `tmux display-popup -C` as a fallback close; if a `Request` with
the same cookie arrives afterwards, it reports cancellation directly without
popping up.

### 4.5 `prompt.rs` (popup single-request auth)

| Item | Content |
|---|---|
| `run` | reads `POLKIT_*` env vars → `Tui::open` → `App::open_prompt` → four-way `select!` event loop → exit code |
| `Outcome` | `Success` / `Failure` / `Error(String)`, the background auth task's report |
| `authenticate_once` | snapshots username/cookie/password, 10s helper connect, 30s per-message PAM loop, returns `Outcome` |
| Cancel-file polling | `POLKIT_CANCEL_FILE` present → set the watch → main loop `break 2` |
| Idle timeout | editing state and `last_activity.elapsed() >= input_timeout` → `break 1` (neither 0 nor 2, mapped to Failed) |

### 4.6 `helper.rs` (polkit-agent-helper-1 client)

| Item | Content |
|---|---|
| `SOCKET_PATH`/`HELPER_BIN` | `/run/polkit/agent-helper.socket` (socket activation preferred) / `/usr/lib/polkit-1/polkit-agent-helper-1` (setuid fallback) |
| `Inner` | `Socket { reader, writer }` (`Box<dyn AsyncRead/Write>`) or `Binary { reader, writer, _child, _stderr }` (holds the Child to keep the process from being killed on drop) |
| `connect` | if the socket exists, connect and write the "username, cookie" two lines per protocol; otherwise spawn the binary, username via argv, cookie on stdin |
| `write_line` | writes in two chunks to avoid string-concat allocation |
| `respond` | writes the password/text back to the helper |
| `next_message` | line-by-line parsing: `PAM_PROMPT_ECHO_OFF/ON`, `PAM_ERROR_MSG`, `PAM_TEXT_INFO`, `SUCCESS`, `FAILURE`; unknown commands tolerated as `Info` |

### 4.7 `protocol.rs` (NDJSON wire protocol)

`AuthRequest` (cookie/user/action/message), `AuthResult` (Ok/Cancel/Failed),
`ServerMsg` (`Request{id,req}` / `Cancel{cookie}`), `ClientMsg`
(`Response{id,result}`). All tagged with
`#[serde(tag="type", rename_all="lowercase")]`. Passwords never enter these
messages.

### 4.8 `ui.rs` (state + rendering)

| Item | Content |
|---|---|
| `PromptAnswer` | `Submit(String)` / `Cancel` |
| `UiEvent` | `Prompt` (carries the oneshot reply channel and the previous round's status) / `Cancel` / `Status` / `Dismiss`, all with a cookie |
| `PromptState` | `Editing` / `Verifying`, rendering and input behavior branch on it |
| `App` | `active: Option<ActivePrompt>` + `input: Vec<char>` + `cursor` |
| `ActivePrompt` | cookie/username/message/action_id/status/state + `reply: Option<oneshot::Sender>` (`Option` enables `take()`) |
| `handle_key` | Esc/Ctrl-C cancels, Enter submits (empty password rejected, switches to Verifying), Backspace/arrows/Home/End edit; Verifying only accepts cancel |
| `on_event` | consumes `UiEvent`, every event validates its cookie against the current dialog |
| `open_prompt` | for `--prompt`, no oneshot reply channel |
| `retry` | on failure, back to Editing, clears input, updates the status line |
| `render`/`render_full` | inline centered 60%×40% / popup full-screen; nothing drawn when no dialog (empty frame = clear) |
| `draw_dialog_at` | Clear → title/user/message/status/mask lines → border → cursor positioning (`PASSWORD_LABEL_W`=6 columns, counted in CJK) |

### 4.9 `tui.rs` (`/dev/tty` terminal wrapper)

| Item | Content |
|---|---|
| `has_controlling_tty` | startup guard: can `/dev/tty` be opened read-write |
| `Tui` | `tty: File` (restored on Drop) + `terminal: Terminal<CrosstermBackend<File>>` (draw target) |
| `Tui::open` | open tty → raw mode → alternate screen → panic hook → build Terminal; any failure restores everything |
| `install_panic_hook` | saves the previous hook; on panic, `disable_raw_mode` then `LeaveAlternateScreen` |
| `Drop` | restores in the same order, guaranteeing the terminal is never left broken on any exit path (including panic unwinding) |
| `PANIC_TTY` | global `Mutex<Option<File>>`, used by the panic hook to operate the terminal |

### 4.10 `logging.rs` (log channels)

| Item | Content |
|---|---|
| `TUI_ACTIVE` | `AtomicBool`, set/reset when the TUI enters/restores |
| `log_line` | logs to stdout; when the TUI is active and stdout is a terminal, writes to the safe area in the top-left corner of the screen |
| `log_line_to_corner` | `SavePosition → MoveTo(0,0) → Clear(CurrentLine) → Print(truncated to column width) → RestorePosition` as one chunk |
| `error_line` | errors to stderr, always verbatim |

## 5. Data flow traces

### 5.1 inline: auth request to password return

```
 polkitd ──BeginAuthentication──▶ begin_authentication (agent.rs)
                                    │ ① pick_username → register cookie→watch
                                    │ ② mpsc<UiEvent>::Prompt{oneshot,status}
                                    ▼
                                App dialog (ui.rs)◀── keyboard EventStream (run_tui)
                                    │ ③ Action → oneshot.send(PromptAnswer)
                                    ▼
                            authenticate_inline gets the password
                                    │ ④ HelperSession::connect (10s timeout)
                                    ▼
                        polkit-agent-helper-1 (root · PAM)
                                    │ ⑤ line-protocol loop (30s/message)
                                    │    PAM_ERROR/TEXT_INFO → UiEvent::Status
                                    │ ⑥ SUCCESS → UiEvent::Dismiss
                                    ▼
                             return Ok → polkitd grants (root helper calls Response2/3)
```

1. polkitd calls `begin_authentication` → `pick_username` picks an identity →
   inserts `(cookie, watch::Sender)` into the `pending` table.
2. `authenticate_inline` creates a fresh oneshot each round and sends
   `UiEvent::Prompt` (with `reply_tx` and the previous round's status) to
   `mpsc<UiEvent>`.
3. `run_tui`'s `ui_events` branch sets `needs_full_redraw` on receipt and
   `app.on_event` opens the dialog.
4. Keyboard events produce an `Action` via `app.handle_key` → `send_answer`
   `take()`s the oneshot → `reply.send(PromptAnswer)`.
5. The agent's `select!` receives the password → connects to the helper → PAM
   loop; `PAM_ERROR_MSG`/`PAM_TEXT_INFO` during the process are pushed to the
   dialog's status line in real time via `UiEvent::Status`.
6. `SUCCESS` → send `UiEvent::Dismiss` to close the dialog →
   `begin_authentication` returns `Ok`; on failure, update the status and
   retry from step 2.

### 5.2 daemon chain: request forwarding, popup, result reporting

```
 begin_authentication (Daemon backend)
       │ ① daemon.request(req)
       ▼
 ┌────────────────────────┐  ② ServerMsg::Request{id,req}  ┌───────────────────┐
 │  daemon (socket server) │ ─────────────────────────────▶ │ controller (read loop)│
 │  id → pending table     │         (socket NDJSON)        │ current=cookie        │
 └────────────────────────┘                                └─────────┬─────────┘
       ▲                                                           │ ③ spawn run_popup
       │ ⑥ ClientMsg::Response{id,result}                           │    tmux display-popup -E
       │    pending.remove(id) → oneshot                            │    -e POLKIT_COOKIE/...
       └────────────────────────────────────────────────────────────┘
                                                                     ▼
                                                           ┌──────────────────┐
                                                           │ --prompt process │
                                                           │ ④ App + helper PAM│
                                                           │ ⑤ exit code 0/2/other│
                                                           └──────────────────┘
```

1. `begin_authentication` (Daemon backend) → `daemon.request(AuthRequest)`.
2. `request` allocates an `id`, inserts the oneshot into the `pending` table,
   sends `ServerMsg::Request{id, req}` to the current controller, and
   `select!`s on the cancel token or the reply.
3. The controller read loop receives it → records the current cookie → spawns
   `run_popup`.
4. `run_popup` starts `--prompt` via `tmux display-popup -E`, passing
   cookie/user/action/message/cancel_file through `-e POLKIT_*`; the command
   body is `"<exe>" --prompt`.
5. Inside `--prompt`: `App::open_prompt` draws the dialog → on submit spawns
   `authenticate_once` → helper PAM → exit code.
6. The controller maps the exit code to an `AuthResult` → `ClientMsg::Response{id,
   result}` → socket → daemon read loop → `pending.remove(id)` → oneshot into
   `request`.
7. `begin_authentication` returns Ok/Cancelled/Failed per the `AuthResult`.

### 5.3 cancellation chain (polkitd-initiated cancel)

```
 polkitd ──CancelAuthentication(cookie)──▶ cancel_authentication (agent.rs)
                                            │ pending[cookie].watch.send(true)
                                            ▼
                                 begin_authentication's select! hits → return Cancelled
                                            │
                    ┌───────────────────────┴────────────────────────┐
                    ▼ inline                                      ▼ daemon
             UiEvent::Cancel closes dialog                daemon.cancel(cookie)
                                                                   │ ServerMsg::Cancel
                                                                   ▼
                                                            controller matches current
                                                                   │ write cancel file + display-popup -C
                                                                   ▼
                                                            --prompt polls the file → exit 2
```

1. polkitd calls `cancel_authentication(cookie)`.
2. The agent sets the watch for `pending[cookie]`; the inline backend also
   sends `UiEvent::Cancel`, the daemon backend calls `daemon.cancel(cookie)`.
3. inline: `authenticate_inline`'s `select!` hits `cancel_rx.changed()` → sends
   `UiEvent::Dismiss` → returns `Cancelled`.
4. daemon chain: `daemon.cancel` sends `ServerMsg::Cancel`; the controller
   matches the current cookie → writes the cancel file + `tmux display-popup
   -C`; `--prompt` polls the file and exits 2; a late `Response` from the
   controller is harmless (the daemon already returned via the local cancel
   token, and the `pending` entry was cleaned by `PendingGuard`).

### 5.4 key object lifetimes

- **cookie**: from polkitd it travels agent→daemon→controller→`--prompt`→helper,
  and is the alignment key for the cancel file, `UiEvent`, and
  `ServerMsg::Cancel`.
- **request id**: `next_id` increments; it exists only in the daemon's `pending`
  table and socket messages, tying the controller's reply to its request.
- **cancel token (watch)**: inserted by `begin_authentication`, cleaned up on
  exit; `cancel` only looks up and sets — the two never block each other.
- **`PendingGuard`**: Drop cleanup guarantees the `pending` table has no
  residue after a `request` is abandoned by `select!` or times out.
- **socket file**: `Daemon::start` refuses to start if a stale socket is
  connectable, otherwise deletes and rebinds.

## 6. Concurrency and synchronization model

All async code runs on the same `#[tokio::main]` multi-threaded runtime;
`select!` decides who proceeds first, and `Mutex` only protects small pieces of
shared state — no lock is ever held for long.

`tokio::spawn` points:

| Location | Task | Note |
|---|---|---|
| `daemon.rs start` | `accept_loop` | socket accept loop |
| `daemon.rs accept_loop` | `handle_connection` | one per connection |
| `daemon.rs handle_connection` | writer task | `ServerMsg` mpsc → NDJSON to socket |
| `controller.rs run` | `write_loop` | `ClientMsg` → socket |
| `controller.rs run` | `run_popup` task | one per request; read loop stays unblocked while a popup is open |
| `main.rs tmux_main` | `controller::run` | embedded self-connecting controller |
| `prompt.rs run` | cancel-file polling | checks the file every 200ms |
| `prompt.rs run` | `authenticate_once` | one per submission, main loop stays responsive |

`tokio::select!` points: `main.rs run_tui` (keys/ui_events/tick),
`agent.rs begin_authentication` (queueing: cancel/acquire; auth: cancel/request),
`agent.rs authenticate_inline` (cancel/reply/timeout/activity), `prompt.rs run`
(keys/outcome/cancel/tick).

`Mutex`-protected points: `agent.pending` (cancel-token table), `Daemon.pending`
(request-reply table), `Daemon.active` (current controller),
`controller.current` (current popup cookie), `controller.cancelled` (cancelled
cookie set), `PANIC_TTY`. `AtomicBool`: `TUI_ACTIVE`; `AtomicU64`: `conn_seq`
(connection generation), `next_id` (request sequence).

## 7. Timeout system

| Timeout | Location | Default | Consequence |
|---|---|---|---|
| helper connect | `agent.rs`/`prompt.rs` | 10s | treated as failure, back to retry |
| PAM single message | `agent.rs`/`prompt.rs` | 30s | treated as failure, back to retry (guard against hung PAM) |
| input idle | `agent.rs`/`prompt.rs` | 30s (`POLKIT_TUI_TIMEOUT` overrides) | inline: whole round fails; prompt: exit code 1 |
| daemon response | `daemon.rs request` | 120s | actively `cancel`s and closes the popup, returns a timeout error |

inline and popup differ in implementation but share the same semantics —
**keyboard input, submission, and failed verification all count as activity**:

- **inline (`agent.rs`)**: every keypress refreshes `last_activity` back
  through the `activity` watch channel; submission and failed verification also
  refresh when the loop returns to its top. Only continuous inactivity times
  out.
- **popup (`prompt.rs`)**: `last_activity` refreshes on keyboard events and
  failure reports; only the editing state counts toward the timeout (verifying
  does not).

## 8. Security boundaries

- **Passwords live only on the private channel**: between the agent and the
  root helper over a Unix socket or anonymous stdin/stdout pipes; no password
  ever appears on D-Bus, socket NDJSON, environment variables, or logs.
- **daemon socket accepts only the same user**: `handle_connection` checks that
  `peer_cred().uid()` equals the current uid; the socket lives inside
  `$XDG_RUNTIME_DIR` (0700), and peer_cred is defense in depth.
- **No dual daemon**: `Daemon::start` probes whether a stale socket can be
  connected to; if so, it refuses to start.
- **Cancel file**: only a cancel signal (content fixed as `"cancel"`); its path
  contains an FNV-1a hash and does not leak the raw cookie.
- **Popup args via environment variables**: `run_popup` passes request fields
  through `-e`; the command body only concatenates the current exe path (quoted
  against spaces), never injecting message content into a shell.
- **Identity parsing**: manually decodes the `(String, HashMap)` tuple and
  skips unsupported identities such as `unix-group`; unknown PAM commands are
  tolerated as `Info` instead of failing hard.
- **Diagnostics redaction**: cookies in logs default to an FNV-1a hash (16 hex
  digits) via `log_cookie`, so the full session identifier is not exposed; add
  `--full-cookie-log` when troubleshooting to print full values.

## 9. Known behavior differences (inline vs popup)

Differences that are honestly documented but not forced to match in code:

- **PAM message display**: inline shows `PAM_ERROR_MSG`/`PAM_TEXT_INFO` on the
  dialog's status line via `UiEvent::Status`; the popup's `authenticate_once`
  does not display them and simply treats FAILURE/EOF as failure (wrong
  passwords usually come back as `FAILURE` from the helper, so the impact is
  limited).
- **Cancelling during verification**: the popup still accepts Esc/Ctrl-C
  cancellation in the Verifying state (the keyboard branch in `run` is checked
  before `handle_key`); inline's `App::handle_key` returns `None` in the
  Verifying state, and `run_tui`'s exit check requires `active.is_none()`, so
  inline cannot cancel during verification and can only wait for the result.
