//! Ready, process-lifetime light profile data loaded from the configuration directory.

use anyhow::{Context as _, Result};
use hass_light::{BUILT_IN_LIGHT_PROFILES_CSV, LightProfileCatalog, LightProfileParseIssue};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PROFILE_FILE: &str = "light_profiles.csv";

/// Complete profile initialization before config-entry providers can accept calls.
/// A file IO failure leaves the entire catalog empty, as the pinned initializer
/// only assigns its accumulated profiles after its file-loading job succeeds.
pub async fn load(configuration: &Path) -> Arc<LightProfileCatalog> {
    let path = configuration
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PROFILE_FILE);
    let worker_path = path.clone();
    match tokio::task::spawn_blocking(move || read_catalog(&worker_path)).await {
        Ok(Ok((catalog, issues))) => {
            for issue in issues {
                let source = if issue.source_index == 0 {
                    PathBuf::from("<built-in light_profiles.csv>")
                } else {
                    path.clone()
                };
                tracing::error!(
                    path = %source.display(),
                    record = issue.record_index,
                    row = ?issue.row,
                    reason = issue.reason,
                    "Error parsing light profile row"
                );
            }
            Arc::new(catalog)
        }
        Ok(Err(error)) => {
            tracing::error!(path = %path.display(), %error, "Unable to initialize light profiles");
            Arc::new(LightProfileCatalog::default())
        }
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "Light profile initialization worker failed");
            Arc::new(LightProfileCatalog::default())
        }
    }
}

fn read_catalog(path: &Path) -> Result<(LightProfileCatalog, Vec<LightProfileParseIssue>)> {
    let custom = if path.is_file() {
        Some(std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)
    } else {
        None
    };
    let mut sources = vec![BUILT_IN_LIGHT_PROFILES_CSV];
    if let Some(custom) = custom.as_deref() {
        sources.push(custom);
    }
    Ok(LightProfileCatalog::from_csv_sources_with_report(sources))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn configuration_directory_catalog_is_ready_and_overrides_builtins() {
        let directory = tempfile::tempdir().unwrap();
        let configuration = directory.path().join("configuration.yaml");
        std::fs::write(
            directory.path().join(PROFILE_FILE),
            "id,x,y,brightness,transition\nrelax,,,7,2\nlight.kitchen.default,,,31,3\n",
        )
        .unwrap();
        let profiles = load(&configuration).await;
        assert_eq!(profiles.get("relax").unwrap().brightness, Some(7));
        assert_eq!(profiles.get("relax").unwrap().transition, Some(2.0));
        assert_eq!(profiles.get("concentrate").unwrap().brightness, Some(219));
        assert_eq!(
            profiles.get("light.kitchen.default").unwrap().brightness,
            Some(31)
        );
    }

    #[test]
    fn invalid_row_retains_prior_rows_and_reports_the_custom_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(PROFILE_FILE);
        std::fs::write(&path, "header\nkept,,,4\nbad,,,999\nignored,,,6\n").unwrap();
        let (profiles, issues) = read_catalog(&path).unwrap();
        assert_eq!(profiles.get("kept").unwrap().brightness, Some(4));
        assert!(profiles.get("ignored").is_none());
        assert!(profiles.get("relax").is_some());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].source_index, 1);
        assert_eq!(issues[0].record_index, 2);
    }

    #[tokio::test]
    async fn missing_or_non_file_profiles_keep_builtins_but_read_failure_keeps_empty_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let configuration = directory.path().join("configuration.yaml");
        assert!(load(&configuration).await.get("relax").is_some());
        let path = directory.path().join(PROFILE_FILE);
        std::fs::create_dir(&path).unwrap();
        assert!(load(&configuration).await.get("relax").is_some());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(load(&configuration).await.get("relax").is_none());
    }
}
