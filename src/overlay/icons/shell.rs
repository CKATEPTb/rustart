mod pixels;

use std::{cell::OnceCell, os::windows::ffi::OsStrExt, ptr::null_mut};

use windows::{
    Win32::{
        Foundation::SIZE,
        System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        },
        UI::Shell::{
            FOLDERID_AppsFolder, IShellItem, IShellItemImageFactory, KF_FLAG_DEFAULT,
            SHCreateItemFromParsingName, SHCreateItemFromRelativeName, SHGetIDListFromObject,
            SHGetKnownFolderItem, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
        },
    },
    core::{Interface, PCWSTR},
};
use windows_sys::Win32::{
    Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateCompatibleDC,
        CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW, HBITMAP,
    },
    UI::{
        Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_PIDL, SHGetFileInfoW},
        WindowsAndMessaging::{CopyImage, CreateIconIndirect, GetIconInfo, ICONINFO, IMAGE_ICON},
    },
};

use crate::catalog::Source;

use super::Icon;

pub(super) struct Extractor {
    apps_folder: OnceCell<Option<IShellItem>>,
}

impl Extractor {
    pub(super) fn new() -> Option<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
            .ok()
            .ok()?;
        Some(Self {
            apps_folder: OnceCell::new(),
        })
    }

    pub(super) fn get(&self, source: &Source, px: i32) -> Option<Icon> {
        let item = self.item(source)?;
        factory_icon(&item, px).or_else(|| file_info_icon(&item).map(|icon| resize(icon, px)))
    }

    fn item(&self, source: &Source) -> Option<IShellItem> {
        unsafe {
            match source {
                Source::Desktop(path) => {
                    let name: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
                    SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None).ok()
                }
                Source::Packaged(id) => {
                    let folder = self.apps_folder.get_or_init(|| {
                        SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None).ok()
                    });
                    if let Some(folder) = folder {
                        let name: Vec<_> = id.encode_utf16().chain([0]).collect();
                        if let Ok(item) =
                            SHCreateItemFromRelativeName(folder, PCWSTR(name.as_ptr()), None)
                        {
                            return Some(item);
                        }
                    }
                    let name: Vec<_> = "shell:AppsFolder\\"
                        .encode_utf16()
                        .chain(id.encode_utf16())
                        .chain([0])
                        .collect();
                    SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None).ok()
                }
            }
        }
    }
}

impl Drop for Extractor {
    fn drop(&mut self) {
        drop(self.apps_folder.take());
        unsafe { CoUninitialize() };
    }
}

struct Bitmap(HBITMAP);

impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
    }
}

fn factory_icon(item: &IShellItem, px: i32) -> Option<Icon> {
    let factory = item.cast::<IShellItemImageFactory>().ok()?;
    let source = unsafe {
        factory.GetImage(
            SIZE { cx: px, cy: px },
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        )
    }
    .ok()?;
    let color = Bitmap(source.0);
    sized_bitmap_icon(&color, px)
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn sized_bitmap_icon(color: &Bitmap, px: i32) -> Option<Icon> {
    let mut bitmap = BITMAP::default();
    if unsafe {
        GetObjectW(
            color.0,
            size_of::<BITMAP>() as i32,
            (&raw mut bitmap).cast(),
        )
    } == 0
        || bitmap.bmBitsPixel != 32
        || !(1..=512).contains(&bitmap.bmWidth)
        || !(1..=512).contains(&bitmap.bmHeight)
    {
        return None;
    }
    let width = bitmap.bmWidth as usize;
    let height = bitmap.bmHeight as usize;
    let mut image = vec![[0u8; 4]; width * height];
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return None;
        }
        let mut info = bitmap_info(bitmap.bmWidth, bitmap.bmHeight);
        let copied = GetDIBits(
            dc,
            color.0,
            0,
            height as u32,
            image.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        );
        DeleteDC(dc);
        if copied != bitmap.bmHeight || image.iter().all(|pixel| pixel[3] == 0) {
            return None;
        }
    }
    if bitmap.bmWidth == px && bitmap.bmHeight == px {
        return alpha_icon(color, px);
    }
    let resized = pixels::resize(&image, width, height, px as usize);
    let mut bits = null_mut();
    let output = Bitmap(unsafe {
        CreateDIBSection(
            null_mut(),
            &bitmap_info(px, px),
            DIB_RGB_COLORS,
            &mut bits,
            null_mut(),
            0,
        )
    });
    if output.0.is_null() || bits.is_null() {
        return None;
    }
    unsafe { std::ptr::copy_nonoverlapping(resized.as_ptr(), bits.cast(), resized.len()) };
    alpha_icon(&output, px)
}

fn alpha_icon(color: &Bitmap, px: i32) -> Option<Icon> {
    let stride = (px as usize).div_ceil(16) * 2;
    let zeros = vec![0u8; stride * px as usize];
    let mask = Bitmap(unsafe { CreateBitmap(px, px, 1, 1, zeros.as_ptr().cast()) });
    if mask.0.is_null() {
        return None;
    }
    let info = ICONINFO {
        fIcon: 1,
        hbmMask: mask.0,
        hbmColor: color.0,
        ..Default::default()
    };
    Icon::from_raw(unsafe { CreateIconIndirect(&info) })
}

fn resize(icon: Icon, px: i32) -> Icon {
    let mut info = ICONINFO::default();
    if unsafe { GetIconInfo(icon.raw(), &mut info) } != 0 {
        let color = Bitmap(info.hbmColor);
        let _mask = Bitmap(info.hbmMask);
        if let Some(quality) = sized_bitmap_icon(&color, px) {
            return quality;
        }
    }
    // Keep Win32's monochrome mask handling for legacy pre-ARGB icons.
    Icon::from_raw(unsafe { CopyImage(icon.raw(), IMAGE_ICON, px, px, 0) }).unwrap_or(icon)
}

fn file_info_icon(item: &IShellItem) -> Option<Icon> {
    let pidl = unsafe { SHGetIDListFromObject(item) }.ok()?;
    let mut info = SHFILEINFOW::default();
    unsafe {
        SHGetFileInfoW(
            pidl.cast(),
            0,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_PIDL | SHGFI_ICON | SHGFI_LARGEICON,
        );
        CoTaskMemFree(Some(pidl.cast()));
    }
    Icon::from_raw(info.hIcon)
}

#[cfg(test)]
mod tests {
    use super::super::wire::roundtrip_for_test;
    use super::*;
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, GdiFlush, SelectObject,
        },
        UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx, GetIconInfo},
    };

    fn assert_pixels(icon: &Icon, px: i32, transparent: bool) {
        let header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: px,
                biHeight: -px,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = null_mut();
        unsafe {
            let mut info = ICONINFO::default();
            assert_ne!(GetIconInfo(icon.raw(), &mut info), 0);
            let color = Bitmap(info.hbmColor);
            let _mask = Bitmap(info.hbmMask);
            let mut dimensions = BITMAP::default();
            assert_ne!(
                GetObjectW(
                    color.0,
                    size_of::<BITMAP>() as i32,
                    (&raw mut dimensions).cast()
                ),
                0
            );
            assert_eq!((dimensions.bmWidth, dimensions.bmHeight), (px, px));
            let image = Bitmap(CreateDIBSection(
                null_mut(),
                &header,
                DIB_RGB_COLORS,
                &mut bits,
                null_mut(),
                0,
            ));
            assert!(!image.0.is_null() && !bits.is_null());
            bits.cast::<u32>().write_bytes(0, (px * px) as usize);
            let dc = CreateCompatibleDC(null_mut());
            assert!(!dc.is_null());
            let old = SelectObject(dc, image.0);
            assert_ne!(
                DrawIconEx(dc, 0, 0, icon.raw(), px, px, 0, null_mut(), DI_NORMAL),
                0
            );
            GdiFlush();
            SelectObject(dc, old);
            DeleteDC(dc);
            let pixels = std::slice::from_raw_parts(bits.cast::<u32>(), (px * px) as usize);
            assert!(
                pixels.iter().any(|p| p >> 24 >= 127),
                "Shell image contains no opaque pixels"
            );
            assert!(
                pixels.iter().any(|p| {
                    let [b, g, r, a] = p.to_le_bytes();
                    a > 127
                        && [r, g, b].iter().max().unwrap() - [r, g, b].iter().min().unwrap() > 20
                }),
                "Shell image contains no usable color"
            );
            if transparent {
                assert!(
                    pixels.iter().any(|p| p >> 24 == 0),
                    "Shell image lost transparent pixels"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires the local Windows Explorer and installed package catalog"]
    fn reads_real_desktop_and_packaged_shell_pixels() {
        let extractor = Extractor::new().expect("Shell STA");
        let mut executable = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap());
        executable.push("explorer.exe");
        let desktop = Source::Desktop(executable);
        for px in [34, 43, 51] {
            let icon = extractor.get(&desktop, px).expect("Explorer icon");
            assert_pixels(&roundtrip_for_test(&icon), px, true);
        }
        let apps = crate::catalog::discover().expect("read-only catalog discovery");
        let desktop_link = apps
            .iter()
            .find_map(|app| {
                let Source::Desktop(path) = &app.source else {
                    return None;
                };
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
                    .then_some(&app.source)
            })
            .expect("an installed Start menu shortcut");
        for px in [34, 43, 51] {
            let icon = extractor
                .get(desktop_link, px)
                .expect("Start shortcut icon");
            assert_pixels(&roundtrip_for_test(&icon), px, true);
        }
        eprintln!("Win32 executable and Start shortcut contain colored alpha pixels.");

        let package = apps
            .iter()
            .find_map(|app| {
                let Source::Packaged(id) = &app.source else {
                    return None;
                };
                id.starts_with("Microsoft.WindowsStore_")
                    .then_some(&app.source)
            })
            .or_else(|| {
                apps.iter().find_map(|app| {
                    matches!(app.source, Source::Packaged(_)).then_some(&app.source)
                })
            })
            .expect("an installed packaged application");
        for px in [34, 43, 51] {
            let icon = extractor
                .get(package, px)
                .expect("packaged AppsFolder icon");
            assert_pixels(&roundtrip_for_test(&icon), px, false);
        }
        eprintln!("Actual packaged AppsFolder icon contains colored alpha pixels.");
    }
}
