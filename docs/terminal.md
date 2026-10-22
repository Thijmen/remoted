# Terminal

rd opens a shell on the machine you reach, in a terminal in the browser tab or in rd Desktop:
zsh (or your login shell) on a Mac, PowerShell on Windows. It travels over the same secure
WebSocket on port 443 as the screen, so it works wherever rd works. No SSH server is needed,
and none has to be reachable.

## Using it

- **Without a session:** click **Terminal** on the machine's card in the portal, or **Open
  terminal** on its connect screen. The terminal opens in its own page (`/t/{machine}`) and
  does not take over the screen.
- **During a session:** open the session menu (the ▾ tab at the top) and click **Terminal**,
  or press Ctrl+Alt+Shift+`` ` `` (the key left of 1). The terminal opens in a panel next to the remote screen. While
  it has the focus, your keys go to the shell, not to the remote screen.
- **Several shells:** every terminal page or panel runs its own shell. Open another one in a
  new browser tab.
- **Copy and paste:** select text to copy it with Ctrl+Shift+C (Cmd+C on a Mac); paste with
  Ctrl+Shift+V (Cmd+V). Links in the output open in a new tab with Ctrl/Cmd+click.
- **Closing:** close the terminal with its **×** button or by typing `exit`. Closing the tab
  or losing the connection does *not* end the shell (see below).

### A dropped connection keeps the shell

When the connection drops (Wi-Fi, a laptop that sleeps, a gateway restart) the shell keeps
running on the machine. The terminal reconnects by itself and shows what the shell printed in
the meantime, and you continue where you were. A reloaded page reattaches the same way.

The agent keeps a shell without a terminal attached for 15 minutes from the moment it was
detached (`keep_minutes`, see
[Settings](#settings)), then ends it. It keeps the last 1 MiB of output for the replay; when
more was printed while you were away, the terminal shows the most recent 1 MiB. A shell is also
ended when the agent restarts (for example to install an update), and then the terminal offers
a new one.

One shell shows in one terminal at a time: opening it somewhere else (only possible by
reattaching, e.g. a reload in a second tab) moves it there, and the first terminal says so and
offers to take it back.

### Who can open one

Every user who can sign in to the portal can open a terminal on every machine whose agent
offers one, just as they can control its screen. The shell runs as the user the agent runs as
(the person logged in), with that user's rights, in their home folder. On a Mac it inherits the
agent's privacy permissions too, so with Full Disk Access granted to RD Agent a shell can read
every folder.

The gateway logs every terminal in its audit log (`terminal_open`, `terminal_close`: who, from
where, which machine, how long, how many bytes). What is typed is not logged.

To turn the terminal off on a machine, set `enabled = false` in the agent's config (below)
and restart the agent. The agent then no longer offers it, and the portal shows no Terminal
button for that machine.

## Settings

In the agent's `config.toml` (macOS: `~/Library/Application Support/dev.thijmen.rd.agent/config.toml`,
Windows: `%LOCALAPPDATA%\rd-agent\config.toml`), all optional:

```toml
[terminal]
enabled = true        # false: this machine offers no terminal
shell = "/bin/bash"   # what to start instead of the default (Windows: a command line, e.g. "cmd.exe")
keep_minutes = 15     # how long a shell without a terminal attached keeps running (1-1440)
```

The default shell is the user's login shell on macOS (started as a login shell, like
Terminal.app), and on Windows PowerShell 7 (`pwsh.exe`) when it is installed, else Windows
PowerShell. The shell gets `COLORTERM=truecolor`, `TERM_PROGRAM=rd` and
`TERM_PROGRAM_VERSION` (the agent's version); on macOS also `TERM=xterm-256color`,
`LANG=en_US.UTF-8` when no locale is set, and `HOME`, `USER`, `LOGNAME` and `SHELL` from the
user database when the agent's environment lacks them.

`shell` on macOS: a single word (a path, or a name looked up in `PATH`) that is listed in
`/etc/shells` starts as a login shell; any other single word runs as given; a line with spaces
runs through the login shell (`-l -c "<line>"`). On Windows it is a command line.

Changes take effect when the agent restarts.

### Requirements

- macOS: any version the agent supports.
- Windows 10 version 1809 or later, or Windows 11 (the pseudo console, ConPTY). An agent on an
  older Windows does not offer the terminal.
- rd-agent and gateway of the same release or later; an older agent does not offer it.

## Protocol

Version 1. The message types live in `crates/rd-proto/src/terminal.rs` (JSON) and
`crates/rd-proto/src/bin.rs` (TERM_OUT, TERM_ACK, TERM_IN); the viewer mirrors them in
`viewer/src/proto/terminal.ts`.

### Channel

A terminal channel is a WebSocket pair joined by the gateway exactly like a
[file channel](files.md#channel), next to and independent of the screen session and the file
channels.

1. Viewer → `GET /ws/terminal/{machine}` (viewer port; same auth and Origin check as
   `/ws/view`).
   - Machine unknown or offline → close **4003**. The agent does not offer a terminal (it did
     not list `terminal` in `register.caps`) → close **4011**. Already 8 terminal channels to
     the machine → close **1013** (try again later).
2. Gateway → agent (control socket):
   `{"t":"terminal_request","channel":"<32 hex>","user":"<email>","client_ip":"<ip>"}`. The
   agent dials `GET /ws/agent/terminal/{channel}` (agent port, same bearer token) within 10 s,
   else the viewer is closed with **4004**.
3. Gateway → viewer: `{"t":"session","machine":"mbp","session":"<channel>","agent":{AgentState}}`
   once joined; from then on messages are copied unchanged in both directions (WebSocket
   Ping/Pong per leg). Messages up to 1 MiB each way.
4. The channel ends when either side closes. The gateway closes it after
   `RD_SESSION_IDLE_TIMEOUT` (default 3600 s) without any message in either direction (PING
   and PONG not counted) with **4008**, and with 1012 when it shuts down; revoking the device
   ends it with 4003. Closing a channel never ends its shell (only `close`, the shell exiting,
   the keep time or the agent stopping do).

Agents that offer the terminal send `"caps":["terminal"]` in `register` (with the others,
e.g. `["files","terminal"]`); `GET /api/machines` reports it as `"terminal": true`. An agent
lists it only when the terminal is enabled in its config and the OS has a pseudo console.

### Messages

JSON text frames tagged by `t`, binary frames as in [protocol.md](protocol.md) (`[type][flags]`,
little-endian). Unknown `t` values, unknown fields and unknown binary types are ignored.

**Hello.** The viewer's first message, within 10 s of the channel opening (else the agent
closes it with 4005):

```json
{"t":"hello","proto":1,"cols":120,"rows":32}
{"t":"hello","proto":1,"cols":120,"rows":32,"attach":"<shell id>","offset":48213}
```

Without `attach` the agent starts a new shell of `cols` × `rows` (each clamped to 2…1000). With
`attach` it reattaches to that shell, resizes it to `cols` × `rows`, and replays its output from
`offset` (the number of output bytes the viewer already shows; 0 for everything it kept). The
agent answers

```json
{"t":"hello","proto":1,"id":"<32 hex>","os":"macos","shell":"zsh","offset":48213,"reset":false,"state":{AgentState}}
```

and then sends output as TERM_OUT frames starting at `offset`.

- `id`: the shell, for a later `attach`. Unguessable (128 random bits).
- `offset`: where the replay starts. For a new shell 0. For an attach the viewer's `offset` when
  the agent still has all output after it; otherwise the oldest output it kept.
- `reset`: the viewer clears its screen and scrollback before showing what follows (a new
  shell, or an attach whose `offset` the agent could not continue from).
- Errors (`{"t":"err","code":…,"msg":…}`, then close 1000):
  `not_found` (an `attach` to a shell the agent does not have: it ended, was closed, its keep
  time ran out or the agent restarted), `busy` (the agent already runs 8 shells), `spawn` (the
  shell could not be started; `msg` says why), `bad_request`, `proto_mismatch` (also closes
  with 4005).

A shell is attached to one channel at a time. An `attach` to a shell that another channel
shows takes it over: that channel gets `{"t":"detached","reason":"taken_over"}` and is closed
with **4001** (the viewer does not reconnect by itself).

Any other message before `hello` (or one that is not JSON) is answered with `bad_request` and
the channel is closed (1000). After `hello`, messages that are not JSON are ignored.

**While attached.**

| Message | Dir | Meaning |
|---|---|---|
| TERM_IN (binary) | V→A | Bytes for the shell's input: keys as the terminal encodes them (UTF-8), pastes. At most 64 KiB per frame; the viewer splits a larger paste. Written to the shell in order. |
| `{"t":"resize","cols":100,"rows":30}` | V→A | The terminal's size changed (clamped like `hello`). |
| `{"t":"close"}` | V→A | End the shell. The agent hangs it up (Unix: SIGHUP to its process group, SIGKILL after 3 s; Windows: closes the pseudo console, then terminates the process after 3 s) and answers `exit`. |
| TERM_OUT (binary) | A→V | The shell's output, from `offset`. |
| TERM_ACK (binary) | V→A | The viewer has shown output up to a TERM_OUT `seq`. |
| `{"t":"state",…AgentState}` | A→V | The agent's state changed (`in_session` sets the output's target delay, below). |
| `{"t":"exit","code":0}` | A→V | The shell ended (`code`: its exit status; on macOS `signal` instead when a signal ended it; on Windows the process exit code, negative for NTSTATUS values such as 0xC000013A). Sent after its last output; then the agent closes the channel with 1000. |
| PING / PONG | both | As on a file channel: the agent answers every PING at once; the viewer pings every 5 s and reconnects after 20 s of silence. |

A shell that ends while no channel is attached is kept, with its output and exit status, until
the keep time since its detach runs out, so a reattach still shows how it ended (replay, then `exit`).

### Binary frames

| type | name | dir | layout (after `[type][flags]`) | len |
|---|---|---|---|---|
| 0x50 | TERM_OUT | A→V | pad u16, seq u32, offset u64, then the output bytes | 16+ |
| 0x51 | TERM_ACK | V→A | pad u16, seq u32 | 8 |
| 0x52 | TERM_IN | V→A | then the input bytes | 2+ |

- TERM_OUT: `seq` counts the TERM_OUT frames on a channel, 1, 2, 3, …; `offset` is the position
  of the first byte in the shell's output (bytes since the shell started), so the viewer always
  knows how much it shows. A frame carries at most 64 KiB. Output is a byte stream: a frame may
  end in the middle of a UTF-8 character or an escape sequence, and the next one continues it.
- TERM_ACK is cumulative: every TERM_OUT up to and including `seq` was handed to the terminal
  and drawn. The viewer acknowledges once the terminal has processed the bytes (its write
  callback), at most once per animation frame, so a slow renderer slows the agent down too.

### Flow control

The agent reads the shell's output all the time, into a history of the last 1 MiB, also while
no channel is attached; so a shell never blocks on a slow or absent viewer. It sends the
history to the attached channel through a delay-based window (`rd_flow::BulkWindow`, the same
as [file transfers](files.md#flow-control)): target 10 ms of queueing while a screen session is
open, else 80 ms, RTT samples from TERM_ACK. A keystroke's echo goes out at once (the window
always lets a frame out when nothing is in flight); a command that prints a lot (`cat` of a
large file, `yes`) fills the window and no more, so the screen stream next to it stays smooth.
When the history overtakes what the channel has sent (more than 1 MiB printed faster than the
link carries it), the agent skips to the oldest byte it still has; the viewer notices the jump in
`offset` and the terminal simply continues with the newer output.

### Agent side

`crates/rd-agent-core/src/runtime/terminal/` (the shells, their history and the channels),
the pseudo terminal in `crates/rd-agent-core/src/pty.rs` (Unix: `openpty`) and
`crates/rd-platform-windows/src/pty.rs` (ConPTY, looked up at run time so the agent still
starts on Windows before 1809). At most 8 shells per agent (attached or not). The shell's
environment is the agent's, without `RD_AGENT_TOKEN`, plus the variables listed in
[Settings](#settings).

- Input goes to the shell through a queue of at most 1 MiB. While a shell does not read its
  input (a large paste into a busy program), the agent stops reading that channel, so its PINGs
  wait too; a paste the shell leaves unread for over 20 s makes the viewer reconnect.
- When the agent stops, it closes the terminal channels (1001), hangs up every shell and kills
  what is left after 1 s.
