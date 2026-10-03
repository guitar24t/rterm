# Install, upgrade and uninstall the MSI the way a user would, checking that
# rterm works from the installed location and that upgrading while a session
# runs succeeds without a reboot and leaves the session reachable.
# Usage: packaging/windows/test-msi.ps1 -Msi <new.msi> -OldMsi <older.msi> -Version 0.1.5
param(
    [Parameter(Mandatory)] [string] $Msi,
    [Parameter(Mandatory)] [string] $OldMsi,
    [Parameter(Mandatory)] [string] $Version
)
$ErrorActionPreference = 'Stop'
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Rob Terminal'
$rterm = Join-Path $installDir 'rterm.exe'

function Invoke-Msiexec([string[]] $arguments) {
    $log = Join-Path ([IO.Path]::GetTempPath()) "msiexec-$([guid]::NewGuid()).log"
    $p = Start-Process msiexec.exe -ArgumentList ($arguments + @('/qn', '/l*v', "`"$log`"")) -Wait -PassThru
    if ($p.ExitCode -ne 0) {
        Get-Content $log -Tail 60
        throw "msiexec $arguments failed with exit code $($p.ExitCode)"
    }
}

function Assert($condition, [string] $message) {
    if (-not $condition) { throw "FAILED: $message" }
    Write-Host "ok: $message"
}

$env:RTERM_SHELL = 'cmd.exe'

# 1. Install an older build and start a session from it.
Invoke-Msiexec @('/i', "`"$OldMsi`"")
Assert (Test-Path $rterm) "installed into $installDir"
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
Assert ($userPath -split ';' -contains $installDir.TrimEnd('\') -or $userPath -split ';' -contains "$installDir\") "install folder is on the user PATH"
& $rterm new -d upgrade-survivor -- cmd.exe /d
Assert ($LASTEXITCODE -eq 0) 'started a session from the installed rterm'

# 2. Upgrade while that session is running: no reboot needed, session kept.
Invoke-Msiexec @('/i', "`"$Msi`"")
$reported = (& $rterm --version)
Assert ($reported -eq "rterm $Version") "upgraded in place to $Version (got '$reported')"
Assert ((& (Join-Path $installDir 'rterm-connect.exe') --help) -match 'Choose an rterm session') 'rterm-connect.exe runs'
$list = (& $rterm ls) -join "`n"
Assert ($list -match 'upgrade-survivor') "the session started before the upgrade is still running:`n$list"
& $rterm kill upgrade-survivor
Assert ($LASTEXITCODE -eq 0) 'killed it with the new rterm'

# 3. Uninstall removes the program and the PATH entry.
Invoke-Msiexec @('/x', "`"$Msi`"")
Assert (-not (Test-Path $rterm)) 'uninstalled'
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
Assert (-not ($userPath -like "*Rob Terminal*")) 'removed from the user PATH'
Write-Host 'MSI tests passed'
