use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::ProviderKind;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct DaemonSettings {
    pub disabled_providers: Vec<ProviderKind>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub provider_binary_overrides: HashMap<ProviderKind, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            disabled_providers: Vec::new(),
            provider_binary_overrides: HashMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl DaemonSettings {
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".raku")
            .join("settings.json")
    }

    pub fn discard_legacy_app_keys(&mut self) {
        for key in [
            "analytics_enabled",
            "favorite_models",
            "theme",
            "language",
            "computer_use_enabled",
            "computer_use_allowed_apps",
        ] {
            self.extra.remove(key);
        }
    }
}
