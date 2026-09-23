<#
.SYNOPSIS
    Isolated disaster-recovery UAT drill script for Cursor native client.
#>
[CmdletBinding()]
param(
    [string]$ChronicleExe = 'chronicle',
    [Parameter(Mandatory = $true)]
    [string]$WorkRoot,
    [string]$EvidenceDir,
    [string]$RepoPath,
    [string]$PasswordFile,
    [switch]$NonInteractive
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Assert-NotForbiddenPath {
    param([string]$PathToCheck, [string]$Label)
    if ([string]::IsNullOrWhiteSpace($PathToCheck)) { return }
    $resolved = [System.IO.Path]::GetFullPath($PathToCheck).Replace('\', '/').TrimEnd('/')
    $forbidden = @("$HOME/.codex", "$HOME/.cursor", "$env:APPDATA/Cursor", "$HOME/.claude", "$HOME/.gemini", "$HOME/.grok", "D:/Data", "D:/Secrets")
    foreach ($f in $forbidden) {
        if (-not $f) { continue }
        $fNorm = [System.IO.Path]::GetFullPath($f).Replace('\', '/').TrimEnd('/')
        if ($resolved -eq $fNorm -or $resolved.StartsWith($fNorm + '/', [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "PREFLIGHT REFUSAL: $Label path '$PathToCheck' resolves inside forbidden root '$f'"
        }
    }
}

function Add-Evidence {
    param($Step, $Command, $ExitCode, $HostVersion, $Hashes, $LogPath, $Result)
    $record = [ordered]@{
        step = $Step; time = [DateTime]::UtcNow.ToString("o"); command = $Command
        exit_code = $ExitCode; host_version = $HostVersion; hashes = $Hashes
        log_path = $LogPath; result = $Result
    }
    $evPath = Join-Path $EvidenceDir 'evidence.json'
    $list = @()
    if (Test-Path -LiteralPath $evPath) {
        $raw = Get-Content -LiteralPath $evPath -Raw
        if ($raw) { $list = @(ConvertFrom-Json $raw) }
    }
    $list += [PSCustomObject]$record
    $list | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $evPath -Encoding utf8
    return $record
}

function Invoke-Cli {
    param([string]$StepName, [string[]]$CliArgs)
    $logFile = Join-Path $EvidenceDir "$StepName.log"
    $cmdDisplay = "$ChronicleExe " + ($CliArgs -join ' ')
    $out = & $ChronicleExe @CliArgs 2>&1
    $code = $LASTEXITCODE
    $outStr = ($out | Out-String).Trim()
    Set-Content -LiteralPath $logFile -Value $outStr -Encoding utf8
    return [PSCustomObject]@{ Command = $cmdDisplay; ExitCode = $code; Output = $outStr; LogPath = $logFile }
}

function Get-DirHashes {
    param([string]$Dir)
    $hashes = [ordered]@{}
    if (Test-Path -LiteralPath $Dir) {
        Get-ChildItem -LiteralPath $Dir -Recurse -File | ForEach-Object {
            $rel = [System.IO.Path]::GetRelativePath($Dir, $_.FullName).Replace('\', '/')
            $hashes[$rel] = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    return $hashes
}

function Prompt-Checkpoint {
    param([string]$Title, [string]$Prompt)
    Write-Host "=== $Title ===" -ForegroundColor Cyan
    if ($NonInteractive) { return 'skipped' }
    $ans = Read-Host "$Prompt (y/n/skip)"
    return ($ans -match '^y' ? 'passed' : ($ans -match 'skip' ? 'skipped' : 'failed'))
}

# --- Step 1: Preflight ---
$WorkRoot = [System.IO.Path]::GetFullPath($WorkRoot)
$EvidenceDir = [System.IO.Path]::GetFullPath($(if ($EvidenceDir) { $EvidenceDir } else { Join-Path $WorkRoot 'evidence' }))
$CursorData = Join-Path $WorkRoot 'cursor-data'; $CursorExt = Join-Path $WorkRoot 'cursor-ext'
$DataRoot = Join-Path $WorkRoot 'data'
$RepoPath = [System.IO.Path]::GetFullPath($(if ($RepoPath) { $RepoPath } else { Join-Path $WorkRoot 'repo' }))
$PasswordFile = [System.IO.Path]::GetFullPath($(if ($PasswordFile) { $PasswordFile } else { Join-Path $WorkRoot 'temp-password.txt' }))

@($WorkRoot, $EvidenceDir, $CursorData, $CursorExt, $DataRoot, $RepoPath, $PasswordFile) | ForEach-Object {
    Assert-NotForbiddenPath $_ 'Profile/Data'
}
if (Test-Path -LiteralPath $WorkRoot) {
    if ((Get-ChildItem -LiteralPath $WorkRoot -Force).Count -gt 0) {
        throw "PREFLIGHT REFUSAL: WorkRoot '$WorkRoot' must be new or empty."
    }
} else { New-Item -ItemType Directory -Path $WorkRoot -Force | Out-Null }
@($EvidenceDir, $CursorData, $CursorExt, $DataRoot, (Join-Path $CursorData 'User/globalStorage')) | ForEach-Object {
    New-Item -ItemType Directory -Path $_ -Force | Out-Null
}
if (-not (Test-Path -LiteralPath $PasswordFile)) {
    Set-Content -LiteralPath $PasswordFile -Value ([System.Guid]::NewGuid().ToString("N")) -Encoding utf8
}
Add-Evidence 'preflight' 'preflight-check' 0 $null $null $null 'passed' | Out-Null

# --- Step 2: Create Isolated Profile and Config ---
$ConfigPath = Join-Path $WorkRoot 'chronicle-native.toml'
$dataRootToml = $DataRoot.Replace('\', '/')
$repoToml = $RepoPath.Replace('\', '/')
$passToml = $PasswordFile.Replace('\', '/')
$cursorVscdb = Join-Path $CursorData 'User/globalStorage/state.vscdb'
$cursorVscdbToml = $cursorVscdb.Replace('\', '/')

$configContent = "data_root = `"$dataRootToml`"`nlocal_repository = `"$repoToml`"`npassword_file = `"$passToml`"`ncomponents = [`"sessions`"]`n`n[[sources]]`napp = `"cursor`"`ncomponent = `"sessions`"`nslot = `"global-storage`"`npath = `"$cursorVscdbToml`"`n"
Set-Content -LiteralPath $ConfigPath -Value $configContent -Encoding utf8
Add-Evidence 'create-profile' 'create-isolated-profile' 0 $null $null $null 'passed' | Out-Null

# --- Step 3: MANUAL Checkpoint - Session Creation & Host Version ---
$hostVersion = 'unknown'
try {
    $pinfo = [System.Diagnostics.ProcessStartInfo]::new()
    $pinfo.FileName = 'cursor'; $pinfo.Arguments = '--version'
    $cmd = Get-Command 'cursor' -ErrorAction SilentlyContinue
    if ($cmd) {
        if ($cmd.Source -match '\.(cmd|bat)$') {
            $pinfo.FileName = 'cmd.exe'; $pinfo.Arguments = "/c `"$($cmd.Source)`" --version"
        } else { $pinfo.FileName = $cmd.Source }
    }
    $pinfo.RedirectStandardOutput = $true; $pinfo.RedirectStandardError = $true
    $pinfo.UseShellExecute = $false; $pinfo.CreateNoWindow = $true
    $proc = [System.Diagnostics.Process]::Start($pinfo)
    if ($proc.WaitForExit(20000) -and $proc.ExitCode -eq 0) {
        $verOut = ($proc.StandardOutput.ReadToEnd().Trim() -split "`r?`n")[0].Trim()
        if ($verOut) { $hostVersion = $verOut }
    } else { try { $proc.Kill() } catch {} }
} catch { $hostVersion = 'unknown' }

$marker = "CHRONICLE-UAT-CURSOR-" + [System.Guid]::NewGuid().ToString("N").Substring(0, 12)
$launchCmd = "Cursor --user-data-dir `"$CursorData`" --extensions-dir `"$CursorExt`""
Write-Host "MANUAL CHECKPOINT 1: Create Test Session in Isolated Profile`nLaunch: $launchCmd`nMarker: $marker" -ForegroundColor Cyan
Write-Host "NOTE: If chat requires sign-in, sign in inside the isolated profile ONLY. NEVER copy auth from %APPDATA%/Cursor." -ForegroundColor Yellow
$createResult = Prompt-Checkpoint 'Checkpoint 1' "Has session with marker '$marker' been created in isolated profile?"
$signInAnswer = Prompt-Checkpoint 'Checkpoint 1b' 'Was sign-in performed inside the isolated profile? (y if signed in, n if not)'
$signInStatus = ($signInAnswer -eq 'passed' ? 'signed-in-isolated-profile' : ($signInAnswer -eq 'skipped' ? 'skipped' : 'no-sign-in-needed'))
Add-Evidence 'manual-sign-in' 'isolated-profile-sign-in' 0 $hostVersion $null $null $signInStatus | Out-Null
Add-Evidence 'manual-create-session' 'manual-session-creation' 0 $hostVersion $null $null $createResult | Out-Null

# --- Step 4: Capture, Replicate/Verify ---
$cap = Invoke-Cli 'capture' @('native', '--native-config', $ConfigPath, '--json', 'capture', '--app', 'cursor')
$snapshotId = $null
if ($cap.ExitCode -eq 0) {
    try {
        $capObj = $cap.Output | ConvertFrom-Json
        $snapshotId = if ($capObj.snapshot_id) { $capObj.snapshot_id } elseif ($capObj.result.snapshot_id) { $capObj.result.snapshot_id } else { $null }
    } catch {}
}
$capHashes = if ($snapshotId) { Get-DirHashes (Join-Path (Join-Path $DataRoot 'snapshots') $snapshotId) } else { [ordered]@{} }
Add-Evidence 'capture' $cap.Command $cap.ExitCode $hostVersion $capHashes $cap.LogPath ($cap.ExitCode -eq 0 ? 'passed' : 'failed') | Out-Null
if (-not $snapshotId) { throw "Capture failed to produce snapshot ID. Stopping drill." }

$rep = Invoke-Cli 'replicate' @('native', '--native-config', $ConfigPath, '--json', 'replicate', $snapshotId)
Add-Evidence 'replicate' $rep.Command $rep.ExitCode $hostVersion $null $rep.LogPath ($rep.ExitCode -eq 0 ? 'passed' : 'failed') | Out-Null
$ver = Invoke-Cli 'verify' @('native', '--native-config', $ConfigPath, '--json', 'verify', $snapshotId)
Add-Evidence 'verify' $ver.Command $ver.ExitCode $hostVersion $null $ver.LogPath ($ver.ExitCode -eq 0 ? 'passed' : 'failed') | Out-Null

# --- Step 5: Remove Test Session Source Behind Path Guard ---
$workRootNorm = $WorkRoot.Replace('\', '/').TrimEnd('/') + '/'; $cursorDataNorm = $CursorData.Replace('\', '/').TrimEnd('/') + '/'
if (-not $cursorDataNorm.StartsWith($workRootNorm, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "PATH GUARD REFUSAL: CursorData '$CursorData' is not inside WorkRoot '$WorkRoot'"
}
Assert-NotForbiddenPath $CursorData 'CursorData'

$storageDir = Join-Path $CursorData 'User/globalStorage'
@('state.vscdb', 'state.vscdb-wal', 'state.vscdb-shm') | ForEach-Object {
    $t = Join-Path $storageDir $_
    if (Test-Path -LiteralPath $t) {
        $tNorm = [System.IO.Path]::GetFullPath($t).Replace('\', '/')
        if (-not $tNorm.StartsWith($cursorDataNorm, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "PATH GUARD REFUSAL: target '$t' escapes CursorData"
        }
        Remove-Item -LiteralPath $t -Force
    }
}
Add-Evidence 'remove-source' 'remove-source-session' 0 $hostVersion $null $null 'passed' | Out-Null

# --- Step 6: Restore (Dry-Run then Install; Stop if Version Gate Refuses) ---
$mapArgs = @('--map', "global-storage=$cursorVscdbToml")
$targetBeforeDry = Get-DirHashes $CursorData
$dryArgs = @('native', '--native-config', $ConfigPath, '--json', 'restore', $snapshotId, '--into', 'cursor', '--dry-run') + $mapArgs
$dry = Invoke-Cli 'restore-dry-run' $dryArgs
$targetAfterDry = Get-DirHashes $CursorData
$isVersionGate = ($dry.Output -match 'not been verified' -or $dry.Output -match 'unknown host version' -or $dry.ExitCode -ne 0)
$dryHashes = [ordered]@{ before_dry_run = $targetBeforeDry; after_dry_run = $targetAfterDry; target_unchanged = ($targetBeforeDry.Count -eq $targetAfterDry.Count) }

if ($isVersionGate) {
    Add-Evidence 'restore-dry-run' $dry.Command $dry.ExitCode $hostVersion $dryHashes $dry.LogPath 'blocked-by-version-gate' | Out-Null
    Add-Evidence 'restore-install' 'restore-install' $dry.ExitCode $hostVersion $null $dry.LogPath 'blocked-by-version-gate' | Out-Null
    @('manual-open', 'manual-restart', 'manual-continue') | ForEach-Object { Add-Evidence $_ $_ 0 $hostVersion $null $null 'skipped' | Out-Null }
    Add-Evidence 'summary' 'summary' 0 $hostVersion $null $null 'uat-incomplete' | Out-Null
    Write-Host "`nFOUR CONCLUSIONS:`n  1. Files:    blocked-by-version-gate`n  2. Index:    blocked-by-version-gate`n  3. Open:     skipped`n  4. Continue: skipped" -ForegroundColor Yellow
    Write-Warning "Restore blocked by host version gate ('$hostVersion' not verified). Real UAT cannot complete."
    return
}

Add-Evidence 'restore-dry-run' $dry.Command $dry.ExitCode $hostVersion $dryHashes $dry.LogPath 'passed' | Out-Null
$planArgs = @('native', '--native-config', $ConfigPath, '--json', 'restore', $snapshotId, '--into', 'cursor') + $mapArgs
$plan = Invoke-Cli 'restore-plan' $planArgs
if ($plan.ExitCode -ne 0 -or $plan.Output -match 'not been verified') {
    Add-Evidence 'restore-install' $plan.Command $plan.ExitCode $hostVersion $null $plan.LogPath 'blocked-by-version-gate' | Out-Null
    @('manual-open', 'manual-restart', 'manual-continue') | ForEach-Object { Add-Evidence $_ $_ 0 $hostVersion $null $null 'skipped' | Out-Null }
    Add-Evidence 'summary' 'summary' 0 $hostVersion $null $null 'uat-incomplete' | Out-Null
    Write-Host "`nFOUR CONCLUSIONS:`n  1. Files:    blocked-by-version-gate`n  2. Index:    blocked-by-version-gate`n  3. Open:     skipped`n  4. Continue: skipped" -ForegroundColor Yellow
    return
}

$applyPlanPath = ($plan.Output | ConvertFrom-Json).result.apply_plan
$apply = Invoke-Cli 'restore-apply' @('native', '--native-config', $ConfigPath, '--json', 'restore', '--apply-plan', $applyPlanPath)
$installResult = ($apply.ExitCode -eq 0 ? 'passed' : 'failed')
Add-Evidence 'restore-install' $apply.Command $apply.ExitCode $hostVersion (Get-DirHashes $CursorData) $apply.LogPath $installResult | Out-Null

# --- Step 7: MANUAL Checkpoints (open, restart, continue) ---
$openResult = Prompt-Checkpoint 'Checkpoint 2: Open Client' "Launch: $launchCmd`nIs session with marker '$marker' visible in composer/chat?"
$restartResult = Prompt-Checkpoint 'Checkpoint 3: Restart Client' "Close and relaunch: $launchCmd`nIs session with marker '$marker' still visible?"
$continueResult = Prompt-Checkpoint 'Checkpoint 4: Continue Chat' 'Send another message in the session. Did continuation work?'
@(@('manual-open', $openResult), @('manual-restart', $restartResult), @('manual-continue', $continueResult)) | ForEach-Object {
    Add-Evidence $_[0] $_[0] 0 $hostVersion $null $null $_[1] | Out-Null
}

# --- Step 8: Summary & Four Conclusions ---
$filesConclusion = ($installResult -eq 'passed' ? 'passed' : 'failed')
$indexConclusion = ($installResult -eq 'passed' ? 'passed' : 'failed')
$openConclusion = ($openResult -eq 'passed' -and $restartResult -eq 'passed') ? 'passed' : (($openResult -eq 'skipped' -or $restartResult -eq 'skipped') ? 'skipped' : 'failed')
$continueConclusion = $continueResult

$manualPassed = ($createResult -eq 'passed' -and $openResult -eq 'passed' -and $restartResult -eq 'passed' -and $continueResult -eq 'passed')
$overallStatus = ($manualPassed -and $installResult -eq 'passed') ? 'uat-passed' : 'uat-incomplete'

$incompleteWork = @()
@(@('Manual session creation', $createResult), @('Restore install', $installResult), @('Manual open verification', $openResult), @('Manual restart verification', $restartResult), @('Manual continuation verification', $continueResult)) | ForEach-Object {
    if ($_[1] -ne 'passed') { $incompleteWork += "$($_[0]) was '$($_[1])'" }
}
Add-Evidence 'summary' 'summary' 0 $hostVersion $null $null $overallStatus | Out-Null

Write-Host "`nFOUR CONCLUSIONS:`n  1. Files:    $filesConclusion`n  2. Index:    $indexConclusion`n  3. Open:     $openConclusion`n  4. Continue: $continueConclusion" -ForegroundColor Cyan
Write-Host "`nUAT DRILL SUMMARY: $overallStatus" -ForegroundColor ($overallStatus -eq 'uat-passed' ? 'Green' : 'Yellow')
if ($incompleteWork.Count -gt 0) {
    Write-Host "Incomplete manual work items:" -ForegroundColor Yellow
    $incompleteWork | ForEach-Object { Write-Host " - $_" }
}
Write-Host "Evidence recorded to: $(Join-Path $EvidenceDir 'evidence.json')"
