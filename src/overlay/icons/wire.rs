use std::{
    io::{self, Read, Write},
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr::null_mut,
};

use windows_sys::Win32::{
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::{CreateIconIndirect, GetIconInfo, ICONINFO},
};

use super::{Icon, Key};
use crate::catalog::Source;

const MAX_SIDE: usize = 512;
const MAX_NAME: usize = 32_767;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid icon-worker packet")
}

fn tag(input: &mut impl Read) -> io::Result<Option<u8>> {
    let mut value = [0];
    match input.read(&mut value)? {
        0 => Ok(None),
        _ => Ok(Some(value[0])),
    }
}

pub(super) fn write_key(output: &mut impl Write, key: &Key) -> io::Result<()> {
    let (kind, name) = match &key.source {
        Source::Desktop(path) => (
            1,
            path.as_os_str()
                .encode_wide()
                .take(MAX_NAME + 1)
                .collect::<Vec<_>>(),
        ),
        Source::Packaged(id) => (2, id.encode_utf16().take(MAX_NAME + 1).collect::<Vec<_>>()),
    };
    if !(1..=MAX_SIDE as i32).contains(&key.px)
        || !(1..=MAX_NAME).contains(&name.len())
        || name.contains(&0)
    {
        return Err(invalid());
    }
    output.write_all(&[kind])?;
    output.write_all(&(key.px as u16).to_le_bytes())?;
    output.write_all(&(name.len() as u16).to_le_bytes())?;
    let name: Vec<_> = name.into_iter().flat_map(u16::to_le_bytes).collect();
    output.write_all(&name)?;
    output.flush()
}

pub(super) fn read_key(input: &mut impl Read) -> io::Result<Option<Key>> {
    let Some(kind) = tag(input)? else {
        return Ok(None);
    };
    let mut header = [0; 4];
    input.read_exact(&mut header)?;
    let px = u16::from_le_bytes([header[0], header[1]]);
    let length = u16::from_le_bytes([header[2], header[3]]) as usize;
    if !matches!(kind, 1 | 2)
        || !(1..=MAX_SIDE as u16).contains(&px)
        || !(1..=MAX_NAME).contains(&length)
    {
        return Err(invalid());
    }
    let mut bytes = vec![0; length * 2];
    input.read_exact(&mut bytes)?;
    let name: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect();
    if name.contains(&0) {
        return Err(invalid());
    }
    let source = if kind == 1 {
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&name));
        if !path.is_absolute() {
            return Err(invalid());
        }
        Source::Desktop(path)
    } else {
        let id = String::from_utf16(&name).map_err(|_| invalid())?;
        if id.contains(['\\', '/']) {
            return Err(invalid());
        }
        Source::Packaged(id)
    };
    Ok(Some(Key {
        source,
        px: i32::from(px),
    }))
}

#[derive(Debug, PartialEq, Eq)]
struct Pixels {
    width: usize,
    height: usize,
    bgra: Option<Vec<u8>>,
    mask: Vec<u8>,
}

impl Pixels {
    fn mask_height(height: usize, color: bool) -> usize {
        if color { height } else { height * 2 }
    }

    fn lengths(width: usize, height: usize, color: bool) -> io::Result<(usize, usize)> {
        if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) {
            return Err(invalid());
        }
        Ok((
            if color { width * height * 4 } else { 0 },
            width.div_ceil(32) * 4 * Self::mask_height(height, color),
        ))
    }

    fn capture(icon: &Icon) -> Option<Self> {
        let mut info = ICONINFO::default();
        if unsafe { GetIconInfo(icon.raw(), &mut info) } == 0 {
            return None;
        }
        let color = Bitmap(info.hbmColor);
        let mask = Bitmap(info.hbmMask);
        let (width, mask_height) = mask.dimensions()?;
        let has_color = !color.0.is_null();
        if (!has_color && mask_height % 2 != 0)
            || (has_color && color.dimensions()? != (width, mask_height))
        {
            return None;
        }
        let height = if has_color {
            mask_height
        } else {
            mask_height / 2
        };
        let (color_len, mask_len) = Self::lengths(width, height, has_color).ok()?;
        let dc = Dc::new()?;
        let bgra = if has_color {
            Some(dc.read(&color, width, height, 32, color_len)?)
        } else {
            None
        };
        Some(Self {
            width,
            height,
            bgra,
            mask: dc.read(&mask, width, mask_height, 1, mask_len)?,
        })
    }

    fn icon(&self) -> Option<Icon> {
        let color = match &self.bgra {
            Some(bgra) => Bitmap::from_bits(self.width, self.height, 32, bgra)?,
            None => Bitmap(null_mut()),
        };
        let mask = Bitmap::from_bits(
            self.width,
            Self::mask_height(self.height, self.bgra.is_some()),
            1,
            &self.mask,
        )?;
        let info = ICONINFO {
            fIcon: 1,
            hbmColor: color.0,
            hbmMask: mask.0,
            ..Default::default()
        };
        Icon::from_raw(unsafe { CreateIconIndirect(&info) })
    }

    fn write(&self, output: &mut impl Write) -> io::Result<()> {
        output.write_all(&[1, u8::from(self.bgra.is_some())])?;
        output.write_all(&(self.width as u16).to_le_bytes())?;
        output.write_all(&(self.height as u16).to_le_bytes())?;
        if let Some(color) = &self.bgra {
            output.write_all(color)?;
        }
        output.write_all(&self.mask)
    }

    fn read(input: &mut impl Read) -> io::Result<Option<Self>> {
        match tag(input)? {
            Some(0) => return Ok(None),
            Some(1) => (),
            _ => return Err(invalid()),
        }
        let mut header = [0; 5];
        input.read_exact(&mut header)?;
        let color = match header[0] {
            0 => false,
            1 => true,
            _ => return Err(invalid()),
        };
        let width = u16::from_le_bytes([header[1], header[2]]) as usize;
        let height = u16::from_le_bytes([header[3], header[4]]) as usize;
        let (color_len, mask_len) = Self::lengths(width, height, color)?;
        let bgra = if color {
            let mut pixels = vec![0; color_len];
            input.read_exact(&mut pixels)?;
            Some(pixels)
        } else {
            None
        };
        let mut mask = vec![0; mask_len];
        input.read_exact(&mut mask)?;
        Ok(Some(Self {
            width,
            height,
            bgra,
            mask,
        }))
    }
}

pub(super) fn write_icon(output: &mut impl Write, icon: Option<&Icon>) -> io::Result<()> {
    if let Some(pixels) = icon.and_then(Pixels::capture) {
        pixels.write(output)?;
    } else {
        output.write_all(&[0])?;
    }
    output.flush()
}

pub(super) fn read_icon(input: &mut impl Read) -> io::Result<Option<Icon>> {
    Ok(Pixels::read(input)?.and_then(|pixels| pixels.icon()))
}

#[cfg(test)]
pub(super) fn roundtrip_for_test(icon: &Icon) -> Icon {
    let original = Pixels::capture(icon).expect("capture Shell icon for wire test");
    let mut message = Vec::new();
    write_icon(&mut message, Some(icon)).unwrap();
    let restored = read_icon(&mut message.as_slice())
        .unwrap()
        .expect("restore Shell icon from wire");
    assert_eq!(Pixels::capture(&restored).unwrap(), original);
    restored
}

#[repr(C)]
struct DibInfo {
    header: BITMAPINFOHEADER,
    palette: [RGBQUAD; 2],
}

fn info(width: usize, height: usize, depth: u16) -> DibInfo {
    DibInfo {
        header: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: depth,
            biCompression: BI_RGB,
            ..Default::default()
        },
        palette: [
            RGBQUAD::default(),
            RGBQUAD {
                rgbRed: 255,
                rgbGreen: 255,
                rgbBlue: 255,
                rgbReserved: 0,
            },
        ],
    }
}

struct Bitmap(HBITMAP);

impl Bitmap {
    fn dimensions(&self) -> Option<(usize, usize)> {
        let mut bitmap = BITMAP::default();
        let read =
            unsafe { GetObjectW(self.0, size_of::<BITMAP>() as i32, (&raw mut bitmap).cast()) };
        if read == 0 || bitmap.bmWidth <= 0 || bitmap.bmHeight <= 0 {
            return None;
        }
        Some((bitmap.bmWidth as usize, bitmap.bmHeight as usize))
    }

    fn from_bits(width: usize, height: usize, depth: u16, bytes: &[u8]) -> Option<Self> {
        let info = info(width, height, depth);
        let mut bits = null_mut();
        let bitmap = Self(unsafe {
            CreateDIBSection(
                null_mut(),
                (&raw const info).cast(),
                DIB_RGB_COLORS,
                &mut bits,
                null_mut(),
                0,
            )
        });
        if bitmap.0.is_null() || bits.is_null() {
            return None;
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), bits.cast(), bytes.len()) };
        Some(bitmap)
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
    }
}

struct Dc(HDC);

impl Dc {
    fn new() -> Option<Self> {
        let dc = unsafe { CreateCompatibleDC(null_mut()) };
        if dc.is_null() { None } else { Some(Self(dc)) }
    }

    fn read(
        &self,
        bitmap: &Bitmap,
        width: usize,
        height: usize,
        depth: u16,
        length: usize,
    ) -> Option<Vec<u8>> {
        let mut info = info(width, height, depth);
        let mut bytes = vec![0; length];
        let lines = unsafe {
            GetDIBits(
                self.0,
                bitmap.0,
                0,
                height as u32,
                bytes.as_mut_ptr().cast(),
                (&raw mut info).cast(),
                DIB_RGB_COLORS,
            )
        };
        (lines == height as i32).then_some(bytes)
    }
}

impl Drop for Dc {
    fn drop(&mut self) {
        unsafe { DeleteDC(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_preserves_bgra_and_legacy_xor_and_masks() {
        let width = 8usize;
        let height = 2usize;
        let mut mask = vec![0, 0, 0, 0, 255, 0, 0, 0];
        for bgra in [
            None,
            Some(vec![[0, 0, 80, 128]; width * height].into_flattened()),
            Some(vec![[0, 0, 255, 0]; width * height].into_flattened()),
        ] {
            let mut packet = Pixels {
                width,
                height,
                bgra,
                mask: mask.clone(),
            };
            packet.mask.resize(
                Pixels::lengths(width, height, packet.bgra.is_some())
                    .unwrap()
                    .1,
                0,
            );
            let icon = packet.icon().expect("create a native icon");
            let original = Pixels::capture(&icon).expect("capture the native bits");
            let mut pipe = Vec::new();
            write_icon(&mut pipe, Some(&icon)).unwrap();
            let restored = read_icon(&mut pipe.as_slice())
                .unwrap()
                .expect("deserialize native icon");
            assert_eq!(Pixels::capture(&restored).unwrap(), original);
            mask.reverse();
        }
    }

    #[test]
    fn packet_rejects_oversized_dimensions_before_reading_pixels() {
        assert!(read_icon(&mut [1, 1, 255, 255, 1, 0].as_slice()).is_err());
    }
}
