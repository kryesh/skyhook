//! A configuration built in code.

use super::{
    Config, ConfigError,
    mode::{Mode, ModeName},
};
use crate::provider::{
    dialect::RawProviderConfig,
    model::{ModelRef, ProviderName},
};
use crate::target::TargetConfig;

/// A [`Config`] built in code and admitted as a parsed document is. It declares the named
/// entries a configuration is made of, each a section a host writes or deserializes on its
/// own, and selects among them; every other setting is a public field of what it builds.
#[derive(Default)]
pub struct ConfigBuilder(Config);

impl ConfigBuilder {
    /// Declare a provider with the models served through it, after those already declared.
    pub fn provider(mut self, name: ProviderName, entry: RawProviderConfig) -> Self {
        self.0.providers.insert(name, entry);
        self
    }

    /// Declare a mode after the built-in `general`, or in its place under that name.
    pub fn mode(mut self, name: ModeName, mode: Mode) -> Self {
        self.0.modes.insert(name, mode);
        self
    }

    /// Declare an execution target.
    pub fn target(mut self, name: impl Into<String>, target: TargetConfig) -> Self {
        self.0.targets.entries.insert(name.into(), target);
        self
    }

    /// Start new sessions with `model` instead of the first model of the first provider.
    pub fn default_model(mut self, model: ModelRef) -> Self {
        self.0.default_model = Some(model);
        self
    }

    /// Start new sessions in `mode` instead of `general`.
    pub fn default_mode(mut self, mode: ModeName) -> Self {
        self.0.default_mode = mode;
        self
    }

    /// The configuration, refused for what [`Config::into_runtime`] refuses.
    pub fn build(self) -> Result<Config, ConfigError> {
        self.0.validate()?;
        Ok(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml::parse;

    /// A configuration built from its entries is the one their document parses to, and
    /// is refused as that document is.
    #[test]
    fn a_built_configuration_is_its_documents() {
        const PROVIDER: &str = "{dialect: compatible, codec: chat_completions, base_url: 'http://127.0.0.1:1/v1', models: {main: {model: served, max_context: 4096, max_output: 512}}}";
        const MODE: &str = "{capabilities: [read], hint: Reads}";
        const TARGET: &str = "{host: box.test, transport: {ssh: {}}, via: hop}";
        const HOP: &str = "{host: hop.test, transport: {ssh: {}}}";
        let review: ModeName = "review".parse().unwrap();
        let builder = || {
            Config::builder()
                .provider("local".parse().unwrap(), parse(PROVIDER).unwrap())
                .mode(review.clone(), parse(MODE).unwrap())
                .target("box", parse(TARGET).unwrap())
                .target("hop", parse(HOP).unwrap())
                .default_model("local/main".parse().unwrap())
                .default_mode(review.clone())
        };
        let document = format!(
            "default_mode: review\nmodes: {{review: {MODE}}}\nproviders: {{local: {PROVIDER}}}\ndefault_model: local/main\ntargets: {{box: {TARGET}, hop: {HOP}}}\n"
        );
        let built = builder().build().unwrap();
        let parsed = Config::from_yaml(&document).unwrap();
        assert_eq!(built.to_yaml().unwrap(), parsed.to_yaml().unwrap());
        built.into_runtime().unwrap();
        let looped = parse("{host: hop.test, transport: {ssh: {}}, via: box}").unwrap();
        let refused = builder().target("hop", looped).build();
        assert!(matches!(refused, Err(ConfigError::Refused(_))));
    }
}
