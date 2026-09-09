//! Bounded HTTP requests. Redirects are deliberately handled here, never by reqwest.
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use reqwest::{
    Client, Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    fetch_text,
    workspace::{resolve_existing, resolve_writable},
};
use crate::tool::{
    PathArgument, PathKind, RegistryError, ToolContext, ToolError, ToolOptions, ToolOutput,
    ToolPlacement, ToolRegistryBuilder,
    policy::{Capability, PathAccess, PermissionUse, ResourceId},
};

mod diagnostics;
mod progress;
use diagnostics::{DnsFailure, FetchDiagnostic, FetchErrorKind, FetchPhase};
use progress::{FetchFailureOutput, FetchProgress, classified_error, diagnostic_error};

#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum FetchResultSchema {
    Response(FetchOutput),
    Failure(FetchFailureOutput),
}

const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_UPLOAD_BYTES: u64 = MAX_BYTES;
fn default_method() -> String {
    "GET".into()
}
const fn default_timeout() -> u64 {
    30
}
const fn default_connect_timeout() -> u64 {
    10
}
const fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}
const fn default_redirects() -> usize {
    5
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct FetchArgs {
    /// HTTP(S), without URL credentials.
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    /// Appends to the URL query.
    #[serde(default)]
    pub query: Vec<(String, String)>,
    #[serde(default)]
    pub headers: BTreeMap<String, HeaderValues>,
    pub body: Option<RequestBody>,
    pub auth: Option<Auth>,
    /// Extract readable HTML text.
    #[serde(default)]
    pub text: bool,
    #[serde(default)]
    pub response_format: ResponseFormat,
    pub save_to: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
    /// Seconds.
    #[serde(default = "default_timeout")]
    #[schemars(range(min = 1, max = 3600))]
    pub timeout: u64,
    /// Seconds.
    #[serde(default = "default_connect_timeout")]
    #[schemars(range(min = 1, max = 3600))]
    pub connect_timeout: u64,
    /// Decoded response bytes; excess fails.
    #[serde(default = "default_max_bytes")]
    #[schemars(range(min = 1, max = 104857600))]
    pub max_bytes: u64,
    #[serde(default)]
    pub redirects: RedirectPolicy,
    #[serde(default = "default_redirects")]
    #[schemars(range(min = 0, max = 20))]
    pub max_redirects: usize,
    pub proxy: Option<String>,
    /// Skip TLS certificate verification.
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub(super) enum HeaderValues {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RequestBody {
    Text { value: String },
    Json { value: Value },
    Form { fields: Vec<(String, String)> },
    Base64 { value: String },
    File { path: String },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Auth {
    Basic {
        username: String,
        #[serde(default)]
        password: String,
    },
    Bearer {
        token: String,
    },
}

#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ResponseFormat {
    #[default]
    Auto,
    Text,
    Base64,
}
#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum RedirectPolicy {
    #[default]
    Safe,
    Follow,
    Manual,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct FetchOutput {
    status: u16,
    ok: bool,
    url: String,
    method: String,
    headers: BTreeMap<String, Vec<String>>,
    redirects: Vec<Redirect>,
    body: ResponseBody,
    /// Decoded entity bytes received (before text extraction).
    received_bytes: u64,
    elapsed_ms: u64,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
struct Redirect {
    status: u16,
    url: String,
    location: String,
    method: String,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ResponseBody {
    Text {
        #[schemars(extend("x-skyhook-truncatable" = true))]
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<fetch_text::ExtractionMetadata>,
    },
    Base64 {
        #[schemars(extend("x-skyhook-truncatable" = true))]
        data: String,
    },
    File {
        path: String,
        bytes: u64,
    },
    Empty,
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register_dynamic(
        "fetch",
        "HTTP(S) from the selected target. HTTP error statuses are normal results. Safe redirects follow GET/HEAD only; no HTTPS downgrade or retries.",
        serde_json::to_value(schema_for!(FetchArgs)).expect("fetch schema serializes"),
        ToolOptions::new(vec![Capability::Network])
            .argument_validator(|arguments| {
                let args: FetchArgs = serde_json::from_value(arguments.clone()).map_err(invalid)?;
                validate(&args)
            })
            .argument_permissions(|location, arguments| {
                let args: FetchArgs = serde_json::from_value(arguments.clone()).map_err(invalid)?;
                let url = parse_url(&args.url)?;
                Ok(vec![PermissionUse::new(Capability::Network, ResourceId::network(&location.target, &url.origin().ascii_serialization()))])
            })
            .argument_paths(|arguments| {
                let args: FetchArgs = serde_json::from_value(arguments.clone()).map_err(invalid)?;
                let mut paths = Vec::new();
                if args.save_to.is_some() { paths.push(PathArgument::pointer("/save_to", PathAccess::Write, PathKind::Writable)); }
                if matches!(args.body, Some(RequestBody::File { .. })) {
                    paths.push(PathArgument::pointer("/body/path", PathAccess::Read, PathKind::Existing));
                }
                Ok(paths)
            })
            .placement(ToolPlacement::TargetedWorkspace).background().named()
            .output_schema(serde_json::to_value(schema_for!(FetchResultSchema)).expect("fetch output schema serializes")),
        |context, arguments| async move {
            let args: FetchArgs = serde_json::from_value(arguments).map_err(invalid)?;
            validate(&args)?;
            let mut progress = FetchProgress::new(&args);
            let outcome = tokio::select! {
                biased;
                () = context.cancelled() => return Err(ToolError::Cancelled),
                result = tokio::time::timeout(Duration::from_secs(args.timeout), execute(&context, &args, &mut progress)) => result,
            };
            match outcome {
                Ok(Ok(result)) => Ok(ToolOutput::new(serde_json::to_value(result).map_err(failed)?)),
                Ok(Err(error)) => Err(progress.failure(error)),
                Err(_) => Err(progress.timeout(args.timeout)),
            }
        },
    )?;
    Ok(())
}

fn invalid(error: impl std::fmt::Display) -> ToolError {
    ToolError::InvalidArguments(error.to_string())
}
fn failed(error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(error.to_string())
}
fn network(error: reqwest::Error, phase: FetchPhase) -> ToolError {
    diagnostic_error(FetchDiagnostic::from_reqwest(&error, phase))
}

/// Same system lookup as reqwest's default resolver, with a typed failure marker.
/// There is no additional lookup or preflight connection.
#[derive(Debug)]
struct FetchResolver;
impl reqwest::dns::Resolve for FetchResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(|error| {
                    Box::new(DnsFailure::new(error)) as Box<dyn std::error::Error + Send + Sync>
                })?
                .collect::<Vec<_>>();
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn parse_url(value: &str) -> Result<Url, ToolError> {
    let url = Url::parse(value).map_err(|_| invalid("invalid absolute URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid("URL must use HTTP or HTTPS and have a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(
            "embedded URL credentials are not supported; use auth",
        ));
    }
    Ok(url)
}
fn validate(args: &FetchArgs) -> Result<(), ToolError> {
    parse_url(&args.url)?;
    Method::from_bytes(args.method.as_bytes()).map_err(invalid)?;
    if args.timeout == 0
        || args.timeout > 3600
        || args.connect_timeout == 0
        || args.connect_timeout > 3600
    {
        return Err(invalid("timeouts must be between 1 and 3600 seconds"));
    }
    if args.max_bytes == 0 || args.max_bytes > MAX_BYTES {
        return Err(invalid("max_bytes must be between 1 and 104857600"));
    }
    if args.max_redirects > 20 {
        return Err(invalid("max_redirects must not exceed 20"));
    }
    if args.text && (args.save_to.is_some() || args.response_format == ResponseFormat::Base64) {
        return Err(invalid(
            "text conflicts with save_to and response_format base64",
        ));
    }
    if args.overwrite && args.save_to.is_none() {
        return Err(invalid("overwrite requires save_to"));
    }
    request_headers(args)?;
    Ok(())
}

fn request_headers(args: &FetchArgs) -> Result<HeaderMap, ToolError> {
    let mut headers = HeaderMap::new();
    for (key, values) in &args.headers {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(invalid)?;
        // Let the HTTP implementation compute framing; conflicting framing is unsafe.
        if matches!(name.as_str(), "content-length" | "transfer-encoding") {
            return Err(invalid(
                "content-length and transfer-encoding are managed by fetch",
            ));
        }
        let values = match values {
            HeaderValues::One(value) => std::slice::from_ref(value),
            HeaderValues::Many(values) => values.as_slice(),
        };
        for value in values {
            headers.append(name.clone(), HeaderValue::from_str(value).map_err(invalid)?);
        }
    }
    if let Some(auth) = &args.auth {
        if headers.contains_key("authorization") {
            return Err(invalid("auth conflicts with the authorization header"));
        }
        let value = match auth {
            Auth::Basic { username, password } => {
                if username.contains(':') {
                    return Err(invalid("basic auth username must not contain ':'"));
                }
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("{username}:{password}"))
                )
            }
            Auth::Bearer { token } => format!("Bearer {token}"),
        };
        let mut value = HeaderValue::from_str(&value).map_err(invalid)?;
        value.set_sensitive(true);
        headers.insert("authorization", value);
    }
    Ok(headers)
}

struct PreparedBody {
    bytes: Vec<u8>,
    content_type: Option<String>,
}
struct FileUpload {
    snapshot: tempfile::NamedTempFile,
    length: u64,
}
impl FileUpload {
    fn len(&self) -> u64 {
        self.length
    }
    async fn body(&self) -> Result<reqwest::Body, ToolError> {
        let file = tokio::fs::File::open(self.snapshot.path()).await?;
        Ok(reqwest::Body::wrap_stream(
            tokio_util::io::ReaderStream::new(file),
        ))
    }
}
enum Upload {
    Bytes(PreparedBody),
    File(FileUpload),
}

async fn upload_file(
    workspace: &Path,
    path: &str,
    remaining: u64,
) -> Result<FileUpload, ToolError> {
    let path = resolve_existing(workspace, path).await?;
    if !tokio::fs::metadata(&path).await?.is_file() {
        return Err(invalid("upload path must be a regular file"));
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    // Avoid blocking if the path is swapped to a FIFO between metadata and open.
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(&path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(invalid("upload path must be a regular file"));
    }
    if metadata.len() > remaining {
        return Err(invalid("upload exceeds 100 MiB limit"));
    }
    // Snapshot once, with a bounded streaming copy. Redirect replays reopen this
    // immutable private snapshot, not a potentially changed source file.
    let snapshot = tempfile::NamedTempFile::new()?;
    let mut output = tokio::fs::File::from_std(snapshot.as_file().try_clone()?);
    let length = tokio::io::copy(&mut file.take(remaining + 1), &mut output).await?;
    output.flush().await?;
    if length > remaining {
        return Err(invalid("upload exceeds 100 MiB limit"));
    }
    Ok(FileUpload { snapshot, length })
}
fn check_upload_size(size: usize) -> Result<(), ToolError> {
    if size as u64 > MAX_UPLOAD_BYTES {
        Err(invalid("upload exceeds 100 MiB limit"))
    } else {
        Ok(())
    }
}
fn decode_base64(value: &str) -> Result<Vec<u8>, ToolError> {
    if value.len() as u64 > MAX_UPLOAD_BYTES.div_ceil(3) * 4 {
        return Err(invalid("upload exceeds 100 MiB limit"));
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| invalid("invalid base64 body"))?;
    check_upload_size(bytes.len())?;
    Ok(bytes)
}
async fn prepare_body(
    body: Option<&RequestBody>,
    workspace: &Path,
) -> Result<Option<Upload>, ToolError> {
    let Some(body) = body else { return Ok(None) };
    let (bytes, content_type) = match body {
        RequestBody::Text { value } => (
            value.as_bytes().to_vec(),
            Some("text/plain; charset=utf-8".into()),
        ),
        RequestBody::Json { value } => (
            serde_json::to_vec(value).map_err(invalid)?,
            Some("application/json".into()),
        ),
        RequestBody::Form { fields } => {
            let mut url = Url::parse("http://localhost").expect("static URL");
            url.query_pairs_mut()
                .extend_pairs(fields.iter().map(|(k, v)| (k, v)));
            (
                url.query().unwrap_or_default().as_bytes().to_vec(),
                Some("application/x-www-form-urlencoded".into()),
            )
        }
        RequestBody::Base64 { value } => (decode_base64(value)?, None),
        RequestBody::File { path } => {
            return Ok(Some(Upload::File(
                upload_file(workspace, path, MAX_UPLOAD_BYTES).await?,
            )));
        }
    };
    check_upload_size(bytes.len())?;
    Ok(Some(Upload::Bytes(PreparedBody {
        bytes,
        content_type,
    })))
}

async fn apply_body(
    mut request: reqwest::RequestBuilder,
    body: Option<&Upload>,
    headers: &HeaderMap,
) -> Result<reqwest::RequestBuilder, ToolError> {
    match body {
        None => {}
        Some(Upload::Bytes(body)) => {
            if !headers.contains_key("content-type")
                && let Some(content_type) = &body.content_type
            {
                request = request.header("content-type", content_type);
            }
            request = request.body(body.bytes.clone());
        }
        Some(Upload::File(data)) => {
            request = request
                .header("content-length", data.len())
                .body(data.body().await?);
        }
    }
    Ok(request)
}

fn redirect_method(status: u16, method: &Method) -> (Method, bool) {
    if (status == 303 && method != Method::HEAD)
        || (matches!(status, 301 | 302) && method == Method::POST)
    {
        (Method::GET, true)
    } else {
        (method.clone(), false)
    }
}
fn strip_redirect_headers(headers: &mut HeaderMap, from: &Url, to: &Url, drop_body: bool) {
    headers.remove("host");
    if from.origin() != to.origin() {
        // Custom headers often contain API keys. Only carry demonstrably non-secret
        // negotiation headers to a different origin; never guess credential names.
        let safe: HeaderMap = headers
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.as_str(),
                    "accept"
                        | "accept-language"
                        | "accept-encoding"
                        | "user-agent"
                        | "content-type"
                )
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        *headers = safe;
    }
    if drop_body {
        for name in [
            "content-type",
            "content-length",
            "transfer-encoding",
            "content-encoding",
            "content-language",
            "content-location",
            "digest",
        ] {
            headers.remove(name);
        }
    }
}
fn client(args: &FetchArgs) -> Result<Client, ToolError> {
    let mut builder = Client::builder()
        .user_agent(concat!("Skyhook/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .dns_resolver(std::sync::Arc::new(FetchResolver))
        .connect_timeout(Duration::from_secs(args.connect_timeout))
        .timeout(Duration::from_secs(args.timeout))
        .danger_accept_invalid_certs(args.insecure);
    if let Some(proxy) = &args.proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|error| network(error, FetchPhase::ClientPreparation))?,
        );
    }
    builder
        .build()
        .map_err(|error| network(error, FetchPhase::ClientPreparation))
}

async fn execute(
    context: &ToolContext,
    args: &FetchArgs,
    progress: &mut FetchProgress,
) -> Result<FetchOutput, ToolError> {
    let started = progress.started;
    progress.phase = FetchPhase::LocalIo;
    let workspace = &context.execution_location.workspace;
    let destination = match &args.save_to {
        Some(path) => {
            let path = resolve_writable(workspace, path).await?;
            if tokio::fs::try_exists(&path).await? {
                if !args.overwrite {
                    return Err(invalid(
                        "save_to already exists; set overwrite to replace it",
                    ));
                }
                if !tokio::fs::metadata(&path).await?.is_file() {
                    return Err(invalid("save_to must be a regular file"));
                }
            }
            Some(path)
        }
        None => None,
    };
    progress.phase = FetchPhase::ClientPreparation;
    let client = client(args)?;
    let mut url = parse_url(&args.url)?;
    if !args.query.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(args.query.iter().map(|(k, v)| (k, v)));
    }
    url.set_fragment(None);
    let mut method = Method::from_bytes(args.method.as_bytes()).map_err(invalid)?;
    let mut headers = request_headers(args)?;
    progress.phase = FetchPhase::LocalIo;
    let mut body = prepare_body(args.body.as_ref(), workspace).await?;
    let mut redirects = Vec::new();
    let mut authorized_origin = url.origin();
    let response = loop {
        if context.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        progress.begin_request(&url, &method);
        // The initial origin was authorized by argument_permissions. A changed origin
        // must be authorized before sending any headers or replaying any upload.
        if url.origin() != authorized_origin {
            progress.phase = FetchPhase::Authorization;
            authorize_url(context, &url).await?;
            authorized_origin = url.origin();
        }
        progress.phase = FetchPhase::Request;
        let request = client
            .request(method.clone(), url.clone())
            .headers(headers.clone());
        let response = apply_body(request, body.as_ref(), &headers)
            .await?
            .send()
            .await
            .map_err(|error| network(error, FetchPhase::Request))?;
        progress.response(&response);
        let status = response.status().as_u16();
        let follow = matches!(status, 301 | 302 | 303 | 307 | 308)
            && args.redirects != RedirectPolicy::Manual
            && (args.redirects == RedirectPolicy::Follow
                || method == Method::GET
                || method == Method::HEAD);
        if !follow {
            break response;
        }
        progress.phase = FetchPhase::Redirect;
        let Some(location) = response.headers().get("location") else {
            break response;
        };
        if redirects.len() >= args.max_redirects {
            return Err(redirect_error(
                "maximum redirects exceeded",
                &response,
                &method,
                &redirects,
                started,
            ));
        }
        let location = location.to_str().map_err(|_| {
            redirect_error(
                "invalid redirect location header",
                &response,
                &method,
                &redirects,
                started,
            )
        })?;
        let next = url.join(location).map_err(|_| {
            redirect_error(
                "invalid redirect URL",
                &response,
                &method,
                &redirects,
                started,
            )
        })?;
        let mut next = parse_url(next.as_str()).map_err(|_| {
            redirect_error(
                "redirect URL must be HTTP(S) without embedded credentials",
                &response,
                &method,
                &redirects,
                started,
            )
        })?;
        next.set_fragment(None);
        if url.scheme() == "https" && next.scheme() == "http" {
            return Err(redirect_error(
                "HTTPS to HTTP redirect blocked",
                &response,
                &method,
                &redirects,
                started,
            ));
        }
        let (next_method, drop_body) = redirect_method(status, &method);
        strip_redirect_headers(&mut headers, &url, &next, drop_body);
        if drop_body {
            body = None;
        }
        redirects.push(Redirect {
            status,
            url: url.to_string(),
            location: next.to_string(),
            method: method.to_string(),
        });
        progress.redirects(&redirects);
        url = next;
        method = next_method;
    };
    let status = response.status();
    let response_headers = collect_headers(response.headers());
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut received = 0u64;
    progress.phase = FetchPhase::ResponseBody;
    let processed = async {
        // content_length is decoded length when known; the streamed count remains authoritative.
        if method != Method::HEAD
            && response
                .content_length()
                .is_some_and(|length| length > args.max_bytes)
        {
            return Err(classified_error(
                FetchPhase::ResponseBody,
                FetchErrorKind::SizeLimit,
                "response exceeds max_bytes",
            ));
        }
        progress.phase = FetchPhase::LocalIo;
        let mut temp = match &destination {
            Some(path) => Some(tempfile::NamedTempFile::new_in(
                path.parent()
                    .ok_or_else(|| invalid("save_to has no parent"))?,
            )?),
            None => None,
        };
        let mut file = match &temp {
            Some(temp) => Some(tokio::fs::File::from_std(temp.as_file().try_clone()?)),
            None => None,
        };
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        loop {
            progress.phase = FetchPhase::ResponseBody;
            let Some(chunk) = stream.next().await else {
                break;
            };
            let chunk = chunk.map_err(|error| network(error, FetchPhase::ResponseBody))?;
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| failed("response too large"))?;
            progress.received_bytes = received;
            if received > args.max_bytes {
                return Err(classified_error(
                    FetchPhase::ResponseBody,
                    FetchErrorKind::SizeLimit,
                    "response exceeds max_bytes",
                ));
            }
            if let Some(file) = &mut file {
                progress.phase = FetchPhase::LocalIo;
                file.write_all(&chunk).await?;
            } else {
                bytes.extend_from_slice(&chunk);
            }
        }
        let response_body = if let Some(mut file) = file {
            progress.phase = FetchPhase::LocalIo;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            if context.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let temp = temp.take().expect("download temporary file");
            let destination = destination.as_ref().expect("download destination");
            if args.overwrite {
                temp.persist(destination)
                    .map_err(|e| ToolError::from(e.error))?;
            } else {
                temp.persist_noclobber(destination)
                    .map_err(|e| ToolError::from(e.error))?;
            }
            ResponseBody::File {
                path: destination.to_string_lossy().into_owned(),
                bytes: received,
            }
        } else {
            progress.phase = if args.text {
                FetchPhase::Extraction
            } else {
                FetchPhase::Decode
            };
            response_body(bytes, content_type.as_deref(), url.as_str(), args).await?
        };
        Ok::<_, ToolError>(response_body)
    }
    .await;
    let mut output = FetchOutput {
        status: status.as_u16(),
        ok: status.is_success(),
        url: url.to_string(),
        method: method.to_string(),
        headers: response_headers,
        redirects,
        body: ResponseBody::Empty,
        received_bytes: received,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    };
    match processed {
        Ok(body) => {
            output.body = body;
            Ok(output)
        }
        Err(error) => Err(error),
    }
}

fn redirect_error(
    message: &'static str,
    response: &reqwest::Response,
    method: &Method,
    redirects: &[Redirect],
    started: Instant,
) -> ToolError {
    let output = FetchOutput {
        status: response.status().as_u16(),
        ok: response.status().is_success(),
        url: response.url().to_string(),
        method: method.to_string(),
        headers: collect_headers(response.headers()),
        redirects: redirects.to_vec(),
        body: ResponseBody::Empty,
        received_bytes: 0,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    };
    let mut value = serde_json::to_value(output).expect("fetch output serializes");
    let mut diagnostic =
        FetchDiagnostic::new(FetchPhase::Redirect, FetchErrorKind::RedirectFailure);
    diagnostic.message = message.to_owned();
    value["diagnostic"] = serde_json::to_value(diagnostic).expect("fetch diagnostic serializes");
    ToolError::with_output(message, ToolOutput::new(value))
}

async fn authorize_url(context: &ToolContext, url: &Url) -> Result<(), ToolError> {
    context
        .authorize_network(&url.origin().ascii_serialization())
        .await
}
fn collect_headers(headers: &HeaderMap) -> BTreeMap<String, Vec<String>> {
    let mut result: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        result
            .entry(name.to_string())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    result
}
async fn response_body(
    bytes: Vec<u8>,
    content_type: Option<&str>,
    url: &str,
    args: &FetchArgs,
) -> Result<ResponseBody, ToolError> {
    if bytes.is_empty() {
        return Ok(ResponseBody::Empty);
    }
    if args.text {
        if fetch_text::is_html(&bytes, content_type) {
            if bytes.len() > 10 * 1024 * 1024 {
                return Err(failed("HTML extraction input exceeds 10 MiB limit"));
            }
            let extracted =
                fetch_text::extract(fetch_text::decode(&bytes, content_type), url.to_owned())
                    .await?;
            return Ok(ResponseBody::Text {
                text: extracted.text,
                metadata: Some(extracted.metadata),
            });
        }
        if !fetch_text::is_textual(&bytes, content_type) {
            return Err(classified_error(
                FetchPhase::Extraction,
                FetchErrorKind::ExtractionFailure,
                "text extraction is unsupported for this binary content type",
            ));
        }
        return Ok(ResponseBody::Text {
            text: fetch_text::decode(&bytes, content_type),
            metadata: None,
        });
    }
    if args.response_format == ResponseFormat::Base64
        || (args.response_format == ResponseFormat::Auto
            && !fetch_text::is_textual(&bytes, content_type))
    {
        Ok(ResponseBody::Base64 {
            data: STANDARD.encode(bytes),
        })
    } else {
        Ok(ResponseBody::Text {
            text: fetch_text::decode(&bytes, content_type),
            metadata: None,
        })
    }
}

#[cfg(test)]
#[path = "fetch/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "fetch_tls_tests.rs"]
mod tls_tests;
