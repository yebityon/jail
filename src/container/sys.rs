//! The supervisor's calls into libc. Each `unsafe` block states why it is sound.
use anyhow::Result;
use std::{ffi::CString, io, path::Path};

#[cfg(target_os = "linux")]
pub use linux::{bind, bind_recursive, drop_privileges, drop_sys_admin, remount_readonly};

fn c_path(path: &Path) -> Result<CString> {
    Ok(CString::new(path.as_os_str().as_encoded_bytes())?)
}
pub fn is_root() -> bool {
    // SAFETY: geteuid takes no arguments and always succeeds.
    let euid = unsafe { libc::geteuid() };
    euid == 0
}
pub fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let path = c_path(path)?;
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    if unsafe { libc::chown(path.as_ptr(), uid, gid) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
pub fn drop_privileges(_: u32, _: u32) -> Result<()> {
    anyhow::bail!("jail-guest runs only on Linux")
}

#[cfg(target_os = "linux")]
mod linux {
    use super::c_path;
    use anyhow::{Context, Result};
    use std::{io, path::Path};

    /// CAP_SYS_ADMIN in <linux/capability.h>. It is below 32, so it is a bit
    /// of the first capability word.
    const CAP_SYS_ADMIN: u32 = 21;
    /// CAP_LAST_CAP in <linux/capability.h> since Linux 5.9.
    const CAP_LAST_CAP: u32 = 40;

    /// Binds `path` onto itself, without the mounts below it.
    pub fn bind(path: &Path) -> Result<()> {
        bind_with(path, libc::MS_BIND)
    }
    /// Binds `path` onto itself together with the mounts below it.
    pub fn bind_recursive(path: &Path) -> Result<()> {
        bind_with(path, libc::MS_BIND | libc::MS_REC)
    }
    fn bind_with(path: &Path, flags: libc::c_ulong) -> Result<()> {
        let path = c_path(path)?;
        // SAFETY: the source and target are a NUL-terminated string that
        // outlives the call. A bind mount reads no type or data, so both
        // are null.
        if unsafe {
            libc::mount(
                path.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error())
                .context("cannot establish protected mount boundary");
        }
        Ok(())
    }
    /// Makes the topmost mount at `path` read-only; mounts below it keep their flags.
    pub fn remount_readonly(path: &Path) -> Result<()> {
        let path = c_path(path)?;
        // SAFETY: the target is a NUL-terminated string that outlives the
        // call. A remount reads no source, type or data, so they are null.
        if unsafe {
            libc::mount(
                std::ptr::null(),
                path.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND
                    | libc::MS_REMOUNT
                    | libc::MS_RDONLY
                    | libc::MS_NOSUID
                    | libc::MS_NODEV,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error())
                .context("cannot enforce read-only protected path");
        }
        Ok(())
    }
    /// Removes CAP_SYS_ADMIN from this process and from every program it executes.
    pub fn drop_sys_admin() -> Result<()> {
        /// struct __user_cap_header_struct.
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        /// struct __user_cap_data_struct.
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Data {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        /// _LINUX_CAPABILITY_VERSION_3, which uses two `Data` words.
        const VERSION_3: u32 = 0x2008_0522;
        let mut header = Header {
            version: VERSION_3,
            pid: 0,
        };
        let mut data = [Data {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        // SAFETY: Header and Data have the kernel's C layout. With version 3,
        // capget writes one header and exactly the two words of `data`.
        if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let keep = !(1 << CAP_SYS_ADMIN);
        data[0].effective &= keep;
        data[0].permitted &= keep;
        data[0].inheritable &= keep;
        // SAFETY: as for capget; capset only reads `header` and `data`.
        if unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        prctl(libc::PR_CAPBSET_DROP, CAP_SYS_ADMIN.into())
    }
    /// Leaves root for `uid` and `gid` with no capabilities, for good.
    pub fn drop_privileges(uid: u32, gid: u32) -> Result<()> {
        for capability in 0..=CAP_LAST_CAP {
            prctl(libc::PR_CAPBSET_DROP, capability.into())?;
        }
        // SAFETY: with a zero count, setgroups reads no list, so null is allowed.
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: setgid and setuid take only integers.
        if unsafe { libc::setgid(gid) != 0 || libc::setuid(uid) != 0 } {
            return Err(io::Error::last_os_error().into());
        }
        prctl(libc::PR_SET_NO_NEW_PRIVS, 1)
    }
    /// Calls an option that takes one value and requires zero for the rest.
    fn prctl(option: libc::c_int, value: libc::c_ulong) -> Result<()> {
        let zero: libc::c_ulong = 0;
        // SAFETY: prctl reads its variadic arguments as unsigned longs, the
        // type of every value passed. These options take no pointer.
        if unsafe { libc::prctl(option, value, zero, zero, zero) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}
