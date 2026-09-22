//! Unit tests for configuration.

#[cfg(test)]
mod path_expansion_tests {
    use super::super::Config;
    use std::path::PathBuf;

    #[test]
    fn expand_path_handles_tilde() {
        let result = Config::expand_path("~/test");
        // Should not start with ~ after expansion
        assert!(!result.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn expand_path_handles_absolute_path() {
        let result = Config::expand_path("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn expand_path_handles_relative_path() {
        let result = Config::expand_path("relative/path");
        assert_eq!(result, PathBuf::from("relative/path"));
    }

    #[test]
    fn expand_path_handles_env_vars() {
        temp_env::with_var("HSTRY_TEST_VAR", Some("/test/path"), || {
            let result = Config::expand_path("$HSTRY_TEST_VAR/subdir");
            assert!(result.to_string_lossy().contains("/test/path"));
        });
    }
}

#[cfg(test)]
mod default_config_tests {
    use super::super::{AdapterRepoSource, Config, DEFAULT_ADAPTER_REPO};

    #[test]
    fn default_has_database_path() {
        let config = Config::default();
        assert!(config.database.to_string_lossy().contains("hstry"));
        assert!(config.database.to_string_lossy().ends_with(".db"));
    }

    #[test]
    fn default_has_adapter_paths() {
        let config = Config::default();
        assert!(!config.adapter_paths.is_empty());
    }

    #[test]
    fn default_has_official_adapter_repo() {
        let config = Config::default();
        let official = config
            .adapter_repos
            .iter()
            .find(|repo| repo.name == "official")
            .expect("default config should include the official adapter repo");
        match &official.source {
            AdapterRepoSource::Git { url, path, .. } => {
                assert_eq!(url, DEFAULT_ADAPTER_REPO);
                assert_eq!(path, "adapters");
            }
            _ => panic!("default official adapter repo must use git"),
        }
    }

    #[test]
    fn default_js_runtime_is_auto() {
        let config = Config::default();
        assert_eq!(config.js_runtime, "auto");
    }

    #[test]
    fn default_service_disabled() {
        let config = Config::default();
        assert!(!config.service.enabled);
    }

    #[test]
    fn default_search_api_enabled() {
        let config = Config::default();
        assert!(config.service.search_api);
    }

    #[test]
    fn default_http_api_enabled() {
        let config = Config::default();
        assert!(config.service.http_api);
        assert_eq!(config.service.http_port, Some(3000));
    }

    #[test]
    fn standalone_does_not_prefer_hub_search() {
        let config = Config::default();
        assert!(!config.prefers_hub_search());
    }

    #[test]
    fn satellite_with_hub_prefers_hub_search() {
        let mut config = Config::default();
        config.sync.mode = super::super::SyncMode::Satellite;
        config.sync.hub_remote = Some("nas".into());
        assert!(config.prefers_hub_search());
    }
}

#[cfg(test)]
mod search_scope_tests {
    use super::super::{Config, RemoteConfig, SearchScope, SyncMode};

    fn remote(name: &str) -> RemoteConfig {
        RemoteConfig {
            name: name.to_string(),
            host: format!("{name}.example"),
            database_path: None,
            port: None,
            identity_file: None,
            enabled: true,
        }
    }

    #[test]
    fn explicit_search_scope_wins() {
        let mut config = Config::default();
        config.sync.mode = SyncMode::Satellite;
        config.sync.hub_remote = Some("nas".into());
        assert_eq!(
            [
                config.resolve_search_scope(Some(SearchScope::Local)),
                config.resolve_search_scope(Some(SearchScope::All)),
                config.resolve_search_scope(Some(SearchScope::Remote)),
            ],
            [SearchScope::Local, SearchScope::All, SearchScope::Remote]
        );
    }

    #[test]
    fn satellite_with_hub_defaults_to_all_search() {
        let mut config = Config::default();
        config.sync.mode = SyncMode::Satellite;
        config.sync.hub_remote = Some("nas".into());
        assert_eq!(config.resolve_search_scope(None), SearchScope::All);
    }

    #[test]
    fn otherwise_defaults_to_local_search() {
        let mut hub = Config::default();
        hub.sync.mode = SyncMode::Hub;
        hub.sync.hub_remote = Some("nas".into());

        let mut satellite = Config::default();
        satellite.sync.mode = SyncMode::Satellite;

        assert_eq!(
            [
                Config::default().resolve_search_scope(None),
                hub.resolve_search_scope(None),
                satellite.resolve_search_scope(None),
            ],
            [SearchScope::Local, SearchScope::Local, SearchScope::Local]
        );
    }

    #[test]
    fn default_search_remotes_filter_to_hub() {
        let nas = remote("nas");
        let home = remote("home");
        let all = vec![nas.clone(), home.clone()];
        let no_hub = Config {
            remotes: all.clone(),
            ..Config::default()
        };
        assert_eq!(no_hub.remotes_for_default_search().unwrap(), all);

        let mut config = Config::default();
        config.sync.hub_remote = Some("nas".into());
        config.remotes = vec![nas.clone(), home.clone()];
        assert_eq!(
            config.remotes_for_default_search().unwrap(),
            vec![nas.clone()]
        );
        assert_eq!(
            config.remotes_for_search(&["home".into()]).unwrap(),
            vec![home]
        );
    }

    #[test]
    fn missing_hub_remote_is_an_error() {
        let nas = remote("nas");
        let home = remote("home");
        let mut config = Config::default();
        config.sync.hub_remote = Some("ghost".into());
        config.remotes = vec![nas, home];
        assert_eq!(
            config.remotes_for_default_search().unwrap_err().to_string(),
            "Configuration error: sync.hub_remote 'ghost' is not a configured remote (available: nas, home). Fix hub_remote or add a matching [[remotes]] entry"
        );
    }

    #[test]
    fn explicit_remotes_ignore_missing_hub() {
        let home = remote("home");
        let mut config = Config::default();
        config.sync.hub_remote = Some("ghost".into());
        config.remotes = vec![home.clone()];
        assert_eq!(
            config.remotes_for_search(&["home".into()]).unwrap(),
            vec![home]
        );
    }
}

#[cfg(test)]
mod adapter_enabled_tests {
    use super::super::{AdapterConfig, Config};

    #[test]
    fn adapter_enabled_returns_true_by_default() {
        let config = Config::default();
        assert!(config.adapter_enabled("nonexistent"));
        assert!(config.adapter_enabled("opencode"));
    }

    #[test]
    fn adapter_enabled_respects_config() {
        let mut config = Config::default();
        config.adapters.push(AdapterConfig {
            name: "disabled-adapter".to_string(),
            enabled: false,
        });
        config.adapters.push(AdapterConfig {
            name: "enabled-adapter".to_string(),
            enabled: true,
        });

        assert!(!config.adapter_enabled("disabled-adapter"));
        assert!(config.adapter_enabled("enabled-adapter"));
        assert!(config.adapter_enabled("other-adapter")); // Not in config, default true
    }
}

#[cfg(test)]
mod search_index_path_tests {
    use super::super::{Config, SearchConfig};
    use std::path::PathBuf;

    #[test]
    fn uses_explicit_path_when_set() {
        let config = Config {
            search: SearchConfig {
                index_path: Some(PathBuf::from("/custom/index")),
                index_batch_size: 500,
            },
            ..Config::default()
        };
        assert_eq!(config.search_index_path(), PathBuf::from("/custom/index"));
    }

    #[test]
    fn derives_from_database_path_when_not_set() {
        let config = Config {
            database: PathBuf::from("/data/hstry/hstry.db"),
            ..Config::default()
        };

        let index_path = config.search_index_path();
        assert!(index_path.to_string_lossy().contains("search"));
        assert!(index_path.to_string_lossy().contains("index"));
    }
}

#[cfg(test)]
mod config_serialization_tests {
    use super::super::Config;
    use std::path::PathBuf;

    #[test]
    fn toml_roundtrip() {
        let config = Config {
            database: PathBuf::from("/test/db.db"),
            js_runtime: "bun".to_string(),
            workspaces: vec!["~/projects".to_string()],
            ..Config::default()
        };

        let toml_str = toml::to_string(&config).unwrap_or_else(|err| panic!("serialize: {err}"));
        let parsed: Config =
            toml::from_str(&toml_str).unwrap_or_else(|err| panic!("deserialize: {err}"));

        assert_eq!(parsed.database, config.database);
        assert_eq!(parsed.js_runtime, config.js_runtime);
        assert_eq!(parsed.workspaces, config.workspaces);
    }
}

#[cfg(test)]
mod service_config_tests {
    use super::super::ServiceConfig;

    #[test]
    fn default_poll_interval() {
        let config = ServiceConfig::default();
        assert_eq!(config.poll_interval_secs, 300);
    }

    #[test]
    fn default_search_api_enabled() {
        let config = ServiceConfig::default();
        assert!(config.search_api);
    }

    #[test]
    fn default_search_port_none() {
        let config = ServiceConfig::default();
        assert!(config.search_port.is_none());
    }
}

#[cfg(test)]
mod adapter_repo_source_tests {
    use super::super::AdapterRepoSource;

    #[test]
    fn git_adapters_path() {
        let source = AdapterRepoSource::Git {
            url: "https://github.com/test/repo".to_string(),
            git_ref: "main".to_string(),
            path: "custom/adapters".to_string(),
        };
        assert_eq!(source.adapters_path(), "custom/adapters");
    }

    #[test]
    fn archive_adapters_path() {
        let source = AdapterRepoSource::Archive {
            url: "https://example.com/adapters.tar.gz".to_string(),
            path: "adapters".to_string(),
        };
        assert_eq!(source.adapters_path(), "adapters");
    }

    #[test]
    fn local_adapters_path() {
        let source = AdapterRepoSource::Local {
            path: "/local/adapters".to_string(),
        };
        assert_eq!(source.adapters_path(), "/local/adapters");
    }
}

mod remote_config_tests {
    use super::super::RemoteConfig;

    #[test]
    fn serde_roundtrip() {
        let remote = RemoteConfig {
            name: "laptop".to_string(),
            host: "user@laptop.local".to_string(),
            database_path: Some("/custom/path.db".to_string()),
            port: Some(2222),
            identity_file: Some("~/.ssh/id_ed25519".to_string()),
            enabled: true,
        };

        let json = serde_json::to_string(&remote).unwrap_or_else(|err| panic!("serialize: {err}"));
        let parsed: RemoteConfig =
            serde_json::from_str(&json).unwrap_or_else(|err| panic!("deserialize: {err}"));

        assert_eq!(parsed.name, remote.name);
        assert_eq!(parsed.host, remote.host);
        assert_eq!(parsed.port, remote.port);
    }
}

#[cfg(test)]
mod sync_config_tests {
    use super::super::{SyncConfig, sanitize_device_namespace};

    #[test]
    fn sanitize_device_namespace_normalizes_labels() {
        assert_eq!(sanitize_device_namespace("Arknight"), "arknight");
        assert_eq!(sanitize_device_namespace("win-pc"), "win-pc");
        assert_eq!(sanitize_device_namespace("MacBook Pro"), "macbook-pro");
        assert_eq!(sanitize_device_namespace("---"), "unknown");
    }

    #[test]
    fn device_namespace_prefers_configured_device_id() {
        let config = SyncConfig {
            device_id: Some("arknights".to_string()),
            ..SyncConfig::default()
        };
        assert_eq!(config.device_namespace(), "arknights");
    }
}
