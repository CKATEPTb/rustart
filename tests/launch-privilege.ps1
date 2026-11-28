# Run from a console. An already elevated console exercises Explorer delegation.
[CmdletBinding()]
param(
    [string]$Exe = (Join-Path $PSScriptRoot '../target/release/rustart.exe'),
    [switch]$RequireHigh,
    [switch]$CheckConsent
)
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
function Require($condition, [string]$message) { if (!$condition) { throw $message } }

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class RustartPrivilegeShell {
    [DllImport("user32")] static extern IntPtr GetShellWindow();
    [DllImport("user32")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    public static uint ProcessId() {
        uint pid;
        GetWindowThreadProcessId(GetShellWindow(), out pid);
        return pid;
    }
}
'@
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
$high = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($RequireHigh) { Require $high 'Run this test from an already elevated console.' }
$shellPid = [RustartPrivilegeShell]::ProcessId()
Require ($shellPid -ne 0) 'A desktop shell is required for the privilege probe.'
$shell = Get-CimInstance Win32_Process -Filter "ProcessId = $shellPid"
$shellSid = (Invoke-CimMethod -InputObject $shell -MethodName GetOwnerSid).Sid
Require ($shellSid -eq $identity.User.Value) 'This fixture requires a split-token account whose desktop and console share a SID.'

function Read-Exact([IO.Stream]$stream, [int]$length) {
    $data = [byte[]]::new($length)
    $offset = 0
    $deadline = [datetime]::UtcNow.AddSeconds($(if ($CheckConsent) { 180 } else { 35 }))
    while ($offset -lt $length) {
        $remaining = [math]::Max(1, ($deadline - [datetime]::UtcNow).TotalMilliseconds)
        $read = $stream.ReadAsync($data, $offset, $length - $offset)
        Require ($read.Wait([int]$remaining)) 'Timed out receiving the shell worker response.'
        Require ($read.Result -gt 0) 'The shell worker response ended early.'
        $offset += $read.Result
        Require ([datetime]::UtcNow -le $deadline) 'The shell worker response exceeded its deadline.'
    }
    return ,$data
}

function Invoke-Launch([string]$source, [bool]$elevated = $false) {
    $info = [Diagnostics.ProcessStartInfo]::new($Exe, '--shell-action-worker')
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.EnvironmentVariables['RUSTART_WORKER_LANGUAGE'] = 'en'
    $process = [Diagnostics.Process]::Start($info)
    try {
        $request = @{ operation = @{ Launch = $elevated }; name = 'Privilege fixture'; source = @{ Desktop = $source } }
        $body = [Text.Encoding]::UTF8.GetBytes(($request | ConvertTo-Json -Compress -Depth 4))
        $inputPipe = $process.StandardInput.BaseStream
        $inputPipe.WriteByte(1)
        $size = [BitConverter]::GetBytes([int]$body.Length)
        $inputPipe.Write($size, 0, 4)
        $inputPipe.Write($body, 0, $body.Length)
        $inputPipe.Flush()
        $process.StandardInput.Close()
        $pipe = $process.StandardOutput.BaseStream
        $header = Read-Exact $pipe 5
        Require ($header[0] -eq 1) 'Unexpected shell worker reply protocol.'
        $length = [BitConverter]::ToInt32($header, 1)
        Require ($length -gt 0 -and $length -le 65536) 'Invalid shell worker response size.'
        $reply = [Text.Encoding]::UTF8.GetString((Read-Exact $pipe $length)) | ConvertFrom-Json
        Require ($process.WaitForExit(35000) -and $process.ExitCode -eq 0) 'The shell-action worker did not exit successfully.'
        $reply
    } finally {
        if (!$process.HasExited) { $process.Kill() }
        $process.Dispose()
    }
}

$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$tag = 'RustartPrivilege-' + [guid]::NewGuid().ToString('N')
$scratch = Join-Path $tempRoot $tag
$null = [IO.Directory]::CreateDirectory($scratch)
try {
    $compiler = Join-Path $env:SystemRoot 'Microsoft.NET\Framework\v4.0.30319\csc.exe'
    $probeSource = Join-Path $PSScriptRoot 'LaunchProbe.cs'
    $desktop = Join-Path $scratch 'Normal Probe.exe'
    $console = Join-Path $scratch 'Console Probe.exe'
    $administrator = Join-Path $scratch 'Admin Probe.exe'
    $manifest = Join-Path $scratch 'admin.manifest'
    @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges>
    <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
  </requestedPrivileges></security></trustInfo>
</assembly>
'@ | Set-Content -LiteralPath $manifest -Encoding utf8
    & $compiler /nologo /target:winexe "/out:$desktop" $probeSource
    Require ($LASTEXITCODE -eq 0) 'Failed to compile the GUI privilege fixture.'
    & $compiler /nologo /target:exe "/out:$console" $probeSource
    Require ($LASTEXITCODE -eq 0) 'Failed to compile the console privilege fixture.'
    & $compiler /nologo /target:winexe "/win32manifest:$manifest" "/out:$administrator" $probeSource
    Require ($LASTEXITCODE -eq 0) 'Failed to compile the manifest-requiring-admin fixture.'
    $working = Join-Path $scratch 'Working Directory'
    $null = [IO.Directory]::CreateDirectory($working)
    $wsh = New-Object -ComObject WScript.Shell

    function New-ProbeLink([string]$name, [string]$target) {
        $path = Join-Path $scratch "$name.lnk"
        $link = $wsh.CreateShortcut($path)
        $link.TargetPath = $target
        $link.WorkingDirectory = $working
        $link.WindowStyle = 7
        $link.Arguments = '{0} --privilege-probe "value with spaces & ampersand" second' -f $name
        $link.Save()
        $path
    }
    foreach ($item in @(@('normal', $desktop, $false), @('console', $console, $true))) {
        $link = New-ProbeLink $item[0] $item[1]
        $reply = Invoke-Launch $link
        Require ($reply.PSObject.Properties.Name -contains 'Ok') "Normal launch failed for $($item[0]): $($reply.Err)"
        $marker = Join-Path $scratch "ran-$($item[0]).marker"
        $deadline = [datetime]::UtcNow.AddSeconds(10)
        while (![IO.File]::Exists($marker) -and [datetime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 20 }
        Require ([IO.File]::Exists($marker)) "Missing token result for $($item[0])."
        $result = @{}
        foreach ($line in Get-Content -LiteralPath $marker) {
            $key, $value = $line -split '=', 2
            $result[$key] = $value
        }
        Require ($result.Elevation -eq '0' -and $result.Integrity -eq '8192' -and $result.Sid -eq $shellSid) 'Target token is not the verified interactive medium user.'
        Require ($result.WorkingDirectory -eq $working) 'Shortcut working directory was not preserved.'
        Require ($result.Arguments -eq 'value with spaces & ampersand|second') 'Shortcut argument quoting was not preserved.'
        Require ($result.ShowWindow -eq '7') 'Shortcut show command was not preserved.'
        if ($item[2]) {
            Require ($result.ConsoleInput -eq 'True' -and $result.ConsoleOutput -eq 'True' -and $result.ConsoleWindow -eq 'True') 'Console fixture lacks a separate, usable console.'
        }
        [pscustomobject]@{ Fixture = $item[0]; CallerHigh = $high; TargetElevation = $result.Elevation; TargetIntegrity = $result.Integrity;
            TargetSid = $result.Sid; WorkingDirectory = $result.WorkingDirectory; Arguments = $result.Arguments; ShowWindow = $result.ShowWindow;
            ConsoleInput = $result.ConsoleInput; ConsoleOutput = $result.ConsoleOutput; ConsoleWindow = $result.ConsoleWindow }
    }

    $adminCases = @()
    if ($CheckConsent) {
        # Interactive check: approve both Windows consent prompts.
        $adminCases += [pscustomobject]@{ Link = New-ProbeLink 'requires-admin' $administrator; Elevated = $false }
        $flagged = New-ProbeLink 'runas-shortcut' $desktop
        $bytes = [IO.File]::ReadAllBytes($flagged)
        Require ($bytes.Length -ge 24 -and [BitConverter]::ToInt32($bytes, 0) -eq 76) 'Unexpected shortcut header.'
        $flags = [BitConverter]::GetBytes([uint32]([BitConverter]::ToUInt32($bytes, 20) -bor 0x2000))
        [Array]::Copy($flags, 0, $bytes, 20, 4)
        [IO.File]::WriteAllBytes($flagged, $bytes)
        $adminCases += [pscustomobject]@{ Link = $flagged; Elevated = $false }
    }
    if ($RequireHigh) {
        $adminCases += [pscustomobject]@{ Link = New-ProbeLink 'explicit-admin' $administrator; Elevated = $true }
    }
    foreach ($case in $adminCases) {
        $name = [IO.Path]::GetFileNameWithoutExtension($case.Link)
        $reply = Invoke-Launch $case.Link $case.Elevated
        Require ($reply.PSObject.Properties.Name -contains 'Ok') "Administrator launch failed for ${name}: $($reply.Err)"
        $marker = Join-Path $scratch "ran-$name.marker"
        $deadline = [datetime]::UtcNow.AddSeconds(10)
        while (![IO.File]::Exists($marker) -and [datetime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 20 }
        Require ([IO.File]::Exists($marker)) "Missing token for $name."
        $result = @{}
        foreach ($line in Get-Content -LiteralPath $marker) {
            $key, $value = $line -split '=', 2
            $result[$key] = $value
        }
        Require ($result.Elevation -eq '1' -and [int]$result.Integrity -ge 12288) "The $name fixture did not use a high token."
        [pscustomobject]@{ Fixture = $name; CallerHigh = $high; TargetElevation = $result.Elevation;
            TargetIntegrity = $result.Integrity; TargetSid = $result.Sid }
    }
} finally {
    $expected = [IO.Path]::GetFullPath((Join-Path $tempRoot $tag))
    if ((Resolve-Path -LiteralPath $scratch).Path -eq $expected -and
        !((Get-Item -LiteralPath $scratch).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
