//! Strict allowlist-first non-credential components support for Chronicle native backup.

use anyhow::{Result, bail, ensure};
use std::path::{Component, Path};

use crate::ComponentKind;

/// Allowlist of supported components per host app.
/// Every host must explicitly allowlist supported component kinds.
pub fn host_supported_components(app: &str) -> &'static [ComponentKind] {
    match app {
        "codex" => &[
            ComponentKind::Sessions,
            ComponentKind::Settings,
            ComponentKind::Skills,
            ComponentKind::Projects,
        ],
        "claude-code" => &[
            ComponentKind::Sessions,
            ComponentKind::Settings,
            ComponentKind::Skills,
            ComponentKind::Plugins,
            ComponentKind::Projects,
        ],
        "cursor" => &[
            ComponentKind::Sessions,
            ComponentKind::Settings,
            ComponentKind::Projects,
        ],
        "antigravity" => &[
            ComponentKind::Sessions,
            ComponentKind::Settings,
            ComponentKind::Projects,
        ],
        "grok" => &[
            ComponentKind::Sessions,
            ComponentKind::Settings,
            ComponentKind::Projects,
        ],
        _ => &[ComponentKind::Sessions],
    }
}

/// Checks whether a host supports a component kind, failing closed if unsupported.
pub fn ensure_component_supported(app: &str, component: ComponentKind) -> Result<()> {
    let supported = host_supported_components(app);
    ensure!(
        supported.contains(&component),
        "component '{:?}' is not supported for host app '{}'",
        component,
        app
    );
    Ok(())
}

/// Guard check to prevent backing up project source code or repository files.
/// Only metadata files (.json, .jsonl, .toml, .yaml, .yml) describing project association are allowed.
/// Any project source trees, git trees, or dependency directories are rejected.
pub fn validate_project_component_file(path: &Path) -> Result<()> {
    for component in path.components() {
        if let Component::Normal(os_str) = component {
            let name = os_str.to_string_lossy().to_ascii_lowercase();
            if matches!(
                name.as_str(),
                ".git" | ".svn" | ".hg" | "node_modules" | "target"
            ) {
                bail!(
                    "project component cannot include VCS or dependency directories: {}",
                    path.display()
                );
            }
        }
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    ensure!(
        matches!(ext.as_str(), "json" | "jsonl" | "toml" | "yaml" | "yml"),
        "project component can only store metadata files, not source code ({})",
        path.display()
    );

    Ok(())
}
