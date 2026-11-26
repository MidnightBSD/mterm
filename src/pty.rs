use crate::profile::EmulationType;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// Build the child's environment (as `KEY=VALUE` C strings) entirely in the parent.
fn build_child_env(emulation: EmulationType) -> Vec<CString> {
    const OVERRIDDEN: [&str; 3] = ["TERM", "COLORTERM", "TERM_PROGRAM"];
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(k, _)| !OVERRIDDEN.iter().any(|o| k.as_bytes() == o.as_bytes()))
        .filter_map(|(k, v)| {
            let mut kv = k.as_bytes().to_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            CString::new(kv).ok()
        })
        .collect();

    let mut push = |key: &str, val: &[u8]| {
        let mut kv = format!("{key}=").into_bytes();
        kv.extend_from_slice(val);
        if let Ok(c) = CString::new(kv) {
            env.push(c);
        }
    };
    push("TERM", emulation.term_env().as_bytes());
    if emulation.supports_256_color() {
        push("COLORTERM", b"truecolor");
    }
    push("TERM_PROGRAM", b"mterm");
    env
}

/// Events received from the background PTY reader thread
pub enum PtyEvent {
    Data(Vec<u8>),
    Exited,
}

/// Manages a POSIX pseudo-terminal master file descriptor and child process
pub struct PtyHandle {
    master_fd: RawFd,
    child_pid: libc::pid_t,
    pub rx: Receiver<PtyEvent>,
}

impl PtyHandle {
    /// Spawn a new shell process inside a pseudo-terminal with `$TERM` set according to `emulation`
    pub fn spawn(
        cols: u16,
        rows: u16,
        emulation: EmulationType,
        working_dir: Option<&str>,
    ) -> io::Result<Self> {
        let mut master_fd: libc::c_int = -1;
        let ws = libc::winsize {
            ws_row: rows.max(2),
            ws_col: cols.max(2),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // Pre-allocate everything in the parent BEFORE forkpty() so the forked child of this
        // multithreaded process only makes async-signal-safe calls (no malloc/setenv) before exec.
        let c_working_dir = working_dir.and_then(|d| CString::new(d).ok());
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| s.starts_with('/'))
            .unwrap_or_else(|| "/bin/sh".to_string());
        let c_shell = CString::new(shell).unwrap_or_else(|_| CString::new("/bin/sh").unwrap());
        let c_env = build_child_env(emulation);
        let mut envp: Vec<*const libc::c_char> = c_env.iter().map(|c| c.as_ptr()).collect();
        envp.push(std::ptr::null());
        let argv = [c_shell.as_ptr(), std::ptr::null()];

        let pid = unsafe {
            libc::forkpty(
                &mut master_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &ws as *const libc::winsize as *mut libc::winsize,
            )
        };

        if pid < 0 {
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            // Child process: ONLY async-signal-safe calls before execve!
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                if let Some(ref dir) = c_working_dir {
                    libc::chdir(dir.as_ptr());
                }
                libc::execve(c_shell.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(1);
            }
        }

        // Parent process: spawn reader thread on duplicated fd so reading and writing don't contend
        let read_fd = unsafe { libc::dup(master_fd) };
        if read_fd < 0 {
            unsafe {
                libc::close(master_fd);
            }
            return Err(io::Error::last_os_error());
        }

        let (tx, rx) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("pty-reader".to_string())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let mut buf = vec![0u8; 8192];
                loop {
                    let n = unsafe {
                        libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                    };
                    if n > 0 {
                        let bytes = buf[..(n as usize)].to_vec();
                        if tx.send(PtyEvent::Data(bytes)).is_err() {
                            break;
                        }
                    } else if n == 0 {
                        let _ = tx.send(PtyEvent::Exited);
                        break;
                    } else {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        let _ = tx.send(PtyEvent::Exited);
                        break;
                    }
                }
                unsafe {
                    libc::close(read_fd);
                }
            });

        Ok(Self {
            master_fd,
            child_pid: pid,
            rx,
        })
    }

    #[cfg(test)]
    pub fn dummy() -> Self {
        let (_tx, rx) = mpsc::channel();
        Self {
            master_fd: -1,
            child_pid: -1,
            rx,
        }
    }

    /// Write raw bytes to the PTY master
    pub fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = unsafe {
                libc::write(
                    self.master_fd,
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            data = &data[(n as usize)..];
        }
        Ok(())
    }

    /// Notify the PTY of a terminal grid resize (`TIOCSWINSZ`)
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows.max(2),
            ws_col: cols.max(2),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let res = unsafe { libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws) };
        if res < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        if self.master_fd >= 0 {
            unsafe {
                libc::close(self.master_fd);
                libc::kill(self.child_pid, libc::SIGHUP);
            }
        }
    }
}
