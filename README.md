# polkit-tui-agent

> 中文版本: [README_cn.md](README_cn.md)

A **terminal polkit authentication agent** written in Rust, built for
headless environments such as ssh / tmux.

> This project is developed with AI assistance.

When `pkexec`, `pkcheck`, and friends trigger a privilege escalation request,
polkit needs an **authentication agent** to show a password prompt. This program
provides two UIs:

- **inline TUI**: draws a dialog in the terminal it runs in (ratatui).
- **tmux popup**: registers and stays resident inside a tmux session; when an
  auth request arrives it pops up a floating dialog in the center of the screen
  via `tmux display-popup` (`--tmux` all-in-one mode, or a `--daemon` +
  `--controller` split deployment).

Pure Rust, zero GTK/glib dependencies.

## Features

- Pure terminal rendering (ratatui + crossterm), no GUI dependencies
- Uses only the **system bus** — no dependency on `DBUS_SESSION_BUS_ADDRESS`
  (works over ssh sessions)
- tmux floating popup (`display-popup`), does not disturb your layout
- `--tmux` all-in-one mode: a single process does registration + popup, no
  need for two processes
- Dual helper paths: systemd socket activation preferred, setuid binary
  fallback
- Passwords never cross D-Bus; they travel only through the private
  agent ↔ root helper channel
- Wrong-password auto retry, Esc/Ctrl-C cancel, 10s connection timeout
  fallback (helper connect, both inline and popup modes)
- Concurrent auth requests are FIFO-queued: when several `pkexec` fire at once,
  each is verified one dialog at a time without clobbering each other; requests
  cancelled while queued never show a dialog nor occupy a slot
- 30s timeout per PAM message: if the helper stays silent (hung PAM) it is
  treated as a failure and retried, never blocks authentication forever
- Password-input idle timeout (default 30s, overridable via the
  `POLKIT_TUI_TIMEOUT` env var): idle-timeout semantics — typing, submitting,
  and failure feedback all count as activity and refresh the timer; only
  continuous inactivity times out
- 120s daemon-side auth timeout fallback: if the controller never reports back,
  the request fails automatically instead of hanging forever
- Daemon socket validates the peer uid (`SO_PEERCRED`), only accepts
  controllers owned by the same user
- Two-channel popup cancellation: a cancel file makes the popup process exit
  by itself, plus `display-popup -C` as a fallback

## Build & Run

```bash
cargo build --release
```

Four run modes (plus the internal `--prompt` popup mode):

| Mode | Command | Use case |
|---|---|---|
| inline TUI | `./target/release/polkit-tui-agent` | Pops a dialog directly in the current terminal |
| **tmux all-in-one (recommended)** | `./target/release/polkit-tui-agent --tmux` | Run inside a tmux pane; floating popup on requests |
| Background daemon | `./target/release/polkit-tui-agent --daemon` | Headless, with a systemd user service |
| tmux controller | `./target/release/polkit-tui-agent --controller` | Split deployment with `--daemon`; must run in a tmux pane |
| Popup (internal) | `./target/release/polkit-tui-agent --prompt` | Spawned by the controller; do not run manually |

Options:

| Flag | Description |
|---|---|
| `--locale <LOCALE>` | Locale sent to polkitd, defaults to `$LANG` |
| `--full-cookie-log` | Log the full polkit cookie instead of the FNV-1a hash (troubleshooting/debugging) |
| `-h, --help` | Help |

### tmux all-in-one mode (recommended)

```bash
# Start it in a tmux pane (a dedicated small pane/window works well)
./target/release/polkit-tui-agent --tmux
```

From then on, any `pkexec` / `pkcheck` request pops an auth dialog right in the
center of your screen.

### Split deployment (systemd user service + tmux controller)

`~/.config/systemd/user/polkit-tui-agent.service`:

```ini
[Unit]
Description=polkit-tui-agent daemon
After=dbus.service

[Service]
Type=simple
# --uid-session: register to the uid's graphical session, so auth requests from
# SSH-attached tmux panes / desktop processes are served (the behavior of a
# desktop polkit agent); drop it if you only use the local desktop session
ExecStart=%h/polkit-tui-agent/target/release/polkit-tui-agent --daemon --uid-session
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now polkit-tui-agent
# Run a controller in a tmux pane
./target/release/polkit-tui-agent --controller
```

### Coexisting with other auth agents

Only one agent can exist per session scope at a time — this is a polkit
limitation: `RegisterAuthenticationAgent` accepts a single registration per
subject, and the `fallback` option does not change that (it only applies when
matching). If a polkit-gnome-authentication-agent or similar is already
running, registration fails:

```
An authentication agent already exists for the given subject
```

**Coexistence is not possible** — you must stop the existing agent and switch
to this program (e.g. niri's gnome agent):

```bash
systemctl --user stop 'app-niri-polkit\x2dgnome\x2dauthentication\x2dagent\x2d1-2352.scope'
```

> Note: a `unix-process` scope registration can only serve auth requests made
> by that process itself, never other processes (such as `pkexec`), so there is
> no usable coexistence scheme.

### Using over SSH

polkit matches a request to the agent only by "**same session**", so the
session the agent registers must equal the session the request is attributed
to. An SSH login is itself a logind session (`XDG_SESSION_ID` is injected by
pam_systemd); running inline or `--tmux` directly in an SSH terminal just
works:

```bash
# Option 1: run inline directly in the SSH terminal (registers to the SSH session)
./target/release/polkit-tui-agent

# Option 2: start tmux inside the SSH session and run --tmux in a pane
tmux new -As main
# inside the pane:
./target/release/polkit-tui-agent --tmux
```

**Escalating from a tmux pane** has a subtlety: if the tmux server was started
from a desktop terminal (SSH merely attaches), the pane process is not inside
any logind session's cgroup, so polkit attributes it to the **desktop
graphical session** by uid — in that case only an agent registered in the
desktop session receives the request, which is exactly how the desktop gnome
agent works. To give this program the same behavior, register with
`--uid-session`:

```bash
# Desktop side: register to the uid's graphical session (desktop polkit agent behavior)
./target/release/polkit-tui-agent --daemon --uid-session
# Inside the SSH-attached tmux pane: connect to the daemon to show the dialog
./target/release/polkit-tui-agent --controller
```

From then on, `pkexec` requests from SSH-attached tmux panes (and even the
desktop environment) pop a dialog. Note that `--uid-session` registers to the
desktop session and conflicts with the desktop gnome agent — stop the latter
first (see the previous section).

## Testing

Inline mode:

```bash
# Terminal A: start the agent
./target/release/polkit-tui-agent
# Terminal B (same session): trigger an auth
pkexec echo ok
```

tmux mode:

```bash
# tmux pane A: all-in-one mode
./target/release/polkit-tui-agent --tmux
# tmux pane B: trigger an auth
pkexec echo ok
```

Verification points: dialog pops up → wrong password shows
「认证失败，请重试」(auth failed, try again) → Esc cancels (pkexec reports
`Request dismissed`) → correct password succeeds.

Additional verification points:
- Leave the popup untouched: it closes after 30s and pkexec reports auth
  failure (duration adjustable via `POLKIT_TUI_TIMEOUT`)
- Press Ctrl-C to kill the escalating pkexec after the popup appears: the popup
  should close immediately; the daemon and controller print
  `begin_authentication/cancel_authentication/daemon cancel/controller cancel`
  logs to stderr for tracing the cancellation chain.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE).

> Developer docs: internal architecture in [ARCHITECTURE.md](ARCHITECTURE.md),
> code-change notes in [AGENTS.md](AGENTS.md).
