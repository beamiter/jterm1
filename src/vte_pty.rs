//! Man-in-the-middle PTY pair for block mode's active VTE.
//!
//! Background — in block mode the active VTE used to run on a dummy PTY: VTE's
//! own writes (keypresses + answers to PTY-mediated queries like DSR / DA /
//! OSC 4 color / mouse / focus / bracketed paste) had nowhere to go. jterm1
//! intercepted those queries by sniffing the upstream byte stream and
//! synthesising replies. That is ~300 lines of fragile parser code that must
//! be updated every time xterm or the application protocol grows a new escape.
//!
//! With a real PTY here, libvte answers everything natively. We open a fresh
//! PTY pair, hand the *master* end to VTE via [`vte4::Pty::foreign_sync`]
//! (libvte's foreign_sync requires a master fd — grantpt/unlockpt/ioctl
//! probes return EINVAL on a slave), and keep the slave end on the jterm1
//! side. Bytes still flow symmetrically across the pair: writes to the slave
//! arrive at the master (VTE renders them) and writes by VTE on the master
//! arrive at the slave (jterm1 splices them onto the shell PTY).
//!
//! ```text
//!   shell-PTY  <──┐                          ┌──> active VTE
//!                 │  parser (OSC 133 only)   │
//!     shell ── reads/writes ── jterm1 ── reads/writes ── VTE
//!                 │                          │
//!                 └──> finished blocks       └──> VtePty (this module)
//! ```
//!
//! The reader thread on the jterm1-owned (slave) fd delivers VTE's writes
//! back to jterm1, which forwards them straight to the shell PTY:
//!
//!   shell → parser → vte_pty.write_bytes()       (jterm1 → VTE)
//!   vte_pty reader → shell_pty.write_bytes()     (VTE → shell)

use gtk4::glib;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::{mpsc, Arc, Mutex};

extern "C" {
    fn g_unix_fd_add_full(
        priority: i32,
        fd: i32,
        condition: u32,
        function: extern "C" fn(fd: i32, condition: u32, user_data: *mut std::ffi::c_void) -> i32,
        user_data: *mut std::ffi::c_void,
        notify: extern "C" fn(data: *mut std::ffi::c_void),
    ) -> u32;
}

const G_IO_IN: u32 = 1;
const G_PRIORITY_DEFAULT: i32 = 0;

struct FdWatchData<F: FnMut() -> bool> {
    callback: F,
}

extern "C" fn fd_watch_callback<F: FnMut() -> bool>(
    _fd: i32,
    _condition: u32,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    let data = unsafe { &mut *(user_data as *mut FdWatchData<F>) };
    if (data.callback)() {
        1
    } else {
        0
    }
}

extern "C" fn fd_watch_destroy<F: FnMut() -> bool>(user_data: *mut std::ffi::c_void) {
    unsafe {
        drop(Box::from_raw(user_data as *mut FdWatchData<F>));
    }
}

fn unix_fd_add_local<F: FnMut() -> bool + 'static>(fd: RawFd, func: F) {
    let data = Box::new(FdWatchData { callback: func });
    let ptr = Box::into_raw(data) as *mut std::ffi::c_void;
    unsafe {
        g_unix_fd_add_full(
            G_PRIORITY_DEFAULT,
            fd,
            G_IO_IN,
            fd_watch_callback::<F>,
            ptr,
            fd_watch_destroy::<F>,
        );
    }
}

/// A PTY pair owned by jterm1 whose *master* end is wrapped in a `vte4::Pty`
/// and attached to the active VTE. Read the module docs for the data flow.
pub struct VtePty {
    /// jterm1's local end of the pair — the slave fd. Wrapped in Mutex so
    /// close/drop and writes serialise. Named "local" to avoid confusion: the
    /// master is owned by libvte (and closed when `vte_pty` drops).
    local: Arc<Mutex<Option<OwnedFd>>>,
    /// The `vte4::Pty` holding the master end. The caller passes this to
    /// `Terminal::set_pty`. Stored so it lives as long as VtePty does.
    vte_pty: vte4::Pty,
}

impl VtePty {
    /// Allocate a fresh PTY pair. The master fd is moved into a
    /// `vte4::Pty::foreign_sync` so libvte owns it (libvte's foreign_sync
    /// requires a master — passing a slave fails with EINVAL on the
    /// grantpt/unlockpt path). Initial winsize is set to 80×24; callers
    /// should resize after the GTK widget is realised.
    pub fn new() -> io::Result<Self> {
        let initial_size = nix::pty::Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let OpenptyResult { master, slave } =
            openpty(Some(&initial_size), None).map_err(io::Error::other)?;
        // Put the slave in raw mode. openpty(3) leaves it in the kernel default
        // canonical/cooked discipline (ICANON + ECHO + ICRNL + OPOST). That
        // discipline mangles the MITM splice in two ways: ICANON line-buffers
        // VTE's keystrokes so the slave reader only sees a chunk after Enter,
        // and ICRNL rewrites Enter's `\r` to `\n` — which rsh-style line
        // editors treat as a literal newline, not the "execute command"
        // signal. Net effect: typing `pwd` + Enter rendered into the active
        // VTE but the shell never saw a CR, no command ever ran, and OSC 133;C
        // / ;D never fired (so no finished block, ever). cfmakeraw turns off
        // ICANON, ECHO, ICRNL, OPOST, etc., letting bytes splice through
        // verbatim in both directions.
        unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(slave.as_raw_fd(), &mut tio) == 0 {
                libc::cfmakeraw(&mut tio);
                libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &tio);
            }
        }
        // vte4::Pty::foreign_sync takes io_lifetimes::OwnedFd, which is the
        // same underlying type as std's; round-trip via raw fd to bridge the
        // two crates without taking a hard dep on io-lifetimes here.
        let master_raw = master.into_raw_fd();
        let master_fd = unsafe { io_lifetimes::OwnedFd::from_raw_fd(master_raw) };
        let vte_pty = vte4::Pty::foreign_sync(master_fd, None::<&gtk4::gio::Cancellable>)
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(VtePty {
            local: Arc::new(Mutex::new(Some(slave))),
            vte_pty,
        })
    }

    pub fn vte_pty(&self) -> &vte4::Pty {
        &self.vte_pty
    }

    /// Feed bytes from the shell PTY → the VTE side. Best-effort: a partial
    /// write or EAGAIN is dropped (the master PTY's kernel buffer is large
    /// enough for normal terminal traffic and a few-byte loss after disconnect
    /// is preferable to blocking the UI thread).
    pub fn write_bytes(&self, data: &[u8]) {
        if let Ok(guard) = self.local.lock() {
            if let Some(fd) = guard.as_ref() {
                let raw = fd.as_raw_fd();
                unsafe {
                    libc::write(raw, data.as_ptr() as *const libc::c_void, data.len());
                }
            }
        }
    }

    /// Mirror the kernel TIOCSWINSZ on the master so SIGWINCH propagates and
    /// the slave's reported window size matches what VTE believes. Called
    /// from block.rs alongside the shell-PTY resize so both sides stay in sync.
    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(guard) = self.local.lock() {
            if let Some(fd) = guard.as_ref() {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws);
                }
            }
        }
    }

    /// Spawn a background reader on the master fd. Each delivered chunk is the
    /// raw byte stream VTE wrote — typically `commit` keystrokes plus answers
    /// to PTY queries (DSR / DA / OSC color / mouse / focus / bracketed paste).
    /// Callback runs on the GLib main thread via an eventfd-signalled mpsc,
    /// mirroring `OwnedPty::start_reader`'s approach.
    pub fn start_reader<F>(&self, mut callback: F)
    where
        F: FnMut(Vec<u8>) + 'static,
    {
        let fd = match self
            .local
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|f| f.as_raw_fd()))
        {
            Some(fd) => fd,
            None => return,
        };
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let efd: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if efd < 0 {
            // No eventfd → fall back to a glib idle poll. VTE's outgoing
            // traffic is light (only response data + commits), so a 10ms tick
            // is acceptable here.
            let local = self.local.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(10), move || {
                let raw = match local.lock().ok().and_then(|g| g.as_ref().map(|f| f.as_raw_fd())) {
                    Some(fd) => fd,
                    None => return glib::ControlFlow::Break,
                };
                let mut buf = [0u8; 4096];
                let n = unsafe {
                    libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n > 0 {
                    let n = n as usize;
                    callback(buf[..n].to_vec());
                }
                glib::ControlFlow::Continue
            });
            return;
        }
        let efd_for_thread = efd;
        std::thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut buf = [0u8; 65536];
            loop {
                match file.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        std::mem::forget(file);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            std::mem::forget(file);
                            break;
                        }
                        let one: u64 = 1;
                        unsafe {
                            libc::write(
                                efd_for_thread,
                                &one as *const u64 as *const libc::c_void,
                                8,
                            );
                        }
                    }
                }
            }
        });
        unix_fd_add_local(efd, move || {
            let mut val: u64 = 0;
            unsafe {
                libc::read(efd, &mut val as *mut u64 as *mut libc::c_void, 8);
            }
            loop {
                match rx.try_recv() {
                    Ok(data) => callback(data),
                    Err(mpsc::TryRecvError::Empty) => return true,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        unsafe {
                            libc::close(efd);
                        }
                        return false;
                    }
                }
            }
        });
    }
}

impl Drop for VtePty {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.local.lock() {
            guard.take();
        }
    }
}
