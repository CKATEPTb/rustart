# By default, validate the production logon definition in Windows Task Scheduler.
# Use -Installed to inspect the current account's existing enabled task and ACL.
# Use -CheckRegistration from an already elevated console to verify a disabled fixture.
[CmdletBinding(DefaultParameterSetName = 'Validate')]
param(
    [Parameter(Mandatory, ParameterSetName = 'Installed')][switch]$Installed,
    [Parameter(Mandatory, ParameterSetName = 'Register')][switch]$CheckRegistration
)

$ErrorActionPreference = 'Stop'

function Require($condition, [string]$message) {
    if (!$condition) { throw $message }
}

if (!$Installed) {
    if ($CheckRegistration) {
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        $principal = [Security.Principal.WindowsPrincipal]::new($identity)
        Require ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) 'Run -CheckRegistration from an already elevated console.'
    }
    $cargoCommand = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $cargo = if ($cargoCommand) { $cargoCommand.Source } else { Join-Path $HOME '.cargo\bin\cargo.exe' }
    Require ([IO.File]::Exists($cargo)) 'Cargo is required to build the scheduler fixture.'
    $manifest = Join-Path $PSScriptRoot '..\Cargo.toml'
    if ($CheckRegistration) {
        $test = 'startup::scheduler::registration_tests::registers_and_removes_disabled_fixture'
        $previous = $env:RUSTART_TEST_REGISTER_DISABLED_TASK
        try {
            $env:RUSTART_TEST_REGISTER_DISABLED_TASK = '1'
            $output = & $cargo test --manifest-path $manifest --locked --bin rustart $test -- --exact --ignored --color never
            $result = $LASTEXITCODE
        } finally {
            $env:RUSTART_TEST_REGISTER_DISABLED_TASK = $previous
        }
    } else {
        $test = 'startup::scheduler::tests::logon_definition_round_trips_in_windows_without_registering_it'
        $output = & $cargo test --manifest-path $manifest --locked --bin rustart $test -- --exact --color never
        $result = $LASTEXITCODE
    }
    $output | Write-Output
    Require ($result -eq 0 -and $output -ccontains "test $test ... ok") 'Task Scheduler validation did not pass.'
    return
}

function Get-Sid([string]$account) {
    if ($account -match '^S-1-') { return ([Security.Principal.SecurityIdentifier]::new($account)).Value }
    return ([Security.Principal.NTAccount]::new($account)).Translate([Security.Principal.SecurityIdentifier]).Value
}

$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$name = "Rustart.Autostart.$sid"
$scheduler = New-Object -ComObject 'Schedule.Service'
$scheduler.Connect()
$task = $scheduler.GetFolder('\').GetTask($name)
[xml]$xml = $task.Xml
$ns = [Xml.XmlNamespaceManager]::new($xml.NameTable)
$ns.AddNamespace('t', 'http://schemas.microsoft.com/windows/2004/02/mit/task')

function Read-TaskXml([string]$path, [string]$default = '') {
    $node = $xml.SelectSingleNode("/t:Task/$path", $ns)
    if ($node) { return $node.InnerText }
    return $default
}

Require ($task.Enabled) "Task '$name' is disabled."
Require ((Read-TaskXml 't:RegistrationInfo/t:Source') -ceq 'Rustart.Windows.Logon') 'Unexpected task source.'
Require ($xml.SelectNodes('/t:Task/t:Principals/*', $ns).Count -eq 1) 'Expected one task principal.'
$principal = 't:Principals/t:Principal'
Require ((Get-Sid (Read-TaskXml "$principal/t:UserId")) -eq $sid) 'The task principal belongs to another account.'
Require ((Read-TaskXml "$principal/t:LogonType") -ceq 'InteractiveToken') 'Expected an interactive task logon.'
Require ((Read-TaskXml "$principal/t:RunLevel") -ceq 'HighestAvailable') 'Expected the highest available run level.'

Require ($xml.SelectNodes('/t:Task/t:Triggers/*', $ns).Count -eq 1) 'Expected one task trigger.'
$trigger = 't:Triggers/t:LogonTrigger'
Require ((Get-Sid (Read-TaskXml "$trigger/t:UserId")) -eq $sid) 'The logon trigger belongs to another account.'
Require ((Read-TaskXml "$trigger/t:Enabled" 'true') -ceq 'true') 'The logon trigger is disabled.'

$programFiles = [Environment]::GetFolderPath('ProgramFiles')
$exe = Join-Path $programFiles 'CKATEPTb\Rustart\rustart.exe'
Require ([IO.File]::Exists($exe)) "Installed Rustart is missing at '$exe'."
Require ($xml.SelectNodes('/t:Task/t:Actions/*', $ns).Count -eq 1) 'Expected one task action.'
$action = 't:Actions/t:Exec'
Require ((Read-TaskXml "$action/t:Command") -eq $exe) 'The task command does not point to installed Rustart.'
Require ((Read-TaskXml "$action/t:WorkingDirectory") -eq (Split-Path -Parent $exe)) 'Unexpected task working directory.'
Require ((Read-TaskXml "$action/t:Arguments") -ceq '--admin-logon') 'Unexpected task arguments.'

$settings = 't:Settings'
Require ((Read-TaskXml "$settings/t:AllowStartOnDemand" 'true') -ceq 'false') 'On-demand start is enabled.'
Require ((Read-TaskXml "$settings/t:DisallowStartIfOnBatteries" 'true') -ceq 'false') 'The task cannot start on battery power.'
Require ((Read-TaskXml "$settings/t:StopIfGoingOnBatteries" 'true') -ceq 'false') 'The task stops on battery power.'
Require ((Read-TaskXml "$settings/t:ExecutionTimeLimit" 'PT72H') -ceq 'PT0S') 'The task has an execution time limit.'
Require ((Read-TaskXml "$settings/t:MultipleInstancesPolicy" 'IgnoreNew') -ceq 'IgnoreNew') 'The task allows duplicate instances.'

# OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION
$security = [Security.AccessControl.RawSecurityDescriptor]::new($task.GetSecurityDescriptor(5))
$privilegedOwners = @('S-1-5-18', 'S-1-5-32-544')
Require ($security.Owner.Value -in $privilegedOwners) 'The task owner must be SYSTEM or Administrators.'
Require ($null -ne $security.DiscretionaryAcl) 'The task has no discretionary ACL.'
Require (($security.ControlFlags -band [Security.AccessControl.ControlFlags]::DiscretionaryAclProtected) -ne 0) 'The task ACL allows inherited entries.'
# FILE_GENERIC_READ | GENERIC_READ
$readRights = 0x00120089L -bor 2147483648L
$userCanRead = $false
foreach ($ace in $security.DiscretionaryAcl) {
    Require ($ace.AceType -eq [Security.AccessControl.AceType]::AccessAllowed) 'Unexpected task ACL entry type.'
    if ($ace.SecurityIdentifier.Value -in $privilegedOwners) { continue }
    $rights = [int64]$ace.AccessMask -band 4294967295L
    Require (($rights -band (-bnot $readRights)) -eq 0) 'A non-administrator has task rights beyond read access.'
    if ($ace.SecurityIdentifier.Value -eq $sid -and $rights -ne 0) { $userCanRead = $true }
}
Require $userCanRead 'The current account lacks a read entry on the task ACL.'

$runKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Run')
$run = $null
if ($runKey) {
    try {
        $run = $runKey.GetValue('Rustart', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } finally {
        $runKey.Dispose()
    }
}
$quotedExe = '"{0}"' -f $exe
if ($run -is [string] -and $run.StartsWith($quotedExe, [StringComparison]::OrdinalIgnoreCase)) {
    $arguments = $run.Substring($quotedExe.Length)
    Require ($arguments -cne ' --user-logon') 'The installed Rustart executable is also registered in the Run key.'
}

[pscustomobject]@{ Task = $task.Path; UserSid = $sid; Command = $exe; RunLevel = 'HighestAvailable' }
