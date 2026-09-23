//! Chronicle / hstry 3-2-1 backup: integrity, NAS, Oracle, Google Drive.
//!
//! Never read the live database into memory for hashing. Never fall back to
//! test fixtures. Never use a hardcoded encryption passphrase.
//!
//! # Offsite Remote Retention & Pruning (Issue #27)
//!
//! After replicating a snapshot to Oracle and Google Drive, Chronicle performs
//! a conservative, dry-runnable, per-file prune of older remote snapshots.
//!
//! Safety principles:
//! - Default retention: keeps the most recent 14 valid snapshots per offsite target.
//! - Hard retention floor: regardless of setting, at least 1 newest snapshot is
//!   always preserved; `--remote-keep 0` is rejected at startup.
//! - Strict filename whitelisting: only authentic Chronicle archives
//!   (`hstry-YYYYMMDD-HHMMSS(-<counter>)?.db.zst`) and matching encrypted sidecars
//!   (`*.db.zst.enc`) are recognized. Any other filenames or non-conforming entries
//!   are strictly ignored.
//! - Snapshot stem grouping: unencrypted archives and encrypted sidecars sharing
//!   the same stem are grouped together as one snapshot. Both are kept or pruned
//!   together, preventing orphan or mismatched sidecars.
//! - Single-file deletion: files are deleted strictly one by one via exact, validated
//!   filenames. Directory-level sync, recursive deletion, and wildcards are forbidden.
//! - Failure safe: if remote enumeration fails, output is abnormal, or JSON cannot
//!   be parsed, zero deletions occur and a failure is reported.
//! - Dry-run guarantee: in dry-run mode, remote enumeration is read-only; candidates
//!   for deletion are printed, but no remote data is created, copied, or deleted.

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

/// Default number of remote snapshots to keep per offsite destination.
pub const DEFAULT_REMOTE_KEEP: usize = 14;

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
    pub remote_keep: usize,
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
    if opts.remote_keep == 0 {
        bail!(
            "--remote-keep must be at least 1 (default is {DEFAULT_REMOTE_KEEP}; refusing 0 to protect remote backups)"
        );
    }
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
        let oracle_step = run_oracle(payload.as_deref(), &opts)?;
        let copy_ok = oracle_step.status == "ok" || oracle_step.status == "dry-run";
        report.steps.push(oracle_step);
        if copy_ok {
            report.steps.push(prune_oracle(&opts)?);
        }
    }

    if wants_gdrive {
        let gdrive_step = run_gdrive(payload.as_deref(), &opts)?;
        let copy_ok = gdrive_step.status == "ok" || gdrive_step.status == "dry-run";
        report.steps.push(gdrive_step);
        if copy_ok {
            report.steps.push(prune_gdrive(&opts)?);
        }
    }

    let ok = backup_steps_succeeded(&report.steps);
    finish(report, json, ok)
}

fn backup_steps_succeeded(steps: &[StepReport]) -> bool {
    steps
        .iter()
        .all(|step| step.status == "ok" || step.status == "dry-run")
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
    validate_ssh_host(&host)?;
    validate_remote_dir(&dest_dir)?;
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
            &format!("mkdir -p -- {dest_dir}"),
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
    validate_rclone_remote(&remote)?;
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

// ---------------------------------------------------------------------------
// Remote Snapshot Prune Infrastructure (Issue #27)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotGroup {
    pub stem: String,
    pub timestamp: chrono::NaiveDateTime,
    pub counter: u32,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunePlan {
    pub total_valid_snapshots: usize,
    pub kept_snapshots: Vec<String>,
    pub deleted_snapshots: Vec<String>,
    pub files_to_delete: Vec<String>,
}

/// Strictly parse a remote snapshot filename.
///
/// Returns `(stem, is_encrypted, timestamp, counter)` only if the filename conforms
/// exactly to Chronicle's snapshot naming specification:
/// `hstry-YYYYMMDD-HHMMSS(-<counter>)?.db.zst` or `*.db.zst.enc`.
/// Any path traversal, shell characters, spaces, or unrecognized patterns return `None`.
pub fn parse_snapshot_filename(
    filename: &str,
) -> Option<(String, bool, chrono::NaiveDateTime, u32)> {
    if filename.is_empty()
        || !filename
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return None;
    }

    let (stem, is_encrypted) = if let Some(s) = filename.strip_suffix(".db.zst.enc") {
        (s, true)
    } else if let Some(s) = filename.strip_suffix(".db.zst") {
        (s, false)
    } else {
        return None;
    };

    let rest = stem.strip_prefix("hstry-")?;
    if rest.len() < 15 {
        return None;
    }
    let date_str = &rest[..15];
    let naive_dt = chrono::NaiveDateTime::parse_from_str(date_str, "%Y%m%d-%H%M%S").ok()?;

    let counter = if rest.len() == 15 {
        0
    } else {
        let counter_str = rest[15..].strip_prefix('-')?;
        if counter_str.is_empty() || !counter_str.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        counter_str.parse::<u32>().ok()?
    };

    Some((stem.to_string(), is_encrypted, naive_dt, counter))
}

/// Whitelist validator for exact deletion targets.
pub fn validate_exact_filename_for_deletion(filename: &str) -> Result<()> {
    if parse_snapshot_filename(filename).is_none() {
        bail!("refusing to delete unrecognized or potentially unsafe filename: {filename:?}");
    }
    Ok(())
}

/// Validate destination directory string to prevent shell injection or traversal.
pub fn validate_remote_dir(dir: &str) -> Result<()> {
    if dir != dir.trim() {
        bail!("remote directory cannot have leading or trailing whitespace");
    }
    let trimmed = dir;
    if trimmed.is_empty() {
        bail!("remote directory cannot be empty");
    }
    if trimmed.starts_with('-') {
        bail!("remote directory cannot start with an option prefix ('-')");
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '~' | '/' | '.' | '_' | '-'))
    {
        bail!("remote directory contains invalid or unsafe characters: {dir:?}");
    }
    if trimmed
        .split('/')
        .any(|component| matches!(component, "." | ".."))
    {
        bail!("remote directory cannot contain path traversal ('..')");
    }
    if matches!(trimmed.trim_end_matches('/'), "" | "~") {
        bail!("remote directory cannot be a filesystem root");
    }
    Ok(())
}

/// Reject SSH destination strings that could be parsed as OpenSSH options.
pub fn validate_ssh_host(host: &str) -> Result<()> {
    if host.is_empty()
        || host != host.trim()
        || host.starts_with('-')
        || !host.chars().any(|c| c.is_ascii_alphanumeric())
        || !host.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ':' | '[' | ']')
        })
    {
        bail!("SSH host contains invalid or unsafe characters: {host:?}");
    }
    Ok(())
}

/// Validate the configured rclone target before passing it as a CLI argument.
pub fn validate_rclone_remote(remote: &str) -> Result<()> {
    let Some((name, path)) = remote.split_once(':') else {
        bail!("rclone target must use a named remote (for example, gdrive:backups)");
    };
    if name.is_empty()
        || name.starts_with('-')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || remote.chars().any(char::is_control)
        || path.split('/').any(|component| component == "..")
    {
        bail!("rclone target contains invalid or unsafe characters: {remote:?}");
    }
    Ok(())
}

/// Evaluate remote files, group by snapshot stem, sort by timestamp descending,
/// and determine which snapshot files to keep and which to delete.
///
/// Invariant: `effective_keep` is always at least 1; if any valid snapshots exist,
/// at least 1 newest snapshot is guaranteed to be kept.
pub fn plan_remote_prune(filenames: &[String], keep_count: usize) -> PrunePlan {
    let effective_keep = keep_count.max(1);
    let mut groups: std::collections::BTreeMap<String, SnapshotGroup> =
        std::collections::BTreeMap::new();

    for name in filenames {
        if let Some((stem, _is_enc, timestamp, counter)) = parse_snapshot_filename(name) {
            let group = groups.entry(stem.clone()).or_insert_with(|| SnapshotGroup {
                stem,
                timestamp,
                counter,
                files: Vec::new(),
            });
            if !group.files.contains(name) {
                group.files.push(name.clone());
            }
        }
    }

    let mut group_list: Vec<SnapshotGroup> = groups.into_values().collect();
    // Sort descending: newest first
    group_list
        .sort_by(|a, b| (b.timestamp, b.counter, &b.stem).cmp(&(a.timestamp, a.counter, &a.stem)));

    let total_valid_snapshots = group_list.len();
    if total_valid_snapshots <= effective_keep {
        return PrunePlan {
            total_valid_snapshots,
            kept_snapshots: group_list.into_iter().map(|g| g.stem).collect(),
            deleted_snapshots: Vec::new(),
            files_to_delete: Vec::new(),
        };
    }

    let (to_keep, to_prune) = group_list.split_at(effective_keep);
    let kept_snapshots = to_keep.iter().map(|g| g.stem.clone()).collect();
    let deleted_snapshots = to_prune.iter().map(|g| g.stem.clone()).collect();

    let mut files_to_delete = Vec::new();
    for g in to_prune {
        let mut sorted_files = g.files.clone();
        sorted_files.sort();
        files_to_delete.extend(sorted_files);
    }

    PrunePlan {
        total_valid_snapshots,
        kept_snapshots,
        deleted_snapshots,
        files_to_delete,
    }
}

/// Parse newline-delimited output from Oracle `ls -1`.
pub fn parse_oracle_ls_output(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| l.trim_end_matches(&['\r', '\n'][..]).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn is_missing_remote_directory(exit_code: Option<i32>, stderr: &str) -> bool {
    exit_code == Some(2) && stderr.contains("No such file or directory")
}

fn rclone_file_target(remote: &str, file: &str) -> String {
    let (name, path) = remote
        .split_once(':')
        .expect("rclone remote must be validated before joining a file");
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        format!("{name}:{file}")
    } else {
        format!("{name}:{path}/{file}")
    }
}

#[derive(Debug, serde::Deserialize)]
struct RcloneItem {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "IsDir")]
    is_dir: bool,
}

/// Parse JSON array produced by `rclone lsjson --files-only`.
pub fn parse_gdrive_rclone_json(bytes: &[u8]) -> Result<Vec<String>> {
    let items: Vec<RcloneItem> = serde_json::from_slice(bytes)
        .context("failed to parse rclone lsjson output as JSON array")?;
    let mut files = Vec::new();
    for item in items {
        if item.is_dir {
            continue;
        }
        let name = item.name.unwrap_or_else(|| item.path.clone());
        if item.path == name {
            files.push(name);
        }
    }
    Ok(files)
}

pub trait OracleRemote {
    fn list_files(&self, host: &str, dest_dir: &str) -> Result<Vec<String>>;
    fn delete_file(&self, host: &str, dest_dir: &str, file: &str) -> Result<()>;
}

pub trait GdriveRemote {
    fn list_files(&self, remote: &str) -> Result<Vec<String>>;
    fn delete_file(&self, remote: &str, file: &str) -> Result<()>;
}

pub struct RealOracleRemote;

impl OracleRemote for RealOracleRemote {
    fn list_files(&self, host: &str, dest_dir: &str) -> Result<Vec<String>> {
        validate_ssh_host(host)?;
        validate_remote_dir(dest_dir)?;
        let ssh = which("ssh").context("ssh not found on PATH")?;
        let output = ProcessCommand::new(&ssh)
            .args([
                "-o",
                "BatchMode=yes",
                host,
                &format!("LC_ALL=C ls -1 -- {dest_dir}"),
            ])
            .output()
            .context("failed to spawn ssh to list remote files")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if is_missing_remote_directory(output.status.code(), &stderr) {
                return Ok(Vec::new());
            }
            bail!("ssh ls failed with {}: {}", output.status, stderr.trim());
        }
        let stdout =
            String::from_utf8(output.stdout).context("remote ls returned non-UTF8 output")?;
        Ok(parse_oracle_ls_output(&stdout))
    }

    fn delete_file(&self, host: &str, dest_dir: &str, file: &str) -> Result<()> {
        validate_ssh_host(host)?;
        validate_remote_dir(dest_dir)?;
        validate_exact_filename_for_deletion(file)?;
        let ssh = which("ssh").context("ssh not found on PATH")?;
        let status = ProcessCommand::new(&ssh)
            .args([
                "-o",
                "BatchMode=yes",
                host,
                &format!("rm -f -- {dest_dir}/{file}"),
            ])
            .status()
            .context("failed to spawn ssh to delete remote file")?;
        if !status.success() {
            bail!("ssh rm failed with {status} for {file}");
        }
        Ok(())
    }
}

pub struct RealGdriveRemote;

impl GdriveRemote for RealGdriveRemote {
    fn list_files(&self, remote: &str) -> Result<Vec<String>> {
        validate_rclone_remote(remote)?;
        let rclone = which("rclone").context("rclone not found on PATH")?;
        let output = ProcessCommand::new(&rclone)
            .args(["lsjson", "--files-only", remote])
            .output()
            .context("failed to spawn rclone to list remote files")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "rclone lsjson failed with {}: {}",
                output.status,
                stderr.trim()
            );
        }
        parse_gdrive_rclone_json(&output.stdout)
    }

    fn delete_file(&self, remote: &str, file: &str) -> Result<()> {
        validate_rclone_remote(remote)?;
        validate_exact_filename_for_deletion(file)?;
        let rclone = which("rclone").context("rclone not found on PATH")?;
        let target = rclone_file_target(remote, file);
        let status = ProcessCommand::new(&rclone)
            .args(["deletefile", &target])
            .status()
            .context("failed to spawn rclone deletefile")?;
        if !status.success() {
            bail!("rclone deletefile failed with {status} for {target}");
        }
        Ok(())
    }
}

pub fn prune_oracle_impl<R: OracleRemote>(
    remote: &R,
    host: &str,
    dest_dir: &str,
    opts: &BackupOpts,
) -> Result<StepReport> {
    let files = match remote.list_files(host, dest_dir) {
        Ok(f) => f,
        Err(e) => {
            return Ok(StepReport {
                name: "oracle-prune".into(),
                status: "failed".into(),
                detail: format!("failed to list remote snapshots on {host}:{dest_dir}: {e}"),
            });
        }
    };

    let plan = plan_remote_prune(&files, opts.remote_keep);
    let keep_count = opts.remote_keep.max(1);

    if opts.dry_run {
        let detail = if plan.files_to_delete.is_empty() {
            format!(
                "0 old snapshots to prune (keeping all {}, limit {})",
                plan.total_valid_snapshots, keep_count
            )
        } else {
            format!(
                "would delete {} remote snapshot file(s) (keeping {}): {}",
                plan.files_to_delete.len(),
                plan.kept_snapshots.len(),
                plan.files_to_delete.join(", ")
            )
        };
        return Ok(StepReport {
            name: "oracle-prune".into(),
            status: "dry-run".into(),
            detail,
        });
    }

    if plan.files_to_delete.is_empty() {
        return Ok(StepReport {
            name: "oracle-prune".into(),
            status: "ok".into(),
            detail: format!(
                "0 old snapshots to prune (kept {}, limit {})",
                plan.kept_snapshots.len(),
                keep_count
            ),
        });
    }

    for file in &plan.files_to_delete {
        if let Err(e) = remote.delete_file(host, dest_dir, file) {
            return Ok(StepReport {
                name: "oracle-prune".into(),
                status: "failed".into(),
                detail: format!("failed to delete {file} on {host}: {e}"),
            });
        }
    }

    Ok(StepReport {
        name: "oracle-prune".into(),
        status: "ok".into(),
        detail: format!(
            "pruned {} old snapshot file(s) (kept {}): {}",
            plan.files_to_delete.len(),
            plan.kept_snapshots.len(),
            plan.files_to_delete.join(", ")
        ),
    })
}

pub fn prune_gdrive_impl<R: GdriveRemote>(
    remote: &R,
    remote_target: &str,
    opts: &BackupOpts,
) -> Result<StepReport> {
    let files = match remote.list_files(remote_target) {
        Ok(f) => f,
        Err(e) => {
            return Ok(StepReport {
                name: "gdrive-prune".into(),
                status: "failed".into(),
                detail: format!("failed to list remote snapshots on {remote_target}: {e}"),
            });
        }
    };

    let plan = plan_remote_prune(&files, opts.remote_keep);
    let keep_count = opts.remote_keep.max(1);

    if opts.dry_run {
        let detail = if plan.files_to_delete.is_empty() {
            format!(
                "0 old snapshots to prune (keeping all {}, limit {})",
                plan.total_valid_snapshots, keep_count
            )
        } else {
            format!(
                "would delete {} remote snapshot file(s) (keeping {}): {}",
                plan.files_to_delete.len(),
                plan.kept_snapshots.len(),
                plan.files_to_delete.join(", ")
            )
        };
        return Ok(StepReport {
            name: "gdrive-prune".into(),
            status: "dry-run".into(),
            detail,
        });
    }

    if plan.files_to_delete.is_empty() {
        return Ok(StepReport {
            name: "gdrive-prune".into(),
            status: "ok".into(),
            detail: format!(
                "0 old snapshots to prune (kept {}, limit {})",
                plan.kept_snapshots.len(),
                keep_count
            ),
        });
    }

    for file in &plan.files_to_delete {
        if let Err(e) = remote.delete_file(remote_target, file) {
            return Ok(StepReport {
                name: "gdrive-prune".into(),
                status: "failed".into(),
                detail: format!("failed to delete {file} on {remote_target}: {e}"),
            });
        }
    }

    Ok(StepReport {
        name: "gdrive-prune".into(),
        status: "ok".into(),
        detail: format!(
            "pruned {} old snapshot file(s) (kept {}): {}",
            plan.files_to_delete.len(),
            plan.kept_snapshots.len(),
            plan.files_to_delete.join(", ")
        ),
    })
}

fn prune_oracle(opts: &BackupOpts) -> Result<StepReport> {
    if which("ssh").is_err() {
        return Ok(StepReport {
            name: "oracle-prune".into(),
            status: "failed".into(),
            detail: "ssh not found on PATH".into(),
        });
    }
    let host =
        env::var("CHRONICLE_BACKUP_ORACLE_HOST").unwrap_or_else(|_| DEFAULT_ORACLE_HOST.into());
    let dest_dir =
        env::var("CHRONICLE_BACKUP_ORACLE_DIR").unwrap_or_else(|_| DEFAULT_ORACLE_DIR.into());
    prune_oracle_impl(&RealOracleRemote, &host, &dest_dir, opts)
}

fn prune_gdrive(opts: &BackupOpts) -> Result<StepReport> {
    if which("rclone").is_err() {
        return Ok(StepReport {
            name: "gdrive-prune".into(),
            status: "skipped".into(),
            detail: "rclone not installed; cannot prune remote snapshots".into(),
        });
    }
    let remote =
        env::var("CHRONICLE_BACKUP_RCLONE_REMOTE").unwrap_or_else(|_| DEFAULT_RCLONE_REMOTE.into());
    prune_gdrive_impl(&RealGdriveRemote, &remote, opts)
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
    use std::sync::{Mutex, mpsc};
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

    // -----------------------------------------------------------------------
    // Issue #27 Unit Tests: Remote Retention & Pruning
    // -----------------------------------------------------------------------

    struct MockOracle {
        listed_files: Result<Vec<String>, String>,
        deleted_files: Mutex<Vec<String>>,
    }

    impl OracleRemote for MockOracle {
        fn list_files(&self, _host: &str, _dest_dir: &str) -> Result<Vec<String>> {
            match &self.listed_files {
                Ok(files) => Ok(files.clone()),
                Err(e) => bail!("{e}"),
            }
        }

        fn delete_file(&self, _host: &str, _dest_dir: &str, file: &str) -> Result<()> {
            validate_exact_filename_for_deletion(file)?;
            self.deleted_files.lock().unwrap().push(file.to_string());
            Ok(())
        }
    }

    struct MockGdrive {
        listed_files: Result<Vec<String>, String>,
        deleted_files: Mutex<Vec<String>>,
    }

    impl GdriveRemote for MockGdrive {
        fn list_files(&self, _remote: &str) -> Result<Vec<String>> {
            match &self.listed_files {
                Ok(files) => Ok(files.clone()),
                Err(e) => bail!("{e}"),
            }
        }

        fn delete_file(&self, _remote: &str, file: &str) -> Result<()> {
            validate_exact_filename_for_deletion(file)?;
            self.deleted_files.lock().unwrap().push(file.to_string());
            Ok(())
        }
    }

    #[test]
    fn test_parse_snapshot_filename_valid_and_invalid() {
        // Valid unencrypted
        let res = parse_snapshot_filename("hstry-20260909-155119.db.zst");
        assert!(res.is_some());
        let (stem, is_enc, dt, counter) = res.unwrap();
        assert_eq!(stem, "hstry-20260909-155119");
        assert!(!is_enc);
        assert_eq!(
            dt,
            chrono::NaiveDateTime::parse_from_str("20260909-155119", "%Y%m%d-%H%M%S").unwrap()
        );
        assert_eq!(counter, 0);

        // Valid encrypted sidecar
        let res_enc = parse_snapshot_filename("hstry-20260909-155119.db.zst.enc");
        assert!(res_enc.is_some());
        let (stem, is_enc, _, _) = res_enc.unwrap();
        assert_eq!(stem, "hstry-20260909-155119");
        assert!(is_enc);

        // Valid with counter
        let res_counter = parse_snapshot_filename("hstry-20260909-155119-3.db.zst");
        assert!(res_counter.is_some());
        let (stem, _, _, counter) = res_counter.unwrap();
        assert_eq!(stem, "hstry-20260909-155119-3");
        assert_eq!(counter, 3);

        // Invalid: missing prefix
        assert!(parse_snapshot_filename("backup-20260909-155119.db.zst").is_none());

        // Invalid: missing/wrong extension
        assert!(parse_snapshot_filename("hstry-20260909-155119.json").is_none());
        assert!(parse_snapshot_filename("hstry-20260909-155119.db").is_none());
        assert!(parse_snapshot_filename("hstry-20260909-155119.tar.gz").is_none());

        // Invalid date/time values
        assert!(parse_snapshot_filename("hstry-20269999-999999.db.zst").is_none());
        assert!(parse_snapshot_filename("hstry-notadate-123456.db.zst").is_none());

        // Malicious: path traversal
        assert!(parse_snapshot_filename("../hstry-20260909-155119.db.zst").is_none());
        assert!(parse_snapshot_filename("foo/hstry-20260909-155119.db.zst").is_none());
        assert!(parse_snapshot_filename("foo\\hstry-20260909-155119.db.zst").is_none());

        // Malicious: command injection attempts
        assert!(parse_snapshot_filename("hstry-20260909-155119;rm -rf /;.db.zst").is_none());
        assert!(parse_snapshot_filename("hstry-$(whoami).db.zst").is_none());
        assert!(parse_snapshot_filename("hstry-`whoami`.db.zst").is_none());
        assert!(parse_snapshot_filename("hstry-20260909-155119.db.zst|sh").is_none());
    }

    #[test]
    fn test_remote_targets_reject_option_injection_and_traversal() {
        assert!(validate_ssh_host("oracle-arm").is_ok());
        assert!(validate_ssh_host("backup@oracle-arm").is_ok());
        assert!(validate_ssh_host("-oProxyCommand=touch /tmp/pwned").is_err());

        assert!(validate_remote_dir(DEFAULT_ORACLE_DIR).is_ok());
        for unsafe_dir in ["", "-R", " -R", "~", "/", "~/../other", "~/a b"] {
            assert!(
                validate_remote_dir(unsafe_dir).is_err(),
                "remote dir should be rejected: {unsafe_dir:?}"
            );
        }

        assert!(validate_rclone_remote(DEFAULT_RCLONE_REMOTE).is_ok());
        assert!(validate_rclone_remote("--config=/tmp/other.conf").is_err());
        assert!(validate_rclone_remote("gdrive:../outside").is_err());
    }

    #[test]
    fn missing_oracle_directory_is_distinguished_from_other_remote_errors() {
        assert!(is_missing_remote_directory(
            Some(2),
            "ls: cannot access '~/archives/chronicle': No such file or directory"
        ));
        assert!(!is_missing_remote_directory(
            Some(2),
            "ls: Permission denied"
        ));
        assert!(!is_missing_remote_directory(
            Some(255),
            "Load key '/missing/id': No such file or directory"
        ));
        assert!(!is_missing_remote_directory(
            Some(255),
            "ssh: connect timed out"
        ));
    }

    #[test]
    fn rclone_file_target_normalizes_trailing_slashes() {
        assert_eq!(
            rclone_file_target(
                "gdrive:chronicle-cold-backup/",
                "hstry-20260901-000000.db.zst"
            ),
            "gdrive:chronicle-cold-backup/hstry-20260901-000000.db.zst"
        );
        assert_eq!(
            rclone_file_target("gdrive:/", "hstry-20260901-000000.db.zst"),
            "gdrive:hstry-20260901-000000.db.zst"
        );
    }

    #[test]
    fn skipped_destinations_do_not_count_as_a_successful_backup() {
        let report = |status: &str| StepReport {
            name: "gdrive".into(),
            status: status.into(),
            detail: String::new(),
        };
        assert!(backup_steps_succeeded(&[report("ok"), report("dry-run")]));
        assert!(!backup_steps_succeeded(&[report("skipped")]));
        assert!(!backup_steps_succeeded(&[report("failed")]));
    }

    #[test]
    fn test_plan_prune_time_sorting() {
        let files = vec![
            "hstry-20260901-100000.db.zst".to_string(),
            "hstry-20260905-100000.db.zst".to_string(),
            "hstry-20260902-100000.db.zst".to_string(),
            "hstry-20260904-100000.db.zst".to_string(),
            "hstry-20260903-100000.db.zst".to_string(),
        ];

        let plan = plan_remote_prune(&files, 3);
        assert_eq!(plan.total_valid_snapshots, 5);
        assert_eq!(
            plan.kept_snapshots,
            vec![
                "hstry-20260905-100000".to_string(),
                "hstry-20260904-100000".to_string(),
                "hstry-20260903-100000".to_string(),
            ]
        );
        assert_eq!(
            plan.deleted_snapshots,
            vec![
                "hstry-20260902-100000".to_string(),
                "hstry-20260901-100000".to_string(),
            ]
        );
        assert_eq!(
            plan.files_to_delete,
            vec![
                "hstry-20260902-100000.db.zst".to_string(),
                "hstry-20260901-100000.db.zst".to_string(),
            ]
        );
    }

    #[test]
    fn test_plan_prune_stem_grouping_with_encrypted_sidecars() {
        let files = vec![
            "hstry-20260901-000000.db.zst".to_string(),
            "hstry-20260901-000000.db.zst.enc".to_string(),
            "hstry-20260902-000000.db.zst".to_string(),
            "hstry-20260902-000000.db.zst.enc".to_string(),
        ];

        // Keep 1 snapshot: the newest (20260902) should be kept with both files;
        // the oldest (20260901) should be pruned with both files deleted.
        let plan = plan_remote_prune(&files, 1);
        assert_eq!(plan.total_valid_snapshots, 2);
        assert_eq!(
            plan.kept_snapshots,
            vec!["hstry-20260902-000000".to_string()]
        );
        assert_eq!(
            plan.deleted_snapshots,
            vec!["hstry-20260901-000000".to_string()]
        );
        assert_eq!(
            plan.files_to_delete,
            vec![
                "hstry-20260901-000000.db.zst".to_string(),
                "hstry-20260901-000000.db.zst.enc".to_string(),
            ]
        );
    }

    #[test]
    fn test_plan_prune_retention_floor_and_zero_handling() {
        let files = vec![
            "hstry-20260901-000000.db.zst".to_string(),
            "hstry-20260902-000000.db.zst".to_string(),
            "hstry-20260903-000000.db.zst".to_string(),
        ];

        // keep_count = 0 clamped to floor 1
        let plan = plan_remote_prune(&files, 0);
        assert_eq!(plan.total_valid_snapshots, 3);
        assert_eq!(
            plan.kept_snapshots,
            vec!["hstry-20260903-000000".to_string()]
        );
        assert_eq!(plan.deleted_snapshots.len(), 2);
        assert_eq!(plan.files_to_delete.len(), 2);

        // single file with keep_count = 0 clamped to floor 1: 0 deletions
        let single = vec!["hstry-20260901-000000.db.zst".to_string()];
        let plan_single = plan_remote_prune(&single, 0);
        assert_eq!(plan_single.total_valid_snapshots, 1);
        assert_eq!(
            plan_single.kept_snapshots,
            vec!["hstry-20260901-000000".to_string()]
        );
        assert!(plan_single.files_to_delete.is_empty());
    }

    #[test]
    fn test_malicious_and_irrelevant_files_ignored() {
        let files = vec![
            "notes.txt".to_string(),
            "hstry-20260901-000000.db.zst".to_string(),
            "../secret.db.zst".to_string(),
            "hstry-20260901-000000.json".to_string(),
            "hstry-$(whoami).db.zst".to_string(),
            "some_backup.tar.gz".to_string(),
        ];

        let plan = plan_remote_prune(&files, 14);
        assert_eq!(plan.total_valid_snapshots, 1);
        assert_eq!(
            plan.kept_snapshots,
            vec!["hstry-20260901-000000".to_string()]
        );
        assert!(plan.deleted_snapshots.is_empty());
        assert!(plan.files_to_delete.is_empty());
    }

    #[test]
    fn test_oracle_mock_dry_run_does_not_delete() {
        let mut files = Vec::new();
        for day in 1..=16 {
            files.push(format!("hstry-202609{:02}-000000.db.zst", day));
        }

        let mock = MockOracle {
            listed_files: Ok(files),
            deleted_files: Mutex::new(Vec::new()),
        };

        let opts = BackupOpts {
            dry_run: true,
            encrypt: false,
            targets: vec![BackupTarget::Oracle],
            nas_remote: "nas-lan".into(),
            remote_keep: 14,
        };

        let report =
            prune_oracle_impl(&mock, "oracle-test", "~/archives/chronicle", &opts).unwrap();
        assert_eq!(report.status, "dry-run");
        assert!(
            report
                .detail
                .contains("would delete 2 remote snapshot file(s)")
        );
        assert!(
            mock.deleted_files.lock().unwrap().is_empty(),
            "dry-run must not delete any files"
        );
    }

    #[test]
    fn test_oracle_mock_real_delete() {
        let mut files = Vec::new();
        for day in 1..=16 {
            files.push(format!("hstry-202609{:02}-000000.db.zst", day));
        }

        let mock = MockOracle {
            listed_files: Ok(files),
            deleted_files: Mutex::new(Vec::new()),
        };

        let opts = BackupOpts {
            dry_run: false,
            encrypt: false,
            targets: vec![BackupTarget::Oracle],
            nas_remote: "nas-lan".into(),
            remote_keep: 14,
        };

        let report =
            prune_oracle_impl(&mock, "oracle-test", "~/archives/chronicle", &opts).unwrap();
        assert_eq!(report.status, "ok");
        assert!(report.detail.contains("pruned 2 old snapshot file(s)"));

        let deleted = mock.deleted_files.lock().unwrap().clone();
        assert_eq!(
            deleted,
            vec![
                "hstry-20260902-000000.db.zst".to_string(),
                "hstry-20260901-000000.db.zst".to_string(),
            ]
        );
    }

    #[test]
    fn test_oracle_mock_list_failure() {
        let mock = MockOracle {
            listed_files: Err(
                "ssh: connect to host oracle-test port 22: Connection timed out".into(),
            ),
            deleted_files: Mutex::new(Vec::new()),
        };

        let opts = BackupOpts {
            dry_run: false,
            encrypt: false,
            targets: vec![BackupTarget::Oracle],
            nas_remote: "nas-lan".into(),
            remote_keep: 14,
        };

        let report =
            prune_oracle_impl(&mock, "oracle-test", "~/archives/chronicle", &opts).unwrap();
        assert_eq!(report.status, "failed");
        assert!(report.detail.contains("failed to list remote snapshots"));
        assert!(mock.deleted_files.lock().unwrap().is_empty());
    }

    #[test]
    fn test_oracle_mock_empty_remote_succeeds_without_deleting() {
        let mock = MockOracle {
            listed_files: Ok(Vec::new()),
            deleted_files: Mutex::new(Vec::new()),
        };
        let opts = BackupOpts {
            dry_run: true,
            encrypt: false,
            targets: vec![BackupTarget::Oracle],
            nas_remote: "nas-lan".into(),
            remote_keep: 14,
        };

        let report =
            prune_oracle_impl(&mock, "oracle-test", "~/archives/chronicle", &opts).unwrap();
        assert_eq!(report.status, "dry-run");
        assert!(report.detail.contains("keeping all 0"));
        assert!(mock.deleted_files.lock().unwrap().is_empty());
    }

    #[test]
    fn test_gdrive_mock_dry_run_does_not_delete() {
        let mut files = Vec::new();
        for day in 1..=16 {
            files.push(format!("hstry-202609{:02}-000000.db.zst", day));
        }

        let mock = MockGdrive {
            listed_files: Ok(files),
            deleted_files: Mutex::new(Vec::new()),
        };

        let opts = BackupOpts {
            dry_run: true,
            encrypt: false,
            targets: vec![BackupTarget::Gdrive],
            nas_remote: "nas-lan".into(),
            remote_keep: 14,
        };

        let report = prune_gdrive_impl(&mock, "gdrive:test", &opts).unwrap();
        assert_eq!(report.status, "dry-run");
        assert!(
            report
                .detail
                .contains("would delete 2 remote snapshot file(s)")
        );
        assert!(
            mock.deleted_files.lock().unwrap().is_empty(),
            "dry-run must not delete any files"
        );
    }

    #[test]
    fn test_gdrive_mock_real_delete() {
        let mut files = Vec::new();
        for day in 1..=16 {
            files.push(format!("hstry-202609{:02}-000000.db.zst", day));
        }

        let mock = MockGdrive {
            listed_files: Ok(files),
            deleted_files: Mutex::new(Vec::new()),
        };

        let opts = BackupOpts {
            dry_run: false,
            encrypt: false,
            targets: vec![BackupTarget::Gdrive],
            nas_remote: "nas-lan".into(),
            remote_keep: 14,
        };

        let report = prune_gdrive_impl(&mock, "gdrive:test", &opts).unwrap();
        assert_eq!(report.status, "ok");
        assert!(report.detail.contains("pruned 2 old snapshot file(s)"));

        let deleted = mock.deleted_files.lock().unwrap().clone();
        assert_eq!(
            deleted,
            vec![
                "hstry-20260902-000000.db.zst".to_string(),
                "hstry-20260901-000000.db.zst".to_string(),
            ]
        );
    }

    #[test]
    fn test_gdrive_rclone_json_parsing_and_error() {
        let valid_json = br#"[
            {"Path": "hstry-20260901-000000.db.zst", "Name": "hstry-20260901-000000.db.zst", "IsDir": false},
            {"Path": "subfolder", "Name": "subfolder", "IsDir": true},
            {"Path": "subfolder/hstry-20260801-000000.db.zst", "Name": "hstry-20260801-000000.db.zst", "IsDir": false}
        ]"#;
        let files = parse_gdrive_rclone_json(valid_json).unwrap();
        assert_eq!(files, vec!["hstry-20260901-000000.db.zst"]);

        let invalid_json = b"error: 403 rate limit exceeded";
        assert!(parse_gdrive_rclone_json(invalid_json).is_err());

        let missing_is_dir =
            br#"[{"Path":"hstry-20260901-000000.db.zst","Name":"hstry-20260901-000000.db.zst"}]"#;
        assert!(parse_gdrive_rclone_json(missing_is_dir).is_err());
    }
}
