//! Discover transport-specific shims from the filenames embedded at build time.
use skyhook::remote::{ArtifactError, EmbeddedShimCatalog};

#[cfg(skyhook_embed_shims)]
#[derive(rust_embed::RustEmbed)]
#[folder = "target/shims/"]
#[allow_missing = true]
#[include = "*-*-*"]
#[exclude = ".*"]
#[exclude = "**/.*"]
struct EmbeddedShims;

pub(crate) fn catalog() -> Result<EmbeddedShimCatalog, ArtifactError> {
    #[cfg(skyhook_embed_shims)]
    {
        EmbeddedShimCatalog::from_embedded_assets(EmbeddedShims::iter().map(|name| {
            let file = EmbeddedShims::get(&name)
                .expect("an enumerated, compile-time embedded shim must exist");
            (name, file.data)
        }))
    }
    #[cfg(not(skyhook_embed_shims))]
    {
        Ok(EmbeddedShimCatalog::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(skyhook_embed_shims)]
    #[test]
    fn discovers_embedded_shims_without_reading_the_source_directory() {
        let catalog = catalog().unwrap();
        assert!(!catalog.is_empty());
        for name in EmbeddedShims::iter() {
            assert!(!name.starts_with('.'));
            let file = EmbeddedShims::get(&name).unwrap();
            assert!(matches!(file.data, std::borrow::Cow::Borrowed(_)));
            assert!(!file.data.is_empty());
            // Validate every discovered filename without a second target list.
            EmbeddedShimCatalog::from_embedded_assets([(name, file.data)]).unwrap();
        }
    }

    #[cfg(not(skyhook_embed_shims))]
    #[test]
    fn skipped_embedding_has_an_empty_catalog() {
        assert!(catalog().unwrap().is_empty());
    }
}
