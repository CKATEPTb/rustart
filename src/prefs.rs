use std::{
    fs,
    io::{self, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{SeqAccess, Visitor},
    ser::SerializeSeq,
};
use windows_sys::Win32::{
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    UI::Input::KeyboardAndMouse::MOD_WIN,
};

use crate::i18n::{Language, Msg, render, tr};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
    #[serde(default, skip_serializing_if = "KeySet::is_empty")]
    pub extra_keys: KeySet,
}

impl Hotkey {
    pub const fn new(modifiers: u32, vk: u32) -> Self {
        Self {
            modifiers,
            vk,
            extra_keys: KeySet([0; 4]),
        }
    }

    pub fn keys(self) -> impl Iterator<Item = u32> {
        std::iter::once(self.vk)
            .filter(|&vk| vk != 0)
            .chain(self.extra_keys.iter().filter(move |&vk| vk != self.vk))
    }

    pub fn uses_custom(self) -> bool {
        self.vk == 0 || !self.extra_keys.is_empty()
    }
}

/// A set of physical virtual keys. Small and Copy, independent of how a
/// recorder orders simultaneous presses and how preferences render the array.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeySet([u64; 4]);

impl KeySet {
    fn position(vk: u32) -> Option<(usize, u64)> {
        (1..=254)
            .contains(&vk)
            .then(|| ((vk / 64) as usize, 1 << (vk % 64)))
    }

    pub fn insert(&mut self, vk: u32) -> bool {
        let Some((word, bit)) = Self::position(vk) else {
            return false;
        };
        let previous = self.0[word];
        self.0[word] |= bit;
        previous != self.0[word]
    }

    pub fn remove(&mut self, vk: u32) -> bool {
        let Some((word, bit)) = Self::position(vk) else {
            return false;
        };
        let previous = self.0[word];
        self.0[word] &= !bit;
        previous != self.0[word]
    }

    pub fn contains(&self, vk: u32) -> bool {
        Self::position(vk).is_some_and(|(word, bit)| self.0[word] & bit != 0)
    }

    pub fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }

    pub fn iter(self) -> impl Iterator<Item = u32> {
        self.0
            .into_iter()
            .enumerate()
            .flat_map(|(index, mut word)| {
                std::iter::from_fn(move || {
                    if word == 0 {
                        return None;
                    }
                    let bit = word.trailing_zeros();
                    word &= word - 1;
                    Some(index as u32 * 64 + bit)
                })
            })
    }
}

impl Serialize for KeySet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let count = self.0.iter().map(|word| word.count_ones()).sum::<u32>() as usize;
        let mut seq = serializer.serialize_seq(Some(count))?;
        for vk in self.iter() {
            seq.serialize_element(&vk)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for KeySet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Keys;
        impl<'de> Visitor<'de> for Keys {
            type Value = KeySet;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an array of virtual keys in 1..=254")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<KeySet, A::Error> {
                let mut keys = KeySet::default();
                while let Some(vk) = seq.next_element::<u32>()? {
                    if KeySet::position(vk).is_none() {
                        return Err(serde::de::Error::invalid_value(
                            serde::de::Unexpected::Unsigned(u64::from(vk)),
                            &self,
                        ));
                    }
                    keys.insert(vk);
                }
                Ok(keys)
            }
        }
        deserializer.deserialize_seq(Keys)
    }
}

impl Default for Hotkey {
    fn default() -> Self {
        Self::new(MOD_WIN, b'R'.into())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Prefs {
    pub hotkey: Hotkey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
}

fn path() -> Result<PathBuf, String> {
    let root = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| tr(Msg::SettingsDirectoryMissing).to_owned())?;
    Ok(PathBuf::from(root).join("Rustart").join("settings.json"))
}

pub fn load() -> Result<Prefs, String> {
    load_from(&path()?)
}

fn load_from(path: &Path) -> Result<Prefs, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            render(
                Msg::SettingsReadFailed,
                &[
                    ("path", &path.to_string_lossy()),
                    ("error", &error.to_string()),
                ],
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Prefs::default()),
        Err(error) => Err(render(
            Msg::SettingsOpenFailed,
            &[("error", &format!("{}: {error}", path.display()))],
        )),
    }
}

pub fn save(prefs: &Prefs) -> Result<(), String> {
    save_to(&path()?, prefs)
}

fn create_scratch(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    loop {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let scratch = path.with_extension(format!("{pid}.{time}.{sequence}.tmp"));
        match fs::File::create_new(&scratch) {
            Ok(file) => return Ok((scratch, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn save_to(path: &Path, prefs: &Prefs) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(prefs).map_err(|error| error.to_string())?;
    let result = (|| -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let (scratch, mut file) = create_scratch(path)?;
        let result = file.write_all(&bytes).and_then(|_| file.sync_all());
        drop(file);
        let result = result.and_then(|_| replace(&scratch, path));
        if result.is_err() {
            let _ = fs::remove_file(scratch);
        }
        result
    })();
    result.map_err(|error| {
        render(
            Msg::SettingsSaveFailed,
            &[
                ("path", &path.to_string_lossy()),
                ("error", &error.to_string()),
            ],
        )
    })
}

fn replace(scratch: &Path, destination: &Path) -> io::Result<()> {
    let utf16 = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>()
    };
    let from = utf16(scratch);
    let to = utf16(destination);
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_json_preserves_legacy_shape_and_round_trips_complex_chords() {
        let old = r#"{"modifiers":8,"vk":82}"#;
        let parsed: Hotkey = serde_json::from_str(old).unwrap();
        assert_eq!(parsed, Hotkey::default());
        assert_eq!(serde_json::to_string(&parsed).unwrap(), old);
        assert!(!parsed.uses_custom());

        let json = r#"{"modifiers":3,"vk":65,"extra_keys":[64,128,192,254]}"#;
        let chord: Hotkey = serde_json::from_str(json).unwrap();
        assert!(chord.uses_custom());
        assert_eq!(chord.keys().collect::<Vec<_>>(), [65, 64, 128, 192, 254]);
        assert!(chord.extra_keys.contains(254));
        assert_eq!(serde_json::to_string(&chord).unwrap(), json);

        let modifiers_only: Hotkey = serde_json::from_str(r#"{"modifiers":6,"vk":0}"#).unwrap();
        assert!(modifiers_only.uses_custom());
        assert_eq!(modifiers_only.keys().next(), None);
    }

    #[test]
    fn settings_roundtrip_and_replace_in_isolated_directory() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("Rustart.Settings.{}.{stamp}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");

        assert_eq!(load_from(&path).unwrap().hotkey, Hotkey::default());
        fs::write(
            &path,
            r#"{"hotkey":{"modifiers":3,"vk":75},"language":"zh-Hans","exclusions":["Блокнот"]}"#,
        )
        .unwrap();
        let actual = load_from(&path).unwrap();
        assert_eq!(actual.hotkey, Hotkey::new(3, b'K'.into()));
        assert_eq!(actual.language, Some(Language::ZhHans));
        save_to(&path, &actual).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({"hotkey":{"modifiers":3,"vk":75},"language":"zh-Hans"})
        );
        save_to(&path, &Prefs::default()).unwrap();
        let actual = load_from(&path).unwrap();
        assert_eq!(actual.hotkey, Hotkey::default());
        assert!(actual.language.is_none());
        assert_eq!(directory.read_dir().unwrap().count(), 1);
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
