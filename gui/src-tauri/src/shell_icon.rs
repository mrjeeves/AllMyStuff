//! Windows Shell icon extraction for filesystem shortcuts.
//!
//! Asking the shell for the `.lnk` icon preserves target icons, custom shortcut
//! icons, and Windows' own fallback behavior instead of guessing from a suffix.

use std::{
    ffi::c_void,
    ffi::OsString,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use windows::{
    core::PCWSTR,
    Win32::{
        Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
            BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
        },
        Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
        UI::{
            Shell::{
                SHGetFileInfoW, SHGetStockIconInfo, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
                SHGFI_LINKOVERLAY, SHGSI_ICON, SHGSI_LARGEICON, SHSTOCKICONINFO, SIID_RECYCLER,
            },
            WindowsAndMessaging::{DestroyIcon, DrawIconEx, DI_NORMAL, HICON},
        },
    },
};

pub(crate) fn shell_compatible_path(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let verbatim = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    if !wide.starts_with(&verbatim) {
        return path.to_owned();
    }

    let unc = [b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];
    let unc_lower = [b'u' as u16, b'n' as u16, b'c' as u16, b'\\' as u16];
    let normalized = if wide
        .get(4..8)
        .is_some_and(|prefix| prefix == unc || prefix == unc_lower)
    {
        let mut value = vec![b'\\' as u16, b'\\' as u16];
        value.extend_from_slice(&wide[8..]);
        value
    } else {
        wide[4..].to_vec()
    };
    PathBuf::from(OsString::from_wide(&normalized))
}

const ICON_SIZE: i32 = 96;

pub(crate) fn shortcut_icon(path: &Path) -> Option<String> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

    // SHGetFileInfo requires COM on the calling thread. Directory pages run on
    // Tokio workers, so the UI thread's COM apartment does not cover them.
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let icon = shortcut_icon_initialized(path);
    if initialized {
        unsafe { CoUninitialize() };
    }
    icon
}

pub(crate) fn recycle_bin_icon() -> Option<String> {
    let mut info = SHSTOCKICONINFO::default();
    info.cbSize = std::mem::size_of::<SHSTOCKICONINFO>() as u32;
    unsafe {
        SHGetStockIconInfo(SIID_RECYCLER, SHGSI_ICON | SHGSI_LARGEICON, &mut info).ok()?;
    }
    if info.hIcon.0.is_null() {
        return None;
    }
    let rendered = render_icon(info.hIcon);
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    rendered
}

fn shortcut_icon_initialized(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt as _;

    let shell_path = shell_compatible_path(path);
    let mut wide: Vec<u16> = shell_path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut info = SHFILEINFOW::default();
    let found = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_LINKOVERLAY,
        )
    };
    if found == 0 || info.hIcon.0.is_null() {
        return None;
    }

    let rendered = render_icon(info.hIcon);
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    rendered
}

fn render_icon(icon: HICON) -> Option<String> {
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.0.is_null() {
        return None;
    }

    let mut bitmap_info = BITMAPINFO::default();
    bitmap_info.bmiHeader.biSize = std::mem::size_of_val(&bitmap_info.bmiHeader) as u32;
    bitmap_info.bmiHeader.biWidth = ICON_SIZE;
    bitmap_info.bmiHeader.biHeight = -ICON_SIZE;
    bitmap_info.bmiHeader.biPlanes = 1;
    bitmap_info.bmiHeader.biBitCount = 32;
    bitmap_info.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap =
        match unsafe { CreateDIBSection(None, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0) } {
            Ok(bitmap) if !bits.is_null() => bitmap,
            _ => {
                unsafe {
                    let _ = DeleteDC(dc);
                };
                return None;
            }
        };
    let old = unsafe { SelectObject(dc, bitmap.into()) };
    let byte_len = (ICON_SIZE * ICON_SIZE * 4) as usize;

    let rgba = (|| {
        unsafe { std::ptr::write_bytes(bits.cast::<u8>(), 0, byte_len) };
        unsafe { DrawIconEx(dc, 0, 0, icon, ICON_SIZE, ICON_SIZE, 0, None, DI_NORMAL) }.ok()?;
        let black = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), byte_len) }.to_vec();
        let has_alpha = black.chunks_exact(4).any(|pixel| pixel[3] != 0);
        let mut rgba = Vec::with_capacity(byte_len);

        if has_alpha {
            for pixel in black.chunks_exact(4) {
                let alpha = pixel[3];
                let unpremultiply = |channel: u8| -> u8 {
                    if alpha == 0 || alpha == 255 {
                        channel
                    } else {
                        ((u16::from(channel) * 255 / u16::from(alpha)).min(255)) as u8
                    }
                };
                rgba.extend_from_slice(&[
                    unpremultiply(pixel[2]),
                    unpremultiply(pixel[1]),
                    unpremultiply(pixel[0]),
                    alpha,
                ]);
            }
        } else {
            // Older icons carry transparency in a 1-bit mask instead of the
            // 32-bit alpha channel. Draw once more over white and recover the
            // mask from the difference, avoiding a black square around them.
            let surface = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), byte_len) };
            for pixel in surface.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[255, 255, 255, 255]);
            }
            unsafe { DrawIconEx(dc, 0, 0, icon, ICON_SIZE, ICON_SIZE, 0, None, DI_NORMAL) }.ok()?;
            let white = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), byte_len) };
            for (black_pixel, white_pixel) in black.chunks_exact(4).zip(white.chunks_exact(4)) {
                let background = (0..3)
                    .map(|channel| white_pixel[channel].saturating_sub(black_pixel[channel]))
                    .max()
                    .unwrap_or(255);
                let alpha = 255_u8.saturating_sub(background);
                let restore = |channel: u8| -> u8 {
                    if alpha == 0 {
                        0
                    } else {
                        ((u16::from(channel) * 255 / u16::from(alpha)).min(255)) as u8
                    }
                };
                rgba.extend_from_slice(&[
                    restore(black_pixel[2]),
                    restore(black_pixel[1]),
                    restore(black_pixel[0]),
                    alpha,
                ]);
            }
        }
        Some(rgba)
    })();

    if !old.is_invalid() {
        unsafe { SelectObject(dc, old) };
    }
    unsafe {
        let _ = DeleteDC(dc);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
    }

    let rgba = rgba?;
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, ICON_SIZE as u32, ICON_SIZE as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().ok()?.write_image_data(&rgba).ok()?;
    }
    Some(STANDARD.encode(png_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_paths_drop_only_windows_verbatim_syntax() {
        assert_eq!(
            shell_compatible_path(Path::new(r"\\?\C:\Users\Chris\Desktop\App.lnk")),
            PathBuf::from(r"C:\Users\Chris\Desktop\App.lnk")
        );
        assert_eq!(
            shell_compatible_path(Path::new(r"\\?\UNC\server\share\App.lnk")),
            PathBuf::from(r"\\server\share\App.lnk")
        );
        assert_eq!(
            shell_compatible_path(Path::new(r"C:\ordinary\App.lnk")),
            PathBuf::from(r"C:\ordinary\App.lnk")
        );
    }

    #[test]
    fn resolved_verbatim_shell_icon_is_a_png() {
        let path = std::env::var_os("ALLMYSTUFF_ICON_PROBE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_exe().expect("test executable path"))
            .canonicalize()
            .expect("canonical verbatim probe path");
        let encoded = shortcut_icon(&path)
            .unwrap_or_else(|| panic!("Windows Shell returned no icon for {}", path.display()));
        let decoded = STANDARD.decode(encoded).expect("base64 PNG");
        assert_eq!(decoded.get(..8), Some(b"\x89PNG\r\n\x1a\n".as_slice()),);
    }

    #[test]
    fn recycle_bin_stock_icon_is_a_png() {
        let encoded = recycle_bin_icon().expect("Windows Shell returned no Recycle Bin icon");
        let decoded = STANDARD.decode(encoded).expect("base64 PNG");
        assert_eq!(decoded.get(..8), Some(b"\x89PNG\r\n\x1a\n".as_slice()),);
    }
}
