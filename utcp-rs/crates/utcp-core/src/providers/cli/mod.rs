use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::auth::AuthConfig;
use crate::providers::base::{BaseProvider, Provider, ProviderType};

/// Provider definition for CLI-based tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliProvider {
    #[serde(flatten)]
    pub base: BaseProvider,
    pub command_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_vars: Option<HashMap<String, String>>,
    /// When true, execute `command_name` directly (with `args_template` / `stdin_field`) instead of
    /// upstream's legacy `<cmd> call <provider> <tool>` subcommand convention.
    #[serde(default)]
    pub direct_exec: bool,
    /// Optional argument template tokens (`["--layout={layout}", "-", "{output_path}"]`).
    /// Tokens whose `{placeholder}` is absent from `args` are omitted automatically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_template: Option<Vec<String>>,
    /// Optional input field name whose string value is piped raw to `stdin` (and removed from CLI flags).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin_field: Option<String>,
    /// When `direct_exec` is true, whether unconsumed arguments should also be appended as `--key val` flags.
    #[serde(default)]
    pub append_extra_flags: bool,
}

impl Provider for CliProvider {
    fn type_(&self) -> ProviderType {
        ProviderType::Cli
    }

    fn name(&self) -> String {
        self.base.name.clone()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl CliProvider {
    /// Construct a CLI provider with a base command and optional authentication metadata.
    pub fn new(name: String, command_name: String, auth: Option<AuthConfig>) -> Self {
        Self {
            base: BaseProvider {
                name,
                provider_type: ProviderType::Cli,
                auth,
                allowed_communication_protocols: None,
            },
            command_name,
            working_dir: None,
            env_vars: None,
            direct_exec: false,
            args_template: None,
            stdin_field: None,
            append_extra_flags: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_cli_provider_with_minimal_config() {
        let json = json!({
            "name": "test-cli",
            "provider_type": "cli",
            "command_name": "echo"
        });

        let provider: CliProvider = serde_json::from_value(json).unwrap();
        assert_eq!(provider.base.name, "test-cli");
        assert_eq!(provider.command_name, "echo");
        assert!(provider.working_dir.is_none());
        assert!(provider.env_vars.is_none());
        assert_eq!(provider.type_(), ProviderType::Cli);
    }

    #[test]
    fn cli_provider_new_sets_defaults() {
        let provider = CliProvider::new("builder".to_string(), "make".to_string(), None);

        assert_eq!(provider.base.name, "builder");
        assert_eq!(provider.base.provider_type, ProviderType::Cli);
        assert_eq!(provider.command_name, "make");
        assert!(provider.working_dir.is_none());
        assert!(provider.env_vars.is_none());
    }
}
