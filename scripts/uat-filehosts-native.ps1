<#
.SYNOPSIS
    Isolated disaster-recovery UAT drill script for file-based hosts (claude-code, antigravity, grok).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('claude-code', 'antigravity', 'grok')]
    [string]$HostName,
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
    $record = [ordered]@{ step = $Step; time = [DateTime]::UtcNow.ToString("o"); command = $Command
        exit_code = $ExitCode; host_version = $HostVersion; hashes = $Hashes; log_path = $LogPath; result = $Result }
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
    $code = $LASTEXITCODE; $outStr = ($out | Out-String).Trim()
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

# --- Step 1: Preflight & Isolation Verification ---
$WorkRoot = [System.IO.Path]::GetFullPath($WorkRoot)
$EvidenceDir = [System.IO.Path]::GetFullPath($(if ($EvidenceDir) { $EvidenceDir } else { Join-Path $WorkRoot 'evidence' }))
$DataRoot = Join-Path $WorkRoot 'data'; $RepoPath = [System.IO.Path]::GetFullPath($(if ($RepoPath) { $RepoPath } else { Join-Path $WorkRoot 'repo' }))
$PasswordFile = [System.IO.Path]::GetFullPath($(if ($PasswordFile) { $PasswordFile } else { Join-Path $WorkRoot 'temp-password.txt' }))
$ProfileDir = if ($HostName -eq 'claude-code') { Join-Path $WorkRoot 'claude-config' } else { $null }

@($WorkRoot, $EvidenceDir, $DataRoot, $RepoPath, $PasswordFile, $ProfileDir) | ForEach-Object { Assert-NotForbiddenPath $_ 'Preflight' }
if (Test-Path -LiteralPath $WorkRoot) {
    if ((Get-ChildItem -LiteralPath $WorkRoot -Force).Count -gt 0) {
        throw "PREFLIGHT REFUSAL: WorkRoot '$WorkRoot' must be new or empty."
    }
} else { New-Item -ItemType Directory -Path $WorkRoot -Force | Out-Null }
New-Item -ItemType Directory -Path $EvidenceDir -Force | Out-Null

if ($HostName -ne 'claude-code') {
    Add-Evidence 'preflight' 'preflight-check' 0 $null $null $null 'passed' | Out-Null
    @('isolation-check', 'summary') | ForEach-Object { Add-Evidence $_ 'verify-client-isolation' 0 'unknown' $null $null 'unsupported-isolation' | Out-Null }
    Write-Warning "PREFLIGHT REFUSAL: Host '$HostName' has no documented profile isolation mechanism in this repository. Recorded as unsupported-isolation."
    return
}

New-Item -ItemType Directory -Path $DataRoot -Force | Out-Null
if (-not (Test-Path -LiteralPath $PasswordFile)) {
    Set-Content -LiteralPath $PasswordFile -Value ([System.Guid]::NewGuid().ToString("N")) -Encoding utf8
}
Add-Evidence 'preflight' 'preflight-check' 0 $null $null $null 'passed' | Out-Null

# --- Step 2: Create Isolated Profile and Config ---
$ClaudeConfig = $ProfileDir
@('projects', 'tasks', 'file-history') | ForEach-Object { New-Item -ItemType Directory -Path (Join-Path $ClaudeConfig $_) -Force | Out-Null }
$ConfigPath = Join-Path $WorkRoot 'chronicle-native.toml'
$dataRootToml = $DataRoot.Replace('\', '/'); $repoToml = $RepoPath.Replace('\', '/')
$passToml = $PasswordFile.Replace('\', '/'); $claudeConfigToml = $ClaudeConfig.Replace('\', '/')

$sourcesToml = @('projects', 'tasks', 'file-history') | ForEach-Object {
    "[[sources]]`napp = `"claude-code`"`ncomponent = `"sessions`"`nslot = `"$_`"`npath = `"$claudeConfigToml/$_`"`n"
}
$configContent = "data_root = `"$dataRootToml`"`nlocal_repository = `"$repoToml`"`npassword_file = `"$passToml`"`ncomponents = [`"sessions`"]`n`n" + ($sourcesToml -join "`n")
Set-Content -LiteralPath $ConfigPath -Value $configContent -Encoding utf8
Add-Evidence 'create-profile' 'create-isolated-profile' 0 $null $null $null 'passed' | Out-Null

# --- Step 3: MANUAL Checkpoint - Session Creation & Host Version ---
$hostVersion = 'unknown'
try {
    $pinfo = [System.Diagnostics.ProcessStartInfo]::new()
    $pinfo.FileName = 'claude'; $pinfo.Arguments = '--version'
    $cmd = Get-Command 'claude' -ErrorAction SilentlyContinue
    if (-not $cmd) { $cmd = Get-Command 'claude-code' -ErrorAction SilentlyContinue }
    if ($cmd) {
        $pinfo.FileName = if ($cmd.Source -match '\.(cmd|bat)$') { 'cmd.exe' } else { $cmd.Source }
        if ($cmd.Source -match '\.(cmd|bat)$') { $pinfo.Arguments = "/c `"$($cmd.Source)`" --version" }
    }
    $pinfo.RedirectStandardOutput = $true; $pinfo.RedirectStandardError = $true; $pinfo.UseShellExecute = $false; $pinfo.CreateNoWindow = $true
    $proc = [System.Diagnostics.Process]::Start($pinfo)
    if ($proc.WaitForExit(20000) -and $proc.ExitCode -eq 0) {
        $verOut = ($proc.StandardOutput.ReadToEnd().Trim() -split "`r?`n")[0].Trim()
        if ($verOut) { $hostVersion = $verOut }
    } else { try { $proc.Kill() } catch {} }
} catch { $hostVersion = 'unknown' }

$marker = "CHRONICLE-UAT-CLAUDE-" + [System.Guid]::NewGuid().ToString("N").Substring(0, 12)
$launchCmd = "`$env:CLAUDE_CONFIG_DIR = '$ClaudeConfig'; claude"
Write-Host "MANUAL CHECKPOINT 1: Create Test Session in Isolated Profile`nCLAUDE_CONFIG_DIR: $ClaudeConfig`nMarker: $marker`n1. Run: $launchCmd`n2. Create session with marker: $marker`n3. Exit client." -ForegroundColor Cyan
$createResult = Prompt-Checkpoint 'Checkpoint 1' "Has session with marker '$marker' been created in isolated profile?"
Add-Evidence 'manual-create-session' 'manual-session-creation' 0 $hostVersion $null $null $createResult | Out-Null

# --- Step 4: Capture, Replicate/Verify ---
$cap = Invoke-Cli 'capture' @('native', '--native-config', $ConfigPath, '--json', 'capture', '--app', 'claude-code')
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

@('replicate', 'verify') | ForEach-Object {
    $r = Invoke-Cli $_ @('native', '--native-config', $ConfigPath, '--json', $_, $snapshotId)
    Add-Evidence $_ $r.Command $r.ExitCode $hostVersion $null $r.LogPath ($r.ExitCode -eq 0 ? 'passed' : 'failed') | Out-Null
}

# --- Step 5: Remove Test Session Source Behind Path Guard ---
$workRootNorm = $WorkRoot.Replace('\', '/').TrimEnd('/') + '/'; $claudeConfigNorm = $ClaudeConfig.Replace('\', '/').TrimEnd('/') + '/'
if (-not $claudeConfigNorm.StartsWith($workRootNorm, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "PATH GUARD REFUSAL: ClaudeConfig '$ClaudeConfig' is not inside WorkRoot '$WorkRoot'"
}
Assert-NotForbiddenPath $ClaudeConfig 'ClaudeConfig'

$cwdSegs = @()
$projDir = Join-Path $ClaudeConfig 'projects'
if (Test-Path -LiteralPath $projDir) { Get-ChildItem -LiteralPath $projDir -Directory | ForEach-Object { $cwdSegs += $_.Name } }

@('projects', 'tasks', 'file-history') | ForEach-Object {
    $t = Join-Path $ClaudeConfig $_
    if (Test-Path -LiteralPath $t) {
        $tNorm = [System.IO.Path]::GetFullPath($t).Replace('\', '/')
        if (-not $tNorm.StartsWith($claudeConfigNorm, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "PATH GUARD REFUSAL: target '$t' escapes ClaudeConfig"
        }
        Remove-Item -LiteralPath $t -Recurse -Force
    }
}
Add-Evidence 'remove-source' 'remove-source-session' 0 $hostVersion $null $null 'passed' | Out-Null

# --- Step 6: Restore (Dry-Run then Install; Stop if Version Gate Refuses) ---
$mapArgs = @('--map', "projects=$claudeConfigToml/projects", '--map', "tasks=$claudeConfigToml/tasks", '--map', "file-history=$claudeConfigToml/file-history")
foreach ($seg in $cwdSegs) { $mapArgs += @('--map', "$seg=$seg") }

$targetBeforeDry = Get-DirHashes $ClaudeConfig
$dryArgs = @('native', '--native-config', $ConfigPath, '--json', 'restore', $snapshotId, '--into', 'claude-code', '--dry-run') + $mapArgs
$dry = Invoke-Cli 'restore-dry-run' $dryArgs
$targetAfterDry = Get-DirHashes $ClaudeConfig
$isVersionGate = ($dry.Output -match 'not been verified' -or $dry.Output -match 'unknown host version' -or $dry.ExitCode -ne 0)
$dryHashes = [ordered]@{ before_dry_run = $targetBeforeDry; after_dry_run = $targetAfterDry; target_unchanged = ($targetBeforeDry.Count -eq $targetAfterDry.Count) }

if ($isVersionGate) {
    Add-Evidence 'restore-dry-run' $dry.Command $dry.ExitCode $hostVersion $dryHashes $dry.LogPath 'blocked-by-version-gate' | Out-Null
    Add-Evidence 'restore-install' 'restore-install' $dry.ExitCode $hostVersion $null $dry.LogPath 'blocked-by-version-gate' | Out-Null
    @('manual-open', 'manual-restart', 'manual-continue') | ForEach-Object { Add-Evidence $_ $_ 0 $hostVersion $null $null 'skipped' | Out-Null }
    Add-Evidence 'summary' 'summary' 0 $hostVersion $null $null 'uat-incomplete' | Out-Null
    Write-Warning "Restore blocked by host version gate ('$hostVersion' not verified). Real UAT cannot complete."
    return
}

Add-Evidence 'restore-dry-run' $dry.Command $dry.ExitCode $hostVersion $dryHashes $dry.LogPath 'passed' | Out-Null
$planArgs = @('native', '--native-config', $ConfigPath, '--json', 'restore', $snapshotId, '--into', 'claude-code') + $mapArgs
$plan = Invoke-Cli 'restore-plan' $planArgs
if ($plan.ExitCode -ne 0 -or $plan.Output -match 'not been verified') {
    Add-Evidence 'restore-install' $plan.Command $plan.ExitCode $hostVersion $null $plan.LogPath 'blocked-by-version-gate' | Out-Null
    @('manual-open', 'manual-restart', 'manual-continue') | ForEach-Object { Add-Evidence $_ $_ 0 $hostVersion $null $null 'skipped' | Out-Null }
    Add-Evidence 'summary' 'summary' 0 $hostVersion $null $null 'uat-incomplete' | Out-Null
    return
}

$applyPlanPath = ($plan.Output | ConvertFrom-Json).result.apply_plan
$apply = Invoke-Cli 'restore-apply' @('native', '--native-config', $ConfigPath, '--json', 'restore', '--apply-plan', $applyPlanPath)
$installResult = ($apply.ExitCode -eq 0 ? 'passed' : 'failed')
Add-Evidence 'restore-install' $apply.Command $apply.ExitCode $hostVersion (Get-DirHashes $ClaudeConfig) $apply.LogPath $installResult | Out-Null

# --- Step 7: MANUAL Checkpoints (open, restart, continue) ---
$openResult = Prompt-Checkpoint 'Checkpoint 2: Open Client' "Run: $launchCmd`nIs session with marker '$marker' visible?"
$restartResult = Prompt-Checkpoint 'Checkpoint 3: Restart Client' "Close and restart client: $launchCmd`nIs session with marker '$marker' still visible?"
$continueResult = Prompt-Checkpoint 'Checkpoint 4: Continue Chat' 'Send another message in the session. Did continuation work?'
@(@('manual-open', $openResult), @('manual-restart', $restartResult), @('manual-continue', $continueResult)) | ForEach-Object {
    Add-Evidence $_[0] $_[0] 0 $hostVersion $null $null $_[1] | Out-Null
}

# --- Step 8: Summary ---
$manualPassed = ($createResult -eq 'passed' -and $openResult -eq 'passed' -and $restartResult -eq 'passed' -and $continueResult -eq 'passed')
$overallStatus = ($manualPassed -and $installResult -eq 'passed') ? 'uat-passed' : 'uat-incomplete'

$incompleteWork = @()
@(@('Manual session creation', $createResult), @('Restore install', $installResult), @('Manual open verification', $openResult), @('Manual restart verification', $restartResult), @('Manual continuation verification', $continueResult)) | ForEach-Object {
    if ($_[1] -ne 'passed') { $incompleteWork += "$($_[0]) was '$($_[1])'" }
}
Add-Evidence 'summary' 'summary' 0 $hostVersion $null $null $overallStatus | Out-Null

Write-Host "`nUAT DRILL SUMMARY: $overallStatus" -ForegroundColor ($overallStatus -eq 'uat-passed' ? 'Green' : 'Yellow')
if ($incompleteWork.Count -gt 0) {
    Write-Host "Incomplete manual work items:" -ForegroundColor Yellow
    $incompleteWork | ForEach-Object { Write-Host " - $_" }
}
Write-Host "Evidence recorded to: $(Join-Path $EvidenceDir 'evidence.json')"
