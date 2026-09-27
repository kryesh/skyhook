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
    EmbeddedShimCatalog::from_embedded_assets(EmbeddedShims::iter(), |name| {
        EmbeddedShims::get(name).map(|file| file.data)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_staged_shim_is_a_valid_catalog_entry() {
        catalog().unwrap();
        assert!(EmbeddedShims::iter().all(|name| {
            !name.starts_with('.')
                && EmbeddedShims::get(&name).is_some_and(|file| !file.data.is_empty())
        }));
    }
}
