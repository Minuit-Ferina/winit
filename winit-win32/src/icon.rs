use std::ffi::c_void;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::{fmt, io, mem, ptr};

use cursor_icon::CursorIcon;
use dpi::PhysicalSize;
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleBitmap, DeleteObject, GetDC, ReleaseDC, SetBitmapBits,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIcon, CreateIconIndirect, DestroyCursor, DestroyIcon, GetSystemMetrics, HCURSOR, HICON,
    ICON_BIG, ICON_SMALL, ICONINFO, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE, LoadCursorW,
    LoadImageW, SM_CXCURSOR, SM_CYCURSOR,
};
use windows_sys::core::PCWSTR;
use winit_core::cursor::{CursorImage, CustomCursorProvider, CustomCursorSource, MAX_CURSOR_SIZE};
use winit_core::error::CustomCursorError;
use winit_core::icon::*;

use super::util;
use crate::WinIcon;

pub(crate) const PIXEL_SIZE: usize = mem::size_of::<u32>();

unsafe impl Send for WinIcon {}

impl WinIcon {
    pub(crate) fn from_path_impl<P: AsRef<Path>>(
        path: P,
        size: Option<PhysicalSize<u32>>,
    ) -> Result<Self, BadIcon> {
        // width / height of 0 along with LR_DEFAULTSIZE tells windows to load the default icon size
        let (width, height) = size.map(Into::into).unwrap_or((0, 0));

        let wide_path = util::encode_wide(path.as_ref());

        let handle = unsafe {
            LoadImageW(
                ptr::null_mut(),
                wide_path.as_ptr(),
                IMAGE_ICON,
                width,
                height,
                LR_DEFAULTSIZE | LR_LOADFROMFILE,
            )
        };
        if !handle.is_null() {
            Ok(WinIcon::from_handle(handle as HICON))
        } else {
            Err(BadIcon::OsError(io::Error::last_os_error()))
        }
    }

    pub(crate) fn from_resource_impl(
        resource_id: u16,
        size: Option<PhysicalSize<u32>>,
    ) -> Result<Self, BadIcon> {
        Self::from_resource_ptr(resource_id as PCWSTR, size)
    }

    pub(crate) fn from_resource_name_impl(
        resource_name: &str,
        size: Option<PhysicalSize<u32>>,
    ) -> Result<Self, BadIcon> {
        let wide_name = util::encode_wide(resource_name);
        Self::from_resource_ptr(wide_name.as_ptr(), size)
    }

    fn from_resource_ptr(
        resource: PCWSTR,
        size: Option<PhysicalSize<u32>>,
    ) -> Result<Self, BadIcon> {
        // width / height of 0 along with LR_DEFAULTSIZE tells windows to load the default icon size
        let (width, height) = size.map(Into::into).unwrap_or((0, 0));
        let handle = unsafe {
            LoadImageW(
                util::get_instance_handle(),
                resource,
                IMAGE_ICON,
                width,
                height,
                LR_DEFAULTSIZE,
            )
        };
        if !handle.is_null() {
            Ok(WinIcon::from_handle(handle as HICON))
        } else {
            Err(BadIcon::OsError(io::Error::last_os_error()))
        }
    }

    pub(crate) fn as_raw_handle(&self) -> HICON {
        self.inner.handle
    }

    pub(crate) fn from_rgba(rgba: &RgbaIcon) -> Result<Self, BadIcon> {
        let width = rgba.width() as usize;
        // The AND mask is a monochrome bitmap: one bit per pixel, set where the pixel is
        // transparent, with each row padded to a WORD boundary.
        let mask_stride = width.div_ceil(16) * 2;
        let mut and_mask = vec![0u8; mask_stride * rgba.height() as usize];
        let mut bgra = Vec::with_capacity(rgba.buffer().len());
        for (i, pixel) in rgba.buffer().chunks_exact(PIXEL_SIZE).enumerate() {
            if pixel[3] == 0 {
                let (x, y) = (i % width, i / width);
                and_mask[y * mask_stride + x / 8] |= 0x80 >> (x % 8);
            }
            bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        let handle = unsafe {
            CreateIcon(
                ptr::null_mut(),
                rgba.width() as i32,
                rgba.height() as i32,
                1,
                (PIXEL_SIZE * 8) as u8,
                and_mask.as_ptr(),
                bgra.as_ptr(),
            )
        };
        if !handle.is_null() {
            Ok(WinIcon::from_handle(handle))
        } else {
            Err(BadIcon::OsError(io::Error::last_os_error()))
        }
    }

    fn from_handle(handle: HICON) -> Self {
        Self { inner: Arc::new(RaiiIcon { handle }) }
    }
}

impl IconProvider for WinIcon {}

impl fmt::Debug for WinIcon {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        (*self.inner).fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum IconType {
    Small = ICON_SMALL as isize,
    Big = ICON_BIG as isize,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct RaiiIcon {
    handle: HICON,
}

unsafe impl Send for RaiiIcon {}
unsafe impl Sync for RaiiIcon {}

impl Drop for RaiiIcon {
    fn drop(&mut self) {
        unsafe { DestroyIcon(self.handle) };
    }
}

pub(crate) fn load_named_cursor(icon: CursorIcon) -> HCURSOR {
    // Win32 doesn't assign a standard resource ID to the copy cursor. Keep
    // our equivalent alive for every window using it, including while dragging.
    if icon == CursorIcon::Copy {
        static COPY_CURSOR: OnceLock<Option<WinCursor>> = OnceLock::new();
        if let Some(cursor) = COPY_CURSOR.get_or_init(create_copy_cursor) {
            return cursor.0.as_raw_handle();
        }
    }
    // SAFETY: The resource identifier selects a shared stock cursor.
    unsafe { LoadCursorW(ptr::null_mut(), util::to_windows_cursor(icon)) }
}

fn create_copy_cursor() -> Option<WinCursor> {
    // SAFETY: These metrics describe the system's cursor bitmap dimensions.
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXCURSOR), GetSystemMetrics(SM_CYCURSOR)) };
    let size = |metric: i32| {
        u16::try_from(metric).ok().filter(|size| (1..=MAX_CURSOR_SIZE).contains(size)).unwrap_or(32)
    };
    let (width, height) = (size(width), size(height));
    let CustomCursorSource::Image(image) =
        CustomCursorSource::from_rgba(copy_cursor_rgba(width, height), width, height, 0, 0).ok()?
    else {
        return None;
    };
    WinCursor::new(&image).ok()
}

// A tip-anchored arrow and a high-contrast copy badge on a 32-pixel grid.
fn copy_cursor_rgba(width: u16, height: u16) -> Vec<u8> {
    const ARROW: &[&[u8]] = &[
        b"#",
        b"##",
        b"#.#",
        b"#..#",
        b"#...#",
        b"#....#",
        b"#.....#",
        b"#......#",
        b"#.......#",
        b"#........#",
        b"#.........#",
        b"#..........#",
        b"#......######",
        b"#...#..#",
        b"#..##..#",
        b"#.#  #..#",
        b"##   #..#",
        b"#     #..#",
        b"      #..#",
        b"       ##",
    ];
    const BADGE: &[&[u8]] = &[
        b"###########",
        b"#.........#",
        b"#....#....#",
        b"#....#....#",
        b"#....#....#",
        b"#.#######.#",
        b"#....#....#",
        b"#....#....#",
        b"#....#....#",
        b"#.........#",
        b"###########",
    ];
    const GRID: usize = 32;
    const BADGE_X: usize = 13;
    const BADGE_Y: usize = 13;

    let (width, height) = (usize::from(width), usize::from(height));
    let mut rgba = Vec::with_capacity(width * height * PIXEL_SIZE);
    for y in 0..height {
        let row = y * GRID / height;
        for x in 0..width {
            let col = x * GRID / width;
            let badge = row
                .checked_sub(BADGE_Y)
                .and_then(|row| BADGE.get(row))
                .and_then(|row| row.get(col.checked_sub(BADGE_X)?));
            let arrow = ARROW.get(row).and_then(|row| row.get(col));
            let pixel = match badge.or(arrow) {
                Some(b'#') => [0, 0, 0, 255],
                Some(b'.') => [255, 255, 255, 255],
                _ => [0; 4],
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    rgba
}

#[derive(Debug, Clone)]
pub enum SelectedCursor {
    Named(CursorIcon),
    Custom(Arc<RaiiCursor>),
}

impl Default for SelectedCursor {
    fn default() -> Self {
        Self::Named(Default::default())
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct WinCursor(pub(super) Arc<RaiiCursor>);

impl CustomCursorProvider for WinCursor {
    fn is_animated(&self) -> bool {
        false
    }
}

impl WinCursor {
    pub(crate) fn new(image: &CursorImage) -> Result<Self, CustomCursorError> {
        let mut bgra = Vec::from(image.buffer());
        bgra.chunks_exact_mut(4).for_each(|chunk| chunk.swap(0, 2));

        let w = image.width() as i32;
        let h = image.height() as i32;

        unsafe {
            let hdc_screen = GetDC(ptr::null_mut());
            if hdc_screen.is_null() {
                return Err(os_error!(io::Error::last_os_error()).into());
            }
            let hbm_color = CreateCompatibleBitmap(hdc_screen, w, h);
            ReleaseDC(ptr::null_mut(), hdc_screen);
            if hbm_color.is_null() {
                return Err(os_error!(io::Error::last_os_error()).into());
            }
            if SetBitmapBits(hbm_color, bgra.len() as u32, bgra.as_ptr() as *const c_void) == 0 {
                DeleteObject(hbm_color);
                return Err(os_error!(io::Error::last_os_error()).into());
            };

            // Mask created according to https://learn.microsoft.com/en-us/windows/win32/api/wingdi/nf-wingdi-createbitmap#parameters
            let mask_bits: Vec<u8> = vec![0xff; ((((w + 15) >> 4) << 1) * h) as usize];
            let hbm_mask = CreateBitmap(w, h, 1, 1, mask_bits.as_ptr() as *const _);
            if hbm_mask.is_null() {
                DeleteObject(hbm_color);
                return Err(os_error!(io::Error::last_os_error()).into());
            }

            let icon_info = ICONINFO {
                fIcon: 0,
                xHotspot: image.hotspot_x() as u32,
                yHotspot: image.hotspot_y() as u32,
                hbmMask: hbm_mask,
                hbmColor: hbm_color,
            };

            let handle = CreateIconIndirect(&icon_info as *const _);
            DeleteObject(hbm_color);
            DeleteObject(hbm_mask);
            if handle.is_null() {
                return Err(os_error!(io::Error::last_os_error()).into());
            }

            Ok(Self(Arc::new(RaiiCursor { handle })))
        }
    }
}

#[derive(Debug, Hash, Eq, PartialEq)]
pub struct RaiiCursor {
    handle: HCURSOR,
}

unsafe impl Send for RaiiCursor {}
unsafe impl Sync for RaiiCursor {}

impl Drop for RaiiCursor {
    fn drop(&mut self) {
        unsafe { DestroyCursor(self.handle) };
    }
}

impl RaiiCursor {
    pub fn as_raw_handle(&self) -> HICON {
        self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_pointer_badge_remains_visible_when_scaled() {
        for scale in [1usize, 2] {
            let side = 32 * scale;
            let rgba = copy_cursor_rgba(side as u16, side as u16);
            let pixel = |x: usize, y: usize| {
                let index = (y * side + x) * scale * PIXEL_SIZE;
                &rgba[index..index + PIXEL_SIZE]
            };
            assert_eq!(rgba.len(), side * side * PIXEL_SIZE);
            assert_eq!(pixel(0, 0), [0, 0, 0, 255]);
            assert_eq!(pixel(18, 18), [0, 0, 0, 255]);
            assert_eq!(pixel(16, 15), [255, 255, 255, 255]);
            assert_eq!(pixel(31, 31), [0, 0, 0, 0]);
        }
    }

    #[test]
    fn named_copy_cursor_reuses_its_native_handle() {
        let copy = load_named_cursor(CursorIcon::Copy);
        assert!(!copy.is_null());
        assert_ne!(copy, load_named_cursor(CursorIcon::Default));
        assert_ne!(copy, load_named_cursor(CursorIcon::NoDrop));
        assert_eq!(copy, load_named_cursor(CursorIcon::Copy));
    }
}
