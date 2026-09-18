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
