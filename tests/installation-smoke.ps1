# Run in a free, interactive Windows session. Only installation-fixture builds are accepted.
[CmdletBinding()]
param([string]$Exe)

$ErrorActionPreference = 'Stop'
$temp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
$rootName = 'rustart-install-test-' + [guid]::NewGuid().ToString('N')
$root = Join-Path $temp $rootName
$programFiles = Join-Path $root 'Program Files'
$localAppData = Join-Path $root 'LocalAppData'
$client = Join-Path $root 'client\rustart.exe'
$portable = Join-Path $root 'portable\rustart.exe'
$update = Join-Path $root 'update\rustart.exe'
$installed = Join-Path $programFiles 'CKATEPTb\Rustart\rustart.exe'
$fixtureImages = @($client, $portable, $update, $installed)
$sessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId

function Require($condition, [string]$message) {
    if (!$condition) { throw $message }
}

function Invoke-Fixture([string]$image, [string]$arguments, [switch]$Portable) {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $image
    $info.Arguments = $arguments
    $info.WorkingDirectory = $root
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.EnvironmentVariables['LOCALAPPDATA'] = $localAppData
    $info.EnvironmentVariables['TEMP'] = $temp
    $info.EnvironmentVariables['TMP'] = $temp
    $info.EnvironmentVariables.Remove('RUSTART_PACKAGE_ORIGIN')
    if ($Portable) {
        $info.EnvironmentVariables.Remove('RUSTART_FIXTURE_PROGRAM_FILES')
    } else {
        $info.EnvironmentVariables['RUSTART_FIXTURE_PROGRAM_FILES'] = $programFiles
    }
    $child = [Diagnostics.Process]::Start($info)
    try {
        $output = $child.StandardOutput.ReadToEndAsync()
        $diagnostics = $child.StandardError.ReadToEndAsync()
        if (!$child.WaitForExit(45000)) {
            $child.Kill()
            throw "Fixture command '$arguments' did not exit."
        }
        if (![Threading.Tasks.Task]::WaitAll(@($output, $diagnostics), 5000)) {
            throw "Fixture command '$image $arguments' did not close its output streams."
        }
        [pscustomobject]@{
            Code = $child.ExitCode
            Stdout = $output.Result.TrimEnd()
            Stderr = $diagnostics.Result.TrimEnd()
        }
    } finally {
        $child.Dispose()
    }
}

function Invoke-Success([string]$image, [string]$arguments, [switch]$Portable) {
    $result = Invoke-Fixture $image $arguments -Portable:$Portable
    Require ($result.Code -eq 0 -and $result.Stderr -eq '') (
        "'$arguments' exited $($result.Code): $($result.Stderr)")
    $result.Stdout
}

function Get-ImagePath([Diagnostics.Process]$process) {
    $path = $process.MainModule.FileName
    if ($path.StartsWith('\\?\', [StringComparison]::Ordinal)) {
        $path = $path.Substring(4)
    }
    [IO.Path]::GetFullPath($path)
}

function Get-FixtureOwner([string]$expectedImage) {
    $state = Invoke-Success $client 'status' -Portable
    if ($state -cnotmatch '^running ([1-9][0-9]*)$') {
        throw "Expected a running instance, got '$state'."
    }
    $process = [Diagnostics.Process]::GetProcessById([int]$Matches[1])
    try {
        $null = $process.Handle
        Require (!$process.HasExited -and $process.SessionId -eq $sessionId) (
            "The reported owner $($process.Id) is not live in the test session.")
        $actual = Get-ImagePath $process
        Require ($actual -eq $expectedImage) "Expected owner '$expectedImage', got '$actual'."
        $process
    } catch {
        $process.Dispose()
        throw
    }
}

function Assert-InstalledHash([string]$sourceHash) {
    Require ([IO.File]::Exists($installed)) 'The installed executable is missing.'
    Require ((Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash -eq $sourceHash) (
        'Installed bytes differ from the source image.')
}

function Remove-FixtureProcesses {
    foreach ($process in [Diagnostics.Process]::GetProcessesByName('rustart')) {
        try {
            if ($process.SessionId -ne $sessionId) { continue }
            # Hold a handle and verify the actual image before stopping any process.
            $null = $process.Handle
            if (!$process.HasExited -and (Get-ImagePath $process) -in $fixtureImages) {
                $process.Kill()
                $null = $process.WaitForExit(5000)
            }
        } catch {
            Write-Verbose "Could not inspect or stop process $($process.Id): $_"
        } finally {
            $process.Dispose()
        }
    }
}

Require ([Environment]::UserInteractive -and $sessionId -gt 0) 'Use a free, interactive Windows session.'
$null = New-Item -ItemType Directory -Path $root
$owners = @()
try {
    foreach ($directory in @($programFiles, (Join-Path $localAppData 'Rustart'),
            (Split-Path -Parent $client), (Split-Path -Parent $portable), (Split-Path -Parent $update))) {
        $null = New-Item -ItemType Directory -Path $directory -Force
    }

    if (!$Exe) {
        $target = Join-Path $root 'target'
        $manifest = Join-Path $PSScriptRoot '..\Cargo.toml'
        $cargoCommand = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        $cargo = if ($cargoCommand) { $cargoCommand.Source } else { Join-Path $HOME '.cargo\bin\cargo.exe' }
        Require ([IO.File]::Exists($cargo)) 'Cargo is required when -Exe is omitted.'
        & $cargo build --manifest-path $manifest --target-dir $target --locked --release --features installation-fixture
        Require ($LASTEXITCODE -eq 0) 'Could not build the installation fixture.'
        $Exe = Join-Path $target 'release\rustart.exe'
    }
    Copy-Item -LiteralPath (Resolve-Path -LiteralPath $Exe).Path -Destination $client
    $marker = Invoke-Fixture $client '--installation-fixture' -Portable
    Require ($marker.Code -eq 0 -and $marker.Stdout -ceq 'rustart-installation-fixture-v1' -and
        $marker.Stderr -eq '') 'Provide a raw Rustart executable built with --features installation-fixture.'

    $state = Invoke-Fixture $client 'status' -Portable
    Require ($state.Code -eq 1 -and $state.Stdout -ceq 'stopped' -and $state.Stderr -eq '') (
        'Close the existing Rustart instance before running this test.')

    $settings = Join-Path $localAppData 'Rustart\settings.json'
    [IO.File]::WriteAllText($settings, '{"language":"en","hotkey":{"modifiers":3,"vk":135}}')
    $settingsHash = (Get-FileHash -LiteralPath $settings -Algorithm SHA256).Hash
    $sourceHash = (Get-FileHash -LiteralPath $client -Algorithm SHA256).Hash

    # Stale sign-in registrations must never install an EXE or request UAC.
    foreach ($flag in @('--user-logon', '--admin-logon')) {
        $null = Invoke-Success $client $flag
        Require (![IO.File]::Exists($installed)) "A stale $flag entry installed Rustart."
    }

    # Start the pre-installation tray with the fixture install override absent.
    Copy-Item -LiteralPath $client -Destination $portable
    $null = Invoke-Success $portable 'background' -Portable
    $oldOwner = Get-FixtureOwner $portable
    $owners += $oldOwner
    Require (![IO.File]::Exists($installed)) 'The portable resident performed an installation.'

    $null = Invoke-Success $client 'background'
    $firstOwner = Get-FixtureOwner $installed
    $owners += $firstOwner
    Assert-InstalledHash $sourceHash
    Require ($oldOwner.WaitForExit(5000) -and $firstOwner.Id -ne $oldOwner.Id) (
        'Installation did not hand off the portable resident.')

    # The test client runs without an interactive shell. Activate the fixture
    # with its isolated Ctrl+Alt+F24 binding before testing the no-argument
    # entry point; a background process cannot grant itself foreground rights.
    Add-Type -Path (Join-Path $PSScriptRoot 'DesktopProbe.cs')
    [DesktopProbe]::Chord([byte[]](0x11, 0x12, 0x87))
    $deadline = [DateTime]::UtcNow.AddSeconds(8)
    do {
        $overlay = [DesktopProbe]::FindClass('Rustart.Overlay')
        if ($overlay -ne [IntPtr]::Zero -and [DesktopProbe]::GetForegroundWindow() -eq $overlay) { break }
        Start-Sleep -Milliseconds 40
    } while ([DateTime]::UtcNow -lt $deadline)
    Require ($overlay -ne [IntPtr]::Zero -and [DesktopProbe]::GetForegroundWindow() -eq $overlay) (
        'The installed fixture did not receive foreground focus.')
    $null = Invoke-Success $installed ''
    $null = Invoke-Success $installed 'hide'
    Require ((Invoke-Success $installed 'status') -ceq "running $($firstOwner.Id)") (
        'Opening the installed executable replaced the tray instance.')

    $null = Invoke-Success $client 'background'
    $repeated = Get-FixtureOwner $installed
    $owners += $repeated
    Require ($repeated.Id -eq $firstOwner.Id -and !$firstOwner.HasExited) (
        'Launching the same release replaced the installed resident.')
    Assert-InstalledHash $sourceHash

    # A PE overlay changes the on-disk image hash without changing loader sections.
    Copy-Item -LiteralPath $client -Destination $update
    $overlay = [Text.Encoding]::ASCII.GetBytes("rustart install smoke $rootName")
    $file = [IO.File]::Open($update, [IO.FileMode]::Append)
    try { $file.Write($overlay, 0, $overlay.Length) } finally { $file.Dispose() }
    $updateHash = (Get-FileHash -LiteralPath $update -Algorithm SHA256).Hash
    Require ($updateHash -ne $sourceHash -and !$firstOwner.HasExited) (
        'The update needs different bytes and a live installed owner.')
    Require ((Invoke-Success $update 'calc 6*7') -ceq '42') 'Updating via a CLI command lost its output.'
    $newOwner = Get-FixtureOwner $installed
    $owners += $newOwner
    Assert-InstalledHash $updateHash
    Require ($firstOwner.WaitForExit(5000) -and $newOwner.Id -ne $firstOwner.Id) (
        'Updating the running installation did not replace its owner.')

    Require ((Get-FileHash -LiteralPath $settings -Algorithm SHA256).Hash -eq $settingsHash) (
        'Installation or update changed the existing settings.')
    Require ((Invoke-Success $update 'calc 6*7') -ceq '42') 'Unexpected calculator stdout.'
    $invalid = Invoke-Fixture $update 'calc'
    Require ($invalid.Code -eq 1 -and $invalid.Stdout -eq '' -and $invalid.Stderr -cmatch '^Rustart: .+') (
        'Invalid CLI input did not return exit code 1 and a diagnostic on stderr.')
    Assert-InstalledHash $updateHash
    $null = Invoke-Success $installed 'quit'
    Require ($newOwner.WaitForExit(5000)) 'The installed resident did not quit.'

    Write-Output 'Installation smoke test passed.'
} finally {
    foreach ($owner in $owners) { $owner.Dispose() }
    Remove-FixtureProcesses
    $item = Get-Item -LiteralPath $root
    Require ($item.Parent.FullName -eq $temp -and $item.Name -cmatch '^rustart-install-test-[0-9a-f]{32}$' -and
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) 'Unexpected fixture root during cleanup.'
    Remove-Item -LiteralPath $item.FullName -Recurse -Force
}
