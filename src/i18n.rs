//! Selects the interface language and reads the embedded translation table.
//! Language discriminants index the translation columns. With no stored preference,
//! the first supported Windows UI language wins, with English as the fallback.

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::Globalization::{
    GetUserDefaultUILanguage, GetUserPreferredUILanguages, LCIDToLocaleName, MUI_LANGUAGE_NAME,
};

macro_rules! messages {
    ($($name:ident => [$($text:literal),+ $(,)?];)*) => {
        #[derive(Clone, Copy, Debug)]
        pub(crate) enum Msg { $($name,)* }
        static TRANSLATIONS: &[[&str; Language::ALL.len()]] = &[$([$($text),+]),*];
    };
}

include!("../locales/messages.rs");

// Column order in locales/messages.rs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Language {
    En,
    Ru,
    De,
    Fr,
    It,
    Es,
    #[serde(rename = "zh-Hans")]
    ZhHans,
    Ja,
    Pt,
    Ko,
    Pl,
    Tr,
}

impl Language {
    pub(crate) const ALL: [Self; 12] = [
        Self::En,
        Self::Ru,
        Self::De,
        Self::Fr,
        Self::It,
        Self::Es,
        Self::ZhHans,
        Self::Ja,
        Self::Pt,
        Self::Ko,
        Self::Pl,
        Self::Tr,
    ];

    pub(crate) const fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Ru => "Русский",
            Self::De => "Deutsch",
            Self::Fr => "Français",
            Self::It => "Italiano",
            Self::Es => "Español",
            Self::ZhHans => "简体中文",
            Self::Ja => "日本語",
            Self::Pt => "Português (Brasil)",
            Self::Ko => "한국어",
            Self::Pl => "Polski",
            Self::Tr => "Türkçe",
        }
    }

    pub(crate) const fn tag(self) -> &'static str {
        [
            "en", "ru", "de", "fr", "it", "es", "zh-Hans", "ja", "pt", "ko", "pl", "tr",
        ][self as usize]
    }

    fn from_tag(tag: &str) -> Option<Self> {
        let tag = tag.to_ascii_lowercase();
        let mut parts = tag.split('-');
        Some(match parts.next()? {
            "en" => Self::En,
            "ru" => Self::Ru,
            "de" => Self::De,
            "fr" => Self::Fr,
            "it" => Self::It,
            "es" => Self::Es,
            "ja" => Self::Ja,
            "pt" => Self::Pt,
            "ko" => Self::Ko,
            "pl" => Self::Pl,
            "tr" => Self::Tr,
            "zh" => match parts.next() {
                Some("hant" | "cht" | "tw" | "hk" | "mo") => return None,
                Some("hans" | "chs" | "cn" | "sg" | "my") | None => Self::ZhHans,
                _ => return None,
            },
            _ => return None,
        })
    }
}

fn preferred(buffer: &[u16]) -> Option<Language> {
    buffer
        .split(|&unit| unit == 0)
        .take_while(|name| !name.is_empty())
        .find_map(|name| Language::from_tag(&String::from_utf16_lossy(name)))
}

fn windows_language() -> Language {
    let (mut count, mut size) = (0, 0);
    if unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            std::ptr::null_mut(),
            &mut size,
        )
    } != 0
        && size > 0
    {
        let mut names = vec![0; size as usize];
        if unsafe {
            GetUserPreferredUILanguages(
                MUI_LANGUAGE_NAME,
                &mut count,
                names.as_mut_ptr(),
                &mut size,
            )
        } != 0
        {
            return preferred(&names).unwrap_or(Language::En);
        }
    }
    let mut name = [0; 85]; // LOCALE_NAME_MAX_LENGTH
    let id = unsafe { GetUserDefaultUILanguage() };
    let len = unsafe { LCIDToLocaleName(u32::from(id), name.as_mut_ptr(), name.len() as i32, 0) };
    if len > 0
        && let Some(language) = preferred(&name)
    {
        return language;
    }
    Language::En
}

static ACTIVE: OnceLock<AtomicUsize> = OnceLock::new();

pub(crate) fn set_language(language: Option<Language>) {
    // Re-read Windows when the user returns to automatic selection.
    let index = language.unwrap_or_else(windows_language) as usize;
    ACTIVE
        .get_or_init(|| AtomicUsize::new(index))
        .store(index, Ordering::Relaxed);
}

pub(crate) fn current() -> Language {
    #[cfg(test)]
    if let Some(language) = TEST_LANGUAGE.get() {
        return language;
    }
    let active = ACTIVE.get_or_init(|| AtomicUsize::new(windows_language() as usize));
    Language::ALL[active.load(Ordering::Relaxed)]
}

pub(crate) fn tr(message: Msg) -> &'static str {
    translation(message, current() as usize)
}

fn translation(message: Msg, language: usize) -> &'static str {
    TRANSLATIONS[message as usize][language]
}

#[derive(Clone, Copy)]
pub(crate) enum ControlLabel {
    OverlayCue,
    OverlayResults,
    ActionOpen,
    ActionElevated,
    ActionFolder,
    ActionCopy,
}

type WideCatalog = [OnceLock<Box<[u16]>>; ControlLabel::ActionCopy as usize + 1];
static WIDE: [WideCatalog; Language::ALL.len()] =
    [const { [const { OnceLock::new() }; ControlLabel::ActionCopy as usize + 1] };
        Language::ALL.len()];

// Some Win32 controls retain this pointer for their entire lifetime.
pub(crate) fn wide(label: ControlLabel) -> *const u16 {
    let message = match label {
        ControlLabel::OverlayCue => Msg::OverlayCue,
        ControlLabel::OverlayResults => Msg::OverlayResults,
        ControlLabel::ActionOpen => Msg::ActionOpen,
        ControlLabel::ActionElevated => Msg::ActionElevated,
        ControlLabel::ActionFolder => Msg::ActionFolder,
        ControlLabel::ActionCopy => Msg::ActionCopy,
    };
    let language = current() as usize;
    WIDE[language][label as usize]
        .get_or_init(|| {
            translation(message, language)
                .encode_utf16()
                .chain([0])
                .collect()
        })
        .as_ptr()
}

pub(crate) fn render(message: Msg, args: &[(&str, &str)]) -> String {
    let mut template = tr(message);
    let mut text = String::with_capacity(template.len());
    while let Some((literal, tail)) = template.split_once('{') {
        let (name, rest) = tail.split_once('}').expect("validated catalog placeholder");
        let value = args
            .iter()
            .find(|(key, _)| *key == name)
            .expect("missing translation argument")
            .1;
        text.push_str(literal);
        text.push_str(value);
        template = rest;
    }
    text.push_str(template);
    text
}

pub(crate) const WORKER_LANGUAGE: &str = "RUSTART_WORKER_LANGUAGE";

pub(crate) fn select_worker_language() {
    if let Some(language) = std::env::var(WORKER_LANGUAGE)
        .ok()
        .as_deref()
        .and_then(Language::from_tag)
    {
        set_language(Some(language));
    }
}

#[cfg(test)]
thread_local! { static TEST_LANGUAGE: std::cell::Cell<Option<Language>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(crate) fn with_language<T>(language: Language, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Language>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LANGUAGE.set(self.0);
        }
    }
    let _restore = Restore(TEST_LANGUAGE.replace(Some(language)));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_first_supported_windows_language() {
        let preferred_names = "zh-Hant\0nl-NL\0ja-JP\0en-US\0\0"
            .encode_utf16()
            .collect::<Vec<_>>();
        assert_eq!(preferred(&preferred_names), Some(Language::Ja));
        assert_eq!(Language::from_tag("zh-CN"), Some(Language::ZhHans));
        assert_eq!(Language::from_tag("zh-TW"), None);
        assert_eq!(Language::from_tag("pt-PT"), Some(Language::Pt));
        for lang in Language::ALL {
            assert_eq!(Language::from_tag(lang.tag()), Some(lang));
        }
    }

    #[test]
    fn translations_are_complete_and_keep_placeholders() {
        let placeholders = |text: &str| {
            let mut fields = text
                .split('{')
                .skip(1)
                .map(|rest| {
                    rest.split_once('}')
                        .expect("unclosed placeholder")
                        .0
                        .to_owned()
                })
                .collect::<Vec<_>>();
            fields.sort_unstable();
            fields
        };
        for row in TRANSLATIONS {
            let expected = placeholders(row[0]);
            for text in row {
                assert!(!text.trim().is_empty());
                assert!(!text.contains('\0'));
                assert_eq!(placeholders(text), expected, "{text}");
            }
        }
    }
}
