//! Codemode engine precedence rules.

use serde::{Deserialize, Serialize};

use super::ScriptLanguage;

/// Metadata attached to a stored codemode script.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptMetadata {
    /// Language declared by the script itself.
    pub language: Option<ScriptLanguage>,
}

impl ScriptMetadata {
    /// Construct metadata with an explicit script language.
    #[must_use]
    pub const fn with_language(language: ScriptLanguage) -> Self {
        Self {
            language: Some(language),
        }
    }
}

/// Process-level engine choices before considering a script declaration.
///
/// Precedence: script metadata, CLI, configuration, then JavaScript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineSelection {
    /// Explicit command-line choice.
    pub cli: Option<ScriptLanguage>,
    /// Effective user/project configuration value.
    pub config: Option<ScriptLanguage>,
}

impl EngineSelection {
    /// Resolve the language for an optional script.
    #[must_use]
    pub fn resolve(self, script: Option<ScriptMetadata>) -> ScriptLanguage {
        script
            .and_then(|metadata| metadata.language)
            .or(self.cli)
            .or(self.config)
            .unwrap_or_default()
    }

    /// Resolve the process default when no script is involved.
    #[must_use]
    pub fn process_default(self) -> ScriptLanguage {
        self.resolve(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_is_script_cli_config_then_javascript() {
        let selection = EngineSelection {
            cli: Some(ScriptLanguage::Monty),
            config: Some(ScriptLanguage::JavaScript),
        };
        assert_eq!(
            selection.resolve(Some(ScriptMetadata::with_language(
                ScriptLanguage::JavaScript
            ))),
            ScriptLanguage::JavaScript
        );
        assert_eq!(selection.process_default(), ScriptLanguage::Monty);
        assert_eq!(
            EngineSelection {
                config: Some(ScriptLanguage::Monty),
                ..EngineSelection::default()
            }
            .process_default(),
            ScriptLanguage::Monty
        );
        assert_eq!(
            EngineSelection::default().process_default(),
            ScriptLanguage::JavaScript
        );
    }
}
