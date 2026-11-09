//! the linux framebuffer device: open `/dev/fb0`, read its geometry with two
//! ioctls, map the pixels shared read-write. the same calls the kernel's
//! `fbdev` docs describe — a direct libc binding instead of a crate.

use std::fs::{File, OpenOptions};
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::unix::io::AsRawFd;
use std::path::Path;

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;

/// `struct fb_bitfield` (linux/fb.h).
#[repr(C)]
#[derive(Clone, Debug, Default)]
pub struct Bitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

/// `struct fb_var_screeninfo` (linux/fb.h).
#[repr(C)]
#[derive(Clone, Debug, Default)]
pub struct VarScreeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: Bitfield,
    pub green: Bitfield,
    pub blue: Bitfield,
    pub transp: Bitfield,
    pub nonstd: u32,
    pub activate: u32,
    pub height: u32,
    pub width: u32,
    pub accel_flags: u32,
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

/// `struct fb_fix_screeninfo` (linux/fb.h).
#[repr(C)]
#[derive(Clone, Debug, Default)]
pub struct FixScreeninfo {
    pub id: [u8; 16],
    pub smem_start: usize,
    pub smem_len: u32,
    pub fb_type: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: usize,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

/// the mapped pixels. unmapped on drop.
pub struct Map {
    ptr: *mut u8,
    len: usize,
}

// the mapping is plain memory owned by this value alone
unsafe impl Send for Map {}

impl Deref for Map {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr/len came from a successful mmap that lives until drop
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for Map {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and &mut self makes this the only live slice
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the exact region mmap returned
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

pub struct Framebuffer {
    _device: File,
    pub frame: Map,
    pub var_screen_info: VarScreeninfo,
    pub fix_screen_info: FixScreeninfo,
}

fn ioctl_get<T: Default>(dev: &File, req: libc::c_ulong) -> io::Result<T> {
    let mut out = T::default();
    // SAFETY: `out` is a repr(C) mirror of the struct the request fills, owned here
    let r = unsafe { libc::ioctl(dev.as_raw_fd(), req as _, &mut out as *mut T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(out)
    }
}

impl Framebuffer {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Framebuffer> {
        let device = OpenOptions::new().read(true).write(true).open(path)?;
        let var_screen_info: VarScreeninfo = ioctl_get(&device, FBIOGET_VSCREENINFO)?;
        let fix_screen_info: FixScreeninfo = ioctl_get(&device, FBIOGET_FSCREENINFO)?;
        let len = fix_screen_info.line_length as usize * var_screen_info.yres_virtual as usize;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty framebuffer",
            ));
        }
        // SAFETY: a fresh shared mapping of the device, checked for MAP_FAILED below
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                device.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Framebuffer {
            _device: device,
            frame: Map {
                ptr: ptr.cast(),
                len,
            },
            var_screen_info,
            fix_screen_info,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // the kernel ABI: sizes of linux/fb.h's structs on 64-bit linux
    #[test]
    fn structs_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<VarScreeninfo>(), 160);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<FixScreeninfo>(), 80);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(std::mem::size_of::<FixScreeninfo>(), 68);
    }

    #[test]
    fn a_missing_device_is_an_error_not_a_panic() {
        assert!(Framebuffer::new("/dev/definitely-not-a-fb").is_err());
        // a regular file answers neither ioctl
        assert!(Framebuffer::new("/dev/null").is_err());
    }

    // a real device when this machine has one we may open; otherwise nothing to check
    #[test]
    fn a_real_framebuffer_maps_to_its_geometry() {
        let Ok(fb) = Framebuffer::new("/dev/fb0") else {
            return;
        };
        let (v, f) = (&fb.var_screen_info, &fb.fix_screen_info);
        assert!(v.xres > 0 && v.yres > 0 && v.bits_per_pixel >= 8);
        assert_eq!(
            fb.frame.len(),
            f.line_length as usize * v.yres_virtual as usize
        );
        assert!(f.line_length >= v.xres * v.bits_per_pixel / 8);
        let _ = fb.frame[0]; // the mapping is readable
    }
}
