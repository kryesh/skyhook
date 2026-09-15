use super::*;
use crate::config::ProviderConfig;

const VENDOR: &str = "vendor / 任意";

fn config() -> Config {
    toml::from_str(
        r#"
[providers."vendor / 任意"]
kind = 'openai'
api = 'chat_completions'
base_url = 'http://127.0.0.1:1/v1'
[models."z first / 任意"]
provider = 'vendor / 任意'
model = 'external:model/version'
max_context = 8192
max_output = 512
[models."a second"]
provider = 'vendor / 任意'
model = 'another-external-model'
max_context = 4096
max_output = 256
[targets]
import_ssh_config = false
"#,
    )
    .unwrap()
}

fn vendor(config: &mut Config) -> (&mut String, &mut Option<String>) {
    let Some(ProviderConfig::Openai {
        base_url,
        api_key_env,
        ..
    }) = config.providers.get_mut(VENDOR)
    else {
        unreachable!()
    };
    (base_url, api_key_env)
}

#[test]
fn selection_retains_its_generation_and_open_names_in_insertion_order() {
    let original = config();
    let runtime = original.clone().into_runtime().unwrap();
    let selected = runtime.first_model();
    assert_eq!(selected.name(), "z first / 任意");
    let profile = selected.profile();
    assert_eq!(
        (&*profile.provider, &*profile.model),
        (VENDOR, "external:model/version")
    );
    assert_eq!(profile.max_context, 8192);
    let second = runtime.select_model("a second").unwrap();
    assert_eq!(second.name(), "a second");
    assert!(
        matches!(runtime.select_model("unknown / 任意"), Err(ConfigError::Model(name, _)) if name == "unknown / 任意")
    );
    assert!(Arc::ptr_eq(&selected.config.0, &second.config.0));

    let mut reload = original;
    reload.models["z first / 任意"].model = "replacement-wire-model".into();
    reload.models.swap_remove("a second");
    let reloaded = reload.into_runtime().unwrap();
    let rebound = reloaded.select_model(selected.name()).unwrap();
    assert!(!Arc::ptr_eq(&selected.config.0, &rebound.config.0));
    assert_eq!(rebound.profile().model, "replacement-wire-model");
    assert!(reloaded.select_model(second.name()).is_err());
    drop(runtime);
    assert_eq!(selected.profile().model, "external:model/version");
    assert_eq!(second.profile().model, "another-external-model");
}

#[test]
fn admission_checks_all_catalog_members_before_selection_or_credentials() {
    let root = tempfile::tempdir().unwrap();
    let file_name = root.path().file_name().unwrap().to_string_lossy();
    let variable = format!("SKYHOOK_ABSENT_{file_name}");
    assert!(std::env::var_os(&variable).is_none());
    let mut raw = config();
    *vendor(&mut raw).1 = Some(variable.clone());
    raw.models["a second"].provider = "missing provider".into();
    assert!(
        matches!(raw.clone().into_runtime(), Err(ConfigError::UnknownModelProvider { model, provider })
        if model == "a second" && provider == "missing provider")
    );
    raw.models["a second"].provider = VENDOR.into();
    let runtime = raw.into_runtime().unwrap();
    assert!(matches!(
        runtime.select_model("missing model"),
        Err(ConfigError::Model(..))
    ));
    let selected = runtime.first_model();
    assert_eq!(selected.profile().model, "external:model/version");
    assert!(
        matches!(selected.harness_builder(root.path()), Err(ConfigError::MissingEnvironment(name)) if name == variable)
    );
    assert!(!root.path().join(".skyhook").exists());

    // Provider structure precedes catalog checks, and an empty catalog is
    // rejected before any secret lookup.
    let mut raw = config();
    raw.models.clear();
    *vendor(&mut raw).0 = "not an endpoint".into();
    assert!(
        matches!(raw.clone().into_runtime(), Err(ConfigError::Provider(name, _)) if name == VENDOR)
    );
    let (base_url, api_key_env) = vendor(&mut raw);
    *base_url = "http://127.0.0.1:1/v1".into();
    *api_key_env = Some("SKYHOOK_EMPTY_CATALOG_MUST_NOT_LOOK_UP_SECRET".into());
    raw.targets = toml::from_str("[root]\ntype = 'ssh'\nhost = 'unused'").unwrap();
    assert!(matches!(
        raw.clone().into_runtime(),
        Err(ConfigError::Structure(_))
    ));
    raw.targets.entries.clear();
    assert!(matches!(raw.into_runtime(), Err(ConfigError::NoModels)));
}
