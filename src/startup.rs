// SPDX-License-Identifier: AGPL-3.0-only
//! Startup helpers: the XDG file picker and terminal detachment.
//!
//! MuReader accepts one optional positional file path. With no argument it
//! asks the desktop portal for a document, then runs independently of the
//! shell that launched it. The foreground escape hatch exists only for
//! debugging and automated checks.

use std::env;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

const FOREGROUND_ENV: &str = "MUREADER_FOREGROUND";
const DETACH_TIMEOUT_MS: libc::c_int = 5_000;

pub fn pick_document() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Open document")
        .add_filter(
            "Documents",
            &[
                "pdf", "epub", "mobi", "fb2", "xps", "oxps", "cbz", "svg", "html", "htm", "txt",
                "text", "log", "docx", "xlsx", "pptx", "hwpx", "png", "jpg", "jpeg",
            ],
        )
        .add_filter("All files", &["*"])
        .pick_file()
}

/// The file named on the command line, if any.
///
/// This stays separate from the portal picker so a desktop entry or a shell
/// can open a specific document without changing the no-argument behavior.
pub fn document_path() -> Result<Option<PathBuf>, String> {
    let mut args = env::args_os().skip(1);
    let Some(path) = args.next() else {
        return Ok(None);
    };
    if args.next().is_some() {
        return Err("usage: mureader [FILE]".to_string());
    }
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty() {
        return Err("usage: mureader [FILE]".to_string());
    }
    Ok(Some(path))
}

/// Leave the invoking shell and its terminal behind.
///
/// The double fork is deliberate: it prevents MuReader from acquiring a
/// controlling terminal again. The original parent waits on a status pipe
/// until the grandchild has completed `setsid`, the second fork, and stdio
/// redirection, so a failed detach is reported as a non-zero exit instead of
/// silently detaching a broken process. Standard output and error are
/// redirected so a later terminal close cannot terminate a running viewer.
///
/// The working directory is intentionally preserved: a relative
/// `MUREADER_CONFIG` or `XDG_CONFIG_HOME` must keep resolving from where the
/// user launched the reader.
pub fn detach() -> io::Result<()> {
    if env::var_os(FOREGROUND_ENV).is_some() {
        return Ok(());
    }

    // SAFETY: this runs at the very start of `main`, before the event loop or
    // any worker threads exist. `_exit` is used in the parents so destructors
    // do not run in the process that is leaving.
    unsafe {
        let mut pipe = [-1; 2];
        if libc::pipe(pipe.as_mut_ptr()) == -1 {
            return Err(io::Error::last_os_error());
        }
        let (read_fd, write_fd) = (pipe[0], pipe[1]);

        match libc::fork() {
            -1 => {
                let error = io::Error::last_os_error();
                libc::close(read_fd);
                libc::close(write_fd);
                Err(error)
            }
            0 => {
                // First child: become the session leader, then fork again so
                // the viewer can never regain a controlling terminal.
                libc::close(read_fd);
                match detach_child(write_fd) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        eprintln!("MuReader: detach failed: {error}");
                        write_status(write_fd, false);
                        libc::close(write_fd);
                        libc::_exit(1);
                    }
                }
            }
            _ => {
                // Original parent: the shell must not return success until
                // the grandchild is ready to run the event loop.
                libc::close(write_fd);
                let success = wait_status(read_fd);
                libc::close(read_fd);
                if !success {
                    eprintln!("MuReader: detach failed");
                }
                libc::_exit(if success { 0 } else { 1 });
            }
        }
    }
}

/// Run in the first child. On success this function returns only in the
/// grandchild; the intermediate parent exits without running destructors.
unsafe fn detach_child(write_fd: RawFd) -> io::Result<()> {
    if libc::setsid() == -1 {
        return Err(io::Error::last_os_error());
    }
    match libc::fork() {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => libc::_exit(0),
    }

    if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    redirect_standard_fds()?;
    if !write_status(write_fd, true) {
        libc::close(write_fd);
        return Err(io::Error::last_os_error());
    }
    libc::close(write_fd);
    Ok(())
}

unsafe fn write_status(fd: RawFd, success: bool) -> bool {
    let byte = [if success { 0 } else { 1 }];
    loop {
        let written = libc::write(fd, byte.as_ptr().cast(), 1);
        if written == 1 {
            return true;
        }
        if written == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return false;
    }
}

unsafe fn wait_status(fd: RawFd) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let ready = libc::poll(&mut poll, 1, DETACH_TIMEOUT_MS);
        if ready > 0 {
            return read_status(fd);
        }
        if ready == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return false;
    }
}

unsafe fn read_status(fd: RawFd) -> bool {
    let mut byte = [1u8];
    loop {
        let read = libc::read(fd, byte.as_mut_ptr().cast(), 1);
        if read == 1 {
            return byte[0] == 0;
        }
        if read == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return false;
    }
}

fn redirect_standard_fds() -> io::Result<()> {
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let null_fd = null.as_raw_fd();
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        if unsafe { libc::dup2(null_fd, fd) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    let log = log_file();
    let error_fd = log.as_ref().map_or(null_fd, AsRawFd::as_raw_fd);
    if unsafe { libc::dup2(error_fd, libc::STDERR_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn log_file() -> Option<File> {
    let state = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    let directory = state.join("mureader");
    std::fs::create_dir_all(&directory).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("mureader.log"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_pipe_reports_success_and_failure() {
        for expected in [true, false] {
            let mut pipe = [-1; 2];
            assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
            assert!(unsafe { write_status(pipe[1], expected) });
            unsafe { libc::close(pipe[1]) };
            assert_eq!(unsafe { read_status(pipe[0]) }, expected);
            unsafe { libc::close(pipe[0]) };
        }
    }

    #[test]
    fn closed_status_pipe_is_a_failure() {
        let mut pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        unsafe { libc::close(pipe[1]) };
        assert!(!unsafe { wait_status(pipe[0]) });
        unsafe { libc::close(pipe[0]) };
    }
}
