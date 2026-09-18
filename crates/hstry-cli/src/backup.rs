//! Chronicle / hstry 3-2-1 backup: integrity, NAS, Oracle, Google Drive.
//!
//! Never read the live database into memory for hashing. Never fall back to
//! test fixtures. Never use a hardcoded encryption passphrase.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result, bail};
use hstry_core::checkpoint::{create_checkpoint, prune_checkpoints};
use hstry_core::{Config, Database};
use serde::Serialize;
use which::which;

const DEFAULT_NAS_REMOTE: &str = "nas-lan";
const DEFAULT_ORACLE_HOST: &str = "oracle-arm";
const DEFAULT_ORACLE_DIR: &str = "~/archives/chronicle";
const DEFAULT_RCLONE_REMOTE: &str = "gdrive:chronicle-cold-backup";
const BACKUP_KEY_ENV: &str = "CHRONICLE_BACKUP_KEY";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BackupTarget {
    Nas,
    Oracle,
    Gdrive,
    All,
}

#[derive(Debug, Clone)]
pub struct BackupOpts {
    pub dry_run: bool,
    pub encrypt: bool,
    pub targets: Vec<BackupTarget>,
    pub nas_remote: String,
}

#[derive(Debug, Serialize)]
pub struct StepReport {
    pub name: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct BackupReport {
    pub ok: bool,
    pub database: String,
    pub database_bytes: u64,
    pub integrity: Option<String>,
    pub checkpoint_stem: Option<String>,
    pub checkpoint_path: Option<String>,
    pub payload_path: Option<String>,
    pub encrypt: bool,
    pub dry_run: bool,
    pub steps: Vec<StepReport>,
}

pub async fn run(
    db: &Database,
    config: &Config,
    config_path: &Path,
    opts: BackupOpts,
    json: bool,
) -> Result<()> {
    reject_fixture_database(&config.database)?;

    if opts.encrypt {
        match env::var(BACKUP_KEY_ENV) {
            Ok(key) if !key.is_empty() => {}
            _ => {
                bail!(
                    "--encrypt requires a non-empty {BACKUP_KEY_ENV}; refusing a default or hardcoded passphrase"
                );
            }
        }
    }

    let meta = fs::metadata(&config.database).with_context(|| {
        format!(
            "source database missing: {} (configure hstry, do not use tests/fixtures)",
            config.database.display()
        )
    })?;
    let database_bytes = meta.len();

    let mut report = BackupReport {
        ok: true,
        database: config.database.display().to_string(),
        database_bytes,
        integrity: None,
        checkpoint_stem: None,
        checkpoint_path: None,
        payload_path: None,
        encrypt: opts.encrypt,
        dry_run: opts.dry_run,
        steps: Vec::new(),
    };

    if opts.dry_run {
        report.steps.push(StepReport {
            name: "integrity".into(),
            status: "dry-run".into(),
            detail: format!(
                "would run PRAGMA integrity_check on {} ({database_bytes} bytes)",
                config.database.display()
            ),
        });
    } else {
        let integrity = db.integrity_check().await?;
        report.integrity = Some(integrity.clone());
        if integrity != "ok" {
            report.steps.push(StepReport {
                name: "integrity".into(),
                status: "failed".into(),
                detail: integrity.clone(),
            });
            return finish(report, json, false);
        }
        report.steps.push(StepReport {
            name: "integrity".into(),
            status: "ok".into(),
            detail: integrity,
        });
    }

    let wants_nas = wants(&opts.targets, BackupTarget::Nas);
    let wants_oracle = wants(&opts.targets, BackupTarget::Oracle);
    let wants_gdrive = wants(&opts.targets, BackupTarget::Gdrive);
    let needs_snapshot = wants_oracle || wants_gdrive;

    let checkpoint_dir = config.checkpoint.resolve_dir(&config.database);
    let mut payload: Option<PathBuf> = None;

    if needs_snapshot {
        if opts.dry_run {
            report.steps.push(StepReport {
                name: "checkpoint".into(),
                status: "dry-run".into(),
                detail: format!(
                    "would create checkpoint in {} then prune",
                    checkpoint_dir.display()
                ),
            });
        } else {
            let created = create_checkpoint(db, &config.database, &config.checkpoint, None)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            prune_checkpoints(&checkpoint_dir, &config.checkpoint)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            report.checkpoint_stem = Some(created.manifest.stem.clone());
            report.checkpoint_path = Some(created.archive_path.display().to_string());
            report.steps.push(StepReport {
                name: "checkpoint".into(),
                status: "ok".into(),
                detail: format!(
                    "{} ({} bytes compressed)",
                    created.archive_path.display(),
                    created.manifest.compressed_bytes
                ),
            });
            payload = Some(created.archive_path);
        }
    }

    if opts.encrypt && needs_snapshot {
        payload = Some(maybe_encrypt(payload.as_deref(), &opts, &mut report)?);
    }
    report.payload_path = payload.as_ref().map(|p| p.display().to_string());

    if wants_nas {
        report.steps.push(run_nas(config_path, &opts, json)?);
    }

    if wants_oracle {
        report.steps.push(run_oracle(payload.as_deref(), &opts)?);
    }

    if wants_gdrive {
        report.steps.push(run_gdrive(payload.as_deref(), &opts)?);
    }

    let ok = report
        .steps
        .iter()
        .all(|s| s.status == "ok" || s.status == "dry-run" || s.status == "skipped");
    finish(report, json, ok)
}

fn wants(targets: &[BackupTarget], needle: BackupTarget) -> bool {
    targets
        .iter()
        .any(|t| *t == BackupTarget::All || *t == needle)
}

fn reject_fixture_database(path: &Path) -> Result<()> {
    let normalized = path.to_string_lossy().replace('\\', "/").to_lowercase();
    if normalized.contains("tests/fixtures")
        || normalized.contains("/fixtures/")
        || normalized.ends_with("/sample_hstry.db")
    {
        bail!(
            "refusing fixture database {} — Chronicle backup uses the configured live archive only",
            path.display()
        );
    }
    Ok(())
}

fn maybe_encrypt(
    archive: Option<&Path>,
    opts: &BackupOpts,
    report: &mut BackupReport,
) -> Result<PathBuf> {
    let dest_hint = archive
        .map(|p| {
            let mut name = p.as_os_str().to_os_string();
            name.push(".enc");
            PathBuf::from(name)
        })
        .unwrap_or_else(|| PathBuf::from("<checkpoint>.zst.enc"));
    if opts.dry_run {
        report.steps.push(StepReport {
            name: "encrypt".into(),
            status: "dry-run".into(),
            detail: format!(
                "would openssl enc -aes-256-cbc -pbkdf2 using {BACKUP_KEY_ENV} -> {}",
                dest_hint.display()
            ),
        });
        return Ok(dest_hint);
    }
    let Some(archive) = archive else {
        bail!("--encrypt requires a checkpoint payload");
    };
    let dest = {
        let mut name = archive.as_os_str().to_os_string();
        name.push(".enc");
        PathBuf::from(name)
    };
    let openssl = which("openssl").context("openssl not found on PATH (needed for --encrypt)")?;
    let status = ProcessCommand::new(&openssl)
        .args(["enc", "-aes-256-cbc", "-pbkdf2", "-salt", "-in"])
        .arg(archive)
        .arg("-out")
        .arg(&dest)
        .arg("-pass")
        .arg(format!("env:{BACKUP_KEY_ENV}"))
        .status()
        .context("failed to spawn openssl")?;
    if !status.success() {
        bail!("openssl encrypt failed with {status}");
    }
    report.steps.push(StepReport {
        name: "encrypt".into(),
        status: "ok".into(),
        detail: dest.display().to_string(),
    });
    Ok(dest)
}

fn run_nas(config_path: &Path, opts: &BackupOpts, json: bool) -> Result<StepReport> {
    let remote = if opts.nas_remote.is_empty() {
        env::var("CHRONICLE_BACKUP_NAS_REMOTE").unwrap_or_else(|_| DEFAULT_NAS_REMOTE.into())
    } else {
        opts.nas_remote.clone()
    };
    let exe = env::current_exe().context("current_exe")?;
    let detail = format!(
        "{} remote sync -r {remote} -d push --config {}",
        exe.display(),
        config_path.display()
    );
    if opts.dry_run {
        return Ok(StepReport {
            name: "nas".into(),
            status: "dry-run".into(),
            detail,
        });
    }
    let mut cmd = ProcessCommand::new(&exe);
    cmd.args(["remote", "sync", "-r", &remote, "-d", "push", "--config"]);
    cmd.arg(config_path);
    if json {
        let (status, stderr) =
            run_piped_stderr_child(cmd).context("failed to spawn remote sync")?;
        Ok(nas_step_report(detail, status, &stderr))
    } else {
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        let status = cmd.status().context("failed to spawn remote sync")?;
        Ok(nas_step_report(detail, status, &[]))
    }
}

/// Run `cmd` with stdout discarded and stderr piped, draining both until exit.
///
/// `Command::status()` waits without reading a piped stderr handle. A child
/// that writes more than the OS pipe buffer (~64 KiB) then blocks on write
/// while the parent blocks in `wait()` — `chronicle backup --json` hangs.
/// `Child::wait_with_output()` drains the pipe.
fn run_piped_stderr_child(mut cmd: ProcessCommand) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::piped());
    let child = cmd.spawn().context("failed to spawn process")?;
    let output = child
        .wait_with_output()
        .context("failed to wait for process")?;
    Ok((output.status, output.stderr))
}

fn nas_step_report(detail: String, status: std::process::ExitStatus, stderr: &[u8]) -> StepReport {
    if status.success() {
        return StepReport {
            name: "nas".into(),
            status: "ok".into(),
            detail,
        };
    }
    let stderr = stderr_for_report(stderr);
    let detail = if stderr.trim().is_empty() {
        format!("{detail} (exit {status})")
    } else {
        format!("{detail} (exit {status}): {stderr}")
    };
    StepReport {
        name: "nas".into(),
        status: "failed".into(),
        detail,
    }
}

/// Bound captured NAS stderr so a verbose `-v` dump cannot bloat JSON output.
/// Keep the tail: SSH and rsync failures land at the end of the log.
fn stderr_for_report(bytes: &[u8]) -> String {
    const MAX: usize = 8 * 1024;
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX {
        return text.into_owned();
    }
    let mut start = text.len() - MAX;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

fn run_oracle(payload: Option<&Path>, opts: &BackupOpts) -> Result<StepReport> {
    let host =
        env::var("CHRONICLE_BACKUP_ORACLE_HOST").unwrap_or_else(|_| DEFAULT_ORACLE_HOST.into());
    let dest_dir =
        env::var("CHRONICLE_BACKUP_ORACLE_DIR").unwrap_or_else(|_| DEFAULT_ORACLE_DIR.into());
    let dest = format!("{host}:{dest_dir}/");
    if opts.dry_run {
        return Ok(StepReport {
            name: "oracle".into(),
            status: "dry-run".into(),
            detail: format!(
                "would ssh {host} mkdir -p {dest_dir}; scp {} {dest}",
                payload
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<checkpoint>".into())
            ),
        });
    }
    let Some(payload) = payload else {
        return Ok(StepReport {
            name: "oracle".into(),
            status: "failed".into(),
            detail: "no checkpoint payload to copy".into(),
        });
    };
    let ssh = match which("ssh") {
        Ok(p) => p,
        Err(_) => {
            return Ok(StepReport {
                name: "oracle".into(),
                status: "failed".into(),
                detail: "ssh not found on PATH".into(),
            });
        }
    };
    let scp = match which("scp") {
        Ok(p) => p,
        Err(_) => {
            return Ok(StepReport {
                name: "oracle".into(),
                status: "failed".into(),
                detail: "scp not found on PATH".into(),
            });
        }
    };
    let mkdir = ProcessCommand::new(&ssh)
        .args([
            "-o",
            "BatchMode=yes",
            &host,
            &format!("mkdir -p {dest_dir}"),
        ])
        .status()
        .context("failed to spawn ssh")?;
    if !mkdir.success() {
        return Ok(StepReport {
            name: "oracle".into(),
            status: "failed".into(),
            detail: format!("ssh mkdir failed with {mkdir}"),
        });
    }
    let copy = ProcessCommand::new(&scp)
        .args(["-o", "BatchMode=yes"])
        .arg(payload)
        .arg(&dest)
        .status()
        .context("failed to spawn scp")?;
    if copy.success() {
        Ok(StepReport {
            name: "oracle".into(),
            status: "ok".into(),
            detail: format!("{} -> {dest}", payload.display()),
        })
    } else {
        Ok(StepReport {
            name: "oracle".into(),
            status: "failed".into(),
            detail: format!("scp failed with {copy}"),
        })
    }
}

fn run_gdrive(payload: Option<&Path>, opts: &BackupOpts) -> Result<StepReport> {
    let remote =
        env::var("CHRONICLE_BACKUP_RCLONE_REMOTE").unwrap_or_else(|_| DEFAULT_RCLONE_REMOTE.into());
    let rclone = match which("rclone") {
        Ok(p) => p,
        Err(_) => {
            return Ok(StepReport {
                name: "gdrive".into(),
                status: "skipped".into(),
                detail: format!(
                    "rclone not installed; would copy to {remote} after `winget install Rclone.Rclone` and `rclone config`"
                ),
            });
        }
    };
    if opts.dry_run {
        return Ok(StepReport {
            name: "gdrive".into(),
            status: "dry-run".into(),
            detail: format!(
                "would {} copy {} {remote}",
                rclone.display(),
                payload
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<checkpoint>".into())
            ),
        });
    }
    let Some(payload) = payload else {
        return Ok(StepReport {
            name: "gdrive".into(),
            status: "failed".into(),
            detail: "no checkpoint payload to copy".into(),
        });
    };
    let status = ProcessCommand::new(&rclone)
        .arg("copy")
        .arg(payload)
        .arg(&remote)
        .status()
        .context("failed to spawn rclone")?;
    if status.success() {
        Ok(StepReport {
            name: "gdrive".into(),
            status: "ok".into(),
            detail: format!("{} -> {remote}", payload.display()),
        })
    } else {
        Ok(StepReport {
            name: "gdrive".into(),
            status: "failed".into(),
            detail: format!("rclone copy failed with {status}"),
        })
    }
}

fn finish(mut report: BackupReport, json: bool, ok: bool) -> Result<()> {
    report.ok = ok;
    if json {
        let pretty = serde_json::to_string_pretty(&report)?;
        println!("{pretty}");
        if ok {
            return Ok(());
        }
        bail!("backup finished with failures");
    }
    println!(
        "source  {} ({} bytes)",
        report.database, report.database_bytes
    );
    if let Some(integrity) = &report.integrity {
        println!("integrity  {integrity}");
    }
    if let Some(path) = &report.payload_path {
        println!("payload  {path}");
    }
    for step in &report.steps {
        println!("{:<12} {:<8} {}", step.name, step.status, step.detail);
    }
    if ok {
        Ok(())
    } else {
        bail!("backup finished with failures");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Larger than the typical 64 KiB OS pipe buffer so a drained wait must
    /// actually read stderr; `status()` would deadlock here.
    const OVERSIZE: usize = 128 * 1024;
    const MARKER: &str = "NAS_STDERR_MARKER";

    fn oversized_stderr_command(nbytes: usize, marker: &str, exit: i32) -> ProcessCommand {
        #[cfg(windows)]
        {
            let script = format!(
                "$ProgressPreference='SilentlyContinue'; $n={nbytes}; $e=[Console]::OpenStandardError(); $chunk=New-Object byte[] 4096; for($i=0;$i -lt 4096;$i++){{ $chunk[$i]=[byte][char]'X' }}; $left=$n; while($left -gt 0){{ $c=[Math]::Min(4096,$left); $e.Write($chunk,0,$c); $left-=$c }}; [Console]::Error.Write('{marker}'); exit {exit}"
            );
            let mut cmd = ProcessCommand::new("powershell.exe");
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
            cmd
        }
        #[cfg(not(windows))]
        {
            let script = format!(
                "head -c {nbytes} /dev/zero | tr '\\0' 'X' >&2; printf '%s' '{marker}' >&2; exit {exit}"
            );
            let mut cmd = ProcessCommand::new("sh");
            cmd.args(["-c", &script]);
            cmd
        }
    }

    fn quiet_exit_command(exit: i32) -> ProcessCommand {
        #[cfg(windows)]
        {
            let mut cmd = ProcessCommand::new("cmd");
            cmd.args(["/C", &format!("exit {exit}")]);
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = ProcessCommand::new("sh");
            cmd.args(["-c", &format!("exit {exit}")]);
            cmd
        }
    }

    fn run_piped_stderr_child_bounded(
        cmd: ProcessCommand,
        timeout: Duration,
    ) -> Result<(std::process::ExitStatus, Vec<u8>)> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_piped_stderr_child(cmd));
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                bail!(
                    "child did not exit within {timeout:?}; stderr pipe was not drained (backup --json deadlock)"
                )
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("stderr-drain worker thread disconnected")
            }
        }
    }

    #[test]
    fn json_child_drains_oversized_stderr_without_deadlock() {
        let cmd = oversized_stderr_command(OVERSIZE, MARKER, 9);
        let (status, stderr) = run_piped_stderr_child_bounded(cmd, Duration::from_secs(30))
            .expect("piped stderr child should exit once the parent drains the pipe");
        assert!(!status.success(), "synthetic NAS child should fail");
        assert!(
            stderr.len() >= OVERSIZE,
            "expected >= {OVERSIZE} stderr bytes, got {}",
            stderr.len()
        );
        let text = String::from_utf8_lossy(&stderr);
        assert!(
            text.contains(MARKER),
            "captured stderr must include the failure marker, got {} bytes",
            stderr.len()
        );
        let report = nas_step_report("remote sync -d push".into(), status, &stderr);
        assert_eq!(report.status, "failed");
        assert!(
            report.detail.contains(MARKER),
            "failure StepReport must surface stderr, got: {}",
            report.detail
        );
        assert!(report.detail.contains("exit"));
    }

    #[test]
    fn json_child_drains_oversized_stderr_on_success() {
        let cmd = oversized_stderr_command(OVERSIZE, MARKER, 0);
        let (status, stderr) = run_piped_stderr_child_bounded(cmd, Duration::from_secs(30))
            .expect("success path must also drain oversized stderr");
        assert!(status.success());
        assert!(stderr.len() >= OVERSIZE);
        let report = nas_step_report("remote sync -d push".into(), status, &stderr);
        assert_eq!(report.status, "ok");
        assert!(
            !report.detail.contains(MARKER),
            "success StepReport should not dump stderr: {}",
            report.detail
        );
    }

    #[test]
    fn failed_nas_step_without_stderr_still_names_exit() {
        let cmd = quiet_exit_command(3);
        let (status, stderr) = run_piped_stderr_child(cmd).unwrap();
        assert!(!status.success());
        assert!(stderr.is_empty() || stderr.iter().all(|b| b.is_ascii_whitespace()));
        let report = nas_step_report("nas push".into(), status, &stderr);
        assert_eq!(report.status, "failed");
        assert!(report.detail.contains("nas push"));
        assert!(report.detail.contains("exit"));
        assert!(!report.detail.contains(MARKER));
    }

    #[test]
    fn stderr_report_keeps_the_tail() {
        let mut bytes = vec![b'a'; 9 * 1024];
        bytes.extend_from_slice(b"TAIL-MARKER");
        let text = stderr_for_report(&bytes);
        assert!(text.starts_with('…'));
        assert!(
            text.ends_with("TAIL-MARKER"),
            "truncated stderr must keep the tail, got ending {:?}",
            text.chars().rev().take(20).collect::<String>()
        );
        assert!(text.len() <= 8 * 1024 + '…'.len_utf8());
    }
}
