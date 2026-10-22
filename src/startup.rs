//! Startup helpers: the XDG file picker and terminal detachment.
//!
//! MuReader has no positional arguments. It asks the desktop portal for a
//! document, then runs independently of the shell that launched it. The
//! foreground escape hatch exists only for debugging and automated checks.

use std::env;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

const FOREGROUND_ENV: &str = "MUREADER_FOREGROUND";

pub fn pick_document() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Open document")
        .add_filter(
            "Documents",
            &[
                "pdf", "epub", "mobi", "fb2", "xps", "oxps", "cbz", "svg", "html", "htm", "txt",
                "docx", "xlsx", "pptx", "hwpx", "png", "jpg", "jpeg",
            ],
        )
        .add_filter("All files", &["*"])
        .pick_file()
}

/// Leave the invoking shell and its terminal behind.
///
/// The double fork is deliberate: it prevents MuReader from acquiring a
/// controlling terminal again. Standard output and error are redirected so a
/// later terminal close cannot terminate a running viewer.
pub fn detach() -> io::Result<()> {
    if env::var_os(FOREGROUND_ENV).is_some() {
        return Ok(());
    }

    // SAFETY: this runs at the very start of `main`, before the event loop or
    // any worker threads exist. `_exit` is used in the parents so destructors
    // do not run in the process that is leaving.
    unsafe {
        match libc::fork() {
            -1 => return Err(io::Error::last_os_error()),
            0 => {}
            _ => libc::_exit(0),
        }
        if libc::setsid() == -1 {
            return Err(io::Error::last_os_error());
        }
        match libc::fork() {
            -1 => return Err(io::Error::last_os_error()),
            0 => {}
            _ => libc::_exit(0),
        }
    }

    redirect_standard_fds()
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
