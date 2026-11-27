#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
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
        $expectedStageRoot = if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
            [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
        } else {
            $scratch
        }
    } finally {
        $identity.Dispose()
    }
    @'
using System;
using System.Reflection;
using System.Text;

static class Fixture {
    static int Main(string[] args) {
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
} finally {
    $created = Get-Item -LiteralPath $root
    if ($created.Parent.FullName -ne $tempRoot -or $created.Name -ne $name -or
        ($created.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Unexpected fixture directory during cleanup.'
    }
    Remove-Item -LiteralPath $created.FullName -Recurse -Force
}
