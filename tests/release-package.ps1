#requires -Version 7.0
[CmdletBinding()]
param([switch]$CheckCleanInstall)

$ErrorActionPreference = 'Stop'

function Invoke-NoConsole([string]$Image, [string]$WorkingDirectory, [string[]]$Arguments, [string]$Scratch) {
    $start = [Diagnostics.ProcessStartInfo]::new($Image)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.WorkingDirectory = $WorkingDirectory
    $start.Environment['TEMP'] = $Scratch
    $start.Environment['TMP'] = $Scratch
    foreach ($arg in $Arguments) { $start.ArgumentList.Add($arg) }

    $child = [Diagnostics.Process]::Start($start)
    try {
        if (-not $child.WaitForExit(30000)) {
            $child.Kill($true)
            $null = $child.WaitForExit(5000)
            throw "The launch without a console did not exit: $Image"
        }
        $child.ExitCode
    } finally {
        $child.Dispose()
    }
}

if ($CheckCleanInstall -and ($env:GITHUB_ACTIONS -cne 'true' -or $env:RUNNER_ENVIRONMENT -cne 'github-hosted')) {
    throw 'The clean install check requires a disposable GitHub-hosted runner.'
}

$tempRoot = (Get-Item -LiteralPath ([IO.Path]::GetTempPath())).FullName.TrimEnd('\')
$name = 'rustart-release-test-' + [guid]::NewGuid().ToString('N')
$root = Join-Path $tempRoot $name
$null = New-Item -ItemType Directory -Path $root
try {
    $source = Join-Path $root 'Fixture.cs'
    $fixture = Join-Path $root 'test payload.exe'
    $package = Join-Path $root 'download/rustart.exe'
    $scratch = (New-Item -ItemType Directory -Path (Join-Path $root 'temporary files')).FullName
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    try {
        $principal = [Security.Principal.WindowsPrincipal]::new($identity)
        $elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        $expectedStageRoot = if ($elevated) {
            [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
        } else {
            $scratch
        }
    } finally {
        $identity.Dispose()
    }
    @'
using System;
using System.IO;
using System.Reflection;
using System.Text;

static class Fixture {
    static int Main(string[] args) {
        if (args.Length == 0) {
            File.WriteAllText("shell-launch.txt", Assembly.GetExecutingAssembly().Location);
            return unchecked((int)0x87654321);
        }
        Console.WriteLine(Convert.ToBase64String(Encoding.UTF8.GetBytes(Assembly.GetExecutingAssembly().Location)));
        foreach (string arg in args)
            Console.WriteLine("arg=" + Convert.ToBase64String(Encoding.UTF8.GetBytes(arg)));
        Console.WriteLine("stdin=" + Console.ReadLine());
        Console.WriteLine("cwd=" + Convert.ToBase64String(Encoding.UTF8.GetBytes(Environment.CurrentDirectory)));
        Console.WriteLine("origin=" + Convert.ToBase64String(Encoding.UTF8.GetBytes(Environment.GetEnvironmentVariable("RUSTART_PACKAGE_ORIGIN") ?? "")));
        Console.Error.WriteLine("fixture stderr");
        return unchecked((int)0x87654321);
    }
}
'@ | Set-Content -LiteralPath $source -Encoding utf8
    $compiler = Join-Path $env:SystemRoot 'Microsoft.NET/Framework64/v4.0.30319/csc.exe'
    & $compiler /nologo /target:exe "/out:$fixture" $source
    if ($LASTEXITCODE -ne 0) { throw 'Could not compile the release test fixture.' }
    # Make the small fixture compressible enough to include the launcher overhead.
    $file = [IO.File]::OpenWrite($fixture)
    try { $file.SetLength($file.Length + 1MB) } finally { $file.Dispose() }

    $null = New-Item -ItemType Directory -Path (Split-Path -Parent $package)
    [IO.File]::WriteAllText($package, 'previous release')
    & (Join-Path $PSScriptRoot '../scripts/package-release.ps1') -InputExe $fixture -OutputExe $package
    if ((Get-Item -LiteralPath $package).Length -ge (Get-Item -LiteralPath $fixture).Length) {
        throw 'The downloadable EXE did not shrink.'
    }
    $arguments = @('--help', '', 'two words', 'say "hello"', 'path with spaces\', 'окно 窗口')
    $start = [Diagnostics.ProcessStartInfo]::new($package)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.WorkingDirectory = $root
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.Environment['TEMP'] = $scratch
    $start.Environment['TMP'] = $scratch
    foreach ($arg in $arguments) { $start.ArgumentList.Add($arg) }

    $child = [Diagnostics.Process]::Start($start)
    try {
        $stdout = $child.StandardOutput.ReadToEndAsync()
        $stderr = $child.StandardError.ReadToEndAsync()
        $child.StandardInput.WriteLine('fixture stdin')
        $child.StandardInput.Close()
        if (-not $child.WaitForExit(60000)) {
            $child.Kill($true)
            $child.WaitForExit()
            throw 'The packaged test did not exit.'
        }
        if ($child.ExitCode -ne 0x87654321) { throw "Unexpected exit code $($child.ExitCode)." }
        if ($stderr.Result -cne "fixture stderr`r`n") { throw "Unexpected stderr: $($stderr.Result)" }
        $lines = $stdout.Result -split '\r?\n'
        if ($lines.Count -ne $arguments.Count + 5 -or $lines[-1] -ne '') {
            throw "Unexpected stdout: $($stdout.Result)"
        }
        for ($i = 0; $i -lt $arguments.Count; $i++) {
            $expected = 'arg=' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($arguments[$i]))
            if ($lines[$i + 1] -cne $expected) { throw "CLI argument $i changed during launch." }
        }
        if ($lines[-4] -cne 'stdin=fixture stdin' -or
            $lines[-3] -cne ('cwd=' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($root))) -or
            $lines[-2] -cne ('origin=' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((Get-Item -LiteralPath $package).FullName)))) {
            throw "Unexpected stdin, working directory or package origin: $($stdout.Result)"
        }
        $extracted = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($lines[0]))
        $extractedDir = [IO.Path]::GetDirectoryName($extracted)
        $stageRoot = [IO.Path]::GetDirectoryName($extractedDir)
        if ($stageRoot.StartsWith('\\?\')) { $stageRoot = $stageRoot.Substring(4) }
        if ([IO.Path]::GetFileName($extracted) -ine 'rustart.exe' -or
            $stageRoot -ine $expectedStageRoot -or
            [IO.File]::Exists($extracted) -or [IO.Directory]::Exists($extractedDir)) {
            throw "The temporary executable or its directory was not cleaned up: $extracted"
        }
    } finally {
        $child.Dispose()
    }
    Write-Output 'PASS  LZX release package forwards arguments and all standard streams, preserves the exit status, then cleans up.'

    $exitCode = Invoke-NoConsole -Image $package -WorkingDirectory $root -Scratch $scratch
    if ($exitCode -ne 0x87654321) { throw "Launch without a console returned exit code $exitCode." }
    $extracted = [IO.File]::ReadAllText((Join-Path $root 'shell-launch.txt'))
    $extractedDir = [IO.Path]::GetDirectoryName($extracted)
    if ([IO.File]::Exists($extracted) -or [IO.Directory]::Exists($extractedDir)) {
        throw "Launch without a console did not clean up its temporary executable: $extracted"
    }
    Write-Output 'PASS  LZX release package starts without arguments or a console, then cleans up.'

    if ($CheckCleanInstall) {
        if (-not $elevated) { throw 'The clean install check requires an elevated runner.' }
        $installed = Join-Path $expectedStageRoot 'CKATEPTb/Rustart/rustart.exe'
        if (Test-Path -LiteralPath $installed) { throw "Rustart is already installed at $installed." }
        $release = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../dist/rustart.exe')).ProviderPath
        $raw = Join-Path $PSScriptRoot '../target/x86_64-pc-windows-msvc/release/rustart.exe'
        $expectedHash = (Get-FileHash -LiteralPath $raw -Algorithm SHA256).Hash
        $exitCode = Invoke-NoConsole -Image $release -WorkingDirectory $root -Arguments @('--version') -Scratch $scratch
        if ($exitCode -ne 0) { throw "The packaged --version clean install returned exit code $exitCode." }
        if ((Get-FileHash -LiteralPath $installed -Algorithm SHA256).Hash -ne $expectedHash) {
            throw 'The installed executable differs from the raw release executable.'
        }
        Write-Output 'PASS  LZX release package installs from a launch without a console and runs the installed executable.'
    }
} finally {
    $created = Get-Item -LiteralPath $root
    if ($created.Parent.FullName -ne $tempRoot -or $created.Name -ne $name -or
        ($created.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Unexpected fixture directory during cleanup.'
    }
    Remove-Item -LiteralPath $created.FullName -Recurse -Force
}
