//! Process facts the engine needs: who started when, and from where.

use crate::Result;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Process {
    pub pid: u32,
    /// Start time, microseconds since the Unix epoch. With the pid it names one
    /// process for good: a reused pid has a different birth.
    pub birth_micros: u64,
    pub executable: PathBuf,
}

/// The parent of `pid`.
pub fn parent(pid: u32) -> Result<Process> {
    process(parent_pid(pid)?)
}

/// The parent pid of `pid`; readable for any uid's process (root-owned `login` included).
pub fn parent_pid(pid: u32) -> Result<u32> {
    imp::parent_pid(pid)
}

/// `pid`'s executable; readable for any uid's process.
pub fn executable(pid: u32) -> Result<PathBuf> {
    imp::executable(pid)
}

pub fn process(pid: u32) -> Result<Process> {
    let info = imp::info(pid)?;
    Ok(Process {
        pid,
        birth_micros: info.birth_micros,
        executable: imp::executable(pid)?,
    })
}

/// This computer's host name; empty when the OS will not say.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer length is passed alongside the pointer.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Whether `peer_pid` works here (Claude pushes need it).
pub const PEER_PID: bool = cfg!(target_os = "macos");

/// The pid of the process at the other end of a connected Unix socket.
pub fn peer_pid(fd: std::os::fd::RawFd) -> Result<u32> {
    imp::peer_pid(fd)
}

struct Info {
    birth_micros: u64,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Info;
    use crate::Result;
    use std::ffi::{c_void, OsStr};
    use std::mem::{size_of, MaybeUninit};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    pub fn info(pid: u32) -> Result<Info> {
        let mut b = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer is `size` bytes and proc_pidinfo writes at most that.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                b.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        if n != size {
            return Err(format!("no process {pid}").into());
        }
        // SAFETY: the kernel filled the whole struct (n == size).
        let b = unsafe { b.assume_init() };
        Ok(Info {
            birth_micros: b.pbi_start_tvsec * 1_000_000 + b.pbi_start_tvusec,
        })
    }

    /// `PROC_PIDT_SHORTBSDINFO`, unlike `PROC_PIDTBSDINFO`, answers for another uid's process.
    pub fn parent_pid(pid: u32) -> Result<u32> {
        let mut b = MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
        let size = size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        // SAFETY: the buffer is `size` bytes and proc_pidinfo writes at most that.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                b.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        if n != size {
            return Err(format!("no process {pid}").into());
        }
        // SAFETY: the kernel filled the whole struct (n == size).
        Ok(unsafe { b.assume_init() }.pbsi_ppid)
    }

    pub fn executable(pid: u32) -> Result<PathBuf> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer length is passed alongside the pointer.
        let n = unsafe {
            libc::proc_pidpath(
                pid as libc::c_int,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
            )
        };
        if n <= 0 {
            return Err(format!("no executable path for {pid}").into());
        }
        Ok(PathBuf::from(OsStr::from_bytes(&buf[..n as usize])))
    }

    pub fn peer_pid(fd: std::os::fd::RawFd) -> Result<u32> {
        let mut pid: libc::pid_t = 0;
        let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: `pid` is `len` bytes and getsockopt writes at most that.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        } != 0
        {
            return Err("no peer pid".into());
        }
        Ok(pid as u32)
    }
}

// ponytail: Linux (/proc/<pid>/stat + btime) is not written; the crate builds, lookups fail.
#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Info;
    use crate::Result;
    use std::path::PathBuf;

    pub fn info(_pid: u32) -> Result<Info> {
        Err("process lookup is only implemented on macOS".into())
    }

    pub fn executable(_pid: u32) -> Result<PathBuf> {
        Err("process lookup is only implemented on macOS".into())
    }

    pub fn parent_pid(_pid: u32) -> Result<u32> {
        Err("process lookup is only implemented on macOS".into())
    }

    pub fn peer_pid(_fd: std::os::fd::RawFd) -> Result<u32> {
        Err("process lookup is only implemented on macOS".into())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn own_process_and_parent_are_found() {
        let me = process(std::process::id()).unwrap();
        assert!(me.birth_micros > 0);
        assert!(me.executable.is_absolute());
        assert_eq!(
            parent(me.pid).unwrap().pid,
            std::os::unix::process::parent_id()
        );
        assert!(process(999_999).is_err());
        // Root-owned launchd: the walk's two lookups work across uids.
        assert_eq!(parent_pid(1).unwrap(), 0);
        assert_eq!(executable(1).unwrap(), PathBuf::from("/sbin/launchd"));
    }
}
