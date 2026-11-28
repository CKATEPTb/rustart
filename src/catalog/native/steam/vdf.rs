// Steam's library lists and app manifests use the text KeyValues format.
pub(super) struct Object(Vec<(String, Value)>);

pub(super) enum Value {
    Text(String),
    Object(Object),
}

impl Object {
    pub(super) fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }

    pub(super) fn text(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_text()
    }

    pub(super) fn object(&self, key: &str) -> Option<&Object> {
        self.get(key)?.as_object()
    }
}

impl Value {
    pub(super) fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Object(_) => None,
        }
    }

    pub(super) fn as_object(&self) -> Option<&Object> {
        match self {
            Self::Object(object) => Some(object),
            Self::Text(_) => None,
        }
    }
}

pub(super) fn parse(text: &str) -> Option<Object> {
    let mut parser = Parser(text.strip_prefix('\u{feff}').unwrap_or(text));
    parser.object(0)
}

struct Parser<'a>(&'a str);

impl Parser<'_> {
    fn skip_space(&mut self) {
        loop {
            self.0 = self.0.trim_start();
            if !self.0.starts_with("//") {
                return;
            }
            self.0 = self.0.split_once(['\r', '\n']).map_or("", |(_, tail)| tail);
        }
    }

    fn object(&mut self, depth: usize) -> Option<Object> {
        if depth > 32 {
            return None;
        }
        let mut entries = Vec::new();
        loop {
            self.skip_space();
            if self.0.is_empty() {
                return (depth == 0).then_some(Object(entries));
            }
            if let Some(rest) = self.0.strip_prefix('}') {
                self.0 = rest;
                return (depth != 0).then_some(Object(entries));
            }
            let key = self.string()?;
            self.skip_space();
            let value = if let Some(rest) = self.0.strip_prefix('{') {
                self.0 = rest;
                Value::Object(self.object(depth + 1)?)
            } else {
                Value::Text(self.string()?)
            };
            entries.push((key, value));
        }
    }

    fn string(&mut self) -> Option<String> {
        if let Some(quoted) = self.0.strip_prefix('"') {
            let mut chars = quoted.chars();
            let mut text = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '"' => {
                        self.0 = chars.as_str();
                        return Some(text);
                    }
                    // Older files can contain single backslashes in paths.
                    '\\' if chars.as_str().starts_with(['"', '\\']) => text.push(chars.next()?),
                    c => text.push(c),
                }
            }
            return None;
        }
        let end = self
            .0
            .find(|c: char| c.is_whitespace() || matches!(c, '{' | '}' | '"'))
            .unwrap_or(self.0.len());
        if end == 0 {
            return None;
        }
        let (plain, rest) = self.0.split_at(end);
        self.0 = rest;
        Some(plain.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_modern_and_legacy_libraries_with_windows_paths() {
        let doc = r#"// Steam paths
            "LibraryFolders" {
                "1" "D:\\Steam Library"
                "2" { path "E:\new\tools\Игры" "apps" { "42" "0" } }
            }"#;
        let root = parse(&format!("\u{feff}{doc}")).unwrap();
        let libraries = root.object("libraryfolders").unwrap();
        assert_eq!(libraries.text("1"), Some(r"D:\Steam Library"));
        let secondary = libraries.object("2").unwrap();
        assert_eq!(secondary.text("Path"), Some(r"E:\new\tools\Игры"));
        assert_eq!(secondary.object("apps").unwrap().text("42"), Some("0"));
    }

    #[test]
    fn quoted_values_preserve_comment_markers_and_decode_escaped_quotes() {
        let root = parse(r#"AppState { name "The \"Game\" // II" empty "" }"#).unwrap();
        let app = root.object("AppState").unwrap();
        assert_eq!(app.text("name"), Some(r#"The "Game" // II"#));
        assert_eq!(app.text("empty"), Some(""));
    }

    #[test]
    fn rejects_incomplete_objects_strings_and_excessive_nesting() {
        for doc in [
            "}",
            "a { b c",
            r#"a "unterminated"#,
            "a",
            "a { b }",
            "a b }",
        ] {
            assert!(parse(doc).is_none(), "{doc}");
        }
        assert!(parse(&format!("{}{}", "a { ".repeat(64), "}".repeat(64))).is_none());
    }
}
