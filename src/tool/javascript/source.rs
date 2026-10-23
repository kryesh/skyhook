//! The submitted async body, compiled as its own module. Declaring a module only
//! compiles it, so a syntax error is known before anything runs.

use rquickjs::{Ctx, Function, Module, module::Declared};
use serde::Serialize;

pub(super) const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

const FILENAME: &str = "skyhook-script";
// Opening the wrapper on the body's first line keeps QuickJS line numbers the
// body's own, including in stacks the script reads itself. The comma keeps the
// function anonymous rather than named `default`.
const PREFIX: &str = "export default (0, async () => {";

pub(super) struct SubmittedSource {
    wrapper: String,
    layout: Layout,
}

/// What failure reports need to place a position in the submitted body.
#[derive(Serialize)]
pub(super) struct Layout {
    filename: &'static str,
    prefix: usize,
    lines: usize,
}

impl SubmittedSource {
    pub(super) fn new(source: &str) -> Self {
        let terminators = source.matches(['\n', '\r', '\u{2028}', '\u{2029}']).count();
        Self {
            wrapper: format!("{PREFIX}{source}\n}})"),
            layout: Layout {
                filename: FILENAME,
                prefix: PREFIX.len(),
                lines: terminators - source.matches("\r\n").count() + 1,
            },
        }
    }

    pub(super) const fn layout(&self) -> &Layout {
        &self.layout
    }

    pub(super) fn declare<'js>(&self, ctx: &Ctx<'js>) -> rquickjs::Result<Module<'js, Declared>> {
        Module::declare(ctx.clone(), FILENAME, self.wrapper.as_bytes())
    }

    /// The body, once its module has run; only a body that closes the wrapper
    /// early runs anything here.
    pub(super) async fn instantiate(
        module: Module<'_, Declared>,
    ) -> rquickjs::Result<Function<'_>> {
        let (module, evaluated) = module.eval()?;
        evaluated.into_future::<()>().await?;
        module.get("default")
    }
}
