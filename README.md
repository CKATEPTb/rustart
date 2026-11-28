# Rustart

Rustart is a simple, lightweight app launcher for Windows, written in Rust. Press **Win+R** and type a name to find Start menu apps, Microsoft Store apps and installed games from your Steam libraries. You can also type a math expression to see the result. Rustart is free and open source under the [MIT license](LICENSE).

![Rustart showing Microsoft Edge and Microsoft Store for the query “Microsoft”](assets/readme/search.png)

## Install on Windows

1. Download `rustart.exe` for Windows x64 from [GitHub Releases](https://github.com/CKATEPTb/rustart/releases).
2. Run the file and approve the Windows prompt. Rustart installs to `%ProgramFiles%\CKATEPTb\Rustart` and adds an icon to the system tray. To update, run the new executable.

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| `Win+R` | Open Rustart |
| `↑` / `↓` | Select a result |
| `Enter` | Launch the selected app or copy the calculator result |
| `Ctrl+Shift+Enter` | Run the selected app as administrator |
| `Esc` | Close the launcher |

The folder button beside an app opens its location in File Explorer. In PowerShell, open the folder containing `rustart.exe` and try `.\rustart.exe search terminal` or `.\rustart.exe calc "(128 * 3) + (42 / 2)"`.

![Rustart calculating (128 × 3) + (42 ÷ 2) and showing 405](assets/readme/calculator.png)

Right-click the tray icon to change the shortcut, start Rustart at sign-in, or choose from 12 interface languages.

![Rustart tray menu with keyboard shortcut, language, and startup settings](assets/readme/tray.png)

## Build from source

On Windows x64, install Rust with the MSVC toolchain, Visual Studio C++ Build Tools, and the Windows SDK. From the repository root, run this in PowerShell to create `dist\rustart.exe`.

```powershell
.\scripts\package-release.ps1
```
