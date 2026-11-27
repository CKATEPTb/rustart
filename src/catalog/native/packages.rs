use std::{
    collections::HashMap,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
    ptr::null_mut,
};

use windows::{
    ApplicationModel::{AppInfo, PackageSignatureKind},
    Win32::{
        Data::Xml::XmlLite::{
            CreateXmlReader, DtdProcessing_Prohibit, IXmlReader, XmlNodeType, XmlNodeType_Element,
            XmlReaderProperty_DtdProcessing, XmlReaderProperty_MaxElementDepth,
        },
        Foundation::{PROPERTYKEY, S_FALSE, S_OK},
        System::{
            Com::{IStream, STGM_READ, STGM_SHARE_DENY_NONE},
            WinRT::{RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize},
        },
        UI::Shell::{
            IShellItem, IShellItem2, PropertiesSystem::PSGetPropertyKeyFromName,
            SHCreateStreamOnFileEx,
        },
    },
    core::{HSTRING, Interface, PCWSTR, PWSTR, w},
};

use super::{owned_string, shortcut};
use crate::catalog::{
    App,
    identity::{Identity, packaged_id},
};

pub(super) struct Index {
    install_path: Option<PROPERTYKEY>,
    manifests: HashMap<PathBuf, Manifest>,
    binaries: HashMap<String, (Identity, String)>,
}

impl Index {
    pub(super) fn new() -> Self {
        let mut key = PROPERTYKEY::default();
        let install_path = unsafe {
            PSGetPropertyKeyFromName(w!("System.AppUserModel.PackageInstallPath"), &mut key)
        }
        .ok()
        .map(|()| key);
        Self {
            install_path,
            manifests: HashMap::new(),
            binaries: HashMap::new(),
        }
    }

    pub(super) fn record(&mut self, item: &IShellItem, id: &str, shortcuts: &mut shortcut::Index) {
        let Some(key) = &self.install_path else {
            return;
        };
        let Some(root) = install_path(item, key) else {
            return;
        };
        let Some((family, application)) = id.split_once('!') else {
            return;
        };
        let Some((package, _)) = family.rsplit_once('_') else {
            return;
        };
        let manifest = self
            .manifests
            .entry(root.clone())
            .or_insert_with(|| read_manifest(&root).unwrap_or_default());
        if manifest.name.eq_ignore_ascii_case(package)
            && let Some(path) = manifest
                .applications
                .get(application)
                .and_then(|app| app.executable.as_ref())
            && let Some(executable) = shortcuts.package_binary(path, &root)
        {
            self.binaries.insert(id.to_owned(), executable);
        }
    }

    pub(super) fn apply(self, apps: &mut [App]) {
        for app in apps {
            if let Some(id) = &app.aumid
                && let Some((identity, display)) = self.binaries.get(id)
            {
                app.identity = Some(identity.clone());
                app.executable_display = Some(display.clone());
            }
        }
    }
}

fn install_path(item: &IShellItem, key: &PROPERTYKEY) -> Option<PathBuf> {
    let item: IShellItem2 = item.cast().ok()?;
    let path = unsafe { item.GetString(key) }.ok()?;
    Some(PathBuf::from(nonempty_string(path)?))
}

#[derive(Default)]
struct Manifest {
    name: String,
    applications: HashMap<String, ManifestApp>,
    elevated_capability: bool,
}

struct ManifestApp {
    executable: Option<PathBuf>,
    standard_activation: bool,
}

pub(super) fn can_activate_standard(id: &str) -> bool {
    let Some(normalized) = packaged_id(id) else {
        return false;
    };
    let Some((family, app)) = normalized.split_once('!') else {
        return false;
    };
    let Some((name, _)) = family.rsplit_once('_') else {
        return false;
    };
    let Some(path) = trusted_installed_path(id) else {
        return false;
    };
    read_manifest(&path).is_some_and(|manifest| {
        manifest.name.eq_ignore_ascii_case(name)
            && !manifest.elevated_capability
            && manifest
                .applications
                .get(app)
                .is_some_and(|app| app.standard_activation)
    })
}

fn trusted_installed_path(id: &str) -> Option<PathBuf> {
    // The XML of a loose developer package can differ from the registration.
    // Require a signed deployment rather than an unpacked developer registration.
    unsafe { RoInitialize(RO_INIT_SINGLETHREADED).ok()? };
    let path = (|| {
        let package = AppInfo::GetFromAppUserModelId(&HSTRING::from(id))
            .ok()?
            .Package()
            .ok()?;
        if package.IsDevelopmentMode().ok()?
            || !matches!(
                package.SignatureKind().ok()?,
                PackageSignatureKind::Store
                    | PackageSignatureKind::System
                    | PackageSignatureKind::Enterprise
                    | PackageSignatureKind::Developer
            )
        {
            return None;
        }
        let path = package.InstalledPath().ok()?;
        Some(PathBuf::from(std::ffi::OsString::from_wide(&path)))
    })();
    unsafe { RoUninitialize() };
    path
}

fn read_manifest(root: &Path) -> Option<Manifest> {
    let path = root.join("AppxManifest.xml");
    let manifest = shortcut::resolve_local_file(&path)?;
    if !manifest.starts_with(root) {
        return None;
    }
    let path: Vec<_> = manifest.as_os_str().encode_wide().chain([0]).collect();
    let stream = unsafe {
        SHCreateStreamOnFileEx(
            PCWSTR(path.as_ptr()),
            (STGM_READ | STGM_SHARE_DENY_NONE).0,
            0,
            false,
            None,
        )
    }
    .ok()?;
    read_xml(&stream, root)
}

fn read_xml(stream: &IStream, root: &Path) -> Option<Manifest> {
    let mut raw = null_mut();
    unsafe { CreateXmlReader(&IXmlReader::IID, &mut raw, None) }.ok()?;
    if raw.is_null() {
        return None;
    }
    let reader = unsafe { IXmlReader::from_raw(raw) };
    unsafe {
        reader
            .SetProperty(
                XmlReaderProperty_DtdProcessing.0 as u32,
                Some(DtdProcessing_Prohibit.0 as isize),
            )
            .ok()?;
        reader
            .SetProperty(XmlReaderProperty_MaxElementDepth.0 as u32, Some(64))
            .ok()?;
        reader.SetInput(stream).ok()?;
    }

    let mut result = Manifest::default();
    let mut namespace = None;
    let mut applications = false;
    let mut capabilities = false;
    let mut seen_applications = false;
    loop {
        let mut node = XmlNodeType::default();
        let status = unsafe { reader.Read(Some(&mut node)) };
        if status == S_FALSE {
            return (namespace.is_some() && !result.name.is_empty()).then_some(result);
        }
        status.ok().ok()?;
        if node != XmlNodeType_Element {
            continue;
        }
        match unsafe { reader.GetDepth() }.ok()? {
            0 => {
                let mut uri = PCWSTR::null();
                unsafe { reader.GetNamespaceUri(&mut uri, None) }.ok()?;
                let allowed = [
                    w!("http://schemas.microsoft.com/appx/manifest/foundation/windows10"),
                    w!("http://schemas.microsoft.com/appx/2010/manifest"),
                ];
                namespace = allowed
                    .into_iter()
                    .find(|&allowed| wide_equals(uri, allowed));
                if !is_element(&reader, w!("Package"), namespace?) {
                    return None;
                }
            }
            1 => {
                let namespace = namespace?;
                applications = is_element(&reader, w!("Applications"), namespace);
                capabilities = is_element(&reader, w!("Capabilities"), namespace);
                if applications {
                    if seen_applications {
                        return None;
                    }
                    seen_applications = true;
                }
                if is_element(&reader, w!("Identity"), namespace) {
                    if !result.name.is_empty() {
                        return None;
                    }
                    result.name = xml_attribute(&reader, w!("Name"))?;
                }
            }
            2 if applications && is_element(&reader, w!("Application"), namespace?) => {
                if let Some(id) = xml_attribute(&reader, w!("Id")) {
                    let executable = xml_attribute(&reader, w!("Executable"))
                        .and_then(|exe| package_executable(root, Path::new(&exe)));
                    let standard_activation = standard_activation(&reader);
                    if result
                        .applications
                        .insert(
                            id.to_ascii_lowercase(),
                            ManifestApp {
                                executable,
                                standard_activation,
                            },
                        )
                        .is_some()
                    {
                        return None;
                    }
                }
            }
            2 if capabilities
                && xml_attribute(&reader, w!("Name"))
                    .is_some_and(|name| name.eq_ignore_ascii_case("allowElevation")) =>
            {
                result.elevated_capability = true;
            }
            _ => {}
        }
    }
}

fn is_element(reader: &IXmlReader, name: PCWSTR, namespace: PCWSTR) -> bool {
    let mut value = PCWSTR::null();
    if unsafe { reader.GetLocalName(&mut value, None) }.is_err() || !wide_equals(value, name) {
        return false;
    }
    unsafe { reader.GetNamespaceUri(&mut value, None) }.is_ok() && wide_equals(value, namespace)
}

fn wide_equals(left: PCWSTR, right: PCWSTR) -> bool {
    !left.is_null() && unsafe { left.as_wide() == right.as_wide() }
}

fn xml_attribute(reader: &IXmlReader, name: PCWSTR) -> Option<String> {
    namespaced_attribute(reader, name, PCWSTR::null())
}

fn namespaced_attribute(reader: &IXmlReader, name: PCWSTR, namespace: PCWSTR) -> Option<String> {
    if unsafe { reader.MoveToAttributeByName(name, namespace) } != S_OK {
        return None;
    }
    let mut value = PCWSTR::null();
    let read = unsafe { reader.GetValue(&mut value, None) }.is_ok() && !value.is_null();
    let result = read.then(|| unsafe { value.to_string() }.ok()).flatten();
    unsafe { reader.MoveToElement() }.ok()?;
    result.filter(|value| !value.is_empty())
}

fn standard_activation(reader: &IXmlReader) -> bool {
    let uap = w!("http://schemas.microsoft.com/appx/manifest/uap/windows10/10");
    if !known_activation_attributes(reader, uap) {
        return false;
    }
    let trust = namespaced_attribute(reader, w!("TrustLevel"), uap);
    let runtime = namespaced_attribute(reader, w!("RuntimeBehavior"), uap);
    if runtime
        .as_deref()
        .is_some_and(|value| !matches!(value, "windowsApp" | "packagedClassicApp" | "win32App"))
    {
        return false;
    }
    let entry = xml_attribute(reader, w!("EntryPoint")).map(|entry| entry.to_ascii_lowercase());
    let special = matches!(
        entry.as_deref(),
        Some("windows.fulltrustapplication" | "windows.partialtrustapplication")
    );
    if entry
        .as_deref()
        .is_some_and(|entry| entry.starts_with("windows."))
        && !special
        || special && runtime.as_deref() == Some("windowsApp")
    {
        return false;
    }
    match (trust.as_deref(), entry.as_deref()) {
        (Some("mediumIL"), Some("windows.partialtrustapplication"))
        | (Some("appContainer"), Some("windows.fulltrustapplication")) => false,
        (Some("appContainer" | "mediumIL"), None) => runtime.is_some(),
        (Some("appContainer" | "mediumIL"), Some(_)) => true,
        (None, Some("windows.fulltrustapplication" | "windows.partialtrustapplication")) => {
            runtime.as_deref() != Some("windowsApp")
        }
        (None, Some(entry)) => {
            !entry.starts_with("windows.")
                && runtime.as_deref().is_none_or(|value| value == "windowsApp")
        }
        _ => false,
    }
}

fn known_activation_attributes(reader: &IXmlReader, uap: PCWSTR) -> bool {
    let mut status = unsafe { reader.MoveToFirstAttribute() };
    if status == S_FALSE {
        return true;
    }
    let mut known = true;
    while status == S_OK {
        let mut name = PCWSTR::null();
        if unsafe { reader.GetLocalName(&mut name, None) }.is_err() || name.is_null() {
            known = false;
            break;
        }
        if wide_equals(name, w!("HostId")) {
            known = false;
            break;
        }
        if wide_equals(name, w!("TrustLevel")) || wide_equals(name, w!("RuntimeBehavior")) {
            let mut uri = PCWSTR::null();
            if unsafe { reader.GetNamespaceUri(&mut uri, None) }.is_err() || !wide_equals(uri, uap)
            {
                known = false;
                break;
            }
        }
        status = unsafe { reader.MoveToNextAttribute() };
    }
    unsafe { reader.MoveToElement() }.is_ok() && status.ok().is_ok() && known
}
fn nonempty_string(pointer: PWSTR) -> Option<std::ffi::OsString> {
    if pointer.is_null() {
        return None;
    }
    let value = owned_string(pointer);
    (!value.is_empty()).then_some(value)
}

fn package_executable(root: &Path, relative: &Path) -> Option<PathBuf> {
    let valid = relative.components().all(|component| match component {
        Component::Normal(name) => name
            .to_str()
            .is_some_and(|name| !name.ends_with(['.', ' ']) && !name.contains(':')),
        _ => false,
    });
    (valid && relative.extension()?.eq_ignore_ascii_case("exe")).then(|| root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Source, search};
    use windows::Win32::UI::Shell::SHCreateMemStream;

    #[test]
    fn streams_only_declared_application_elements_in_the_package_namespace() {
        let document = r#"<f:Package xmlns:f="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
               xmlns:ext="urn:example">
            <f:Identity Name="Demo"/>
            <f:Other><f:Application Id="Outside" Executable="Other.exe"/></f:Other>
            <f:Applications>
                <ext:Application Id="WrongNamespace" Executable="Wrong.exe"/>
                <f:Application ext:Id="WrongAttributeNamespace" Executable="Wrong.exe"/>
                <f:Application Executable="Bin\Пример.exe" Id="Main">
                    <f:Application Id="Nested" Executable="Nested.exe"/>
                </f:Application>
                <f:Application Id="Escape" Executable="..\Outside.exe"/>
            </f:Applications>
        </f:Package>"#;
        let stream = unsafe { SHCreateMemStream(Some(document.as_bytes())) }.unwrap();
        let root = Path::new(r"C:\Package");
        let result = read_xml(&stream, root).unwrap();
        assert_eq!(result.name, "Demo");
        assert_eq!(result.applications.len(), 2);
        assert_eq!(
            result.applications["main"].executable,
            Some(root.join(r"Bin\Пример.exe"))
        );
        assert!(result.applications["escape"].executable.is_none());
    }

    #[test]
    fn rejects_manifest_dtds_and_ambiguous_application_ids() {
        for xml in [
            r#"<!DOCTYPE Package [<!ENTITY name "Demo">]>
            <Package xmlns="http://schemas.microsoft.com/appx/2010/manifest">
                <Identity Name="&name;"/>
            </Package>"#,
            r#"<Package xmlns="http://schemas.microsoft.com/appx/2010/manifest">
                <Identity Name="Demo"/>
                <Applications>
                    <Application Id="Demo"/>
                    <Application Id="Demo" Executable="Tool.exe"/>
                </Applications>
            </Package>"#,
        ] {
            let stream = unsafe { SHCreateMemStream(Some(xml.as_bytes())) }.unwrap();
            assert!(read_xml(&stream, Path::new(r"C:\Package")).is_none());
        }
    }

    #[test]
    fn recognizes_standard_packaged_contracts_and_elevation_capability() {
        let xml = r#"<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
            xmlns:uap10="http://schemas.microsoft.com/appx/manifest/uap/windows10/10"
            xmlns:future="urn:example">
            <Identity Name="Demo"/>
            <Applications>
                <Application Id="Uwp" EntryPoint="Demo.App"/>
                <Application Id="Legacy" EntryPoint="Windows.FullTrustApplication"/>
                <Application Id="Win32" uap10:TrustLevel="mediumIL" uap10:RuntimeBehavior="packagedClassicApp"/>
                <Application Id="Unknown" EntryPoint="Demo.App" future:TrustLevel="unknown"/>
                <Application Id="Contradictory" EntryPoint="Windows.FullTrustApplication" uap10:TrustLevel="appContainer"/>
            </Applications>
            <Capabilities><Capability Name="runFullTrust"/><Capability Name="allowElevation"/></Capabilities>
        </Package>"#;
        let stream = unsafe { SHCreateMemStream(Some(xml.as_bytes())) }.unwrap();
        let manifest = read_xml(&stream, Path::new(r"C:\Package")).unwrap();
        for id in ["uwp", "legacy", "win32"] {
            assert!(manifest.applications[id].standard_activation, "{id}");
        }
        for id in ["unknown", "contradictory"] {
            assert!(!manifest.applications[id].standard_activation, "{id}");
        }
        assert!(manifest.elevated_capability);
    }

    #[test]
    #[ignore = "requires Windows Calculator and Terminal registrations"]
    fn proves_calculator_and_terminal_have_ordinary_activation_contracts() {
        let _apartment = super::super::Apartment::enter().unwrap();
        for id in [
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App",
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App",
        ] {
            assert!(can_activate_standard(id), "{id}");
        }
    }

    #[test]
    fn permits_only_executable_paths_inside_the_package() {
        let root = Path::new(r"C:\Program Files\WindowsApps\Demo");
        assert_eq!(
            package_executable(root, Path::new(r"Tools\Пример.exe")),
            Some(root.join(r"Tools\Пример.exe"))
        );
        for path in [
            r"C:\Windows\explorer.exe",
            r"C:explorer.exe",
            r"\\server\share\Tool.exe",
            r"..\Tool.exe",
            r".. \Tool.exe",
            r":stream\Tool.exe",
            r"Tools\Page.html",
        ] {
            assert!(
                package_executable(root, Path::new(path)).is_none(),
                "{path}"
            );
        }
    }

    #[test]
    fn mapped_packages_and_shortcuts_share_one_executable_without_losing_aumids() {
        let path = PathBuf::from(r"C:\Demo\tool.exe");
        let binary = Identity::LocalFile(1, [2; 16]);
        let exe = r"C:\Demo\tool.exe".to_owned();
        let mut index = Index {
            install_path: None,
            manifests: HashMap::new(),
            binaries: HashMap::new(),
        };
        let two = "Demo_abcdefghijklm!Two";
        for id in ["Demo_abcdefghijklm!One", two] {
            index
                .binaries
                .insert(id.to_ascii_lowercase(), (binary.clone(), exe.clone()));
        }
        let mut desktop = App::new("Demo Desktop".into(), None, Source::Desktop(path));
        desktop.identity = Some(binary);
        let mut alias = App::new(
            "Demo Alternate".into(),
            None,
            Source::Desktop(PathBuf::from(r"C:\Demo\alternate.lnk")),
        );
        alias.aumid = Some(two.to_ascii_lowercase());
        let mut apps = [
            desktop,
            App::new(
                "Demo One".into(),
                None,
                Source::Packaged("Demo_abcdefghijklm!One".into()),
            ),
            App::new("Demo Two".into(), None, Source::Packaged(two.into())),
            alias,
        ];
        index.apply(&mut apps);
        assert_eq!(search(&apps, "demo").len(), 1);
        assert_eq!(search(&apps, "demo alternate"), [3]);
        assert_eq!(apps[2].source, Source::Packaged(two.into()));
    }

    #[test]
    #[ignore = "requires the Windows Calculator package registered for the local user"]
    fn proves_calculator_apps_folder_and_its_executable_share_an_identity() {
        let _apartment = super::super::Apartment::enter().unwrap();
        let id = "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_ascii_lowercase();
        let started = std::time::Instant::now();
        let apps = super::super::discover().unwrap();
        let elapsed = started.elapsed();
        let app = apps
            .iter()
            .find(|app| app.aumid.as_ref() == Some(&id))
            .unwrap();
        let executable = app.executable_display.as_deref().unwrap();
        let mut shortcuts = shortcut::Index::new();
        let direct = shortcuts.identify(Path::new(executable)).unwrap().0;
        assert_eq!(app.identity.as_ref(), Some(&direct));
        eprintln!("Calculator package executable: {executable}; full discovery: {elapsed:?}");
    }
}
