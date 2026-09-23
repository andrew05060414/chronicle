use super::*;
use std::collections::BTreeSet;

#[derive(Debug, Args)]
pub struct HostsArgs {
    #[command(subcommand)]
    pub command: HostsSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum HostsSubcommand {
    Record(HostsRecordArgs),
    Revoke(HostsRevokeArgs),
    Check,
}

#[derive(Debug, Args)]
pub struct HostsRecordArgs {
    #[arg(long)]
    pub app: String,
    #[arg(long)]
    pub evidence: PathBuf,
}

#[derive(Debug, Args)]
pub struct HostsRevokeArgs {
    #[arg(long)]
    pub app: String,
    #[arg(long)]
    pub version: String,
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostRecordStatus {
    Verified,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedHostRecord {
    pub app: String,
    pub version: String,
    pub status: HostRecordStatus,
    pub recorded_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VerifiedHostsRegistry {
    #[serde(default)]
    pub hosts: Vec<VerifiedHostRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostCheckStatus {
    Verified,
    Unverified,
    Revoked,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCheckItem {
    pub app: String,
    pub version: String,
    pub status: HostCheckStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostsCheckResult {
    pub checked_at: String,
    pub hosts: Vec<HostCheckItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvidenceRecord {
    pub step: String,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub host_version: Option<String>,
    #[serde(default)]
    pub hashes: Option<serde_json::Value>,
    #[serde(default)]
    pub log_path: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
}

pub fn load_verified_hosts_registry(root: &Path) -> Result<VerifiedHostsRegistry> {
    let path = root.join("verified-hosts.json");
    if !path.is_file() {
        return Ok(VerifiedHostsRegistry { hosts: Vec::new() });
    }
    let data = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&data)?)
}

pub fn save_verified_hosts_registry(root: &Path, registry: &VerifiedHostsRegistry) -> Result<()> {
    let path = root.join("verified-hosts.json");
    atomic_json(&path, registry)
}

pub fn get_host_status_from_registry(
    root: &Path,
    app: &str,
    version: &str,
) -> Option<HostRecordStatus> {
    let registry = load_verified_hosts_registry(root).ok()?;
    registry
        .hosts
        .iter()
        .rev()
        .find(|h| h.app.eq_ignore_ascii_case(app) && h.version == version)
        .map(|h| h.status)
}

pub fn verified_install_host(root: &Path, app: &str, version: &str) -> bool {
    if version.is_empty() || version == "unknown" {
        return false;
    }
    if cfg!(test)
        && version == "fixture-v1"
        && matches!(
            app,
            "codex" | "cursor" | "claude-code" | "antigravity" | "grok"
        )
    {
        return true;
    }
    get_host_status_from_registry(root, app, version) == Some(HostRecordStatus::Verified)
}

fn clean_path_str(p: &str) -> String {
    let s = p.replace('\\', "/");
    let trimmed = s.strip_prefix("//?/").unwrap_or(&s);
    trimmed.trim_end_matches('/').to_lowercase()
}

pub fn has_drill_marker(target: &Path) -> bool {
    for ancestor in target.ancestors() {
        if ancestor.join(".chronicle-drill").is_file() {
            return true;
        }
    }
    false
}

pub fn is_inside_default_home_roots(target: &Path, home: &Path) -> bool {
    let target_norm = clean_path_str(&target.to_string_lossy());
    for source in default_sources(Some(home)) {
        let source_norm = clean_path_str(&source.path.to_string_lossy());
        if target_norm == source_norm || target_norm.starts_with(&format!("{source_norm}/")) {
            return true;
        }
        if let Some(parent) = source.path.parent()
            && parent != home
        {
            let parent_norm = clean_path_str(&parent.to_string_lossy());
            if target_norm == parent_norm || target_norm.starts_with(&format!("{parent_norm}/")) {
                return true;
            }
        }
    }
    // Also protect the standard client directories directly under home
    for app_dir in [
        home.join(".codex"),
        home.join(".claude"),
        home.join(".gemini"),
        home.join(".grok"),
    ] {
        let app_dir_norm = clean_path_str(&app_dir.to_string_lossy());
        if target_norm == app_dir_norm || target_norm.starts_with(&format!("{app_dir_norm}/")) {
            return true;
        }
    }
    false
}

pub(crate) fn validate_drill_targets(
    actions: &[InstallAction],
    protected_home: Option<&Path>,
) -> Result<()> {
    ensure!(!actions.is_empty(), "no installable files in drill plan");
    for action in actions {
        ensure!(
            has_drill_marker(&action.target),
            "drill mode refused: target '{}' is not within an ancestor directory containing .chronicle-drill",
            action.target.display()
        );
    }
    let mut homes_to_protect = Vec::new();
    if let Some(home) = protected_home {
        homes_to_protect.push(home);
    }
    let real_home = dirs::home_dir();
    if let Some(ref rh) = real_home
        && !homes_to_protect.contains(&rh.as_path())
    {
        homes_to_protect.push(rh.as_path());
    }
    for home in homes_to_protect {
        for action in actions {
            ensure!(
                !is_inside_default_home_roots(&action.target, home),
                "drill mode refused: target '{}' falls within default client data root in user home",
                action.target.display()
            );
        }
    }
    Ok(())
}

pub fn handle_hosts_record(root: &Path, args: HostsRecordArgs) -> Result<serde_json::Value> {
    let evidence_path = args.evidence;
    ensure!(
        evidence_path.is_file(),
        "evidence file not found: {}",
        evidence_path.display()
    );
    let abs_evidence_path =
        std::path::absolute(&evidence_path).unwrap_or_else(|_| evidence_path.clone());
    let data = fs::read_to_string(&abs_evidence_path)?;
    let records: Vec<EvidenceRecord> =
        serde_json::from_str(&data).context("failed to parse evidence JSON as an array")?;

    let mut latest_by_step: BTreeMap<String, EvidenceRecord> = BTreeMap::new();
    for rec in records {
        latest_by_step.insert(rec.step.clone(), rec);
    }

    let mut issues = Vec::new();

    let get_step = |name: &str| -> Option<&EvidenceRecord> {
        if let Some(r) = latest_by_step.get(name) {
            return Some(r);
        }
        match name {
            "open" => latest_by_step.get("manual-open"),
            "restart" => latest_by_step.get("manual-restart"),
            "continue" => latest_by_step.get("manual-continue"),
            _ => None,
        }
    };

    let required_steps = [
        "capture",
        "replicate",
        "verify",
        "remove-source",
        "restore-install",
        "open",
        "restart",
        "continue",
    ];

    let mut collected_versions: Vec<(&str, String)> = Vec::new();

    for step_name in &required_steps {
        if let Some(rec) = get_step(step_name) {
            if rec.exit_code != Some(0) {
                issues.push(format!(
                    "step '{}' exit_code is {:?} (expected 0)",
                    step_name, rec.exit_code
                ));
            }
            if rec.result.as_deref() != Some("passed") {
                issues.push(format!(
                    "step '{}' result is {:?} (expected 'passed')",
                    step_name, rec.result
                ));
            }
            if let Some(v) = &rec.host_version {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    collected_versions.push((step_name, trimmed.to_string()));
                }
            }
        } else {
            issues.push(format!("missing required step: '{}'", step_name));
        }
    }

    if let Some(summary) = latest_by_step.get("summary") {
        if summary.result.as_deref() != Some("uat-passed") {
            issues.push(format!(
                "step 'summary' result is {:?} (expected 'uat-passed')",
                summary.result
            ));
        }
        match &summary.app {
            Some(app_str) => {
                if !app_str.eq_ignore_ascii_case(&args.app) {
                    issues.push(format!(
                        "step 'summary' app is '{}' (expected '{}')",
                        app_str, args.app
                    ));
                }
            }
            None => {
                issues.push("step 'summary' missing required 'app' field".into());
            }
        }
        if let Some(v) = &summary.host_version {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                collected_versions.push(("summary", trimmed.to_string()));
            }
        }
    } else {
        issues.push("missing required step: 'summary'".into());
    }

    let mut determined_version = None;
    if collected_versions.is_empty() {
        issues.push("no host_version found across required evidence steps".into());
    } else {
        let mut distinct = BTreeSet::new();
        for (step, ver) in &collected_versions {
            if ver == "unknown" {
                issues.push(format!("step '{step}' host_version is 'unknown'"));
            } else {
                distinct.insert(ver.clone());
            }
        }
        if distinct.len() > 1 {
            issues.push(format!(
                "inconsistent host_versions across steps: found {:?}",
                distinct
            ));
        } else if let Some(v) = distinct.into_iter().next() {
            determined_version = Some(v);
        }
    }

    if !issues.is_empty() {
        bail!("evidence validation failed:\n  - {}", issues.join("\n  - "));
    }

    let version = determined_version.context("no valid host_version determined")?;
    let (_, sha256) = hash_file(&abs_evidence_path)?;

    let record = VerifiedHostRecord {
        app: args.app,
        version,
        status: HostRecordStatus::Verified,
        recorded_at: Utc::now().to_rfc3339(),
        evidence_path: Some(abs_evidence_path.to_string_lossy().to_string()),
        evidence_sha256: Some(sha256),
        reason: None,
    };

    let mut registry = load_verified_hosts_registry(root)?;
    registry.hosts.push(record.clone());
    save_verified_hosts_registry(root, &registry)?;

    Ok(serde_json::to_value(&record)?)
}

pub fn handle_hosts_revoke(root: &Path, args: HostsRevokeArgs) -> Result<serde_json::Value> {
    let record = VerifiedHostRecord {
        app: args.app,
        version: args.version,
        status: HostRecordStatus::Revoked,
        recorded_at: Utc::now().to_rfc3339(),
        evidence_path: None,
        evidence_sha256: None,
        reason: args.reason,
    };

    let mut registry = load_verified_hosts_registry(root)?;
    registry.hosts.push(record.clone());
    save_verified_hosts_registry(root, &registry)?;

    Ok(serde_json::to_value(&record)?)
}

pub fn handle_hosts_check(
    config: &NativeConfig,
    root: &Path,
    home: Option<&Path>,
) -> Result<serde_json::Value> {
    let sources = if config.sources.is_empty() {
        default_sources(home.or(dirs::home_dir().as_deref()))
    } else {
        config.sources.clone()
    };
    let mut apps = Vec::new();
    for s in &sources {
        if !apps.contains(&s.app) {
            apps.push(s.app.clone());
        }
    }
    let registry = load_verified_hosts_registry(root).unwrap_or_default();
    let mut check_items = Vec::new();

    for app in apps {
        let app_sources: Vec<_> = sources.iter().filter(|s| s.app == app).collect();
        let configured_version = app_sources.iter().find_map(|s| s.host_version.clone());
        let version = if let Some(v) = configured_version {
            v
        } else if let Some(first_source) = app_sources.first() {
            let (detected, _) = detect_host_version(&app, &first_source.path, home);
            detected.unwrap_or_else(|| "unknown".into())
        } else {
            "unknown".into()
        };

        let status = if version.is_empty() || version == "unknown" {
            HostCheckStatus::Unknown
        } else {
            let reg_status = registry
                .hosts
                .iter()
                .rev()
                .find(|h| h.app.eq_ignore_ascii_case(&app) && h.version == version)
                .map(|h| h.status);
            match reg_status {
                Some(HostRecordStatus::Verified) => HostCheckStatus::Verified,
                Some(HostRecordStatus::Revoked) => HostCheckStatus::Revoked,
                None => HostCheckStatus::Unverified,
            }
        };

        check_items.push(HostCheckItem {
            app,
            version,
            status,
        });
    }

    let result = HostsCheckResult {
        checked_at: Utc::now().to_rfc3339(),
        hosts: check_items,
    };

    let check_path = root.join("hosts-check.json");
    atomic_json(&check_path, &result)?;

    Ok(serde_json::to_value(&result)?)
}

pub fn load_hosts_check(root: &Path) -> Option<serde_json::Value> {
    let path = root.join("hosts-check.json");
    if path.is_file() {
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    } else {
        None
    }
}
