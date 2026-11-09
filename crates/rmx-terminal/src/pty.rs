//! A shell on a pseudo terminal, started without `fork`.
//!
//! `tty::new` forks, which copies the page tables of the whole process with its memory map locked
//! and write-protects every page. In an application holding a gigabyte of scrollback and remote
//! desktops, that stops other threads for many milliseconds whenever they fault or map, and makes
//! them fault afterwards. `posix_spawn` shares the address space with the child until it execs.

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::Arc;
use std::{env, ptr};

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event, PollMode, Poller};
use signal_hook::SigId;
use signal_hook::low_level::{pipe as signal_pipe, unregister as unregister_signal};

// What alacritty_terminal's event loop expects these two kinds of readiness to be called.
const PTY_READ_WRITE_TOKEN: usize = 0;
const PTY_CHILD_EVENT_TOKEN: usize = 1;

/// A shell and the controlling side of its terminal.
pub struct Pty {
    child: libc::pid_t,
    /// Whether the child has been reaped, after which its pid can be anyone's.
    exited: bool,
    file: File,
    signals: UnixStream,
    sig_id: SigId,
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 { Err(io::Error::last_os_error()) } else { Ok(result) }
}

/// `posix_spawn` and friends return the error rather than setting `errno`.
fn check_spawn(result: libc::c_int) -> io::Result<()> {
    if result == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(result)) }
}

fn winsize(size: WindowSize) -> libc::winsize {
    libc::winsize {
        ws_row: size.num_lines,
        ws_col: size.num_cols,
        ws_xpixel: size.num_cols * size.cell_width,
        ws_ypixel: size.num_lines * size.cell_height,
    }
}

/// A new pseudo terminal's controlling side, and the path of the side the shell will open.
fn open_pty(size: WindowSize) -> io::Result<(OwnedFd, CString)> {
    let fd = check(unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) })?;
    let master = unsafe { OwnedFd::from_raw_fd(fd) };
    check(unsafe { libc::grantpt(fd) })?;
    check(unsafe { libc::unlockpt(fd) })?;
    let mut name = [0; 128];
    check_spawn(unsafe { libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) })?;
    let path = unsafe { CStr::from_ptr(name.as_ptr()) }.to_owned();

    let window = winsize(size);
    check(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &window as *const libc::winsize) })?;
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } == 0 {
        let mut termios = unsafe { termios.assume_init() };
        termios.c_iflag |= libc::IUTF8;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) };
    }
    Ok((master, path))
}

/// The user's name, home and shell, from the environment or else the password database.
fn shell_user() -> io::Result<(String, String, String)> {
    let from_env = (env::var("USER"), env::var("HOME"), env::var("SHELL"));
    if let (Ok(user), Ok(home), Ok(shell)) = from_env {
        return Ok((user, home, shell));
    }

    let mut buffer = [0; 1024];
    let mut entry = MaybeUninit::<libc::passwd>::uninit();
    let mut found: *mut libc::passwd = ptr::null_mut();
    let status = unsafe {
        let uid = libc::getuid();
        libc::getpwuid_r(uid, entry.as_mut_ptr(), buffer.as_mut_ptr(), buffer.len(), &mut found)
    };
    if status != 0 || found.is_null() {
        return Err(io::Error::other("could not find the user in the password database"));
    }
    let entry = unsafe { entry.assume_init() };
    let text = |field: *const libc::c_char| {
        unsafe { CStr::from_ptr(field) }.to_string_lossy().into_owned()
    };
    Ok((
        env::var("USER").unwrap_or_else(|_| text(entry.pw_name)),
        env::var("HOME").unwrap_or_else(|_| text(entry.pw_dir)),
        env::var("SHELL").unwrap_or_else(|_| text(entry.pw_shell)),
    ))
}

fn c_string(text: impl AsRef<OsStr>) -> io::Result<CString> {
    CString::new(text.as_ref().as_bytes()).map_err(|_| io::Error::from(ErrorKind::InvalidInput))
}

/// `KEY=value` for the shell: this process's environment with the terminal's own settings.
fn environment(id: &str, user: &str, home: &str) -> io::Result<Vec<CString>> {
    const REPLACED: [&str; 4] = ["ALACRITTY_WINDOW_ID", "USER", "HOME", "WINDOWID"];
    // Startup notification is this process's to answer, not its children's.
    const REMOVED: [&str; 2] = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

    let mut entries = Vec::new();
    for (key, value) in env::vars_os() {
        if REPLACED.iter().chain(&REMOVED).any(|name| key == *name) {
            continue;
        }
        let mut entry = key.as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        entries.push(CString::new(entry).map_err(|_| io::Error::from(ErrorKind::InvalidInput))?);
    }
    for (key, value) in
        [("ALACRITTY_WINDOW_ID", id), ("USER", user), ("HOME", home), ("WINDOWID", id)]
    {
        entries.push(c_string(format!("{key}={value}"))?);
    }
    Ok(entries)
}

/// Start `program`, or the user's shell, as session leader on the terminal at `tty_path`.
fn start_child(argv: &[CString], tty_path: &CStr, envp: &[CString]) -> io::Result<libc::pid_t> {
    unsafe {
        let mut actions = MaybeUninit::<libc::posix_spawn_file_actions_t>::uninit();
        check_spawn(libc::posix_spawn_file_actions_init(actions.as_mut_ptr()))?;
        let mut actions = actions.assume_init();
        let mut attributes = MaybeUninit::<libc::posix_spawnattr_t>::uninit();
        check_spawn(libc::posix_spawnattr_init(attributes.as_mut_ptr()))?;
        let mut attributes = attributes.assume_init();

        let result = (|| {
            // Opened by a session leader with no terminal yet, it becomes the controlling one.
            check_spawn(libc::posix_spawn_file_actions_addopen(
                &mut actions,
                0,
                tty_path.as_ptr(),
                libc::O_RDWR,
                0,
            ))?;
            check_spawn(libc::posix_spawn_file_actions_adddup2(&mut actions, 0, 1))?;
            check_spawn(libc::posix_spawn_file_actions_adddup2(&mut actions, 0, 2))?;

            let mut defaults = MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(defaults.as_mut_ptr());
            let mut defaults = defaults.assume_init();
            for signal in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
                libc::SIGPIPE,
            ] {
                libc::sigaddset(&mut defaults, signal);
            }
            let mut empty = MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(empty.as_mut_ptr());
            let empty = empty.assume_init();

            check_spawn(libc::posix_spawnattr_setsigdefault(&mut attributes, &defaults))?;
            check_spawn(libc::posix_spawnattr_setsigmask(&mut attributes, &empty))?;
            let flags = libc::POSIX_SPAWN_SETSID as libc::c_int
                | libc::POSIX_SPAWN_SETSIGDEF
                | libc::POSIX_SPAWN_SETSIGMASK;
            check_spawn(libc::posix_spawnattr_setflags(&mut attributes, flags as libc::c_short))?;

            let mut argv: Vec<*mut libc::c_char> =
                argv.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
            argv.push(ptr::null_mut());
            let mut envp: Vec<*mut libc::c_char> =
                envp.iter().map(|entry| entry.as_ptr().cast_mut()).collect();
            envp.push(ptr::null_mut());

            let mut child = 0;
            check_spawn(libc::posix_spawnp(
                &mut child,
                argv[0],
                &actions,
                &attributes,
                argv.as_ptr(),
                envp.as_ptr(),
            ))?;
            Ok(child)
        })();
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attributes);
        result
    }
}

/// Start `shell`, or the user's own, on a new terminal of `size`.
pub fn spawn(shell: Option<(String, Vec<String>)>, size: WindowSize, id: u64) -> io::Result<Pty> {
    let (master, tty_path) = open_pty(size)?;
    let (user, home, login_shell) = shell_user()?;
    let (program, args) = shell.unwrap_or((login_shell, Vec::new()));
    let argv: Vec<CString> =
        std::iter::once(&program).chain(&args).map(c_string).collect::<io::Result<_>>()?;
    let id_text = id.to_string();
    let envp = environment(&id_text, &user, &home)?;

    let (sender, signals) = UnixStream::pair()?;
    let sig_id = signal_pipe::register(signal_hook::consts::SIGCHLD, sender)?;
    signals.set_nonblocking(true)?;

    let child = match start_child(&argv, &tty_path, &envp) {
        Ok(child) => child,
        Err(err) => {
            unregister_signal(sig_id);
            return Err(io::Error::new(
                err.kind(),
                format!("Failed to spawn command '{program}': {err}"),
            ));
        },
    };

    let flags = check(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) })?;
    check(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
    Ok(Pty { child, exited: false, file: File::from(master), signals, sig_id })
}

impl Drop for Pty {
    fn drop(&mut self) {
        unregister_signal(self.sig_id);
        if !self.exited {
            unsafe {
                libc::kill(self.child, libc::SIGHUP);
                let mut status = 0;
                libc::waitpid(self.child, &mut status, 0);
            }
        }
    }
}

impl EventedReadWrite for Pty {
    type Reader = File;
    type Writer = File;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        unsafe {
            poll.add_with_mode(&self.file, interest, mode)?;
            poll.add_with_mode(
                &self.signals,
                Event::readable(PTY_CHILD_EVENT_TOKEN),
                PollMode::Level,
            )
        }
    }

    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        poll.modify_with_mode(&self.file, interest, mode)?;
        poll.modify_with_mode(
            &self.signals,
            Event::readable(PTY_CHILD_EVENT_TOKEN),
            PollMode::Level,
        )
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        poll.delete(&self.file)?;
        poll.delete(&self.signals)
    }

    fn reader(&mut self) -> &mut File {
        &mut self.file
    }

    fn writer(&mut self) -> &mut File {
        &mut self.file
    }
}

impl EventedPty for Pty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        let mut signal = [0u8; 1];
        if self.signals.read(&mut signal).is_err() || self.exited {
            return None;
        }
        let mut status = 0;
        match unsafe { libc::waitpid(self.child, &mut status, libc::WNOHANG) } {
            0 | -1 => None,
            _ => {
                self.exited = true;
                Some(ChildEvent::Exited(Some(ExitStatus::from_raw(status))))
            },
        }
    }
}

impl OnResize for Pty {
    fn on_resize(&mut self, size: WindowSize) {
        let window = winsize(size);
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), libc::TIOCSWINSZ, &window as *const libc::winsize)
        };
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::time::{Duration, Instant};

    use super::*;

    fn read_until(pty: &mut Pty, end: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut text = String::new();
        let mut buffer = [0u8; 4096];
        while !text.contains(end) && Instant::now() < deadline {
            match pty.file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => text.push_str(&String::from_utf8_lossy(&buffer[..read])),
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                },
                Err(_) => break,
            }
        }
        text
    }

    #[test]
    fn the_shell_gets_a_controlling_terminal_of_the_right_size() {
        let size = WindowSize { num_lines: 31, num_cols: 97, cell_width: 8, cell_height: 16 };
        let script =
            "echo controlling > /dev/tty && stty size && echo id=$ALACRITTY_WINDOW_ID; echo done";
        let shell = ("sh".to_owned(), vec!["-c".to_owned(), script.to_owned()]);
        let mut pty = spawn(Some(shell), size, 4242).unwrap();
        let text = read_until(&mut pty, "done");
        assert!(text.contains("controlling"), "{text}");
        assert!(text.contains("31 97"), "{text}");
        assert!(text.contains("id=4242"), "{text}");
    }

    #[test]
    fn a_program_that_cannot_be_found_is_an_error() {
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let shell = ("/no/such/program".to_owned(), Vec::new());
        assert!(spawn(Some(shell), size, 1).is_err());
    }
}
