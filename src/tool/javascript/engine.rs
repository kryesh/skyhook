//! Trusted modules are compiled once, then instantiated in each isolated runtime.

use std::sync::OnceLock;

use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Context, Ctx, Module, Runtime, WriteOptions,
    context::intrinsic,
    loader::{ImportAttributes, Loader, Resolver},
    module::{Declared, Evaluated},
};

use crate::tool::ToolContext;

/// Each module follows those it imports; the last is the entry point.
const MODULES: &[(&str, &str)] = &[
    ("skyhook:errors", include_str!("js/errors.js")),
    ("skyhook:values", include_str!("js/values.js")),
    ("skyhook:console", include_str!("js/console.js")),
    ("skyhook:tools", include_str!("js/tools.js")),
    ("skyhook:pool", include_str!("js/pool.js")),
    ("skyhook:runtime", include_str!("js/runtime.js")),
];

static BYTECODE: OnceLock<Result<Vec<Vec<u8>>, String>> = OnceLock::new();

fn compile() -> Result<Vec<Vec<u8>>, String> {
    // This runtime has neither host functions nor a caller's interrupt handler:
    // cancellation of one execution cannot poison the process-wide cache.
    let runtime = Runtime::new().map_err(|error| error.to_string())?;
    let context = Context::full(&runtime).map_err(|error| error.to_string())?;
    context.with(|ctx| {
        (MODULES.iter())
            .map(|(name, source)| {
                Module::declare(ctx.clone(), *name, *source)?.write(WriteOptions::default())
            })
            .collect::<rquickjs::Result<_>>()
            .catch(&ctx)
            .map_err(|error| error.to_string())
    })
}

/// Instantiates the modules in `ctx`, returning the entry point.
pub(super) fn load<'js>(ctx: &Ctx<'js>) -> Result<Module<'js, Evaluated>, String> {
    let bytecode = BYTECODE
        .get_or_init(compile)
        .as_ref()
        .map_err(Clone::clone)?;
    let mut entry = None;
    for bytes in bytecode {
        // Only bytecode compiled from the embedded modules by this process enters
        // the cache, which outlives every runtime reading it.
        let module = unsafe { Module::load(ctx.clone(), bytes) }.and_then(|module| {
            let (module, evaluated) = module.eval()?;
            evaluated.finish::<()>()?;
            Ok(module)
        });
        entry = Some(module.catch(ctx).map_err(|error| error.to_string())?);
    }
    Ok(entry.expect("the module list is not empty"))
}

/// The embedded modules import one another; the submitted body imports none.
struct Embedded;

impl Resolver for Embedded {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if MODULES.iter().any(|(module, _)| *module == base) {
            Ok(name.to_owned())
        } else {
            Err(rquickjs::Error::new_resolving(base, name))
        }
    }
}

impl Loader for Embedded {
    fn load<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        Err(rquickjs::Error::new_loading(name))
    }
}

pub(super) async fn create(context: &ToolContext) -> Result<(AsyncRuntime, AsyncContext), String> {
    let runtime = AsyncRuntime::new().map_err(|error| error.to_string())?;
    runtime.set_loader(Embedded, Embedded).await;
    let cancellation = context.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || cancellation.is_cancelled())))
        .await;
    let context = AsyncContext::builder()
        .with::<intrinsic::Eval>()
        .with::<intrinsic::Date>()
        .with::<intrinsic::RegExpCompiler>()
        .with::<intrinsic::RegExp>()
        .with::<intrinsic::TypedArrays>()
        .with::<intrinsic::Performance>()
        .with::<intrinsic::Proxy>()
        .with::<intrinsic::Promise>()
        .with::<intrinsic::Json>()
        .with::<intrinsic::MapSet>()
        .build_async(&runtime)
        .await
        .map_err(|error| error.to_string())?;
    Ok((runtime, context))
}
