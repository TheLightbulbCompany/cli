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
    process(imp::info(pid)?.ppid)
}

/// `pid`'s facts; readable for any uid's process (root-owned `login`, `sshd` included).
pub fn process(pid: u32) -> Result<Process> {
    Ok(Process {
        pid,
        birth_micros: imp::info(pid)?.birth_micros,
        executable: imp::executable(pid)?,
    })
}

/// This computer's short host name (its first label, at most 64 characters, so a member
/// admits it as a computer name); empty when the OS will not say.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer length is passed alongside the pointer.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    short_name(&String::from_utf8_lossy(&buf[..end]))
}

/// `host`'s first label, trimmed, at most 64 characters.
fn short_name(host: &str) -> String {
    host.split('.')
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(64)
        .collect()
}

/// Whether `peer_pid` works here (Claude pushes need it).
pub const PEER_PID: bool = cfg!(target_os = "macos");

/// The pid of the process at the other end of a connected Unix socket.
pub fn peer_pid(fd: std::os::fd::RawFd) -> Result<u32> {
    imp::peer_pid(fd)
}

struct Info {
    ppid: u32,
    /// 0 when the OS will not say (another uid's process).
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

    /// The ppid from `PROC_PIDT_SHORTBSDINFO`, which answers for any uid's process; the
    /// birth from `PROC_PIDTBSDINFO`, which does not (then 0).
    pub fn info(pid: u32) -> Result<Info> {
        let short: libc::proc_bsdshortinfo =
            pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO).ok_or(format!("no process {pid}"))?;
        let birth_micros = pidinfo::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO)
            .map_or(0, |b| b.pbi_start_tvsec * 1_000_000 + b.pbi_start_tvusec);
        Ok(Info {
            ppid: short.pbsi_ppid,
            birth_micros,
        })
    }

    /// One `proc_pidinfo` struct of `flavor`, when the kernel fills it whole.
    fn pidinfo<T>(pid: u32, flavor: libc::c_int) -> Option<T> {
        let mut b = MaybeUninit::<T>::zeroed();
        let size = size_of::<T>() as libc::c_int;
        // SAFETY: the buffer is `size` bytes and proc_pidinfo writes at most that.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                flavor,
                0,
                b.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        // SAFETY: the kernel filled the whole struct (n == size), and these are plain C
        // structs for which any bytes are valid.
        (n == size).then(|| unsafe { b.assume_init() })
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

    pub fn peer_pid(_fd: std::os::fd::RawFd) -> Result<u32> {
        Err("process lookup is only implemented on macOS".into())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn short_name_fits_a_computer_name() {
        // GitHub's macOS runner: 68 bytes in full.
        let runner = "dsm41-ai250-f4f311fd-890c-4d34-aa50-5177d395bc82-F655ED2D4EB5.local";
        assert_eq!(
            short_name(runner),
            "dsm41-ai250-f4f311fd-890c-4d34-aa50-5177d395bc82-F655ED2D4EB5"
        );
        assert_eq!(short_name("mac"), "mac");
        assert_eq!(short_name(&"a".repeat(80)).len(), 64);
    }

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
        // Root-owned launchd: lookups work across uids.
        assert_eq!(imp::info(1).unwrap().ppid, 0);
        assert_eq!(
            process(1).unwrap().executable,
            PathBuf::from("/sbin/launchd")
        );
    }
}
