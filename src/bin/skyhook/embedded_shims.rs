//! Discover transport-specific shims from the files staged in target/shims.
//! Debug builds read the directory live; release builds embed it at compile time.
use skyhook::remote::{ArtifactError, EmbeddedShimCatalog};

#[derive(rust_embed::RustEmbed)]
#[folder = "target/shims/"]
#[allow_missing = true]
#[include = "*-*-*"]
#[exclude = ".*"]
#[exclude = "**/.*"]
struct EmbeddedShims;

pub(crate) fn catalog() -> Result<EmbeddedShimCatalog, ArtifactError> {
    EmbeddedShimCatalog::from_embedded_assets(EmbeddedShims::iter().map(|name| {
        let file = EmbeddedShims::get(&name).expect("an enumerated embedded shim must exist");
        (name, file.data)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_staged_shim_is_a_valid_catalog_entry() {
        catalog().unwrap();
        for name in EmbeddedShims::iter() {
            assert!(!name.starts_with('.'));
            let file = EmbeddedShims::get(&name).unwrap();
            assert!(!file.data.is_empty());
            // Validate every discovered filename without a second target list.
            EmbeddedShimCatalog::from_embedded_assets([(name, file.data)]).unwrap();
        }
    }
}
