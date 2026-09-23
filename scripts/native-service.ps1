<#
.SYNOPSIS
    Manages the Chronicle native recovery watch background service via Windows Scheduled Tasks.
.DESCRIPTION
    Installs, uninstalls, or checks status of the user-level Scheduled Task (\Chronicle\NativeWatch).
    Safe by default: requires -DryRun to preview or explicit -IReallyMeanIt to execute against the scheduler.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('Install', 'Uninstall', 'Status')]
    [string]$Action,

    [Parameter(Mandatory = $false)]
    [string]$ChronicleExe,

    [Parameter(Mandatory = $false)]
    [string]$NativeConfig,

    [Parameter(Mandatory = $false)]
    [switch]$DryRun,

    [Parameter(Mandatory = $false)]
    [switch]$IReallyMeanIt
)

$TaskFolder = '\Chronicle\'
$TaskName = 'NativeWatch'
$FullTaskPath = "$TaskFolder$TaskName"

# Gate: Require -DryRun or -IReallyMeanIt for all operations
if (-not $DryRun -and -not $IReallyMeanIt) {
    Write-Error "Refused: modifying or querying the real Task Scheduler requires either -DryRun (preview) or explicit switch -IReallyMeanIt."
    exit 1
}

# Resolve Chronicle executable
$resolvedExe = $null
if (-not [string]::IsNullOrWhiteSpace($ChronicleExe)) {
    if (Test-Path -LiteralPath $ChronicleExe) {
        $resolvedExe = (Resolve-Path -LiteralPath $ChronicleExe).Path
    } else {
        $resolvedExe = [System.IO.Path]::GetFullPath($ChronicleExe)
    }
} else {
    $cmd = Get-Command 'chronicle' -ErrorAction SilentlyContinue
    if ($null -eq $cmd) {
        $cmd = Get-Command 'hstry' -ErrorAction SilentlyContinue
    }
    if ($null -ne $cmd -and $null -ne $cmd.Source) {
        $resolvedExe = $cmd.Source
    } else {
        $resolvedExe = [System.IO.Path]::GetFullPath("chronicle.exe")
    }
}

# Resolve NativeConfig
$resolvedConfig = $null
if (-not [string]::IsNullOrWhiteSpace($NativeConfig)) {
    if (Test-Path -LiteralPath $NativeConfig) {
        $resolvedConfig = (Resolve-Path -LiteralPath $NativeConfig).Path
    } else {
        $resolvedConfig = [System.IO.Path]::GetFullPath($NativeConfig)
    }
}

switch ($Action) {
    'Install' {
        if ([string]::IsNullOrWhiteSpace($NativeConfig)) {
            Write-Error "-NativeConfig is required for Action Install."
            exit 1
        }

        $arguments = "native --native-config `"$resolvedConfig`" watch"

        if ($DryRun) {
            Write-Host "[DryRun] Scheduled Task Registration Preview:"
            Write-Host "  Task Folder       : $TaskFolder"
            Write-Host "  Task Name         : $TaskName"
            Write-Host "  Task Path         : $FullTaskPath"
            Write-Host "  User Principal    : $env:USERNAME (LogonType: Interactive, RunLevel: Limited, No Stored Password)"
            Write-Host "  Trigger           : AtLogOn (User: $env:USERNAME)"
            Write-Host "  Action Executable : $resolvedExe"
            Write-Host "  Action Arguments  : $arguments"
            Write-Host "  Settings          : MultipleInstances = IgnoreNew, RestartCount = 3, RestartInterval = PT1M, ExecutionTimeLimit = PT0S"
            Write-Host "  Overlap Check     : Would scan tasks for actions matching '(chronicle|hstry).*native.*watch' outside $FullTaskPath (read-only notification, no modification)"
            Write-Host "  Command (Install) : Register-ScheduledTask -TaskName '$TaskName' -TaskPath '$TaskFolder' -Action (New-ScheduledTaskAction -Execute '$resolvedExe' -Argument '$arguments') -Trigger (New-ScheduledTaskTrigger -AtLogOn -User '$env:USERNAME') -Principal (New-ScheduledTaskPrincipal -UserId '$env:USERNAME' -LogonType Interactive -RunLevel Limited) -Settings (New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)) -Force"
            exit 0
        }

        # Step 1: Check for overlapping tasks without modifying them
        Write-Host "Checking for overlapping native watch tasks..."
        $overlaps = @()
        try {
            $allTasks = Get-ScheduledTask -ErrorAction SilentlyContinue
            foreach ($t in $allTasks) {
                if ($t.TaskPath -eq $TaskFolder -and $t.TaskName -eq $TaskName) { continue }
                foreach ($act in $t.Actions) {
                    $cmdLine = "$($act.Execute) $($act.Arguments)"
                    if ($cmdLine -match '(chronicle|hstry)' -and $cmdLine -match 'native' -and $cmdLine -match 'watch') {
                        $overlaps += "$($t.TaskPath)$($t.TaskName) -> $cmdLine"
                    }
                }
            }
        } catch {
            Write-Warning "Failed to query existing scheduled tasks for overlap check: $_"
        }

        if ($overlaps.Count -gt 0) {
            Write-Host "Notice: Found $($overlaps.Count) potential overlapping task(s) (leaving unmodified):"
            $overlaps | ForEach-Object { Write-Host "  - $_" }
        } else {
            Write-Host "No overlapping tasks found."
        }

        # Step 2: Register or update the task idempotently (-Force updates in place)
        Write-Host "Registering scheduled task '$FullTaskPath'..."
        $actionObj = New-ScheduledTaskAction -Execute $resolvedExe -Argument $arguments
        $triggerObj = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
        $principalObj = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
        $settingsObj = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
        Register-ScheduledTask -TaskName $TaskName -TaskPath $TaskFolder -Action $actionObj -Trigger $triggerObj -Principal $principalObj -Settings $settingsObj -Force | Out-Null
        Write-Host "Scheduled task '$FullTaskPath' successfully installed."
        exit 0
    }

    'Uninstall' {
        if ($DryRun) {
            Write-Host "[DryRun] Scheduled Task Unregistration Preview:"
            Write-Host "  Target Task : $FullTaskPath"
            Write-Host "  Command     : Unregister-ScheduledTask -TaskName '$TaskName' -TaskPath '$TaskFolder' -Confirm:`$false"
            exit 0
        }

        Write-Host "Unregistering scheduled task '$FullTaskPath'..."
        Unregister-ScheduledTask -TaskName $TaskName -TaskPath $TaskFolder -Confirm:$false
        Write-Host "Scheduled task '$FullTaskPath' uninstalled."
        exit 0
    }

    'Status' {
        if ($DryRun) {
            Write-Host "[DryRun] Scheduled Task Status Preview:"
            Write-Host "  Target Task : $FullTaskPath"
            Write-Host "  Command     : Get-ScheduledTask -TaskName '$TaskName' -TaskPath '$TaskFolder'; Get-ScheduledTaskInfo -TaskName '$TaskName' -TaskPath '$TaskFolder'"
            exit 0
        }

        $task = Get-ScheduledTask -TaskName $TaskName -TaskPath $TaskFolder -ErrorAction SilentlyContinue
        if ($null -eq $task) {
            Write-Host "Task '$FullTaskPath' is not registered."
        } else {
            $info = Get-ScheduledTaskInfo -TaskName $TaskName -TaskPath $TaskFolder -ErrorAction SilentlyContinue
            Write-Host "Task Path       : $($task.TaskPath)$($task.TaskName)"
            Write-Host "State           : $($task.State)"
            if ($null -ne $info) {
                Write-Host "Last Run Time   : $($info.LastRunTime)"
                Write-Host "Last Result     : $($info.LastTaskResult)"
                Write-Host "Next Run Time   : $($info.NextRunTime)"
            }
        }
        exit 0
    }
}
