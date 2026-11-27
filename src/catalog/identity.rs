use std::path::{Component, Path, Prefix};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) enum Identity {
    Executable(String),
    LocalFile(u64, [u8; 16]),
    Packaged(String),
}

pub(super) fn packaged_id(id: &str) -> Option<String> {
    let (family, app) = id.split_once('!')?;
    let (package, publisher) = family.rsplit_once('_')?;
    let identifier = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    };
    (identifier(package)
        && publisher.len() == 13
        && publisher.bytes().all(|c| c.is_ascii_alphanumeric())
        && identifier(app))
    .then(|| id.to_ascii_lowercase())
}

pub(super) fn apps_folder_id(path: &str) -> Option<String> {
    let path = path.trim();
    let path = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
        .unwrap_or(path);
    let (prefix, id) = path.split_once(['\\', '/'])?;
    let is_apps_folder = prefix.eq_ignore_ascii_case("shell:AppsFolder")
        || prefix.eq_ignore_ascii_case("::{4234d49b-0245-4df3-b780-3893943456e1}");
    is_apps_folder.then_some(id).and_then(packaged_id)
}

pub(super) fn executable_path(path: &Path) -> Option<String> {
    if !path.is_absolute() || !path.extension()?.eq_ignore_ascii_case("exe") {
        return None;
    }
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return None;
    };
    let root = match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => format!("{}:", char::from(drive)),
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            format!(r"\\{}\{}", server.to_str()?, share.to_str()?)
        }
        Prefix::Verbatim(volume) if volume.to_str()?.get(..7)?.eq_ignore_ascii_case("volume{") => {
            format!(r"\\?\{}", volume.to_str()?)
        }
        _ => return None,
    };
    let mut parts = Vec::new();
    for component in components {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir | Component::RootDir => (),
            Component::ParentDir => {
                parts.pop();
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(format!(r"{}\{}", root, parts.join(r"\")).to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_absolute_windows_paths_without_opening_targets() {
        let key = |path| executable_path(Path::new(path));
        assert_eq!(
            key(r"C:\Папка\bin\..\Tool.EXE"),
            key(r"\\?\c:\папка\tool.exe")
        );
        assert_eq!(
            key(r"\\Server\Share\Tool.EXE"),
            key(r"\\?\UNC\server\share\tool.exe")
        );
        assert_ne!(key(r"C:\Папка\Tool.exe"), key(r"D:\Another\Tool.exe"));
        assert!(key(r"C:Tool.exe").is_none());
        assert!(key(r"\\.\Device\Tool.exe").is_none());
    }

    #[test]
    fn recognizes_explicit_packaged_aumids_only() {
        assert_eq!(
            apps_folder_id(r#""shell:AppsFolder\Demo_abcdefghijklm!Editor""#).as_deref(),
            Some("demo_abcdefghijklm!editor")
        );
        assert_ne!(
            apps_folder_id(r"shell:AppsFolder\Demo_abcdefghijklm!Editor"),
            apps_folder_id(r"shell:AppsFolder\Demo_abcdefghijklm!Viewer")
        );
        assert!(apps_folder_id(r"shell:AppsFolder\Microsoft.Windows.Explorer").is_none());
        assert!(apps_folder_id(r"--argument shell:AppsFolder\Demo_abcdefghijklm!Editor").is_none());
        assert!(apps_folder_id("https://example.com").is_none());
    }
}
