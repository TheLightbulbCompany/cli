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
pub const PEER_PID: bool = cfg!(any(target_os = "macos", target_os = "linux"));

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

#[cfg(target_os = "linux")]
mod imp {
    use super::Info;
    use crate::Result;
    use std::mem::size_of;
    use std::path::PathBuf;

    /// From `/proc/<pid>/stat` (readable for any uid's process): the ppid, and the start
    /// time in clock ticks since boot, placed on the epoch by `/proc/stat`'s `btime`.
    pub fn info(pid: u32) -> Result<Info> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|_| format!("no process {pid}"))?;
        // `pid (comm) state ppid …`: comm may hold spaces and parentheses, so fields count
        // from the last `)`; ppid is field 4 and starttime field 22.
        let rest = stat.rsplit_once(')').ok_or("bad /proc stat")?.1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let num = |i: usize| -> Result<u64> {
            Ok(fields.get(i).ok_or("bad /proc stat")?.parse::<u64>()?)
        };
        let (ppid, ticks) = (num(1)? as u32, num(19)?);
        Ok(Info {
            ppid,
            birth_micros: boot_micros().map_or(0, |b| b + ticks * 1_000_000 / clock_ticks()),
        })
    }

    fn clock_ticks() -> u64 {
        // SAFETY: sysconf takes no pointers.
        match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
            n if n > 0 => n as u64,
            _ => 100,
        }
    }

    /// Boot time, microseconds since the epoch (`btime`, whole seconds).
    fn boot_micros() -> Option<u64> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let line = stat.lines().find(|l| l.starts_with("btime "))?;
        Some(line[6..].trim().parse::<u64>().ok()? * 1_000_000)
    }

    /// `/proc/<pid>/exe`, less the ` (deleted)` Linux adds once the binary was replaced (an
    /// agent updated mid-session is still that agent). Another uid's process (pid 1, root
    /// `sshd`/`login`, WSL's `/init`) refuses that link; then its name from `comm`: an agent
    /// never runs as another uid. A same-uid process that refuses it (it made itself
    /// non-dumpable) is an error, so a walk through it fails closed.
    pub fn executable(pid: u32) -> Result<PathBuf> {
        match std::fs::read_link(format!("/proc/{pid}/exe")) {
            Ok(p) => Ok(undeleted(p)),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && other_uid(pid) => {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .map_err(|_| format!("no executable path for {pid}"))?;
                Ok(PathBuf::from(comm.trim_end_matches('\n')))
            }
            Err(_) => Err(format!("no executable path for {pid}").into()),
        }
    }

    fn undeleted(p: PathBuf) -> PathBuf {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let b = p.as_os_str().as_bytes();
        match b.strip_suffix(b" (deleted)") {
            Some(b) => std::ffi::OsString::from_vec(b.to_vec()).into(),
            None => p,
        }
    }

    /// Whether `/proc/<pid>/status` names a uid (real, effective, saved or fs) not ours;
    /// false when it cannot be read.
    fn other_uid(pid: u32) -> bool {
        // SAFETY: getuid takes no arguments and cannot fail.
        let me = unsafe { libc::getuid() }.to_string();
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| {
                let line = s.lines().find(|l| l.starts_with("Uid:"))?;
                Some(line[4..].split_whitespace().any(|u| u != me))
            })
            .unwrap_or(false)
    }

    pub fn peer_pid(fd: std::os::fd::RawFd) -> Result<u32> {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` is `len` bytes and getsockopt writes at most that.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        } != 0
            || cred.pid <= 0
        {
            return Err("no peer pid".into());
        }
        Ok(cred.pid as u32)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::Info;
    use crate::Result;
    use std::path::PathBuf;

    pub fn info(_pid: u32) -> Result<Info> {
        Err("process lookup is only implemented on macOS and Linux".into())
    }

    pub fn executable(_pid: u32) -> Result<PathBuf> {
        Err("process lookup is only implemented on macOS and Linux".into())
    }

    pub fn peer_pid(_fd: std::os::fd::RawFd) -> Result<u32> {
        Err("process lookup is only implemented on macOS and Linux".into())
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
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
        // Root-owned pid 1 (launchd, systemd): lookups work across uids.
        assert_eq!(imp::info(1).unwrap().ppid, 0);
        #[cfg(target_os = "macos")]
        assert_eq!(
            process(1).unwrap().executable,
            PathBuf::from("/sbin/launchd")
        );
        #[cfg(target_os = "linux")]
        {
            // Not root, `/proc/1/exe` is refused and the name comes from `comm`: whatever pid 1
            // is (systemd, init, or a container's tini or sh), it resolves to a name.
            let init = process(1).unwrap().executable;
            assert!(!init.as_os_str().is_empty());
        }
    }

    /// An agent whose binary is replaced mid-session keeps its path (Linux says
    /// `<path> (deleted)`).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_replaced_binary_keeps_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join("codex");
        // `cp`, not `fs::copy`: a write fd held here races a parallel test's fork (ETXTBSY).
        let cp = std::process::Command::new("cp")
            .arg("/bin/sleep")
            .arg(&codex)
            .status();
        assert!(cp.unwrap().success());
        let mut child = std::process::Command::new(&codex)
            .arg("30")
            .spawn()
            .unwrap();
        // Until the child's exec lands, its exe is still this test binary.
        let me = std::env::current_exe().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process(child.id()).unwrap().executable == me {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exec'd"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::remove_file(&codex).unwrap();
        let exe = process(child.id()).unwrap().executable;
        assert_eq!(exe, codex);
        assert_eq!(crate::agents::caller::classify(&exe), Some("codex"));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
