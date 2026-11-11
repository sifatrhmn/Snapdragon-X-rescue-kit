//! Minimal Win32 bindings (kernel32, bcryptprimitives) and a handle wrapper.

use std::ffi::c_void;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
const OPEN_EXISTING: u32 = 3;
pub const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
pub const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
const INVALID_HANDLE: isize = -1;

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        n: *const u16,
        a: u32,
        s: u32,
        sa: *mut c_void,
        d: u32,
        f: u32,
        t: isize,
    ) -> isize;
    fn DeviceIoControl(
        h: isize,
        code: u32,
        inb: *const c_void,
        inl: u32,
        outb: *mut c_void,
        outl: u32,
        ret: *mut u32,
        ov: *mut c_void,
    ) -> i32;
    fn ReadFile(h: isize, b: *mut c_void, n: u32, r: *mut u32, ov: *mut c_void) -> i32;
    fn WriteFile(h: isize, b: *const c_void, n: u32, w: *mut u32, ov: *mut c_void) -> i32;
    fn SetFilePointerEx(h: isize, d: i64, p: *mut i64, m: u32) -> i32;
    fn FlushFileBuffers(h: isize) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn GetLastError() -> u32;
    fn FindFirstVolumeW(name: *mut u16, len: u32) -> isize;
    fn FindNextVolumeW(h: isize, name: *mut u16, len: u32) -> i32;
    fn FindVolumeClose(h: isize) -> i32;
    fn GetVolumePathNamesForVolumeNameW(v: *const u16, b: *mut u16, len: u32, ret: *mut u32)
        -> i32;
    fn SetVolumeMountPointW(mount: *const u16, vol: *const u16) -> i32;
    fn DeleteVolumeMountPointW(mount: *const u16) -> i32;
    fn GetLogicalDrives() -> u32;
    fn Sleep(ms: u32);
}

// No import library ships for bcryptprimitives.dll; std links it the same way.
#[link(name = "bcryptprimitives", kind = "raw-dylib")]
extern "system" {
    fn ProcessPrng(data: *mut u8, len: usize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn last_error() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetLastError() }
}

pub fn sleep_ms(ms: u32) {
    // SAFETY: no preconditions.
    unsafe { Sleep(ms) }
}

pub fn random16() -> [u8; 16] {
    let mut b = [0u8; 16];
    // SAFETY: `b` is valid for 16 bytes. ProcessPrng always succeeds.
    unsafe { ProcessPrng(b.as_mut_ptr(), 16) };
    b
}

/// Win32 error code with a short description.
pub fn describe(code: u32) -> String {
    let t = match code {
        2 => "file not found",
        5 => "access denied",
        19 => "media is write protected",
        21 => "device not ready",
        32 => "in use by another process",
        87 => "parameter incorrect",
        1117 => "I/O device error",
        _ => "",
    };
    if t.is_empty() {
        format!("Win32 error {code}")
    } else {
        format!("Win32 error {code} ({t})")
    }
}

/// An open device handle, closed on drop.
pub struct Device(isize);

impl Device {
    pub fn open(path: &str, access: u32, flags: u32) -> Result<Device, u32> {
        let w = wide(path);
        // SAFETY: `w` is NUL-terminated and outlives the call.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                FILE_SHARE_READ_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                flags,
                0,
            )
        };
        if h == INVALID_HANDLE {
            Err(last_error())
        } else {
            Ok(Device(h))
        }
    }

    /// IOCTL with no input whose output lands in `out`; returns the bytes returned.
    pub fn ioctl_out(&self, code: u32, input: &[u8], out: &mut [u8]) -> Result<usize, u32> {
        let mut ret = 0u32;
        // SAFETY: buffers are valid for their lengths.
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                code,
                if input.is_empty() {
                    std::ptr::null()
                } else {
                    input.as_ptr() as *const c_void
                },
                input.len() as u32,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(last_error())
        } else {
            Ok(ret as usize)
        }
    }

    /// IOCTL that only takes input (e.g. IOCTL_DISK_SET_DRIVE_LAYOUT_EX).
    pub fn ioctl_in(&self, code: u32, input: &[u8]) -> Result<(), u32> {
        let mut ret = 0u32;
        // SAFETY: `input` is valid for its length; no output buffer.
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                code,
                if input.is_empty() {
                    std::ptr::null()
                } else {
                    input.as_ptr() as *const c_void
                },
                input.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(last_error())
        } else {
            Ok(())
        }
    }

    fn seek(&self, offset: u64) -> Result<(), u32> {
        // SAFETY: plain FFI on an open handle.
        if unsafe { SetFilePointerEx(self.0, offset as i64, std::ptr::null_mut(), 0) } == 0 {
            Err(last_error())
        } else {
            Ok(())
        }
    }

    /// Reads exactly `buf.len()` bytes at `offset` (sector-aligned buffer and offset).
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), u32> {
        self.seek(offset)?;
        let mut n = 0u32;
        // SAFETY: `buf` is valid for its length.
        if unsafe {
            ReadFile(
                self.0,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_error());
        }
        if n as usize != buf.len() {
            return Err(1117);
        }
        Ok(())
    }

    /// Writes exactly `buf.len()` bytes at `offset` (sector-aligned buffer and offset).
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), u32> {
        self.seek(offset)?;
        let mut n = 0u32;
        // SAFETY: `buf` is valid for its length.
        if unsafe {
            WriteFile(
                self.0,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(last_error());
        }
        if n as usize != buf.len() {
            return Err(1117);
        }
        // SAFETY: plain FFI on an open handle.
        unsafe { FlushFileBuffers(self.0) };
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// `\\?\Volume{...}\` names of every volume on the system.
pub fn volumes() -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = vec![0u16; 512];
    // SAFETY: `buf` is valid for its length.
    let h = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if h == INVALID_HANDLE {
        return out;
    }
    loop {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        out.push(String::from_utf16_lossy(&buf[..end]));
        // SAFETY: as above.
        if unsafe { FindNextVolumeW(h, buf.as_mut_ptr(), buf.len() as u32) } == 0 {
            break;
        }
    }
    // SAFETY: `h` came from FindFirstVolumeW.
    unsafe { FindVolumeClose(h) };
    out
}

/// Mount points (e.g. `W:\`) of a `\\?\Volume{...}\` name.
pub fn mount_points(volume: &str) -> Vec<String> {
    let v = wide(volume);
    let mut buf = vec![0u16; 1024];
    let mut ret = 0u32;
    // SAFETY: buffers valid for their lengths.
    if unsafe {
        GetVolumePathNamesForVolumeNameW(v.as_ptr(), buf.as_mut_ptr(), buf.len() as u32, &mut ret)
    } == 0
    {
        return Vec::new();
    }
    buf[..ret as usize]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

pub fn set_mount_point(mount: &str, volume: &str) -> Result<(), u32> {
    let (m, v) = (wide(mount), wide(volume));
    // SAFETY: both strings are NUL-terminated.
    if unsafe { SetVolumeMountPointW(m.as_ptr(), v.as_ptr()) } == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

pub fn delete_mount_point(mount: &str) -> Result<(), u32> {
    let m = wide(mount);
    // SAFETY: NUL-terminated.
    if unsafe { DeleteVolumeMountPointW(m.as_ptr()) } == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

/// True when drive letter `l` (A–Z) is in use.
pub fn letter_in_use(l: char) -> bool {
    // SAFETY: no preconditions.
    let mask = unsafe { GetLogicalDrives() };
    let i = (l.to_ascii_uppercase() as u8).wrapping_sub(b'A');
    i < 26 && mask & (1 << i) != 0
}
