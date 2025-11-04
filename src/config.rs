use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct AppConfig {
    pub deck: Option<String>,
    pub model: Option<String>,
    pub template: Option<String>,
    pub source_lang: Option<String>,
    pub target_lang: Option<String>,
    #[serde(default)]
    pub extra_tags: Vec<String>,
    #[serde(default)]
    pub translation_bases: Vec<String>,
    #[serde(default, rename = "translation_base")]
    pub legacy_translation_base: Option<String>,
    pub translate_retries: Option<u32>,
    pub translate_backoff_ms: Option<u64>,
}

pub fn load(path: &Path) -> Result<AppConfig> {
    if path.as_os_str().is_empty() || !path.exists() {
        return Ok(AppConfig::default());
    }

    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file '{}'", path.display()))?;

    let mut config: AppConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse config file '{}'", path.display()))?;

    config.extra_tags = config
        .extra_tags
        .into_iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect();

    config.translation_bases = config
        .translation_bases
        .into_iter()
        .map(|base| base.trim().to_string())
        .filter(|base| !base.is_empty())
        .collect();

    if let Some(base) = config
        .legacy_translation_base
        .as_ref()
        .map(|base| base.trim())
        .filter(|base| !base.is_empty())
    {
        if !config.translation_bases.iter().any(|b| b == base) {
            config.translation_bases.push(base.to_string());
        }
        config.legacy_translation_base = Some(base.to_string());
    } else {
        config.legacy_translation_base = None;
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_returns_default_when_missing() {
        let config = load(Path::new("non-existent-config.toml")).unwrap();
        assert!(config.deck.is_none());
        assert!(config.extra_tags.is_empty());
    }

    #[test]
    fn parses_fields_from_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
deck = "My Deck"
model = "Basic"
template = "simple"
source_lang = "en"
target_lang = "es"
extra_tags = ["custom", " spaced "]
translation_base = "https://example.com"
translate_retries = 3
translate_backoff_ms = 750
"#
        )
        .unwrap();

        let config = load(file.path()).unwrap();
        assert_eq!(config.deck.as_deref(), Some("My Deck"));
        assert_eq!(config.template.as_deref(), Some("simple"));
        assert_eq!(config.source_lang.as_deref(), Some("en"));
        assert_eq!(config.target_lang.as_deref(), Some("es"));
        assert_eq!(
            config.translation_bases,
            vec!["https://example.com".to_string()]
        );
        assert_eq!(config.translate_retries, Some(3));
        assert_eq!(config.translate_backoff_ms, Some(750));
        assert_eq!(
            config.extra_tags,
            vec!["custom".to_string(), "spaced".to_string()]
        );
    }
}
