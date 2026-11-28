mod identity;
mod native;
mod wire;

use identity::Identity;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf};

pub(crate) use native::current_user_sid;
pub use native::{discover, launch, reveal, standard_launch_worker};
pub use wire::{discover_isolated, write_discovery};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Source {
    Desktop(#[serde(with = "wire::windows_path")] PathBuf),
    Packaged(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct App {
    pub name: String,
    pub detail: String,
    pub source: Source,
    search_name: String,
    search_context: String,
    source_sort_key: String,
    identity: Option<Identity>,
    aumid: Option<String>,
    executable_display: Option<String>,
    common_shortcut: bool,
    #[serde(default)]
    startup_shortcut: bool,
}

impl App {
    fn new(name: String, context: Option<String>, source: Source) -> Self {
        let search_name = name.to_lowercase();
        let search_context = context.as_deref().unwrap_or_default().to_lowercase();
        let source_sort_key = match &source {
            Source::Desktop(path) => path.to_string_lossy().to_lowercase(),
            Source::Packaged(id) => id.to_lowercase(),
        };
        let detail = context.unwrap_or_else(|| match &source {
            Source::Desktop(_) => "Меню «Пуск»".into(),
            Source::Packaged(_) => "Приложение Windows".into(),
        });
        let aumid = match &source {
            Source::Packaged(id) => Some(id.to_ascii_lowercase()),
            Source::Desktop(_) => None,
        };
        let identity = aumid.clone().map(Identity::Packaged);
        Self {
            name,
            detail,
            source,
            search_name,
            search_context,
            source_sort_key,
            identity,
            aumid,
            executable_display: None,
            common_shortcut: false,
            startup_shortcut: false,
        }
    }
}

fn describe_name_collisions(apps: &mut [App]) {
    for group in apps.chunk_by_mut(|left, right| left.search_name == right.search_name) {
        let first = &group[0];
        let distinct = group
            .iter()
            .skip(1)
            .any(|app| match (&first.identity, &app.identity) {
                (Some(left), Some(right)) => left != right,
                _ => first.source != app.source,
            });
        if distinct {
            for app in group {
                app.detail = app
                    .executable_display
                    .clone()
                    .unwrap_or_else(|| match &app.source {
                        Source::Desktop(path) => path.to_string_lossy().into_owned(),
                        Source::Packaged(id) => id.clone(),
                    });
            }
        }
    }
}

pub fn search(apps: &[App], query: &str) -> Vec<usize> {
    let query = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }

    let mut matches: Vec<_> = apps
        .iter()
        .enumerate()
        .filter_map(|(index, app)| {
            let name = &app.search_name;
            let context = &app.search_context;
            let score = rank(name, &query).or_else(|| {
                (query.chars().count() > 1 && context.contains(&query)).then_some((7, 0))
            })?;
            Some((
                score,
                name,
                app.startup_shortcut,
                app.common_shortcut,
                &app.source_sort_key,
                index,
            ))
        })
        .collect();

    matches.sort_unstable();
    let mut seen = HashSet::new();
    matches
        .into_iter()
        .filter_map(|(_, _, _, _, _, index)| {
            apps[index]
                .identity
                .as_ref()
                .is_none_or(|identity| seen.insert(identity))
                .then_some(index)
        })
        .collect()
}

fn rank(name: &str, query: &str) -> Option<(u8, usize)> {
    if name == query {
        return Some((0, 0));
    }
    if name.starts_with(query) {
        return Some((1, name.chars().count()));
    }

    let words = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty());
    if words.clone().any(|word| word.starts_with(query)) {
        return Some((2, name.chars().count()));
    }
    if let Some(start) = name.find(query) {
        return Some((3, name[..start].chars().count()));
    }
    if query.chars().count() > 1
        && words
            .clone()
            .filter_map(|word| word.chars().next())
            .eq(query.chars())
    {
        return Some((4, name.chars().count()));
    }
    if query.contains(' ') {
        return query
            .split_whitespace()
            .all(|term| words.clone().any(|word| word.starts_with(term)))
            .then_some((5, name.chars().count()));
    }
    if query.chars().count() < 2 {
        return None;
    }

    let mut wanted = query.chars();
    let mut next = wanted.next();
    for (index, c) in name.chars().enumerate() {
        if next == Some(c) {
            next = wanted.next();
            if next.is_none() {
                return Some((6, index));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> App {
        App::new(
            name.into(),
            None,
            Source::Desktop(PathBuf::from(format!(r"C:\AppData\Programs\{name}.lnk"))),
        )
    }

    #[test]
    fn ranks_name_matches_before_details() {
        let apps = [
            app("My PowerShell"),
            app("PowerShell ISE"),
            app("Windows PowerShell"),
            app("PowerShell"),
        ];
        assert_eq!(search(&apps, "  POWERSHELL  "), [3, 1, 0, 2]);
        assert_eq!(search(&apps, "win pow"), [2]);
        assert!(search(&apps, "   ").is_empty());
    }

    #[test]
    fn supports_unicode_search_and_acronyms() {
        let apps = [
            app("Службы"),
            app("Веб-браузер"),
            app("Visual Studio Code"),
            app("Службы Windows"),
        ];
        assert_eq!(search(&apps, "СЛУЖ"), [0, 3]);
        assert_eq!(search(&apps, "вб"), [1]);
        assert_eq!(search(&apps, "vsc"), [2]);
        assert!(search(&apps, "студия").is_empty());
    }

    #[test]
    fn keeps_matches_beyond_the_visible_rows() {
        let apps: Vec<_> = (0..80)
            .map(|index| app(&format!("Tool {index:02}")))
            .collect();
        let result = search(&apps, "tool");
        assert_eq!(result, (0..80).collect::<Vec<_>>());
    }

    #[test]
    fn matches_menu_context_while_ignoring_shortcut_paths_and_app_ids() {
        let apps = [
            app("Alpha"),
            App::new(
                "Beta".into(),
                Some("Utilities".into()),
                Source::Desktop(PathBuf::from(r"C:\AppData\Programs\Beta.lnk")),
            ),
            App::new(
                "Gamma".into(),
                None,
                Source::Packaged("Publisher.Gamma_abcdef0123456!App".into()),
            ),
        ];
        assert!(search(&apps, "d").is_empty());
        assert!(search(&apps, "appdata").is_empty());
        assert!(search(&apps, "publisher").is_empty());
        assert_eq!(search(&apps, "utilities"), [1]);
    }

    #[test]
    fn deduplicates_by_binary_and_keeps_the_matching_alias() {
        let mut editor = app("Tool Editor");
        editor.identity = Some(Identity::Executable(
            r"c:\program files\tool\tool.exe".into(),
        ));
        let mut viewer = app("Tool Viewer");
        viewer.identity = editor.identity.clone();
        let mut other_editor = App::new(
            "Tool Editor".into(),
            None,
            Source::Desktop(PathBuf::from(r"C:\Different\Tool Editor.lnk")),
        );
        other_editor.identity = Some(Identity::Executable(r"d:\other\tool.exe".into()));
        let apps = [other_editor, viewer, editor];

        assert_eq!(search(&apps, "tool"), [2, 0]);
        assert_eq!(search(&apps, "tool viewer"), [1]);
    }

    #[test]
    fn prefers_user_shortcut_when_same_binary_and_name_tie() {
        let user = app("Editor");
        let mut common = user.clone();
        let mut user = user;
        user.identity = Some(Identity::Executable(r"c:\editor.exe".into()));
        common.identity = user.identity.clone();
        common.common_shortcut = true;
        assert_eq!(search(&[common, user], "editor"), [1]);
    }

    #[test]
    fn prefers_the_regular_shortcut_but_keeps_startup_searchable_by_folder() {
        let mut regular = app("MailCraft");
        regular.identity = Some(Identity::LocalFile(7, [1; 16]));
        regular.common_shortcut = true;
        let mut startup = App::new(
            "MailCraft".into(),
            Some("Startup".into()),
            Source::Desktop(PathBuf::from(r"C:\User\Startup\MailCraft.lnk")),
        );
        startup.identity = regular.identity.clone();
        startup.startup_shortcut = true;
        let apps = [startup, regular];
        assert_eq!(search(&apps, "mailcraft"), [1]);
        assert_eq!(search(&apps, "startup"), [0]);
    }

    #[test]
    fn only_groups_packaged_aliases_with_the_same_aumid() {
        let id = "Demo_abcdefghijklm!Editor";
        let mut shortcut = app("Demo Editor pinned");
        shortcut.identity = Some(Identity::Packaged(id.to_ascii_lowercase()));
        shortcut.aumid = Some(id.to_ascii_lowercase());
        let editor = App::new("Demo Editor".into(), None, Source::Packaged(id.into()));
        let other = App::new(
            "Demo Viewer".into(),
            None,
            Source::Packaged("Demo_abcdefghijklm!Viewer".into()),
        );
        let apps = [shortcut, editor, other];
        assert_eq!(search(&apps, "demo"), [1, 2]);
        assert_eq!(search(&apps, "demo editor pinned"), [0]);
    }

    #[test]
    fn shows_paths_for_same_name_with_different_destinations_without_indexing_them() {
        let mut local = app("Editor");
        local.identity = Some(Identity::Executable(r"c:\editor.exe".into()));
        local.executable_display = Some(r"C:\Editor.exe".into());
        let alias = local.clone();
        let other = App::new(
            "EDITOR".into(),
            None,
            Source::Packaged("Demo_abcdefghijklm!Editor".into()),
        );
        let ordinary = app("Viewer");
        let mut apps = [local, alias, other, ordinary];

        describe_name_collisions(&mut apps);

        assert_eq!(apps[0].detail, r"C:\Editor.exe");
        assert_eq!(apps[2].detail, "Demo_abcdefghijklm!Editor");
        assert_eq!(apps[3].detail, "Меню «Пуск»");
        assert!(search(&apps, "demo").is_empty());
    }
}
