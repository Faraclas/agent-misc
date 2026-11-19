<#
.SYNOPSIS
    Installs utcp for the current Windows user. No administrator rights needed.

.DESCRIPTION
    Run install.cmd from the unzipped release folder (next to utcp.exe); it runs this script:

        .\install.cmd
        .\install.cmd -NoDaemonAtLogon

    install.cmd passes -ExecutionPolicy Bypass, which a direct .\install.ps1 would need: the script is unsigned
    and downloaded, so the default execution policy blocks it before any of it runs.

    It copies utcp.exe to %LOCALAPPDATA%\Programs\utcp, adds that folder to your user PATH, installs the
    utcp-tools and utcp-admin agent skills (utcp sync-ides --global), registers a scheduled task that runs
    "utcp daemon start" at logon, and starts the daemon now. Running it again upgrades in place.

    The task runs without administrator rights even when this script runs elevated: a daemon started as
    administrator could not be used from a normal shell.

    -NoDaemonAtLogon skips the task, and removes it if an earlier install created it. The daemon is optional:
    without it every call still works, just a few milliseconds slower; start it by hand with "utcp daemon start".

    -Uninstall stops the daemon and removes the scheduled task, the PATH entry and the program folder. Your manuals,
    secrets (.env) and the agent skill folders stay; operating.md section 1 lists where they are.

.LINK
    https://github.com/Faraclas/agent-misc/blob/main/utcp-rs/docs/operating.md
#>
[CmdletBinding()]
param(
    [switch]$NoDaemonAtLogon,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\utcp'
$Exe = Join-Path $InstallDir 'utcp.exe'
$TaskName = 'utcp daemon'

# The user PATH is read and written raw (REG_EXPAND_SZ) so entries such as %USERPROFILE%\bin stay unexpanded.
function Get-UserPath {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        return [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } finally {
        $key.Close()
    }
}

function Set-UserPath([string]$Value) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        $key.SetValue('Path', $Value, [Microsoft.Win32.RegistryValueKind]::ExpandString)
    } finally {
        $key.Close()
    }
    # Setting any user variable through .NET broadcasts the change, so new terminals see the new PATH.
    [Environment]::SetEnvironmentVariable('UTCP_INSTALL_REFRESH', '1', 'User')
    [Environment]::SetEnvironmentVariable('UTCP_INSTALL_REFRESH', $null, 'User')
}

function Test-SameDir([string]$Entry, [string]$Dir) {
    $expanded = [Environment]::ExpandEnvironmentVariables($Entry).TrimEnd('\')
    return $expanded -ieq $Dir.TrimEnd('\')
}

function Test-Elevated {
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-UtcpDaemon {
    $ErrorActionPreference = 'Continue'
    if (-not (Test-Path $Exe)) {
        return $false
    }
    return [bool]((& $Exe daemon status 2>&1 | Out-String) -match 'RUNNING')
}

function Stop-UtcpDaemon {
    # Windows PowerShell 5.1 turns redirected stderr of a native program into an error, which 'Stop' would throw.
    $ErrorActionPreference = 'Continue'
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    }
    if (Test-Path $Exe) {
        # Prints "not running" and fails harmlessly when no daemon runs.
        & $Exe daemon stop 2>&1 | Out-Null
    }
}

if ($Uninstall) {
    Stop-UtcpDaemon
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "Removed the scheduled task '$TaskName'."
    }
    $entries = @((Get-UserPath) -split ';' | Where-Object { $_ -ne '' })
    $kept = @($entries | Where-Object { -not (Test-SameDir $_ $InstallDir) })
    if ($kept.Count -ne $entries.Count) {
        Set-UserPath ($kept -join ';')
        Write-Host "Removed $InstallDir from your user PATH."
    }
    if (Test-Path $InstallDir) {
        Remove-Item -Recurse -Force $InstallDir
        Write-Host "Deleted $InstallDir."
    }
    Write-Host 'utcp is uninstalled. Manuals, secrets and agent skills were kept.'
    return
}

$Source = Join-Path $PSScriptRoot 'utcp.exe'
if (-not (Test-Path $Source)) {
    throw "utcp.exe not found next to install.ps1 ($PSScriptRoot). Unzip the whole release archive first."
}

# A running daemon locks utcp.exe; stop it so an upgrade can replace the file. Other agents' calls fall back to
# direct mode meanwhile.
$wasRunning = Test-UtcpDaemon
Stop-UtcpDaemon

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# Any running utcp.exe (utcp ui, an agent's call) blocks overwriting the file, but Windows lets it be renamed. Move
# the old one aside; a later install deletes the leftover once nothing runs it.
Get-ChildItem -Path $InstallDir -Filter 'utcp.exe.old-*' |
    Remove-Item -Force -ErrorAction SilentlyContinue
$Old = $null
if (Test-Path $Exe) {
    $Old = Join-Path $InstallDir ('utcp.exe.old-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    Move-Item -Force $Exe $Old
}
try {
    Copy-Item -Force $Source $Exe
} catch {
    if ($Old) {
        Move-Item -Force $Old $Exe
    }
    throw
}
Unblock-File $Exe
foreach ($doc in 'operating.md', 'SKILL.md', 'LICENSE', 'NOTICE', 'LICENSE-UPSTREAM-RS-UTCP') {
    $path = Join-Path $PSScriptRoot $doc
    if (Test-Path $path) {
        Copy-Item -Force $path $InstallDir
    }
}
Write-Host "Installed $(& $Exe --version) to $InstallDir."

$entries = @((Get-UserPath) -split ';' | Where-Object { $_ -ne '' })
if (-not ($entries | Where-Object { Test-SameDir $_ $InstallDir })) {
    Set-UserPath (($entries + $InstallDir) -join ';')
    Write-Host "Added $InstallDir to your user PATH. Open a new terminal (and restart your editor) to use 'utcp'."
}
if (-not (($env:Path -split ';') | Where-Object { Test-SameDir $_ $InstallDir })) {
    $env:Path = "$env:Path;$InstallDir"
}

& $Exe sync-ides --global
if ($LASTEXITCODE -ne 0) {
    Write-Warning "Installing the agent skills failed. Run 'utcp sync-ides --global' to retry."
}

$taskRegistered = $false
if ($NoDaemonAtLogon) {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "Removed the scheduled task '$TaskName' (-NoDaemonAtLogon)."
    }
} else {
    # The token's name, not %USERDOMAIN%\%USERNAME%: over SSH, USERDOMAIN is the workgroup, which does not resolve.
    $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    $action = New-ScheduledTaskAction -Execute $Exe -Argument 'daemon start'
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
        -ExecutionTimeLimit (New-TimeSpan -Minutes 5)
    try {
        Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal `
            -Settings $settings -Description 'Starts the utcp daemon (utcp-rs) at logon.' -Force | Out-Null
        Write-Host "Registered the scheduled task '$TaskName' (runs 'utcp daemon start' at logon)."
        $taskRegistered = $true
    } catch {
        Write-Warning "Could not register the logon task: $($_.Exception.Message)"
        Write-Warning "Start the daemon by hand with 'utcp daemon start' instead."
    }
}

# Start the daemon now: through the task when there is one, so it never runs elevated, else directly when it was
# running before an upgrade.
if ($taskRegistered) {
    try {
        Start-ScheduledTask -TaskName $TaskName
    } catch {
        Write-Warning "Could not run the logon task now: $($_.Exception.Message)"
    }
    $deadline = (Get-Date).AddSeconds(10)
    while (-not (Test-UtcpDaemon) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
    }
    if (Test-UtcpDaemon) {
        Write-Host 'The utcp daemon is running.'
    } else {
        Write-Warning "The daemon did not start yet. It starts at your next logon, or run 'utcp daemon start'."
    }
} elseif ($wasRunning) {
    if (Test-Elevated) {
        Write-Warning "Not restarting the daemon from this elevated shell. Run 'utcp daemon start' in a normal one."
    } else {
        & $Exe daemon start
    }
}

Write-Host ''
Write-Host "Done. Check with 'utcp doctor'. Next steps (adding tools, secrets): operating.md in $InstallDir."
