# File transfer

rd moves files both ways between the computer you sit at and the machine you reach: browse the
remote machine's folders, download files and folders to this computer, and upload files and
folders by dropping them in. It works in a browser tab and in rd Desktop, during a screen
session or without one (browsing files never takes over the screen).

## Using it

- **During a session:** open the session menu (the ▾ tab at the top) and click **Files**, or
  press Ctrl+Alt+Shift+**E**. The file browser opens on the right; the remote screen stays live
  next to it.
- **Without a session:** click **Browse files** on the machine's connect screen (or **Files** on
  its card in the portal).
- **Download:** double-click a file, or select files and click **Download**. One file arrives as
  itself; several files or a folder arrive as one `.zip`. Downloads land in the browser's
  download folder (rd Desktop: your Downloads folder).
- **Upload:** drop files or folders onto the file list (or onto a folder in it), or use
  **Upload**. During a session you can also drop files straight onto the remote screen: they go
  to the folder the file browser shows (the remote Desktop until you open it).
- **Organise:** new folder, rename (F2), move to the Trash / Recycle Bin (Delete), with
  multi-select (Shift/Ctrl/Cmd-click, arrow keys), sorting by name, size or date, a filter, and
  a switch for hidden files.
- **Name clashes:** an upload whose name already exists asks what to do: replace (folders:
  merge), keep both (the new one gets a ` (2)` name), or skip; optionally for all of them.

Transfers keep running while the file browser is closed (a small indicator shows their
progress), survive a dropped connection (they continue where they stopped once the connection is
back) and can be paused, resumed and cancelled.

### Keeping the screen responsive

A file transfer and the screen stream share the same network path. rd's transfers back off as
soon as they start to build a queue on that path, measured from the round-trip time of every
chunk: while a screen session is open a transfer keeps the delay it adds to about 10 ms (the
screen stays smooth and can still raise its bitrate; the transfer takes what is left),
otherwise about 80 ms (full speed). See
[Flow control](#flow-control) for how.

### macOS privacy prompts

macOS asks before an app reads the Desktop, Documents and Downloads folders, removable volumes
and network shares. The first time the file browser opens one of them, the Mac shows *"RD Agent
would like to access files in your … folder"*; until someone clicks **Allow** (you can, from a
screen session), that folder answers "access denied". To avoid the prompts, give **RD Agent**
Full Disk Access in System Settings › Privacy & Security › Full Disk Access.

### Limits

- Downloads are saved through the browser. Chrome, Edge, Firefox, Safari 26+ and rd Desktop
  stage a download on disk (the browser's private storage) before saving it, so size is not
  limited by memory; other browsers keep it in memory and refuse downloads over 1 GiB.
- Symbolic links to folders are listed but not followed inside a zipped folder (no loops);
  links to files are zipped as the file. Files that cannot be read are left out and listed when
  the download ends.
- A folder listing shows at most 20 000 entries.
- What you can read, write and delete is what the user the agent runs as can.
- On Windows, network shares and removable drives have no Recycle Bin: deleting there asks to
  delete permanently instead.

## Protocol

Version 1. The message types live in `crates/rd-proto/src/files.rs` (JSON) and
`crates/rd-proto/src/bin.rs` (FILE_DATA, FILE_ACK); the viewer mirrors them in
`viewer/src/proto/files.ts`.

### Channel

A file channel is a WebSocket pair joined by the gateway exactly like a screen session, but
separate from it: a machine has at most one screen session and up to 4 file channels at a time,
and opening a file channel never ends a session.

1. Viewer → `GET /ws/files/{machine}` (viewer port; same auth and Origin check as `/ws/view`).
   - Machine unknown or offline → close **4003**. Agent does not support files (it did not
     list `files` in `register.caps`) → close **4011**. Already 4 file channels → close
     **1013** (try again later).
2. Gateway → agent (control socket): `{"t":"files_request","channel":"<32 hex>","user":"<email>","client_ip":"<ip>"}`.
   The agent dials `GET /ws/agent/files/{channel}` (agent port, same bearer token) within 10 s,
   else the viewer is closed with **4004**.
3. Gateway → viewer: `{"t":"session","machine":"mbp","session":"<channel>","agent":{AgentState}}`
   once joined; from then on messages are copied unchanged in both directions (WebSocket
   Ping/Pong per leg, as for sessions). Messages up to 8 MiB each way (a long folder listing).
4. The channel ends when either side closes. The gateway closes it after
   `RD_SESSION_IDLE_TIMEOUT` (default 3600 s) without any message in either direction, keep-alive
PING/PONG not counted (**4008**); the viewer reconnects when it next needs the channel,
   and with 1012 when it shuts down. A file channel is not ended by its machine's control socket
   reconnecting; revoking the device ends it (4003).

Agents that support file channels send `"caps":["files"]` in `register`; `GET /api/machines`
reports it as `"files": true`. Older gateways ignore `caps`; older agents never get a
`files_request` (the gateway answers 4011).

### Messages

JSON text frames tagged by `t`, binary frames as in [protocol.md](protocol.md) (`[type][flags]`,
little-endian). Unknown `t` values and unknown fields are ignored. Requests carry an `id`
(u32, chosen by the viewer, unique per channel); replies and transfer messages carry it back.

Paths are absolute paths of the agent's OS (`/Users/me/a.txt`, `C:\Users\me\a.txt`), as the
agent lists them. The viewer builds child paths as `dir + sep + name` (no `sep` when `dir`
already ends with one). The agent refuses relative paths and paths with `.`/`..` components
(`bad_path`) and names with a separator, `.`/`..`, or (Windows) `<>:"/\|?*` and control
characters (`bad_name`).

**Hello.** Viewer → `{"t":"hello","proto":1}` first. Agent →

```json
{"t":"hello","proto":1,"os":"macos","sep":"/","home":"/Users/me",
 "places":[{"kind":"home","name":"me","path":"/Users/me"},
           {"kind":"desktop","name":"Desktop","path":"/Users/me/Desktop"},
           {"kind":"documents","name":"Documents","path":"/Users/me/Documents"},
           {"kind":"downloads","name":"Downloads","path":"/Users/me/Downloads"},
           {"kind":"drive","name":"Macintosh HD","path":"/"},
           {"kind":"volume","name":"Backup","path":"/Volumes/Backup"}],
 "caps":["trash","zip","resume"],"state":{AgentState}}
```

`places` kinds: `home`, `desktop`, `documents`, `downloads`, `drive` (Windows drive letters, the
macOS root), `volume` (other mounted volumes), `other`. Windows reports the known folders where
they really are (e.g. a Desktop moved to OneDrive). `caps`: `trash` (delete can move to the
Trash / Recycle Bin), `zip` (folder downloads), `resume` (uploads can resume). The agent sends
`{"t":"state",...AgentState}` whenever its state changes (`in_session` sets the transfer
[target delay](#flow-control)).

A `proto` the agent does not speak: `{"t":"err","id":0,"code":"proto_mismatch"}` and close 4005.

**Errors.** `{"t":"err","id":7,"code":"not_found","msg":"…"}`. Codes: `not_found`, `exists`,
`denied` (permissions, or macOS privacy), `not_dir`, `is_dir`, `bad_path`, `bad_name`,
`bad_request`, `changed` (a resumed read whose file changed), `no_space`, `unsupported`,
`proto_mismatch`, `io` (anything else; `msg` says what).

**Browsing and changes.**

| Request | Reply |
|---|---|
| `{"t":"list","id":1,"path":"/Users/me"}` | `{"t":"list","id":1,"path":"/Users/me","parent":"/Users","entries":[Entry…],"truncated":false}` |
| `{"t":"stat","id":2,"path":"/Users/me/a.txt"}` | `{"t":"stat","id":2,"entry":Entry}` (`entry: null` when it does not exist) |
| `{"t":"mkdir","id":3,"path":"/Users/me/new","exist_ok":false}` | `{"t":"ok","id":3}` |
| `{"t":"rename","id":4,"from":"/Users/me/a.txt","to":"/Users/me/b.txt"}` | `{"t":"ok","id":4}` (`exists` if `to` exists) |
| `{"t":"delete","id":5,"paths":["/Users/me/b.txt"],"trash":true}` | `{"t":"ok","id":5}` |

Entry: `{"name":"a.txt","kind":"file","size":1234,"mtime_ms":1729160000000,"hidden":false,"link":false}`.
`kind` is `file`, `dir` or `other`, after following a symbolic link (`link: true`; a broken link
is `other`). `hidden`: a dot name, or the hidden attribute on Windows. `parent` is null at a
root. `list` returns at most 20 000 entries (`truncated: true` beyond), unsorted. `delete` with
`trash: false`, or on an agent without the `trash` cap, deletes permanently (folders
recursively). A path that does not exist counts as deleted (so a re-sent `delete` is harmless);
a root cannot be deleted (`bad_path`). A `delete` that fails part-way reports the first error
and the paths it did not delete in `msg`.

Changes (`mkdir`, `rename`, `delete`, `write`) and upload data are applied in the order they
arrive, one at a time; `list` and `stat` are answered concurrently and may overtake them. A
request sent before `hello` is answered with `bad_request`.

**Download.** `{"t":"read","id":6,"path":"/Users/me/a.txt","offset":0}`. Optional:
`"expect":{"size":N,"mtime_ms":M}` (when resuming: fail with `changed` unless the file still has
that size and time). The agent answers `{"t":"read","id":6,"name":"a.txt","size":1234,"mtime_ms":…}`
and then sends the bytes from `offset` as FILE_DATA frames; the last one has the END flag. The
agent sends END as a separate empty frame after the data (an empty file is just that frame), so
receivers must accept an END frame without bytes. An error during the transfer ends it with `err`.

A **zip** download: `{"t":"read","id":8,"paths":["/Users/me/Photos","/Users/me/a.txt"],"zip":true,"name":"Photos.zip"}`.
The paths must share one parent folder; the archive holds them under their own names. Without a
`name`, one path gives `<its name>.zip` and several give `<parent>.zip`.
The agent first walks the tree and answers `{"t":"read","id":8,"name":"Photos.zip","total":5368709120,"files":1234}`
(`total`: bytes of all files; there is no `size`, the archive is produced as it goes), then
FILE_DATA frames, `{"t":"progress","id":8,"done":…}` (source bytes zipped so far, about 4 times a
second, and once more with the final total) and, before the END frame,
`{"t":"skipped","id":8,"paths":["Photos/locked.jpg"]}` if files could not be read. A symbolic link
to a folder is followed when it is one of `paths`, and skipped silently deeper down; devices,
sockets and pipes are left out. The archive stores files uncompressed (fast, and most large files are
compressed already), with ZIP64 records where sizes need them, UTF-8 names, modification times
(DOS, Unix and NTFS) and empty folders. Zip downloads cannot resume.

`{"t":"cancel","id":6}` stops a download (no reply; FILE_DATA already under way is still
acknowledged and dropped). The agent sends downloads one at a time, in request order.

**Upload.** `{"t":"write","id":9,"path":"/Users/me/Documents/report.pdf","size":1234,"mtime_ms":…,"conflict":"fail","token":"<16 hex>"}`
immediately followed by its FILE_DATA frames (the last with END; an empty file sends one empty
END frame). The viewer does not wait for an answer before sending data, so many small files
stream back to back. Several uploads may be open at once and their FILE_DATA may interleave (the
viewer pauses one and continues another); the agent keeps open uploads by `id`, and an open
upload without data simply waits. The agent writes to a temporary file next to the target
(`.report.pdf.<token>.rdpart`; a very long name is shortened to fit, and on Windows the file is
hidden) and, after the END frame, moves it into place according to
`conflict`:

- `fail`: answer `exists` if the target exists (checked at the start and again at the end);
- `replace`: replace an existing file (`is_dir` if the target is a folder);
- `rename`: keep both; the upload gets the first free `name (2).ext`, `name (3).ext`, ….

Then it sets `mtime_ms` (if given) and answers `{"t":"done","id":9,"path":"/Users/me/Documents/report.pdf","size":1234}`
with the final path. Any error answers `err` and removes the temporary file; data that keeps
arriving for that id is acknowledged and dropped. `{"t":"cancel","id":9}` (in order with the
data) does the same without an answer.

Missing parent folders are not created: an upload of a folder sends `mkdir` (`exist_ok: true`
when merging) for each folder before the files in it.

**Resume.** A `write` with `"resume":true` (and the same `token` as before) asks to continue an
interrupted upload: the agent answers `{"t":"write","id":10,"offset":N}` with the length of the
temporary file it kept for that token (0 if none, also when the upload had already finished and
only its `done` was lost: the viewer then gets `exists` and checks the target with `stat`) and the
viewer sends FILE_DATA from `N`. The
agent keeps the temporary file of an upload whose channel was lost for 10 minutes. If the old
channel's socket has not noticed yet that it is gone (that takes up to 45 s), the resume waits
for it to let go, at most 60 s, instead of opening the same temporary file twice. A download
resumes with `read` from the offset the viewer already has, with `expect`.

### Binary frames

| type | name | dir | layout (after `[type][flags]`) | len |
|---|---|---|---|---|
| 0x40 | FILE_DATA | both | pad u16, id u32, seq u32, offset u64, then the bytes | 20+ |
| 0x41 | FILE_ACK | both | pad u16, seq u32 | 8 |

- FILE_DATA flags: `0x01` END (last frame of transfer `id`). `offset` is where the bytes go in
  the file (for zip downloads: in the archive). `seq` counts the FILE_DATA frames one side sends
  on a channel: 1, 2, 3, … across all transfers.
- FILE_ACK is cumulative: the receiver has handled (written, or dropped) every FILE_DATA with a
  `seq` up to and including this one. Receivers acknowledge every frame, in order, once it is
  written (so a slow disk slows the sender down too).
- A frame carries at most 256 KiB of bytes.
- PING (0x30) and PONG (0x31) work as on a session socket: the agent answers every PING at once
  (also while its disk is busy), and the viewer pings every 5 s and reconnects after 20 s of
  silence.

### Flow control

Each side sends its FILE_DATA through a delay-based window (`rd_flow::BulkWindow`, mirrored in
`viewer/src/files/bulk.ts`), in the spirit of LEDBAT (RFC 6817):

```text
rtt        = ack time − send time of the frame (includes the sender's own socket buffer)
base       = lowest rtt of the last 2 minutes (in 10 s buckets)
q          = min(last 4 rtts) − base                  queueing the transfer causes
target     = 10 ms while a screen session is open (agent `in_session`), else 80 ms
slow start : window += acked bytes, until q > target / 2
afterwards : window += acked · chunk / window · (target − q) / target    (−1 … +1)
             q > 2 · target → window / 2, at most once per rtt
window     ∈ [16 KiB, 16 MiB], starts at 64 KiB; chunk = window / 4 ∈ [16 KiB, 256 KiB]
send while bytes in flight + chunk ≤ window (always when nothing is in flight)
```

So a transfer fills an idle path quickly, keeps its own queue near the target, and leaves room
when the screen stream (or anything else) needs it. 10 ms is below the 15 ms under which the
screen's bitrate controller still raises its rate (protocol.md, Flow control), so a screen that
starts to move gets its bandwidth back. Simulated in `crates/rd-flow/tests/bulk_sim.rs`.
