<#
.SYNOPSIS
    Automated disaster-recovery drill script for Chronicle native hosts.
.DESCRIPTION
    Runs an isolated disaster-recovery drill validating that client sessions
    can be captured, replicated to a dedicated NAS drill repository, pulled
    onto a clean system, and restored with full continuation.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('codex', 'claude-code', 'grok', 'antigravity', 'cursor')]
    [string]$App,

    [Parameter(Mandatory = $true)]
    [string]$DrillRoot,

    [Parameter(Mandatory = $true)]
    [string]$NasRepository,

    [Parameter(Mandatory = $true)]
    [string]$Restic,

    [string[]]$ProtectedRoot = @(),

    [string]$ChronicleExe = 'chronicle',

    [switch]$Register,

    [switch]$Preflight,

    [switch]$NonInteractive
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-ForbiddenRoots {
    $roots = [System.Collections.Generic.List[string]]::new()
    $homeNorm = [System.IO.Path]::GetFullPath($HOME).Replace('\', '/').TrimEnd('/')
    $roots.Add("$homeNorm/.codex")
    $roots.Add("$homeNorm/.cursor")
    $roots.Add("$homeNorm/AppData/Roaming/Cursor")
    $roots.Add("$homeNorm/AppData/Local/Programs/cursor")
    $roots.Add("$homeNorm/.claude")
    $roots.Add("$homeNorm/.claude.json")
    $roots.Add("$homeNorm/.gemini")
    $roots.Add("$homeNorm/.grok")

    if ($ProtectedRoot) {
        foreach ($pr in $ProtectedRoot) {
            if (-not [string]::IsNullOrWhiteSpace($pr)) {
                $roots.Add([System.IO.Path]::GetFullPath($pr).Replace('\', '/').TrimEnd('/'))
            }
        }
    }

    if ($env:CHRONICLE_NATIVE_ROOT -and -not [string]::IsNullOrWhiteSpace($env:CHRONICLE_NATIVE_ROOT)) {
        $roots.Add([System.IO.Path]::GetFullPath($env:CHRONICLE_NATIVE_ROOT).Replace('\', '/').TrimEnd('/'))
    }

    return $roots
}

function Assert-NotForbiddenPath {
    param([string]$PathToCheck, [string]$Label)
    if ([string]::IsNullOrWhiteSpace($PathToCheck)) { return }
    $resolved = [System.IO.Path]::GetFullPath($PathToCheck).Replace('\', '/').TrimEnd('/')
    $homeNorm = [System.IO.Path]::GetFullPath($HOME).Replace('\', '/').TrimEnd('/')
    $forbidden = Get-ForbiddenRoots
    foreach ($f in $forbidden) {
        if (-not $f) { continue }
        $fNorm = [System.IO.Path]::GetFullPath($f).Replace('\', '/').TrimEnd('/')
        if ($resolved -eq $fNorm -or $resolved.StartsWith($fNorm + '/', [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "PREFLIGHT REFUSAL: $Label path '$PathToCheck' resolves inside forbidden root '$f'"
        }
        if ($fNorm.StartsWith($resolved + '/', [System.StringComparison]::OrdinalIgnoreCase) -or $resolved -eq $homeNorm) {
            throw "PREFLIGHT REFUSAL: $Label path '$PathToCheck' contains or is parent of forbidden root '$f'"
        }
    }
}

function Assert-InsideDrillRoot {
    param([string]$PathToCheck)
    $resolved = [System.IO.Path]::GetFullPath($PathToCheck).Replace('\', '/').TrimEnd('/')
    $rootNorm = [System.IO.Path]::GetFullPath($DrillRoot).Replace('\', '/').TrimEnd('/')
    if (-not $resolved.StartsWith($rootNorm + '/', [System.StringComparison]::OrdinalIgnoreCase) -and $resolved -ne $rootNorm) {
        throw "PATH GUARD REFUSAL: Target '$PathToCheck' is not inside DrillRoot '$DrillRoot'"
    }
    Assert-NotForbiddenPath $PathToCheck 'Target inside DrillRoot'
}

function Resolve-Executable {
    param([string]$CommandOrPath, [string]$Label)
    if ([string]::IsNullOrWhiteSpace($CommandOrPath)) {
        throw "$Label executable is not specified."
    }
    if (Test-Path -LiteralPath $CommandOrPath -PathType Leaf) {
        return (Resolve-Path -LiteralPath $CommandOrPath).Path
    }
    $cmd = Get-Command $CommandOrPath -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }
    throw "$Label executable '$CommandOrPath' not found in PATH or as file."
}

function Get-HostVersion {
    param([string]$TargetApp, [string]$CommandName)
    try {
        $cmd = Get-Command $CommandName -ErrorAction SilentlyContinue
        if (-not $cmd) {
            if ($TargetApp -eq 'claude-code') {
                $cmd = Get-Command 'claude-code' -ErrorAction SilentlyContinue
            } elseif ($TargetApp -eq 'antigravity') {
                $cmd = Get-Command 'antigravity' -ErrorAction SilentlyContinue
            }
        }
        if (-not $cmd) { return 'unknown' }

        $pinfo = [System.Diagnostics.ProcessStartInfo]::new()
        if ($cmd.Source -match '\.(cmd|bat)$') {
            $pinfo.FileName = 'cmd.exe'
            $pinfo.Arguments = "/c `"$($cmd.Source)`" --version"
        } elseif ($cmd.Source -match '\.ps1$') {
            $pinfo.FileName = 'pwsh.exe'
            $pinfo.Arguments = "-NoProfile -File `"$($cmd.Source)`" --version"
        } else {
            $pinfo.FileName = $cmd.Source
            $pinfo.Arguments = '--version'
        }
        $pinfo.RedirectStandardOutput = $true
        $pinfo.RedirectStandardError = $true
        $pinfo.UseShellExecute = $false
        $pinfo.CreateNoWindow = $true

        $proc = [System.Diagnostics.Process]::Start($pinfo)
        if ($proc.WaitForExit(15000) -and $proc.ExitCode -eq 0) {
            $raw = $proc.StandardOutput.ReadToEnd().Trim()
            $firstLine = ($raw -split "`r?`n")[0].Trim()
            if ($firstLine) { return $firstLine }
        } else {
            try { $proc.Kill() } catch {}
        }
    } catch {}
    return 'unknown'
}

function Add-Evidence {
    param(
        [string]$Step,
        [string]$Command,
        [int]$ExitCode,
        [string]$HostVersion,
        $Hashes,
        [string]$LogPath,
        [string]$Result,
        [string]$EvidenceApp = $null
    )
    $record = [ordered]@{
        step = $Step
        time = [DateTime]::UtcNow.ToString("o")
        command = $Command
        exit_code = $ExitCode
        host_version = $HostVersion
        hashes = $Hashes
        log_path = $LogPath
        result = $Result
    }
    if ($EvidenceApp) {
        $record['app'] = $EvidenceApp
    }
    $evPath = Join-Path $EvidenceDir 'evidence.json'
    $list = @()
    if (Test-Path -LiteralPath $evPath) {
        $raw = Get-Content -LiteralPath $evPath -Raw
        if ($raw) {
            $list = @(ConvertFrom-Json $raw)
        }
    }
    $list += [PSCustomObject]$record
    $list | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $evPath -Encoding utf8
    return $record
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

function Invoke-Chronicle {
    param([string]$StepName, [string[]]$CliArgs)
    $logFile = Join-Path $EvidenceDir "$StepName.log"
    $cmdDisplay = "$ChronicleExePath " + ($CliArgs -join ' ')
    $pinfo = [System.Diagnostics.ProcessStartInfo]::new()
    if ($ChronicleExePath -match '\.(cmd|bat)$') {
        $pinfo.FileName = 'cmd.exe'
        $pinfo.Arguments = "/c `"$ChronicleExePath`" " + ($CliArgs -join ' ')
    } elseif ($ChronicleExePath -match '\.ps1$') {
        $pinfo.FileName = 'pwsh.exe'
        $pinfo.Arguments = "-NoProfile -File `"$ChronicleExePath`" " + ($CliArgs -join ' ')
    } else {
        $pinfo.FileName = $ChronicleExePath
        $pinfo.Arguments = $CliArgs -join ' '
    }
    $pinfo.RedirectStandardOutput = $true
    $pinfo.RedirectStandardError = $true
    $pinfo.UseShellExecute = $false
    $pinfo.CreateNoWindow = $true

    $proc = [System.Diagnostics.Process]::Start($pinfo)
    $stdout = $proc.StandardOutput.ReadToEndAsync()
    $stderr = $proc.StandardError.ReadToEndAsync()
    $proc.WaitForExit()
    [System.Threading.Tasks.Task]::WaitAll($stdout, $stderr)
    $code = $proc.ExitCode
    $outStr = ($stdout.Result + "`n" + $stderr.Result).Trim()
    Set-Content -LiteralPath $logFile -Value $outStr -Encoding utf8
    return [PSCustomObject]@{ Command = $cmdDisplay; ExitCode = $code; Output = $outStr; LogPath = $logFile }
}

function Invoke-ClientProcess {
    param(
        [string]$CommandPath,
        [string[]]$ArgumentList,
        [hashtable]$Environment = @{},
        [string]$WorkingDirectory = $null,
        [int]$TimeoutSeconds = 120
    )
    $pinfo = [System.Diagnostics.ProcessStartInfo]::new()
    $joinedArgs = ($ArgumentList | ForEach-Object {
        if ($_ -match '[\s"]') {
            '`"' + ($_ -replace '"', '\"') + '`"'
        } else {
            $_
        }
    }) -join ' '

    if ($CommandPath -match '\.(cmd|bat)$') {
        $pinfo.FileName = 'cmd.exe'
        $pinfo.Arguments = "/c `"$CommandPath`" $joinedArgs"
    } elseif ($CommandPath -match '\.ps1$') {
        $pinfo.FileName = 'pwsh.exe'
        $pinfo.Arguments = "-NoProfile -File `"$CommandPath`" $joinedArgs"
    } else {
        $pinfo.FileName = $CommandPath
        $pinfo.Arguments = $joinedArgs
    }
    if ($WorkingDirectory) {
        $pinfo.WorkingDirectory = $WorkingDirectory
    }
    $pinfo.RedirectStandardOutput = $true
    $pinfo.RedirectStandardError = $true
    $pinfo.UseShellExecute = $false
    $pinfo.CreateNoWindow = $true

    foreach ($k in $Environment.Keys) {
        $pinfo.EnvironmentVariables[$k] = [string]$Environment[$k]
    }

    $proc = [System.Diagnostics.Process]::Start($pinfo)
    $stdout = $proc.StandardOutput.ReadToEndAsync()
    $stderr = $proc.StandardError.ReadToEndAsync()

    if ($proc.WaitForExit($TimeoutSeconds * 1000)) {
        [System.Threading.Tasks.Task]::WaitAll($stdout, $stderr)
        return [PSCustomObject]@{
            ExitCode = $proc.ExitCode
            Output = ($stdout.Result + "`n" + $stderr.Result).Trim()
        }
    } else {
        try { $proc.Kill() } catch {}
        return [PSCustomObject]@{
            ExitCode = -1
            Output = "Process timed out after $TimeoutSeconds seconds."
        }
    }
}

function Prompt-Checkpoint {
    param([string]$Title, [string]$Prompt)
    Write-Host "=== $Title ===" -ForegroundColor Cyan
    if ($NonInteractive) { return 'skipped' }
    $ans = Read-Host "$Prompt (y/n/skip)"
    return ($ans -match '^y' ? 'passed' : ($ans -match 'skip' ? 'skipped' : 'failed'))
}

# --- App Drivers Definition ---
$Drivers = @{
    'codex' = @{
        App = 'codex'
        Command = 'codex'
        EnvVar = 'CODEX_HOME'
        Headless = $true
        IsolationSupported = $true
        Sources = @(
            @{ Slot = 'sessions'; RelPath = 'sessions' }
            @{ Slot = 'archived'; RelPath = 'archived_sessions' }
            @{ Slot = 'state'; RelPath = 'state_5.sqlite' }
            @{ Slot = 'thread-history'; RelPath = 'thread_history_1.sqlite' }
            @{ Slot = 'index'; RelPath = 'session_index.jsonl' }
        )
        SessionTargets = @(
            'sessions', 'archived_sessions', 'state_5.sqlite',
            'state_5.sqlite-wal', 'state_5.sqlite-shm',
            'session_index.jsonl', 'thread_history_1.sqlite'
        )
        GetMapArgs = {
            param($HomeDir)
            $h = $HomeDir.Replace('\', '/')
            return @(
                '--map', "sessions=$h/sessions",
                '--map', "archived=$h/archived_sessions",
                '--map', "state=$h/state_5.sqlite",
                '--map', "thread-history=$h/thread_history_1.sqlite",
                '--map', "index=$h/session_index.jsonl"
            )
        }
        LoginHint = "`$env:CODEX_HOME = '{0}'; codex login"
    }

    'claude-code' = @{
        App = 'claude-code'
        Command = 'claude'
        EnvVar = 'CLAUDE_CONFIG_DIR'
        Headless = $true
        IsolationSupported = $true
        Sources = @(
            @{ Slot = 'projects'; RelPath = 'projects' }
            @{ Slot = 'tasks'; RelPath = 'tasks' }
            @{ Slot = 'file-history'; RelPath = 'file-history' }
        )
        SessionTargets = @('projects', 'tasks', 'file-history')
        GetMapArgs = {
            param($HomeDir)
            $h = $HomeDir.Replace('\', '/')
            $m = @(
                '--map', "projects=$h/projects",
                '--map', "tasks=$h/tasks",
                '--map', "file-history=$h/file-history"
            )
            $projDir = Join-Path $HomeDir 'projects'
            if (Test-Path -LiteralPath $projDir) {
                Get-ChildItem -LiteralPath $projDir -Directory | ForEach-Object {
                    $m += @('--map', "$($_.Name)=$($_.Name)")
                }
            }
            return $m
        }
        LoginHint = "`$env:CLAUDE_CONFIG_DIR = '{0}'; claude auth login"
    }

    'grok' = @{
        App = 'grok'
        Command = 'grok'
        EnvVar = 'GROK_HOME'
        Headless = $true
        IsolationSupported = $true
        Sources = @(
            @{ Slot = 'sessions'; RelPath = 'sessions' }
        )
        SessionTargets = @('sessions')
        GetMapArgs = {
            param($HomeDir)
            $h = $HomeDir.Replace('\', '/')
            $m = @('--map', "sessions=$h/sessions")
            $sessDir = Join-Path $HomeDir 'sessions'
            if (Test-Path -LiteralPath $sessDir) {
                Get-ChildItem -LiteralPath $sessDir -Directory | ForEach-Object {
                    $m += @('--map', "$($_.Name)=$($_.Name)")
                }
            }
            return $m
        }
        LoginHint = "`$env:GROK_HOME = '{0}'; grok login"
    }

    'antigravity' = @{
        App = 'antigravity'
        Command = 'agy'
        EnvVar = $null
        Headless = $true
        IsolationSupported = $false
        Sources = @(
            @{ Slot = 'app'; RelPath = '.gemini/antigravity/conversations' }
            @{ Slot = 'cli'; RelPath = '.gemini/antigravity-cli/conversations' }
            @{ Slot = 'brain'; RelPath = '.gemini/antigravity-cli/brain' }
            @{ Slot = 'ide'; RelPath = '.gemini/antigravity-ide' }
            @{ Slot = 'tmp'; RelPath = '.gemini/tmp' }
        )
        SessionTargets = @('.gemini/antigravity', '.gemini/antigravity-cli', '.gemini/antigravity-ide', '.gemini/tmp')
        GetMapArgs = {
            param($HomeDir)
            $h = $HomeDir.Replace('\', '/')
            return @(
                '--map', "app=$h/.gemini/antigravity/conversations",
                '--map', "cli=$h/.gemini/antigravity-cli/conversations",
                '--map', "brain=$h/.gemini/antigravity-cli/brain",
                '--map', "ide=$h/.gemini/antigravity-ide",
                '--map', "tmp=$h/.gemini/tmp"
            )
        }
        LoginHint = "agy login"
    }

    'cursor' = @{
        App = 'cursor'
        Command = 'cursor'
        EnvVar = $null
        Headless = $false
        IsolationSupported = $true
        Sources = @(
            @{ Slot = 'global-storage'; RelPath = 'data/User/globalStorage/state.vscdb' }
        )
        SessionTargets = @(
            'data/User/globalStorage/state.vscdb',
            'data/User/globalStorage/state.vscdb-wal',
            'data/User/globalStorage/state.vscdb-shm'
        )
        GetMapArgs = {
            param($HomeDir)
            $h = $HomeDir.Replace('\', '/')
            return @('--map', "global-storage=$h/data/User/globalStorage/state.vscdb")
        }
        LoginHint = "cursor --user-data-dir '{0}/data' --extensions-dir '{0}/ext' (sign in via GUI in isolated profile)"
    }
}

$driver = $Drivers[$App]
if (-not $driver) {
    throw "Unknown app: $App"
}

# --- Step 0: Preflight ---
$DrillRoot = [System.IO.Path]::GetFullPath($DrillRoot)
Assert-NotForbiddenPath $DrillRoot 'DrillRoot'

if ([string]::IsNullOrWhiteSpace($NasRepository)) {
    throw "NasRepository must not be empty."
}
if ($NasRepository -notmatch '^(rest:|sftp:|http:|https:|s3:)') {
    Assert-NotForbiddenPath $NasRepository 'NasRepository'
}

$ResticExePath = Resolve-Executable $Restic 'Restic'
$ChronicleExePath = Resolve-Executable $ChronicleExe 'Chronicle'

$clientCmd = Get-Command $driver.Command -ErrorAction SilentlyContinue
if (-not $clientCmd) {
    if ($App -eq 'claude-code') {
        $clientCmd = Get-Command 'claude-code' -ErrorAction SilentlyContinue
    } elseif ($App -eq 'antigravity') {
        $clientCmd = Get-Command 'antigravity' -ErrorAction SilentlyContinue
    }
}
if (-not $clientCmd) {
    throw "Client executable for '$App' ('$($driver.Command)') not found in PATH."
}
$clientExePath = $clientCmd.Source

if (-not (Test-Path -LiteralPath $DrillRoot)) {
    New-Item -ItemType Directory -Path $DrillRoot -Force | Out-Null
}
$drillMarker = Join-Path $DrillRoot '.chronicle-drill'
if (-not (Test-Path -LiteralPath $drillMarker)) {
    Set-Content -LiteralPath $drillMarker -Value "chronicle-drill-root" -Encoding utf8
}

# Dedicated isolated drill repository password
$DrillPasswordFile = Join-Path $DrillRoot '.restic-drill-password'
if (-not (Test-Path -LiteralPath $DrillPasswordFile -PathType Leaf)) {
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    $bytes = [byte[]]::new(32)
    $rng.GetBytes($bytes)
    $passBase64 = [Convert]::ToBase64String($bytes)
    Set-Content -LiteralPath $DrillPasswordFile -Value $passBase64 -Encoding utf8 -NoNewline
    $passBase64 = $null
    $bytes = $null
}

$AvatarHome = Join-Path $DrillRoot "$App/home"
if (-not (Test-Path -LiteralPath $AvatarHome)) {
    New-Item -ItemType Directory -Path $AvatarHome -Force | Out-Null
}
if ($App -eq 'cursor') {
    New-Item -ItemType Directory -Path (Join-Path $AvatarHome 'data/User/globalStorage') -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $AvatarHome 'ext') -Force | Out-Null
}

$runTimestamp = [DateTime]::UtcNow.ToString("yyyyMMdd-HHmmss")
$RunDir = Join-Path $DrillRoot "$App/runs/$runTimestamp"
$NativeRoot = Join-Path $RunDir 'data'
$LocalRepo = Join-Path $RunDir 'local-repo'
$EvidenceDir = Join-Path $RunDir 'evidence'
$CwdWorkspace = Join-Path $RunDir 'cwd'

@($RunDir, $NativeRoot, $LocalRepo, $EvidenceDir, $CwdWorkspace) | ForEach-Object {
    New-Item -ItemType Directory -Path $_ -Force | Out-Null
}

$hostVersion = Get-HostVersion -TargetApp $App -CommandName $driver.Command
if ($hostVersion -eq 'unknown') {
    Write-Warning "Host version probe returned 'unknown' for app '$App'."
}

if (-not $driver.IsolationSupported) {
    Add-Evidence 'preflight' 'preflight-check' 0 $hostVersion $null $null 'passed' | Out-Null
    Add-Evidence 'isolation-check' 'verify-client-isolation' 1 $hostVersion $null $null 'unsupported-isolation' | Out-Null
    Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'unsupported-isolation' $App | Out-Null
    Write-Warning "PREFLIGHT REFUSAL: App '$App' has no confirmed profile isolation mechanism in --help/--version. Recorded as unsupported-isolation."
    return
}

Add-Evidence 'preflight' 'preflight-check' 0 $hostVersion $null $null 'passed' | Out-Null

if ($Preflight) {
    Write-Host "Preflight check passed for app '$App'." -ForegroundColor Green
    Write-Host "  Host Version:       $hostVersion"
    Write-Host "  DrillRoot:          $DrillRoot"
    Write-Host "  Avatar Home:        $AvatarHome"
    Write-Host "  Run Directory:      $RunDir"
    Write-Host "  Restic Path:        $ResticExePath"
    Write-Host "  Chronicle Path:     $ChronicleExePath"
    Write-Host "  Client Executable:  $clientExePath"
    Write-Host "  Drill Password:     $DrillPasswordFile (isolated dedicated drill password)"
    Write-Host "  Isolation Mode:     Supported"
    return
}

# --- Step 1: Login Check ---
Write-Host "=== Step 1: Login Check ===" -ForegroundColor Cyan
$loginPassed = $false
$loginHintMsg = $driver.LoginHint -f $AvatarHome

if ($App -eq 'cursor') {
    $loginAns = Prompt-Checkpoint 'Login Check' "Has Cursor been signed in inside isolated profile ($AvatarHome/data)? (y/n/skip)"
    if ($loginAns -eq 'passed') {
        $loginPassed = $true
        Add-Evidence 'login-check' 'login-check' 0 $hostVersion $null $null 'passed' | Out-Null
    } else {
        Add-Evidence 'login-check' 'login-check' 1 $hostVersion $null $null 'blocked-login' | Out-Null
        Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'blocked-login' $App | Out-Null
        Write-Host "分身未登录。请运行以下命令完成登录:`n$loginHintMsg" -ForegroundColor Yellow
        exit 1
    }
} else {
    $probeArgs = switch ($App) {
        'codex'       { @('exec', '--json', '--skip-git-repo-check', '-C', $CwdWorkspace, 'ping') }
        'claude-code' { @('-p', '--output-format', 'json', 'ping') }
        'grok'        { @('-p', 'ping', '--output-format', 'json', '--cwd', $CwdWorkspace) }
    }
    $envTable = @{}
    if ($driver.EnvVar) {
        $envTable[$driver.EnvVar] = $AvatarHome
    }
    $probeRes = Invoke-ClientProcess -CommandPath $clientExePath -ArgumentList $probeArgs -Environment $envTable -WorkingDirectory $CwdWorkspace -TimeoutSeconds 30
    $probeLog = Join-Path $EvidenceDir 'login-check.log'
    Set-Content -LiteralPath $probeLog -Value $probeRes.Output -Encoding utf8

    $outLower = $probeRes.Output.ToLowerInvariant()
    $isUnauthenticated = ($probeRes.ExitCode -ne 0 -or $outLower -match 'unauthorized|not logged in|login required|authentication required|please log in|sign in|auth token')
    if ($isUnauthenticated) {
        Add-Evidence 'login-check' $loginHintMsg 1 $hostVersion $null $probeLog 'blocked-login' | Out-Null
        Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'blocked-login' $App | Out-Null
        Write-Host "分身未登录。请运行以下命令完成登录:`n$loginHintMsg" -ForegroundColor Yellow
        exit 1
    } else {
        $loginPassed = $true
        Add-Evidence 'login-check' 'login-check' 0 $hostVersion $null $probeLog 'passed' | Out-Null
    }
}

# --- Step 2: Create Session ---
Write-Host "=== Step 2: Create Session ===" -ForegroundColor Cyan
$secretGuid = [System.Guid]::NewGuid().ToString("D")
$secretMarker = "CHRONICLE-DRILL-$secretGuid"
$createPrompt = "Please reply with verbatim: $secretMarker"
$sessionId = $null

if ($App -eq 'cursor') {
    $createAns = Prompt-Checkpoint 'Create Session' "Launch: cursor --user-data-dir '$AvatarHome/data' --extensions-dir '$AvatarHome/ext'`nCreate a session with marker '$secretMarker'. Done?"
    if ($createAns -ne 'passed') {
        Add-Evidence 'create-session' 'manual-create-session' 1 $hostVersion $null $null 'failed' | Out-Null
        Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
        throw "Failed at create-session."
    }
    $sessionId = "cursor-session-$secretGuid"
    Add-Evidence 'create-session' 'manual-create-session' 0 $hostVersion $null $null 'passed' | Out-Null
} else {
    $envTable = @{}
    if ($driver.EnvVar) {
        $envTable[$driver.EnvVar] = $AvatarHome
    }

    if ($App -eq 'codex') {
        $createArgs = @('exec', '--json', '--skip-git-repo-check', '-C', $CwdWorkspace, $createPrompt)
        $createRes = Invoke-ClientProcess -CommandPath $clientExePath -ArgumentList $createArgs -Environment $envTable -WorkingDirectory $CwdWorkspace -TimeoutSeconds 120
        $createLog = Join-Path $EvidenceDir 'create-session.log'
        Set-Content -LiteralPath $createLog -Value $createRes.Output -Encoding utf8

        foreach ($line in ($createRes.Output -split "`r?`n")) {
            try {
                $j = ConvertFrom-Json $line
                if ($j.session_id) { $sessionId = $j.session_id }
                elseif ($j.id) { $sessionId = $j.id }
                elseif ($j.thread_id) { $sessionId = $j.thread_id }
            } catch {}
        }
        if (-not $sessionId -and (Test-Path (Join-Path $AvatarHome 'sessions'))) {
            $newest = Get-ChildItem -LiteralPath (Join-Path $AvatarHome 'sessions') -File | Sort-Object LastWriteTime -Descending | Select-Object -First 1
            if ($newest) { $sessionId = [System.IO.Path]::GetFileNameWithoutExtension($newest.Name) }
        }
    } elseif ($App -eq 'claude-code') {
        $sessionId = [System.Guid]::NewGuid().ToString("D")
        $createArgs = @('-p', '--session-id', $sessionId, '--output-format', 'json', $createPrompt)
        $createRes = Invoke-ClientProcess -CommandPath $clientExePath -ArgumentList $createArgs -Environment $envTable -WorkingDirectory $CwdWorkspace -TimeoutSeconds 120
        $createLog = Join-Path $EvidenceDir 'create-session.log'
        Set-Content -LiteralPath $createLog -Value $createRes.Output -Encoding utf8
    } elseif ($App -eq 'grok') {
        $sessionId = [System.Guid]::NewGuid().ToString("D")
        $createArgs = @('-p', $createPrompt, '--session-id', $sessionId, '--output-format', 'json', '--cwd', $CwdWorkspace)
        $createRes = Invoke-ClientProcess -CommandPath $clientExePath -ArgumentList $createArgs -Environment $envTable -WorkingDirectory $CwdWorkspace -TimeoutSeconds 120
        $createLog = Join-Path $EvidenceDir 'create-session.log'
        Set-Content -LiteralPath $createLog -Value $createRes.Output -Encoding utf8
    }

    if (-not $sessionId) {
        Add-Evidence 'create-session' 'create-session' 1 $hostVersion $null $createLog 'failed' | Out-Null
        Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
        throw "Failed to extract session ID from create-session."
    }
    Add-Evidence 'create-session' 'create-session' 0 $hostVersion $null $createLog 'passed' | Out-Null
}

# --- Step 3: Capture / Step 4: Replicate / Step 5: Verify ---
Write-Host "=== Steps 3-5: Capture, Replicate, Verify ===" -ForegroundColor Cyan
$ConfigPath = Join-Path $RunDir 'chronicle-native.toml'
$dataRootToml = $NativeRoot.Replace('\', '/')
$localRepoToml = $LocalRepo.Replace('\', '/')
$nasRepoToml = $NasRepository.Replace('\', '/')
$passToml = $DrillPasswordFile.Replace('\', '/')
$resticToml = $ResticExePath.Replace('\', '/')

$sourcesToml = @()
foreach ($s in $driver.Sources) {
    $srcPath = (Join-Path $AvatarHome $s.RelPath).Replace('\', '/')
    $sourcesToml += "[[sources]]`napp = `"$App`"`ncomponent = `"sessions`"`nslot = `"$($s.Slot)`"`npath = `"$srcPath`"`nhost_version = `"$hostVersion`"`n"
}

$configContent = "data_root = `"$dataRootToml`"`nlocal_repository = `"$localRepoToml`"`nremote_repository = `"$nasRepoToml`"`npassword_file = `"$passToml`"`nrestic = `"$resticToml`"`ncomponents = [`"sessions`"]`n`n" + ($sourcesToml -join "`n")
Set-Content -LiteralPath $ConfigPath -Value $configContent -Encoding utf8

# Check/Init NAS Restic repository using isolated drill password
$env:RESTIC_PASSWORD_FILE = $DrillPasswordFile
$catProc = [System.Diagnostics.Process]::Start([System.Diagnostics.ProcessStartInfo]@{
    FileName = $ResticExePath
    Arguments = "-r `"$NasRepository`" cat config"
    RedirectStandardOutput = $true
    RedirectStandardError = $true
    UseShellExecute = $false
    CreateNoWindow = $true
})
$catProc.WaitForExit()
if ($catProc.ExitCode -ne 0) {
    Write-Host "Initializing NAS restic repository at '$NasRepository'..." -ForegroundColor Cyan
    $initProc = [System.Diagnostics.Process]::Start([System.Diagnostics.ProcessStartInfo]@{
        FileName = $ResticExePath
        Arguments = "-r `"$NasRepository`" init"
        RedirectStandardOutput = $true
        RedirectStandardError = $true
        UseShellExecute = $false
        CreateNoWindow = $true
    })
    $initProc.WaitForExit()
    if ($initProc.ExitCode -ne 0) {
        throw "Failed to initialize NAS restic repository at '$NasRepository'."
    }
}

$cap = Invoke-Chronicle 'capture' @('native', '--native-config', $ConfigPath, '--json', 'capture', '--app', $App)
$snapshotId = $null
if ($cap.ExitCode -eq 0) {
    try {
        $capObj = $cap.Output | ConvertFrom-Json
        $snapshotId = if ($capObj.snapshot_id) { $capObj.snapshot_id } elseif ($capObj.result.snapshot_id) { $capObj.result.snapshot_id } else { $null }
    } catch {}
}
if (-not $snapshotId) {
    Add-Evidence 'capture' $cap.Command $cap.ExitCode $hostVersion $null $cap.LogPath 'failed' | Out-Null
    Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
    throw "Capture failed to produce snapshot ID."
}
$capHashes = Get-DirHashes (Join-Path (Join-Path $NativeRoot 'snapshots') $snapshotId)
Add-Evidence 'capture' $cap.Command $cap.ExitCode $hostVersion $capHashes $cap.LogPath 'passed' | Out-Null

$rep = Invoke-Chronicle 'replicate' @('native', '--native-config', $ConfigPath, '--json', 'replicate', $snapshotId)
if ($rep.ExitCode -ne 0) {
    Add-Evidence 'replicate' $rep.Command $rep.ExitCode $hostVersion $null $rep.LogPath 'failed' | Out-Null
    Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
    throw "Replicate failed."
}
Add-Evidence 'replicate' $rep.Command $rep.ExitCode $hostVersion $null $rep.LogPath 'passed' | Out-Null

$ver = Invoke-Chronicle 'verify' @('native', '--native-config', $ConfigPath, '--json', 'verify', $snapshotId)
if ($ver.ExitCode -ne 0) {
    Add-Evidence 'verify' $ver.Command $ver.ExitCode $hostVersion $null $ver.LogPath 'failed' | Out-Null
    Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
    throw "Verify failed."
}
Add-Evidence 'verify' $ver.Command $ver.ExitCode $hostVersion $null $ver.LogPath 'passed' | Out-Null

# --- Step 6: Remove-Source ---
Write-Host "=== Step 6: Remove-Source ===" -ForegroundColor Cyan
foreach ($targetRel in $driver.SessionTargets) {
    $t = Join-Path $AvatarHome $targetRel
    if (Test-Path -LiteralPath $t) {
        Assert-InsideDrillRoot $t
        Remove-Item -LiteralPath $t -Recurse -Force
    }
}
Assert-InsideDrillRoot $NativeRoot
Assert-InsideDrillRoot $LocalRepo
if (Test-Path -LiteralPath $NativeRoot) {
    Remove-Item -LiteralPath $NativeRoot -Recurse -Force
}
if (Test-Path -LiteralPath $LocalRepo) {
    Remove-Item -LiteralPath $LocalRepo -Recurse -Force
}
Add-Evidence 'remove-source' 'remove-source' 0 $hostVersion $null $null 'passed' | Out-Null

# --- Step 7: Pull from Remote NAS ---
Write-Host "=== Step 7: Pull from Remote NAS ===" -ForegroundColor Cyan
$PulledDataRoot = Join-Path $RunDir 'pulled-data'
New-Item -ItemType Directory -Path $PulledDataRoot -Force | Out-Null
$pulledDataToml = $PulledDataRoot.Replace('\', '/')

$PullConfigPath = Join-Path $RunDir 'chronicle-pull.toml'
$pullConfigContent = "data_root = `"$pulledDataToml`"`nremote_repository = `"$nasRepoToml`"`npassword_file = `"$passToml`"`nrestic = `"$resticToml`"`ncomponents = [`"sessions`"]`n"
Set-Content -LiteralPath $PullConfigPath -Value $pullConfigContent -Encoding utf8

$pull = Invoke-Chronicle 'pull' @('native', '--native-config', $PullConfigPath, '--json', 'pull', $snapshotId, '--from', 'remote')
if ($pull.ExitCode -ne 0) {
    Add-Evidence 'pull' $pull.Command $pull.ExitCode $hostVersion $null $pull.LogPath 'failed' | Out-Null
    Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
    throw "Pull failed."
}
Add-Evidence 'pull' $pull.Command $pull.ExitCode $hostVersion $null $pull.LogPath 'passed' | Out-Null

# --- Step 8: Restore-Install ---
Write-Host "=== Step 8: Restore-Install ===" -ForegroundColor Cyan
$mapArgs = & $driver.GetMapArgs $AvatarHome
$planArgs = @('native', '--native-config', $PullConfigPath, '--json', 'restore', $snapshotId, '--into', $App) + $mapArgs
$plan = Invoke-Chronicle 'restore-plan' $planArgs
if ($plan.ExitCode -ne 0) {
    Add-Evidence 'restore-install' $plan.Command $plan.ExitCode $hostVersion $null $plan.LogPath 'failed' | Out-Null
    Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
    throw "Restore plan generation failed."
}
$planObj = $plan.Output | ConvertFrom-Json
$applyPlanPath = if ($planObj.apply_plan) { $planObj.apply_plan } elseif ($planObj.result.apply_plan) { $planObj.result.apply_plan } else { $null }
if (-not $applyPlanPath) {
    Add-Evidence 'restore-install' $plan.Command 1 $hostVersion $null $plan.LogPath 'failed' | Out-Null
    Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
    throw "Restore plan did not return apply_plan path."
}

$apply = Invoke-Chronicle 'restore-apply' @('native', '--native-config', $PullConfigPath, '--json', 'restore', '--apply-plan', $applyPlanPath)
if ($apply.ExitCode -ne 0) {
    Add-Evidence 'restore-install' $apply.Command $apply.ExitCode $hostVersion $null $apply.LogPath 'failed' | Out-Null
    Add-Evidence 'summary' 'summary' 1 $hostVersion $null $null 'uat-incomplete' $App | Out-Null
    throw "Restore apply failed."
}
$restoredHashes = Get-DirHashes $AvatarHome
Add-Evidence 'restore-install' $apply.Command 0 $hostVersion $restoredHashes $apply.LogPath 'passed' | Out-Null

# --- Step 9: Open & Step 10: Restart ---
Write-Host "=== Steps 9-10: Open and Restart ===" -ForegroundColor Cyan
$openToken = "OPEN_TOKEN_" + [System.Guid]::NewGuid().ToString("N").Substring(0, 8)
$restartToken = "RESTART_TOKEN_" + [System.Guid]::NewGuid().ToString("N").Substring(0, 8)
$openPrompt = "本对话里的暗号是什么？只回复暗号 [$openToken]"
$restartPrompt = "本对话里的暗号是什么？只回复暗号 [$restartToken]"

if ($App -eq 'cursor') {
    $openAns = Prompt-Checkpoint 'Open Client' "Launch: cursor --user-data-dir '$AvatarHome/data' --extensions-dir '$AvatarHome/ext'`nIs session with marker '$secretMarker' visible?"
    Add-Evidence 'open' 'manual-open' ($openAns -eq 'passed' ? 0 : 1) $hostVersion $null $null $openAns | Out-Null

    $restartAns = Prompt-Checkpoint 'Restart Client' "Close and relaunch cursor.`nIs session with marker '$secretMarker' still visible?"
    Add-Evidence 'restart' 'manual-restart' ($restartAns -eq 'passed' ? 0 : 1) $hostVersion $null $null $restartAns | Out-Null

    $continueAns = Prompt-Checkpoint 'Continue Chat' "Send another message in the session. Did continuation work?"
    Add-Evidence 'continue' 'manual-continue' ($continueAns -eq 'passed' ? 0 : 1) $hostVersion $null $null $continueAns | Out-Null

    $allPassed = ($openAns -eq 'passed' -and $restartAns -eq 'passed' -and $continueAns -eq 'passed')
} else {
    $envTable = @{}
    if ($driver.EnvVar) {
        $envTable[$driver.EnvVar] = $AvatarHome
    }

    # Open check
    $openArgs = switch ($App) {
        'codex'       { @('exec', 'resume', $sessionId, $openPrompt, '--skip-git-repo-check', '--json') }
        'claude-code' { @('-p', '--resume', $sessionId, '--output-format', 'json', $openPrompt) }
        'grok'        { @('-p', $openPrompt, '--resume', $sessionId, '--output-format', 'json', '--cwd', $CwdWorkspace) }
    }
    $openRes = Invoke-ClientProcess -CommandPath $clientExePath -ArgumentList $openArgs -Environment $envTable -WorkingDirectory $CwdWorkspace -TimeoutSeconds 120
    $openLog = Join-Path $EvidenceDir 'open.log'
    Set-Content -LiteralPath $openLog -Value $openRes.Output -Encoding utf8
    $openPassed = ($openRes.ExitCode -eq 0 -and $openRes.Output -match [regex]::Escape($secretMarker))
    Add-Evidence 'open' 'open-resume-check' ($openPassed ? 0 : 1) $hostVersion $null $openLog ($openPassed ? 'passed' : 'failed') | Out-Null

    # Restart check
    $restartArgs = switch ($App) {
        'codex'       { @('exec', 'resume', $sessionId, $restartPrompt, '--skip-git-repo-check', '--json') }
        'claude-code' { @('-p', '--resume', $sessionId, '--output-format', 'json', $restartPrompt) }
        'grok'        { @('-p', $restartPrompt, '--resume', $sessionId, '--output-format', 'json', '--cwd', $CwdWorkspace) }
    }
    $restartRes = Invoke-ClientProcess -CommandPath $clientExePath -ArgumentList $restartArgs -Environment $envTable -WorkingDirectory $CwdWorkspace -TimeoutSeconds 120
    $restartLog = Join-Path $EvidenceDir 'restart.log'
    Set-Content -LiteralPath $restartLog -Value $restartRes.Output -Encoding utf8
    $restartPassed = ($restartRes.ExitCode -eq 0 -and $restartRes.Output -match [regex]::Escape($secretMarker))
    Add-Evidence 'restart' 'restart-resume-check' ($restartPassed ? 0 : 1) $hostVersion $null $restartLog ($restartPassed ? 'passed' : 'failed') | Out-Null

    # --- Step 11: Continue ---
    Write-Host "=== Step 11: Continue ===" -ForegroundColor Cyan
    $continuePassed = $false
    if ($openPassed -and $restartPassed) {
        Get-ChildItem -LiteralPath $AvatarHome -Recurse -File | ForEach-Object {
            try {
                $content = [System.IO.File]::ReadAllText($_.FullName)
                if ($content.Contains($openToken) -and $content.Contains($restartToken)) {
                    $continuePassed = $true
                }
            } catch {}
        }
    }
    Add-Evidence 'continue' 'verify-continuation-appended' ($continuePassed ? 0 : 1) $hostVersion $null $null ($continuePassed ? 'passed' : 'failed') | Out-Null

    $allPassed = ($openPassed -and $restartPassed -and $continuePassed)
}

# --- Step 12: Summary ---
Write-Host "=== Step 12: Summary ===" -ForegroundColor Cyan
$overallStatus = ($allPassed ? 'uat-passed' : 'uat-incomplete')
$summaryExit = ($allPassed ? 0 : 1)
$evPath = Join-Path $EvidenceDir 'evidence.json'
Add-Evidence 'summary' 'summary' $summaryExit $hostVersion $null $null $overallStatus $App | Out-Null

Write-Host "`nUAT DRILL SUMMARY: $overallStatus" -ForegroundColor ($allPassed ? 'Green' : 'Yellow')
Write-Host "Evidence JSON: $evPath"

if ($Register -and $allPassed) {
    Write-Host "`nRegistering verified host record with Chronicle..." -ForegroundColor Cyan
    $recRes = & $ChronicleExePath native hosts record --app $App --evidence $evPath 2>&1
    Write-Host ($recRes | Out-String)
}

if (-not $allPassed) {
    exit 1
}
