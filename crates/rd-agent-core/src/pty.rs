//! Pseudo terminals on Unix (docs/terminal.md, Agent side): the [`Platform::spawn_pty`] of the
//! macOS platform (and of the runtime's tests).
//!
//! [`spawn`] opens a terminal pair with `openpty`, starts the shell on the terminal side in a
//! session of its own (`setsid`, the terminal as its controlling terminal), and keeps the
//! controller side, which is shared by the reader, the writer and the [`PtyChild`]:
//!
//! - Waiting reaps the shell, then closes the write end of an *exit pipe*. The reader and the
//!   writer poll it next to the terminal, so they end once the shell has exited (after the
//!   reader has read everything that is buffered) even when a process the shell left behind
//!   keeps the terminal open.
//! - Signals go to the shell's process group (and the terminal's foreground job) only while
//!   the shell is not reaped yet, so a reused process id can never be hit.
//!
//! [`Platform::spawn_pty`]: crate::Platform::spawn_pty

use std::ffi::{CStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::platform::{PtyChild, PtyExit, PtyProcess, PtyRequest, PtySize};

/// The shell when the user database names none (or one that does not exist).
const FALLBACK_SHELL: &str = "/bin/zsh";

/// What to run in the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    /// `argv[0]`: "-zsh" for a login shell.
    pub arg0: OsString,
    pub args: Vec<OsString>,
    /// For the title: "zsh".
    pub name: String,
}

impl Launch {
    /// `program` as a login shell, as Terminal.app starts it: `argv[0]` is "-" and its name.
    pub fn login(program: impl Into<PathBuf>) -> Launch {
        let program = program.into();
        let name = file_name(&program);
        Launch {
            arg0: format!("-{name}").into(),
            args: Vec::new(),
            name,
            program,
        }
    }

    /// `program` with `args`, as given.
    pub fn plain(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Launch {
        let program = program.into();
        Launch {
            arg0: program.clone().into_os_string(),
            args: args.into_iter().map(Into::into).collect(),
            name: file_name(&program),
            program,
        }
    }

    /// What `[terminal] shell` asks for, `None` for the user's login shell:
    /// - a single word: that program (a name is looked up in `PATH`), as a login shell when it
    ///   is listed in /etc/shells, else as given;
    /// - a command line (anything with a space): run by the login shell (`-l -c`), so it sees
    ///   the same `PATH` as an interactive shell.
    pub fn for_setting(shell: Option<&str>) -> Launch {
        let shell = shell.map(str::trim).filter(|s| !s.is_empty());
        let login = login_shell();
        match shell {
            None => Launch::login(login),
            Some(line) if line.contains(char::is_whitespace) => {
                let mut launch = Launch::plain(&login, ["-l", "-c", line]);
                launch.name = line
                    .split_whitespace()
                    .next()
                    .map(|w| file_name(Path::new(w)))
                    .unwrap_or_else(|| launch.name.clone());
                launch
            }
            Some(word) => {
                let program = find_program(word);
                if listed_shell(&program) {
                    Launch::login(program)
                } else {
                    Launch::plain(program, Vec::<OsString>::new())
                }
            }
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// `word` itself when it contains a slash, else the first match in `PATH` (or `word`).
fn find_program(word: &str) -> PathBuf {
    if word.contains('/') {
        return PathBuf::from(word);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(word))
        .find(|p| is_executable(p))
        .unwrap_or_else(|| PathBuf::from(word))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `program` is one of the shells in /etc/shells.
fn listed_shell(program: &Path) -> bool {
    fs::read_to_string("/etc/shells").is_ok_and(|list| {
        list.lines()
            .map(str::trim)
            .any(|l| !l.starts_with('#') && Path::new(l) == program)
    })
}

/// The user's entry in the user database.
struct User {
    name: Option<OsString>,
    home: Option<PathBuf>,
    shell: Option<PathBuf>,
}

fn current_user() -> User {
    let mut user = User {
        name: None,
        home: None,
        shell: None,
    };
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: getpwuid_r writes the entry into `pw` and its strings into `buf`.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut pw,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return user;
        }
        let field = |p: *const libc::c_char| {
            // SAFETY: the entry's strings are NUL-terminated and live in `buf`.
            (!p.is_null())
                .then(|| OsString::from_vec(unsafe { CStr::from_ptr(p) }.to_bytes().to_vec()))
                .filter(|s| !s.is_empty())
        };
        user.name = field(pw.pw_name);
        user.home = field(pw.pw_dir).map(PathBuf::from);
        user.shell = field(pw.pw_shell).map(PathBuf::from);
        return user;
    }
}

/// The user's login shell from the user database, else /bin/zsh.
pub fn login_shell() -> PathBuf {
    current_user()
        .shell
        .filter(|s| is_executable(s))
        .unwrap_or_else(|| PathBuf::from(FALLBACK_SHELL))
}

/// Starts `[terminal] shell` (or the login shell) in a new pseudo terminal of `req.size`.
pub fn spawn(req: &PtyRequest) -> io::Result<PtyProcess> {
    spawn_with(req, Launch::for_setting(req.shell.as_deref()))
}

/// Starts `launch` in a new pseudo terminal of `req.size`, in the user's home folder, with the
/// agent's environment changed as `req` says, plus `TERM=xterm-256color` and, when no locale
/// is set, `LANG=en_US.UTF-8`.
pub fn spawn_with(req: &PtyRequest, launch: Launch) -> io::Result<PtyProcess> {
    let (master, slave) = open_pty(req.size)?;
    let (exit_r, exit_w) = pipe()?;
    let user = current_user();

    let mut cmd = Command::new(&launch.program);
    cmd.arg0(&launch.arg0).args(&launch.args);
    let home = user
        .home
        .clone()
        .or_else(crate::platform::home_dir)
        .filter(|h| h.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"));
    cmd.current_dir(&home);
    for key in &req.env_remove {
        cmd.env_remove(key);
    }
    cmd.env("TERM", "xterm-256color");
    let has_locale = ["LANG", "LC_ALL", "LC_CTYPE"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
    if !has_locale {
        cmd.env("LANG", "en_US.UTF-8");
    }
    // What login(1) would set, for an agent started without them.
    let defaults = [
        ("HOME", Some(home.into_os_string())),
        ("USER", user.name.clone()),
        ("LOGNAME", user.name),
        ("SHELL", user.shell.map(PathBuf::into_os_string)),
    ];
    for (key, value) in defaults {
        if let Some(value) = value
            && std::env::var_os(key).is_none_or(|v| v.is_empty())
        {
            cmd.env(key, value);
        }
    }
    for (key, value) in &req.env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    let max_fd = max_fd();
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // stdin is the terminal: make it the controlling terminal of the new session.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Nothing of the agent's leaks into the shell (std's own pipe is close-on-exec
            // already and must stay open until exec).
            for fd in 3..max_fd {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let pid = child.id() as libc::pid_t;
    // The terminal side now belongs to the shell: the agent keeps no handle to it.
    drop(cmd);
    drop(child);

    let shared = Arc::new(Shared {
        master,
        pid,
        reaped: Mutex::new(false),
        exit_r,
        exit_w: Mutex::new(Some(exit_w)),
    });
    Ok(PtyProcess {
        name: launch.name,
        reader: Box::new(Reader(shared.clone())),
        writer: Box::new(Writer(shared.clone())),
        child: Arc::new(Child(shared)),
    })
}

fn winsize(size: PtySize) -> libc::winsize {
    libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl on a descriptor we own.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }).map(drop)
}

/// (controller, terminal) of a new pseudo terminal. The controller is non-blocking; both are
/// close-on-exec.
fn open_pty(size: PtySize) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (-1, -1);
    let mut ws = winsize(size);
    // SAFETY: openpty writes two descriptors; the name and termios are optional.
    cvt(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // `*mut` on macOS, `*const` on Linux: a raw pointer fits both.
            &raw mut ws,
        )
    })?;
    // SAFETY: openpty returned two new descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_cloexec(master.as_raw_fd())?;
    set_cloexec(slave.as_raw_fd())?;
    // SAFETY: fcntl / tcgetattr / tcsetattr on our descriptors.
    unsafe {
        let flags = cvt(libc::fcntl(master.as_raw_fd(), libc::F_GETFL))?;
        cvt(libc::fcntl(
            master.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        ))?;
        // Input is UTF-8: backspace erases a whole character, as in Terminal.app.
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave.as_raw_fd(), &mut t) == 0 {
            t.c_iflag |= libc::IUTF8;
            libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t);
        }
    }
    Ok((master, slave))
}

/// (read end, write end), both close-on-exec.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: pipe writes two new descriptors.
    cvt(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    // SAFETY: nothing else owns them.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(r.as_raw_fd())?;
    set_cloexec(w.as_raw_fd())?;
    Ok((r, w))
}

/// Descriptors to mark close-on-exec in the child: up to the soft limit, at most 64 Ki.
fn max_fd() -> libc::c_int {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit fills `lim`.
    let cur = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
        lim.rlim_cur
    } else {
        1024
    };
    cur.clamp(256, 1 << 16) as libc::c_int
}

/// The controller side and the shell, shared by [`Reader`], [`Writer`] and [`Child`].
struct Shared {
    master: OwnedFd,
    pid: libc::pid_t,
    /// The shell was reaped: its process id may belong to someone else now.
    reaped: Mutex<bool>,
    /// Readable (end of file) once the shell was reaped.
    exit_r: OwnedFd,
    exit_w: Mutex<Option<OwnedFd>>,
}

impl Shared {
    /// Waits until the controller is ready for `events` or the shell exited (false).
    fn poll(&self, events: libc::c_short) -> io::Result<(bool, libc::c_short)> {
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: self.master.as_raw_fd(),
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.exit_r.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: two valid pollfds.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok((fds[1].revents == 0, fds[0].revents));
        }
    }

    /// Signals the shell's process group and the terminal's foreground job, unless the shell
    /// is gone.
    fn signal(&self, sig: libc::c_int) {
        let reaped = self.reaped.lock();
        if *reaped {
            return;
        }
        // SAFETY: plain syscalls; the process group exists while its leader is not reaped.
        unsafe {
            let foreground = libc::tcgetpgrp(self.master.as_raw_fd());
            if foreground > 0 && foreground != self.pid {
                libc::killpg(foreground, sig);
            }
            libc::killpg(self.pid, sig);
        }
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: reads into `buf`.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

struct Reader(Arc<Shared>);

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self.0.master.as_raw_fd();
        loop {
            let (running, revents) = self.0.poll(libc::POLLIN)?;
            if revents != 0 || !running {
                match read_fd(fd, buf) {
                    Ok(n) => return Ok(n),
                    // The terminal side is closed by everyone (Linux reports EIO).
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Exited and nothing is buffered any more: done. A hangup without
                        // data is the end too.
                        if !running || revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                            return Ok(0);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

struct Writer(Arc<Shared>);

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let (running, revents) = self.0.poll(libc::POLLOUT)?;
            if !running || revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            // SAFETY: writes from `buf`.
            let n =
                unsafe { libc::write(self.0.master.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => {}
                _ => return Err(e),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Child(Arc<Shared>);

impl PtyChild for Child {
    fn resize(&self, size: PtySize) -> io::Result<()> {
        let ws = winsize(size);
        // SAFETY: TIOCSWINSZ reads a winsize; the kernel tells the foreground job (SIGWINCH).
        cvt(unsafe { libc::ioctl(self.0.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) }).map(drop)
    }

    fn hangup(&self) {
        self.0.signal(libc::SIGHUP);
    }

    fn kill(&self) {
        self.0.signal(libc::SIGKILL);
    }

    fn wait(&self) -> PtyExit {
        let pid = self.0.pid;
        // Wait without reaping, so `signal` can still trust the pid until it is reaped below.
        loop {
            // SAFETY: waitid fills `info`.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
        let mut status = 0;
        let rc = {
            let mut reaped = self.0.reaped.lock();
            let rc = loop {
                // SAFETY: reaps our child (a zombie by now, so this does not block for long).
                let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
                if rc >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break rc;
                }
            };
            *reaped = true;
            rc
        };
        // Tell the reader and the writer.
        drop(self.0.exit_w.lock().take());
        if rc != pid {
            return PtyExit::default();
        }
        if libc::WIFEXITED(status) {
            PtyExit {
                code: Some(libc::WEXITSTATUS(status)),
                signal: None,
            }
        } else if libc::WIFSIGNALED(status) {
            PtyExit {
                code: None,
                signal: Some(libc::WTERMSIG(status)),
            }
        } else {
            PtyExit::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn request(cols: u16, rows: u16) -> PtyRequest {
        PtyRequest {
            size: PtySize { cols, rows },
            shell: None,
            env: vec![("RD_PTY_TEST".into(), "yes".into())],
            env_remove: vec!["RD_AGENT_TOKEN".into()],
        }
    }

    /// Reads until `pred` holds for everything read so far (bounded).
    fn read_until(r: &mut dyn Read, out: &mut Vec<u8>, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; 4096];
        while !pred(&String::from_utf8_lossy(out)) {
            assert!(
                Instant::now() < deadline,
                "timed out; got {:?}",
                String::from_utf8_lossy(out)
            );
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => panic!("read failed: {e}"),
            }
        }
    }

    fn read_to_end(r: &mut dyn Read, out: &mut Vec<u8>) {
        read_until(r, out, |_| false);
    }

    #[test]
    fn launches() {
        let login = Launch::login("/bin/zsh");
        assert_eq!(login.arg0, "-zsh");
        assert_eq!(login.name, "zsh");
        assert!(login.args.is_empty());
        // /bin/sh is in /etc/shells on every Unix: a login shell.
        let sh = Launch::for_setting(Some("/bin/sh"));
        assert_eq!((sh.arg0.to_str(), sh.name.as_str()), (Some("-sh"), "sh"));
        // Something else runs as given.
        let cat = Launch::for_setting(Some("/bin/cat"));
        assert_eq!(cat.arg0, "/bin/cat");
        // Looked up in PATH.
        assert!(Launch::for_setting(Some("cat")).program.is_absolute());
        // A command line goes through the login shell.
        let line = Launch::for_setting(Some("tmux new -A -s main"));
        assert_eq!(line.program, login_shell());
        assert_eq!(line.args, ["-l", "-c", "tmux new -A -s main"]);
        assert_eq!(line.name, "tmux");
        assert_eq!(
            Launch::for_setting(Some("  ")),
            Launch::login(login_shell())
        );
    }

    #[test]
    fn runs_a_shell() {
        let launch = Launch::plain(
            "/bin/sh",
            [
                "-c",
                "stty size; echo \"$TERM $RD_PTY_TEST ${RD_AGENT_TOKEN-unset}\"; pwd; read x; echo got:$x; exit 3",
            ],
        );
        let mut p = spawn_with(&request(100, 30), launch).unwrap();
        assert_eq!(p.name, "sh");
        let child = p.child.clone();
        let waiter = std::thread::spawn(move || child.wait());
        let mut out = Vec::new();
        read_until(&mut *p.reader, &mut out, |s| s.contains("unset"));
        p.writer.write_all(b"hello\r").unwrap();
        read_to_end(&mut *p.reader, &mut out);
        let out = String::from_utf8_lossy(&out);
        assert!(out.contains("30 100"), "{out}");
        assert!(out.contains("xterm-256color yes unset"), "{out}");
        assert!(out.contains("got:hello"), "{out}");
        let home = current_user().home.unwrap();
        assert!(out.contains(&*home.to_string_lossy()), "{out}");
        assert_eq!(
            waiter.join().unwrap(),
            PtyExit {
                code: Some(3),
                signal: None
            }
        );
        // After the exit: writes fail, signals are ignored.
        assert!(p.writer.write_all(b"x").is_err());
        p.child.hangup();
        p.child.kill();
    }

    #[test]
    fn hangup_and_resize() {
        let launch = Launch::plain(
            "/bin/sh",
            [
                "-c",
                "trap 'stty size; exit 0' WINCH; echo ready; while :; do sleep 0.05; done",
            ],
        );
        let mut p = spawn_with(&request(80, 24), launch).unwrap();
        let mut out = Vec::new();
        read_until(&mut *p.reader, &mut out, |s| s.contains("ready"));
        p.child
            .resize(PtySize {
                cols: 120,
                rows: 40,
            })
            .unwrap();
        read_until(&mut *p.reader, &mut out, |s| s.contains("40 120"));
        assert_eq!(
            p.child.wait(),
            PtyExit {
                code: Some(0),
                signal: None
            }
        );

        // A shell that ignores nothing ends on SIGHUP.
        let launch = Launch::plain(
            "/bin/sh",
            ["-c", "echo ready; while :; do sleep 0.05; done"],
        );
        let mut p = spawn_with(&request(80, 24), launch).unwrap();
        let mut out = Vec::new();
        read_until(&mut *p.reader, &mut out, |s| s.contains("ready"));
        p.child.hangup();
        assert_eq!(
            p.child.wait(),
            PtyExit {
                code: None,
                signal: Some(libc::SIGHUP)
            }
        );
        read_to_end(&mut *p.reader, &mut out);

        // One that ignores SIGHUP needs SIGKILL.
        let launch = Launch::plain(
            "/bin/sh",
            [
                "-c",
                "trap '' HUP; echo ready; while :; do sleep 0.05; done",
            ],
        );
        let mut p = spawn_with(&request(80, 24), launch).unwrap();
        let mut out = Vec::new();
        read_until(&mut *p.reader, &mut out, |s| s.contains("ready"));
        p.child.hangup();
        std::thread::sleep(Duration::from_millis(200));
        p.child.kill();
        assert_eq!(
            p.child.wait(),
            PtyExit {
                code: None,
                signal: Some(libc::SIGKILL)
            }
        );
    }

    #[test]
    fn output_ends_with_the_shell_even_if_a_job_keeps_the_terminal() {
        // The background `sleep` keeps the terminal open after the shell exited.
        let launch = Launch::plain("/bin/sh", ["-c", "sleep 5 & echo bye"]);
        let mut p = spawn_with(&request(80, 24), launch).unwrap();
        let child = p.child.clone();
        let waiter = std::thread::spawn(move || child.wait());
        let started = Instant::now();
        let mut out = Vec::new();
        read_to_end(&mut *p.reader, &mut out);
        assert!(String::from_utf8_lossy(&out).contains("bye"));
        assert_eq!(waiter.join().unwrap().code, Some(0));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
