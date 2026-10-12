# rterm over real ssh on Windows, with each shell Windows OpenSSH can run
# commands in (cmd.exe, Windows PowerShell, PowerShell 7): both session
# pickers list sessions, a session started over ssh outlives the connection,
# and `rterm get` / `rterm put` reach the computer through the transfer
# endpoint rterm-connect.exe starts. This machine is both host and computer,
# with Rob Terminal installed from the MSI as a user would.
# Needs an administrator (to set up sshd). CI runs it on a throwaway runner.
# Usage: tests/windows-ssh.ps1 -Msi rterm-<version>-windows-x86_64.msi
param([Parameter(Mandatory)] [string] $Msi)
$ErrorActionPreference = 'Continue'
$PSNativeCommandArgumentPassing = 'Standard'
$repo = Split-Path $PSScriptRoot

function Assert($condition, [string] $message, [string] $detail = '') {
    if (-not $condition) { throw "FAILED: $message`n$detail" }
    Write-Host "ok: $message"
}

# Run a program with stdin closed, output captured, and a time limit (a
# leaked pipe handle would otherwise hang the job).
function Invoke-Native([string] $exe, [string[]] $arguments, [int] $seconds = 60) {
    $psi = [Diagnostics.ProcessStartInfo]::new($exe)
    foreach ($a in $arguments) { $psi.ArgumentList.Add($a) }
    $psi.UseShellExecute = $false
    $psi.WorkingDirectory = (Get-Location).ProviderPath  # Push-Location doesn't set the process's
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [Diagnostics.Process]::Start($psi)
    $p.StandardInput.Close()
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit($seconds * 1000)) {
        $p.Kill($true)
        throw "FAILED: $exe $arguments did not finish in $seconds s"
    }
    if (-not ($out.Wait(10000) -and $err.Wait(10000))) {
        throw "FAILED: $exe $arguments exited but something kept its output open"
    }
    [pscustomobject]@{ Code = $p.ExitCode; Text = $out.Result + $err.Result }
}

# Rob Terminal from the MSI: per-user, on the user PATH. R ships an
# Rterm.exe, Windows names ignore case, and the runner has R on the machine
# PATH (which comes first), so take R off it; machines without R don't care.
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
$withoutR = ($machinePath -split ';' | Where-Object { $_ -and -not (Test-Path (Join-Path $_ 'Rterm.exe')) }) -join ';'
[Environment]::SetEnvironmentVariable('Path', $withoutR, 'Machine')
$log = Join-Path $env:RUNNER_TEMP 'msiexec.log'
$p = Start-Process msiexec.exe -ArgumentList @('/i', "`"$Msi`"", '/qn', '/l*v', "`"$log`"") -Wait -PassThru
if ($p.ExitCode -ne 0) { Get-Content $log -Tail 60; throw "FAILED: installing $Msi ($($p.ExitCode))" }
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Rob Terminal'
$rterm = Join-Path $installDir 'rterm.exe'
$connect = Join-Path $installDir 'rterm-connect.exe'
Assert (Test-Path $connect) "installed into $installDir"

# An OpenSSH server on localhost that lets this user in with a key.
if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
    Write-Host 'Installing OpenSSH Server'
    Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 -ErrorAction Stop | Out-Null
}
Restart-Service sshd -ErrorAction Stop  # picks up the new PATH
$sshDir = Join-Path $HOME '.ssh'
New-Item -ItemType Directory -Force $sshDir | Out-Null
$key = Join-Path $sshDir 'id_ed25519'
if (-not (Test-Path $key)) { ssh-keygen -q -t ed25519 -f $key -N '' }
$pub = Get-Content "$key.pub"
$adminKeys = Join-Path $env:ProgramData 'ssh\administrators_authorized_keys'
Set-Content -Path $adminKeys -Value $pub -Encoding ascii
icacls.exe $adminKeys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null
Add-Content -Path (Join-Path $sshDir 'authorized_keys') -Value $pub -Encoding ascii
Add-Content -Path (Join-Path $sshDir 'config') -Encoding ascii -Value "Host localhost`n  StrictHostKeyChecking accept-new"
Write-Host "ssh client: $((Get-Command ssh).Source) $(ssh -V 2>&1)"
$r = Invoke-Native ssh @('-o', 'BatchMode=yes', 'localhost', 'echo', 'ssh-works')
Assert ($r.Text -match 'ssh-works') 'ssh to localhost with a key' $r.Text
$r = Invoke-Native ssh @('localhost', 'rterm --version')
Assert ($r.Text -match '^rterm \d') 'ssh logins find the installed rterm on the user PATH' $r.Text

function Set-HostShell([string] $shell) {
    $reg = 'HKLM:\SOFTWARE\OpenSSH'
    if (-not (Test-Path $reg)) { New-Item -Path $reg | Out-Null }
    if ($shell) {
        New-ItemProperty -Path $reg -Name DefaultShell -Value $shell -PropertyType String -Force | Out-Null
    } else {
        Remove-ItemProperty -Path $reg -Name DefaultShell -ErrorAction SilentlyContinue
    }
}

$shells = [ordered]@{
    'cmd.exe'            = ''
    'Windows PowerShell' = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    'PowerShell 7'       = (Get-Command pwsh).Source
}
$n = 0
foreach ($shell in $shells.GetEnumerator()) {
    $n++
    $label = $shell.Key
    $name = "viassh$n"
    Set-HostShell $shell.Value
    Write-Host "--- host shell: $label"

    # The session pickers' listings.
    $r = Invoke-Native python @("$repo\contrib\rterm-connect.py", '--list', 'localhost')
    Assert ($r.Code -eq 0) "rterm-connect.py lists sessions [$label]" $r.Text
    $r = Invoke-Native $connect @('--list', 'localhost')
    Assert ($r.Code -eq 0) "rterm-connect.exe lists sessions [$label]" $r.Text
    $r = Invoke-Native python @("$repo\contrib\rterm-connect.py", '--list', '--rterm', 'no-such-rterm', 'localhost')
    Assert ($r.Text -match 'runs Windows: install Rob Terminal') "rterm-connect.py says how to install on a Windows host [$label]" $r.Text
    $r = Invoke-Native $connect @('--list', '--rterm', 'no-such-rterm', 'localhost')
    Assert ($r.Text -match 'runs Windows: install Rob Terminal') "rterm-connect.exe says how to install on a Windows host [$label]" $r.Text

    # A session started over ssh keeps running after ssh disconnects.
    $r = Invoke-Native ssh @('localhost', "rterm new -d $name")
    Assert ($r.Text -match "created session '$name'") "started session $name over ssh [$label]" $r.Text
    Start-Sleep 2
    $r = Invoke-Native $connect @('--list', 'localhost')
    Assert ($r.Text -match $name) "$name outlives its ssh connection [$label]" $r.Text

    # rterm get / put in that session, served by the endpoint that
    # rterm-connect.exe starts (as itself) over its own ssh connection.
    $root = Join-Path $env:RUNNER_TEMP "xfer$n"
    $work, $dl, $laptop = "$root\work", "$root\dl", "$root\laptop"
    New-Item -ItemType Directory -Force $work, $dl, $laptop | Out-Null
    [IO.File]::WriteAllText("$work\notes.txt", 'from the server')
    $bytes = [byte[]]::new(3MB)
    [Random]::new($n).NextBytes($bytes)
    [IO.File]::WriteAllBytes("$work\big.bin", $bytes)
    [IO.File]::WriteAllText("$laptop\photo.jpg", 'jpeg bytes')
    $psi = [Diagnostics.ProcessStartInfo]::new($connect)
    foreach ($a in @('__transfer-endpoint', '--session', $name, '--host', 'localhost',
            '--download-dir', $dl, '--cwd', $laptop, '--parent', $PID)) {
        $psi.ArgumentList.Add($a)
    }
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.Environment['RTERM_TRANSFER_CONFIRM'] = 'off'
    $endpoint = [Diagnostics.Process]::Start($psi)

    $env:RTERM_SESSION = $name
    Push-Location $work
    $deadline = (Get-Date).AddSeconds(60)
    do {
        $r = Invoke-Native $rterm @('get', 'notes.txt', 'big.bin')
        if ($r.Code -eq 0) { break }
        Start-Sleep 1
    } while ((Get-Date) -lt $deadline)
    Assert ($r.Code -eq 0) "rterm get over ssh [$label]" $r.Text
    Assert ([IO.File]::ReadAllText("$dl\notes.txt") -eq 'from the server') "got notes.txt [$label]"
    Assert ((Get-FileHash "$dl\big.bin").Hash -eq (Get-FileHash "$work\big.bin").Hash) "got a 3 MB binary file intact [$label]"
    $r = Invoke-Native $rterm @('put', 'photo.jpg')
    Assert ($r.Code -eq 0 -and (Test-Path "$work\photo.jpg")) "rterm put over ssh [$label]" $r.Text
    Assert ([IO.File]::ReadAllText("$work\photo.jpg") -eq 'jpeg bytes') "put photo.jpg intact [$label]"
    Pop-Location
    Remove-Item env:RTERM_SESSION

    $endpoint.Kill($true)
    $r = Invoke-Native $rterm @('kill', $name)
    Assert ($r.Code -eq 0) "killed $name [$label]" $r.Text
}
Set-HostShell ''
Write-Host 'ssh tests passed'
exit 0
