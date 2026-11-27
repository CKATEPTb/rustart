# Run in an interactive Windows session with Rustart closed.
[CmdletBinding()]
param([string]$Exe,
      [string]$Screenshots, [switch]$CheckCalculatorApp, [switch]$CheckRunDialog,
      [switch]$CheckReveal, [switch]$CheckClipboard)
$ErrorActionPreference = 'Stop'
if (!$Exe) {
    $target = Join-Path $PSScriptRoot '../target/installation-fixture'
    & cargo build --manifest-path (Join-Path $PSScriptRoot '../Cargo.toml') --locked --release --features installation-fixture --target-dir $target
    if ($LASTEXITCODE -ne 0) { throw 'Could not build the desktop test executable.' }
    $Exe = Join-Path $target 'release/rustart.exe'
}
$Exe = (Resolve-Path -LiteralPath $Exe).Path
$installed = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'CKATEPTb/Rustart/rustart.exe'
if ($Exe -ne [IO.Path]::GetFullPath($installed) -and (& $Exe --installation-fixture 2>$null) -ne 'rustart-installation-fixture-v1') {
    throw 'Use the installed Rustart executable or a build with the installation-fixture feature.'
}
Add-Type -Path (Join-Path $PSScriptRoot 'DesktopProbe.cs')
$probe = [DesktopProbe]
$null = $probe::SetThreadDpiAwarenessContext([IntPtr](-4))
function Require($condition, [string]$message) { if (!$condition) { throw $message } }
function Await([scriptblock]$condition, [string]$message, [int]$seconds = 6) {
    $deadline = [DateTime]::UtcNow.AddSeconds($seconds)
    do {
        if (& $condition) { return }
        Start-Sleep -Milliseconds 35
    } while ([DateTime]::UtcNow -lt $deadline)
    throw $message
}
function Wait-Launch([string]$name) {
    $markerPath = Join-Path $scratch "ran-$name.marker"
    # Poll existence only. Opening the marker while a fixture writes it can
    # cause a sharing violation in the new process.
    Await { [IO.File]::Exists($markerPath) } "The fixture program '$name' did not start." 25
    Await { !$probe::Visible($overlay) } "The launcher stayed open after starting '$name'." 25
    [IO.File]::Delete($markerPath)
}
function Measure-Memory([Diagnostics.Process]$process, [string]$phase) {
    $memory = $probe::Memory($process.Handle)
    [pscustomobject]@{
        Phase = $phase
        CounterSource = $memory.Source
        PrivateWorkingSetMiB = if ($null -ne $memory.PrivateWorkingSetBytes) { [math]::Round($memory.PrivateWorkingSetBytes / 1MB, 2) }
        PrivateBytesMiB = [math]::Round($memory.PrivateBytes / 1MB, 2)
        WorkingSetMiB = [math]::Round($memory.WorkingSetBytes / 1MB, 2)
    }
}
function Await-SingleRustartProcess([string]$phase) {
    Await { $probe::GroupMemory($Exe).ProcessCount -eq 1 } "Expected only the resident Rustart process for '$phase'."
}
function Capture([IntPtr]$hwnd, [string]$name) {
    if (!$Screenshots) { return }
    Require ($probe::GetForegroundWindow() -eq $hwnd) 'Capture requires a foreground Rustart window.'
    Add-Type -AssemblyName System.Drawing
    $null = New-Item -ItemType Directory -Path $Screenshots -Force
    $savedPoint = New-Object DesktopProbe+Point
    $moved = $false
    try {
        if ($hwnd -eq $overlay -and $probe::GetCursorPos([ref]$savedPoint)) {
            $editBounds = New-Object DesktopProbe+Rect
            $null = $probe::GetWindowRect($edit, [ref]$editBounds)
            $neutralX = $editBounds.Right - 6
            $neutralY = [int](($editBounds.Bottom + $editBounds.Top) / 2)
            $moved = $probe::SetCursorPos($neutralX, $neutralY)
            $end = $probe::Value($edit, 0x000e)
            $null = $probe::Send($edit, 0x00b1, [IntPtr]$end, [IntPtr]$end)
            Start-Sleep -Milliseconds 130
        }
        $image = $probe::CaptureWindow($hwnd)
        $format = [Drawing.Imaging.PixelFormat]::Format32bppArgb
        $bmp = [Drawing.Bitmap]::new($image.Width, $image.Height, $format)
        try {
            $rect = [Drawing.Rectangle]::new(0, 0, $image.Width, $image.Height)
            $data = $bmp.LockBits($rect, [Drawing.Imaging.ImageLockMode]::WriteOnly, $format)
            try {
                Require ($data.Stride -eq $image.Width * 4) 'Unexpected screenshot bitmap layout.'
                [Runtime.InteropServices.Marshal]::Copy($image.Pixels, 0, $data.Scan0, $image.Pixels.Length)
            } finally { $bmp.UnlockBits($data) }
            $bmp.Save((Join-Path $Screenshots "$name.png"), [Drawing.Imaging.ImageFormat]::Png)
        } finally { $bmp.Dispose() }
    } finally {
        $currentPoint = New-Object DesktopProbe+Point
        if ($moved -and $probe::GetCursorPos([ref]$currentPoint) -and $currentPoint.X -eq $neutralX -and $currentPoint.Y -eq $neutralY) {
            $null = $probe::SetCursorPos($savedPoint.X, $savedPoint.Y)
        }
    }
}
function Press([byte[]]$keys) {
    Require ($probe::GetForegroundWindow() -eq $overlay) 'Rustart lost focus before input.'
    $probe::Chord($keys)
}
function Open-Overlay {
    & $Exe show
    Require ($LASTEXITCODE -eq 0) 'CLI show failed.'
    Await { $probe::Visible($overlay) -and $probe::GetForegroundWindow() -eq $overlay } 'Overlay did not get focus.'
    Await { $probe::FocusId($overlay) -eq 101 } 'Overlay did not pass keyboard focus to the search field.'
}
function Open-Tray {
    $null = $probe::Post($owner, 0x8029, [IntPtr]0, [IntPtr]0x1007b)
    Await { $script:menu = $probe::FindClass('Rustart.TrayMenu'); $probe::Visible($menu) } 'Tray menu did not open.'
}
function Click-Menu([string]$label) {
    $button = $probe::FindButton($menu, $label)
    Require ($button -ne 0) "Missing tray action $label."
    $null = $probe::Post($button, 0x00f5, [IntPtr]0, [IntPtr]0)
}
function Await-MenuRow([string]$label) {
    Await {
        $script:menu = $probe::FindClass('Rustart.TrayMenu')
        $probe::Visible($menu) -and $probe::FindButton($menu, $label) -ne 0
    } "Missing menu row '$label'."
}
function Record-Key([byte[]]$keys) {
    Click-Menu 'Горячая клавиша'
    Await-MenuRow 'Нажмите клавишу'
    Require ($probe::GetForegroundWindow() -eq $menu) 'The listening menu lost focus.'
    $probe::Chord($keys)
}
function Click-RowAction([int]$id) {
    $button = $probe::GetDlgItem($overlay, $id)
    Require ($probe::CanClick($button)) "Row button $id is hidden or covered."
    $null = $probe::Post($button, 0x00f5, [IntPtr]0, [IntPtr]0)
}

function Has-ProbeLogo([int]$row) {
    # The fixture's embedded Rustart icon has a cyan terminal prompt. Capture just
    # the icon column, in device pixels, to distinguish it from the neutral fallback.
    Add-Type -AssemblyName System.Drawing
    $bounds = New-Object DesktopProbe+Rect
    $null = $probe::GetWindowRect($list, [ref]$bounds)
    $height = $probe::Value($list, 0x01a1)
    $bitmap = [Drawing.Bitmap]::new([int]($height * 0.82), $height)
    try {
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        try { $graphics.CopyFromScreen($bounds.Left, ($bounds.Top + $row * $height), 0, 0, $bitmap.Size) } finally { $graphics.Dispose() }
        $cyanPixels = 0
        for ($y = 0; $y -lt $bitmap.Height; $y++) {
            for ($x = 0; $x -lt $bitmap.Width; $x++) {
                $pixel = $bitmap.GetPixel($x, $y)
                if ($pixel.R -ge 105 -and $pixel.R -le 142 -and $pixel.G -ge 194 -and $pixel.G -le 234 -and $pixel.B -ge 232) {
                    if (++$cyanPixels -ge 6) { return $true }
                }
            }
        }
        return $false
    } finally { $bitmap.Dispose() }
}

function Check-Folder([string]$folder, [string]$selectedFile, [string]$appId) {
    $explorer = New-Object -ComObject Shell.Application
    $windowsBefore = @($explorer.Windows() | ForEach-Object { $_.HWND })
    Click-RowAction 112
    Await {
        foreach ($window in $explorer.Windows()) {
            try {
                $view = $window.Document
                $path = $view.Folder.Self.Path
                $selected = $view.SelectedItems()
                $inFolder = $path -eq $folder -and (!$selectedFile -or
                    ($selected.Count -eq 1 -and $selected.Item(0).Path -eq $selectedFile))
                $inAppsFolder = $appId -and $path -match '4234d49b-0245-4df3-b780-3893943456e1' -and
                    $selected.Count -eq 1 -and ($selected.Item(0).ExtendedProperty('System.AppUserModel.ID') -eq $appId)
            } catch { continue } # Explorer tabs can close during enumeration.
            if ($inFolder -or $inAppsFolder) {
                $script:fixtureExplorer = $window
                return $true
            }
        }
        return $false
    } 'Explorer did not show the expected application location.'
    $explorerWindow = [IntPtr]$fixtureExplorer.HWND
    if ($fixtureExplorer.HWND -notin $windowsBefore) {
        $fixtureExplorer.Quit()
        Await { !$probe::Visible($explorerWindow) } 'The fixture Explorer window did not close.'
    }
}

Require ((& $Exe status) -eq 'stopped') 'Close the existing Rustart session before running the smoke test.'
$tag = 'RustartSmoke' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$programs = [Environment]::GetFolderPath('Programs')
$fixtures = [IO.Path]::Combine($programs, $tag)
$tempRoot = [IO.Path]::GetTempPath()
$scratch = [IO.Path]::Combine($tempRoot, $tag)
$previousLocal = $env:LOCALAPPDATA
$started = $false
try {
    $null = New-Item -ItemType Directory -Path $fixtures,$scratch
    $env:LOCALAPPDATA = $scratch
    $preferences = Join-Path $scratch 'Rustart/settings.json'
    $null = New-Item -Path (Split-Path $preferences) -ItemType Directory
    $legacyExcludedName = "$tag 05"
    $legacySettings = @{ language = 'ru'; exclusions = @($legacyExcludedName) } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText($preferences, $legacySettings)
    $shell = New-Object -ComObject WScript.Shell
    $probeExe = Join-Path $scratch 'launch-probe.exe'
    $csc = Join-Path $env:SystemRoot 'Microsoft.NET/Framework/v4.0.30319/csc.exe'
    $logo = (Resolve-Path (Join-Path $PSScriptRoot '../assets/rustart.ico')).Path
    & $csc /nologo /target:winexe "/win32icon:$logo" "/out:$probeExe" (Join-Path $PSScriptRoot 'LaunchProbe.cs')
    Require ($LASTEXITCODE -eq 0) 'Failed to build fixture application.'
    1..10 | ForEach-Object {
        $rank = $_.ToString('00')
        $targetExe = Join-Path $scratch "Program-$rank.exe"
        Copy-Item -LiteralPath $probeExe -Destination $targetExe
        $link = $shell.CreateShortcut((Join-Path $fixtures "$tag $rank.lnk"))
        $link.TargetPath = $targetExe
        $link.Arguments = $rank
        $link.Save()
    }
    $aliasName = "$tag Alternate shortcut"
    $alias = $shell.CreateShortcut((Join-Path $fixtures "$aliasName.lnk"))
    $alias.TargetPath = Join-Path $scratch 'Program-01.exe'
    $alias.Arguments = 'alternate'
    $alias.Save()
    Require ((& $Exe calc '2*(3+4)') -eq '14') 'CLI calculator failed.'
    $cliResults = @(& $Exe search $tag)
    Require ($LASTEXITCODE -eq 0 -and $cliResults.Count -eq 10) 'CLI must return ten programs for eleven shortcuts with legacy settings.'
    $cliNames = @($cliResults | ForEach-Object { ($_ -split "`t", 2)[0] })
    Require ($legacyExcludedName -cin $cliNames) 'A legacy exclusion still hid the fixture in CLI search.'
    Require (@(& $Exe search $aliasName).Count -eq 1) 'The alternate shortcut lost its searchable name.'
    $clients = @(1..6 | ForEach-Object { Start-Process $Exe background -PassThru -WindowStyle Hidden })
    $started = $true
    foreach ($client in $clients) {
        Require ($client.WaitForExit(10000) -and $client.ExitCode -eq 0) 'Concurrent startup failed.'
    }
    $state = & $Exe status
    Require ($state -match '^running (\d+)$') 'Resident unavailable.'
    $resident = Get-Process -Id ([int]$Matches[1])
    & $Exe background-admin
    Require ($LASTEXITCODE -eq 0 -and (& $Exe status) -eq $state) 'A subsequent admin startup replaced the resident.'
    Require (@(Get-Process rustart).Count -eq 1) 'More than one resident.'
    $owner = $probe::FindClass('CKATEPTb.Rustart.Owner.1')
    Start-Sleep -Milliseconds 2500
    Measure-Memory $resident 'Cold tray before opening'
    $probe::Chord([byte[]](0x5b,0x52))
    $overlay = $probe::FindClass('Rustart.Overlay')
    Await { $script:overlay = $probe::FindClass('Rustart.Overlay'); $probe::Visible($overlay) } 'Win+R did not open the overlay.'
    Await { $probe::GetForegroundWindow() -eq $overlay } 'Win+R did not focus the overlay.'
    $edit = $probe::GetDlgItem($overlay, 101)
    $list = $probe::GetDlgItem($overlay, 102)
    Require ($probe::Visible($edit) -and $list -ne [IntPtr]::Zero) 'The launched overlay did not create both controls.'
    Await { $probe::FocusId($overlay) -eq 101 } 'The empty launcher did not focus the search field.'
    Require ($probe::Value($edit, 0x000e) -eq 0) 'The search field was not empty on first open.'
    Require (!$probe::Visible($list) -and $probe::Value($list, 0x018b) -eq 0) 'The empty launcher displayed a results list.'
    Capture $overlay 'empty'
    $bounds = New-Object DesktopProbe+Rect
    $null = $probe::GetWindowRect($edit, [ref]$bounds)
    $work = $probe::WorkArea($overlay)
    $position = (($bounds.Top + $bounds.Bottom) / 2 - $work.Top) / ($work.Bottom - $work.Top)
    Require ($position -ge 0.34 -and $position -le 0.48) "Input not slightly above midpoint ($position)."
    # Start with an indexed catalog so a missing match tests the settled layout.
    $sampler = [DesktopProbe+ProcessGroupSampler]::new($Exe, 15)
    try {
        $firstQueryMs = $probe::MeasureQueryToFirstRow($edit, $list, $tag, "$tag 01")
        Await { $probe::Visible($list) -and $probe::Value($list, 0x018b) -eq 10 } 'Ten test results did not appear with legacy settings.'
        Require ($probe::Row($list, 4) -eq $legacyExcludedName) 'A legacy exclusion still hid the fixture in the overlay.'
        Await { Has-ProbeLogo 0 } 'The embedded application icon did not reach the first search row.'
        Await-SingleRustartProcess 'first visible search'
        $peak = $sampler.Finish()
    } finally {
        $sampler.Dispose()
    }
    Require ($peak.MaxProcessCount -ge 1) 'The first-search sampler did not observe Rustart.'
    [pscustomobject]@{
        Phase = 'First application search process group peak'
        SamplingPeriodMs = $peak.SamplingPeriodMs
        MaxSampleGapMs = [math]::Round($peak.MaxSampleGapMs, 2)
        Samples = $peak.Samples
        MaxProcessCount = $peak.MaxProcessCount
        ColdQueryToFirstRowMs = [math]::Round($firstQueryMs, 2)
        PeakPrivateWorkingSetMiB = if ($null -ne $peak.PrivateWorkingSetBytes) { [math]::Round($peak.PrivateWorkingSetBytes / 1MB, 2) }
        PeakPrivateBytesMiB = [math]::Round($peak.PrivateBytes / 1MB, 2)
    }
    Measure-Memory $resident 'First visible application search after helpers exit'
    $probe::Query($edit, '')
    Require (!$probe::Visible($list) -and $probe::Value($list, 0x018b) -eq 0) 'Empty input left a results list.'
    $compactBounds = New-Object DesktopProbe+Rect
    Require ($probe::GetWindowRect($overlay, [ref]$compactBounds)) 'Could not measure the compact launcher.'
    $compactHeight = $compactBounds.Bottom - $compactBounds.Top
    Require ($compactHeight -gt 0) 'The compact launcher has no height.'
    $noMatch = 'RustartNoMatch' + [guid]::NewGuid().ToString('N')
    foreach ($query in @('=2**3', '1/0', $noMatch, '2+')) {
        $probe::Query($edit, $query)
        Require (!$probe::Visible($list) -and $probe::Value($list, 0x018b) -eq 0) "Query '$query' left a results list."
        $queryBounds = New-Object DesktopProbe+Rect
        Require ($probe::GetWindowRect($overlay, [ref]$queryBounds)) "Could not measure the launcher for '$query'."
        $queryHeight = $queryBounds.Bottom - $queryBounds.Top
        Require ($queryHeight -eq $compactHeight) "Query '$query' changed the compact height from $compactHeight to $queryHeight."
    }
    Capture $overlay 'incomplete-expression'
    $probe::Query($edit, '(180 + 60) / 1,5')
    Await { $probe::Row($list, 0) -eq '160' } 'Calculator result did not appear.'
    $resultBounds = New-Object DesktopProbe+Rect
    Require ($probe::Visible($list) -and $probe::Value($list, 0x018b) -eq 1 -and
        $probe::GetWindowRect($overlay, [ref]$resultBounds) -and
        ($resultBounds.Bottom - $resultBounds.Top) -gt $compactHeight) 'A valid calculation did not expand the launcher.'
    Start-Sleep -Milliseconds 180
    Require ($probe::CanClick($probe::GetDlgItem($overlay, 119))) 'Calculator copy button is missing.'
    Require (!($probe::Visible($probe::GetDlgItem($overlay, 110)))) 'Calculator exposes an application action.'
    Capture $overlay 'calculator'
    if ($CheckClipboard) {
        # This optional check writes the known result to the session clipboard.
        Press @(0x0d)
        Await { !($probe::Visible($overlay)) -and (Get-Clipboard -Raw) -eq '160' } 'Enter did not copy the calculation.'
        Open-Overlay
        $probe::Query($edit, '28+9')
        Click-RowAction 119
        Await { !($probe::Visible($overlay)) -and (Get-Clipboard -Raw) -eq '37' } 'Copy button failed.'
        Open-Overlay
    }
    $probe::Query($edit, '')
    Require (!$probe::Visible($list) -and $probe::Value($list, 0x018b) -eq 0) 'Clearing the calculation did not hide its result.'
    $probe::Query($edit, $tag)
    Await { $probe::Value($list, 0x018B) -eq 10 } 'Ten test results did not appear.'
    Require ($probe::Visible($list) -and $probe::GetWindowRect($overlay, [ref]$resultBounds) -and
        ($resultBounds.Bottom - $resultBounds.Top) -gt $compactHeight) 'Application results did not expand the launcher.'
    foreach ($row in 0..2) {
        Await { Has-ProbeLogo $row } "The embedded application icon did not reach visible row $row."
    }
    Await-SingleRustartProcess 'visible search with application results and icons'
    Measure-Memory $resident 'Visible search with application results and icons'
    foreach ($id in 110..118) {
        Require ($probe::CanClick($probe::GetDlgItem($overlay, $id))) "Missing action $id."
    }
    $listRect = New-Object DesktopProbe+Rect
    $openRect = New-Object DesktopProbe+Rect
    $folderRect = New-Object DesktopProbe+Rect
    $null = $probe::GetWindowRect($list, [ref]$listRect)
    $null = $probe::GetWindowRect($probe::GetDlgItem($overlay, 110), [ref]$openRect)
    $null = $probe::GetWindowRect($probe::GetDlgItem($overlay, 112), [ref]$folderRect)
    Require ($openRect.Left -gt ($listRect.Left + $listRect.Right) / 2 -and
        $openRect.Right -lt $folderRect.Left -and $folderRect.Right -le $listRect.Right -and
        ($listRect.Right - $folderRect.Right) -lt ($folderRect.Right - $folderRect.Left)) 'Row actions do not occupy the right edge.'
    $probe::Query($edit, $aliasName)
    Require ($probe::Value($list, 0x018b) -eq 1 -and $probe::Row($list, 0) -eq $aliasName) 'Alternate name did not resolve to one program.'
    Press @(0x0d)
    Wait-Launch 'alternate'
    Open-Overlay
    $probe::Query($edit, $tag)
    # Leave the query in place while a nested Start Menu entry is added, renamed, and removed.
    $nested = Join-Path $fixtures 'Новая группа'
    $targetDir = Join-Path $scratch 'Папка программы'
    $null = New-Item -ItemType Directory -Path $nested,$targetDir
    $target = Join-Path $targetDir 'Проверка.exe'
    Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32/where.exe') -Destination $target
    $newLink = Join-Path $nested "$tag 11.lnk"
    $link = $shell.CreateShortcut($newLink)
    $link.TargetPath = $target
    $link.Save()
    Await { $probe::Value($list, 0x018b) -eq 11 -and $probe::Row($list, 10) -eq "$tag 11" } 'Live index did not pick up nested shortcut.'
    Rename-Item -LiteralPath $newLink -NewName "$tag 00.lnk"
    Await { $probe::Row($list, 0) -eq "$tag 00" } 'Live index did not pick up rename.'
    if ($CheckReveal) {
        Check-Folder $targetDir $target
        Open-Overlay
        $probe::Query($edit, $tag)
    }
    Remove-Item -LiteralPath (Join-Path $nested "$tag 00.lnk")
    Await { $probe::Value($list, 0x018b) -eq 10 -and $probe::Row($list, 0) -eq "$tag 01" } 'Live index did not remove deleted shortcut.'
    $bounds = New-Object DesktopProbe+Rect
    $null = $probe::GetClientRect($list, [ref]$bounds)
    Require ($bounds.Bottom -eq (3 * $probe::Value($list, 0x01A1))) 'List does not display exactly three rows.'
    # Start on a secondary row action. Moving the selection must restore the
    # normal Enter action on the newly selected row.
    foreach ($step in 1..4) { Press @(0x09) }
    Require ($probe::FocusId($overlay) -eq 112) 'Tab did not focus the first row folder action.'
    foreach ($i in 1..4) { Press @(0x28) }
    Await { $probe::Value($list, 0x0188) -eq 4 -and $probe::Value($list, 0x018E) -ge 2 } 'Arrow navigation did not scroll.'
    $selectedName = $probe::Row($list, $probe::Value($list, 0x0188))
    Require ($selectedName -eq "$tag 05") "Unexpected fifth result: $selectedName."
    $initialFocus = $probe::FocusId($overlay)
    Press @(0x0d)
    try {
        Wait-Launch '05'
    } catch {
        $markers = (Get-ChildItem -LiteralPath $scratch -Filter 'ran-*.marker').Name -join ','
        if ($probe::GetForegroundWindow() -eq $overlay) { Capture $overlay 'failed-enter' }
        throw "Enter failed on $selectedName (visible=$($probe::Visible($overlay)), markers=$markers, selected=$($probe::Value($list, 0x0188)), focus=$initialFocus to $($probe::FocusId($overlay)))."
    }
    Open-Overlay
    $probe::Query($edit, $tag)
    Click-RowAction 113
    Wait-Launch '02'
    Open-Overlay
    $probe::Query($edit, $tag)
    foreach ($i in 1..6) { Press @(0x28) }
    $top = $probe::Value($list, 0x018e)
    Require ($top -gt 0 -and $top -lt 7) 'No scrolled viewport for row-action test.'
    $expectedRank = ($top + 1).ToString('00')
    Click-RowAction 110
    Wait-Launch $expectedRank
    Open-Overlay
    $probe::Query($edit, $tag)
    Press @(0x12,0x30)
    Press @(0x12,0x39)
    Require ($probe::Visible($overlay) -and $probe::Value($list, 0x0188) -eq 0) 'An Alt+digit shortcut still selected or launched a result.'
    foreach ($i in 1..9) { Press @(0x28) }
    Press @(0x0d)
    Wait-Launch '10'
    Require (!($probe::Visible($overlay))) 'Launcher remained open after launching.'
    $lateLink = Join-Path $nested "$tag 12.lnk"
    $link = $shell.CreateShortcut($lateLink)
    $link.TargetPath = $target
    $link.Save()
    Open-Overlay
    $probe::Query($edit, "$tag 12")
    Await { $probe::Row($list, 0) -eq "$tag 12" } 'Shortcut created while hidden was missing on reopen.'
    Remove-Item -LiteralPath $lateLink
    Await { $probe::Value($list, 0x018b) -eq 0 } 'Deleted shortcut remained visible.'
    $probe::Query($edit, 'к')
    if ($probe::Value($list, 0x018b) -lt 3) { $probe::Query($edit, 'windows') }
    Require ($probe::Value($list, 0x0188) -eq 0 -and $probe::Value($list, 0x018e) -eq 0) 'First search result is outside the viewport.'
    Start-Sleep -Milliseconds 550
    Capture $overlay 'search'
    Press @(0x1b)
    Await { !($probe::Visible($overlay)) } 'Escape did not hide the overlay.'

    # Deliver Explorer's context-menu notification to the real tray callback.
    Open-Tray
    Start-Sleep -Milliseconds 150
    Capture $menu 'tray'
    Require ($probe::FindButton($menu, 'Обновить') -eq 0) 'Tray still exposes a manual refresh action.'
    Require ($probe::FindButton($menu, 'Открыть') -eq 0) 'The tray still offers the Open action.'
    Require ($probe::FindButton($menu, 'Исключения') -eq 0) 'The Russian tray menu still offers Exclusions.'
    Await-MenuRow 'Запускать при входе в Windows'
    Await-MenuRow 'Выйти из Rustart'
    Click-Menu 'Язык'
    Await-MenuRow 'English'
    foreach ($name in @('Как в Windows', 'Deutsch', 'Français', 'Italiano', 'Español', '简体中文', '日本語', 'Português (Brasil)', '한국어', 'Polski', 'Türkçe')) {
        Require ($probe::FindButton($menu,$name) -ne 0) "Missing language choice '$name'."
    }
    Click-Menu 'English'
    Await-MenuRow 'Quit Rustart'
    Require ($probe::FindButton($menu, 'Exclusions') -eq 0) 'The English tray menu still offers Exclusions.'
    $savedSettings = Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json
    Require ($savedSettings.language -eq 'en') 'English was not persisted.'
    Require ($null -eq $savedSettings.PSObject.Properties['exclusions']) 'Saving settings kept the legacy exclusions field.'
    $null = $probe::Send($menu, 0x0010, [IntPtr]0, [IntPtr]0)
    Open-Overlay
    Require ($probe::WindowLabel($list) -eq 'Search results') 'The existing overlay did not switch to English.'
    & $Exe hide
    Open-Tray
    Click-Menu 'Language'
    Await-MenuRow 'Русский'
    Click-Menu 'Русский'
    Await-MenuRow 'Горячая клавиша'
    Require ((Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json).language -eq 'ru') 'Russian was not persisted.'

    Record-Key @(0x1b)
    Await-MenuRow 'Горячая клавиша'
    Require ((Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json).hotkey.vk -eq 0x52) 'Escape changed the shortcut.'
    Require ($probe::RegisterHotKey([IntPtr]::Zero, 4930, 3, 0x86)) 'Could not reserve the conflict fixture.'
    try {
        Record-Key @(0x11,0x12,0x86)
        Await-MenuRow 'Повторить'
        $probe::Chord([byte[]](0x1b))
        Await-MenuRow 'Горячая клавиша'
        Require ((Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json).hotkey.vk -eq 0x52) 'A rejected shortcut overwrote the working binding.'
    } finally {
        $null = $probe::UnregisterHotKey([IntPtr]::Zero,4930)
    }
    # A single key can open the launcher and can still be typed once it is focused.
    Record-Key @(0x51)
    Await-MenuRow 'Горячая клавиша'
    $binding = (Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json).hotkey
    Require ($binding.vk -eq 0x51 -and $binding.modifiers -eq 0) 'A bare key was not saved.'
    $null = $probe::Send($menu, 0x0010, [IntPtr]0, [IntPtr]0)
    $probe::Chord([byte[]](0x51))
    Await { $probe::Visible($overlay) -and $probe::GetForegroundWindow() -eq $overlay -and $probe::FocusId($overlay) -eq 101 } 'The single-key binding did not open and focus search.'
    Require ($probe::Value($edit,0x000e) -eq 0) 'The opening key leaked into the query.'
    $probe::Chord([byte[]](0x51))
    Await { $probe::Value($edit,0x000e) -eq 1 } 'A single-key binding intercepted typing in the search field.'
    & $Exe hide
    Open-Tray
    # A pair of function keys, then a lone modifier. Give forwarded key events
    # a disposable foreground window throughout these custom-binding checks.
    Add-Type -AssemblyName System.Windows.Forms
    $target = [Windows.Forms.Form]::new()
    $target.Text = 'Rustart shortcut test'
    $target.ShowInTaskbar = $false
    $target.FormBorderStyle = 'FixedToolWindow'
    try {
        Record-Key @(0x86,0x87)
        Await-MenuRow 'Горячая клавиша'
        $binding = (Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json).hotkey
        Require ($binding.extra_keys.Count -eq 1) 'The two-key shortcut was not saved.'
        $null = $probe::Send($menu, 0x0010, [IntPtr]0, [IntPtr]0)
        $target.Show()
        $target.Activate()
        [Windows.Forms.Application]::DoEvents()
        Require ($probe::GetForegroundWindow() -eq $target.Handle) 'Shortcut test target did not get focus.'
        $probe::Chord([byte[]](0x86,0x87))
        Await { [Windows.Forms.Application]::DoEvents(); $probe::Visible($overlay) } 'The two-key shortcut did not open search.'
        & $Exe hide
        Open-Tray
        Record-Key @(0x11)
        Await-MenuRow 'Горячая клавиша'
        $binding = (Get-Content -LiteralPath $preferences -Raw | ConvertFrom-Json).hotkey
        Require ($binding.vk -eq 0 -and $binding.modifiers -eq 2) 'The lone Ctrl binding was not saved.'
        $null = $probe::Send($menu, 0x0010, [IntPtr]0, [IntPtr]0)
        $target.Activate()
        [Windows.Forms.Application]::DoEvents()
        Require ($probe::GetForegroundWindow() -eq $target.Handle) 'Shortcut test target lost focus.'
        $probe::Chord([byte[]](0x11,0x86))
        [Windows.Forms.Application]::DoEvents()
        Require (!$probe::Visible($overlay)) 'Ctrl used with another key opened the launcher.'
        $probe::Chord([byte[]](0x11))
        Await { [Windows.Forms.Application]::DoEvents(); $probe::Visible($overlay) } 'A tap of the lone modifier did not open search.'
        & $Exe hide
        foreach ($key in @(0x12,0x5b)) {
            Open-Tray
            Record-Key @($key)
            Await-MenuRow 'Горячая клавиша'
            $null = $probe::Send($menu, 0x0010, [IntPtr]0, [IntPtr]0)
            $target.Activate()
            [Windows.Forms.Application]::DoEvents()
            Require ($probe::GetForegroundWindow() -eq $target.Handle) 'A menu-key test lost foreground focus.'
            $probe::Chord([byte[]]($key))
            Await { [Windows.Forms.Application]::DoEvents(); $probe::GetForegroundWindow() -eq $overlay } 'The assigned menu key did not focus search.'
            Start-Sleep -Milliseconds 450
            Require ($probe::Visible($overlay) -and $probe::GetForegroundWindow() -eq $overlay) 'A system menu took focus after the assigned key opened search.'
            & $Exe hide
        }
    } finally {
        $target.Dispose()
    }
    Open-Tray
    Record-Key @(0x11,0x12,0x52)
    Await-MenuRow 'Горячая клавиша'
    $null = $probe::Send($menu, 0x0010, [IntPtr]0, [IntPtr]0)
    $probe::Chord([byte[]](0x11,0x12,0x52))
    Await { $probe::Visible($overlay) } 'Reassigned shortcut did not open the overlay.'
    if ($CheckCalculatorApp) {
        Require (!(Get-Process CalculatorApp -ErrorAction SilentlyContinue)) 'Close Windows Calculator before its launch test.'
        $calculator = Get-StartApps | Where-Object AppId -eq 'Microsoft.WindowsCalculator_8wekyb3d8bbwe!App' | Select-Object -First 1
        Require ($null -ne $calculator) 'Windows Calculator must be installed for this optional test.'
        $probe::Query($edit, $calculator.Name)
        Require ($probe::Row($list, 0) -eq $calculator.Name) 'Packaged Calculator is not first.'
        if ($CheckReveal) {
            $package = Get-AppxPackage -Name Microsoft.WindowsCalculator | Select-Object -First 1
            Require (![string]::IsNullOrEmpty($package.InstallLocation)) 'Calculator install directory unavailable.'
            Check-Folder $package.InstallLocation '' $calculator.AppId
            Open-Overlay
            $probe::Query($edit, $calculator.Name)
        }
        Press @(0x0d)
        Await { $script:calcProcess = Get-Process CalculatorApp -ErrorAction SilentlyContinue | Select-Object -First 1; $null -ne $calcProcess } 'Packaged Calculator did not start.'
        Await { $calcProcess.Refresh(); $calcProcess.MainWindowHandle -ne 0 } 'Calculator window unavailable.'
        Require ($calcProcess.CloseMainWindow()) 'Unable to close the test Calculator window.'
    }
    & $Exe hide
    # Exercise lifetime of GDI fonts/brushes and transient HWNDs after warmup.
    foreach ($cycle in 0..16) {
        Open-Overlay
        $probe::Query($edit, '1+1')
        & $Exe hide
        Open-Tray
        $null = $probe::Send($menu, 0x0010, [IntPtr]0, [IntPtr]0)
        Require ($probe::FindClass('Rustart.TrayMenu') -eq 0) 'Tray menu did not release its window.'
        if ($cycle -eq 0) {
            $gdi = $probe::GetGuiResources($resident.Handle, 0)
            $userObjects = $probe::GetGuiResources($resident.Handle, 1)
        }
    }
    $endGdi = $probe::GetGuiResources($resident.Handle, 0)
    $endUser = $probe::GetGuiResources($resident.Handle, 1)
    Require ($endGdi -le $gdi + 3) "GDI objects accumulate after closing UI ($gdi -> $endGdi)."
    Require ($endUser -le $userObjects + 3) "USER objects accumulate after closing UI ($userObjects -> $endUser)."
    Await-SingleRustartProcess 'warm tray'
    $hiddenIdle = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Milliseconds 2500
    Measure-Memory $resident 'Warm tray after 2.5 seconds hidden'
    $remainingIdleMs = [math]::Max(0, 13500 - $hiddenIdle.ElapsedMilliseconds)
    Start-Sleep -Milliseconds $remainingIdleMs
    Require (!$probe::Visible($overlay)) 'The launcher appeared during its longer tray idle.'
    Await-SingleRustartProcess 'long warm tray idle'
    Measure-Memory $resident 'Warm tray after 13.5 seconds hidden'
    $hiddenSeconds = $hiddenIdle.Elapsed.TotalSeconds
    $longIdleTiming = $probe::MeasureWarmSearch($tag, "$tag 01", [byte[]](0x11,0x12))
    [pscustomobject]@{
        Phase = 'Reopen after long warm tray idle'
        HiddenSeconds = [math]::Round($hiddenSeconds, 2)
        HotkeyToEditFocusMs = [math]::Round($longIdleTiming.HotkeyToEditFocusMs, 2)
        QueryToFirstRowMs = [math]::Round($longIdleTiming.QueryToFirstRowMs, 2)
    }
    & $Exe hide
    Require ($LASTEXITCODE -eq 0) 'Could not hide the launcher after the long-idle trial.'
    Await { !$probe::Visible($overlay) } 'The launcher remained visible after the long-idle trial.'
    Start-Sleep -Milliseconds 2500
    # Time observable focus and list readiness, not the compositor or CLI.
    # The settings test has reassigned the launcher shortcut to Ctrl+Alt+R.
    foreach ($trial in 1..3) {
        $timing = $probe::MeasureWarmSearch($tag, "$tag 01", [byte[]](0x11,0x12))
        [pscustomobject]@{
            Trial = $trial
            HotkeyToEditFocusMs = [math]::Round($timing.HotkeyToEditFocusMs, 2)
            QueryToFirstRowMs = [math]::Round($timing.QueryToFirstRowMs, 2)
        }
        & $Exe hide
        Require ($LASTEXITCODE -eq 0) 'Could not hide the launcher after the timing trial.'
        Await { !$probe::Visible($probe::FindClass('Rustart.Overlay')) } 'Launcher remained visible after a timing trial.'
        Start-Sleep -Milliseconds 2500
    }
    $resident.Refresh()
    $usage = $resident.TotalProcessorTime
    $watch = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds 5
    $resident.Refresh()
    [pscustomobject]@{
        GdiAfterFirstCycle = $gdi
        GdiAfterCycles = $endGdi
        UserAfterFirstCycle = $userObjects
        UserAfterCycles = $endUser
        IdleCpuMs = ($resident.TotalProcessorTime - $usage).TotalMilliseconds
        IdleSeconds = [math]::Round($watch.Elapsed.TotalSeconds,2)
    }
    Open-Tray
    Click-Menu 'Горячая клавиша'
    Await-MenuRow 'Нажмите клавишу'
    & $Exe quit
    Require ($LASTEXITCODE -eq 0 -and $resident.WaitForExit(5000) -and (& $Exe status) -eq 'stopped') 'Resident did not exit during shortcut recording.'
    $started = $false
    if ($CheckRunDialog) {
        $titles = @('Выполнить','Run','Ausführen')
        Require (!($titles | Where-Object { $probe::Visible($probe::FindWindow('#32770', $_)) })) 'Run dialog was already open.'
        $probe::Chord([byte[]](0x5b,0x52))
        Await { $titles | Where-Object { $probe::FindWindow('#32770', $_) -eq $probe::GetForegroundWindow() -and $probe::GetForegroundWindow() -ne 0 } } 'Win+R did not return to Windows.'
        $probe::Chord([byte[]](0x1b))
    }
} finally {
    if ($started) { & $Exe quit }
    $env:LOCALAPPDATA = $previousLocal
    foreach ($entry in @(@($programs,$fixtures),@($tempRoot,$scratch))) {
        $folder = [IO.Path]::GetFullPath($entry[1])
        $expected = [IO.Path]::GetFullPath([IO.Path]::Combine($entry[0],$tag))
        if ($folder -eq $expected -and (Test-Path -LiteralPath $folder)) {
            $item = Get-Item -LiteralPath $folder
            if (!($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                # Windows can release a just-launched image after its process
                # has published the marker. Retry only the validated fixtures.
                $cleanupDeadline = [DateTime]::UtcNow.AddSeconds(8)
                do {
                    try {
                        Remove-Item -LiteralPath $item.FullName -Recurse -Force -ErrorAction Stop
                        break
                    } catch {
                        if ([DateTime]::UtcNow -ge $cleanupDeadline) { throw }
                        Start-Sleep -Milliseconds 120
                    }
                } while (Test-Path -LiteralPath $item.FullName)
            }
        }
    }
}

exit 0
