//! The request headers a provider composes and their values: fixed when the
//! provider is built, produced by a command on first use, or issued by an
//! authenticator per request. Composition finishes before any value is produced,
//! so a replaced source never runs. Only successful, validated command output is
//! cached, until a 401 answers a request that carried it.

use std::{
    fmt,
    process::Stdio,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tokio::{io::AsyncReadExt, process::Command, sync::OnceCell};

use super::auth::Authenticator;
use crate::provider::{ProviderError, ProviderErrorKind};

// Credentials should be small; cap even unsuccessful or never-ending output.
const MAX_STDOUT_BYTES: usize = 64 * 1024;

/// A header value.
#[derive(Clone, Debug)]
pub(crate) enum Value {
    Fixed(HeaderValue),
    Command(CommandValue),
    /// What the authenticator issues under the header's name; it is asked once
    /// per request, whatever the number of names it serves.
    Issued(Arc<dyn Authenticator>),
}

/// A value as sent, with the command generations it came from.
struct Sent {
    value: HeaderValue,
    leases: Vec<Lease>,
}

impl Value {
    /// The value sent under `name`; none when an authenticator issues nothing there.
    async fn resolve(
        &self,
        name: &HeaderName,
        issued: &mut Issued,
    ) -> Result<Option<Sent>, ProviderError> {
        Ok(match self {
            Self::Fixed(value) => Some(Sent {
                value: value.clone(),
                leases: Vec::new(),
            }),
            Self::Command(command) => {
                let (value, lease) = command.lease().await?;
                Some(Sent {
                    value,
                    leases: vec![lease],
                })
            }
            Self::Issued(authenticator) => {
                issued
                    .headers(authenticator)
                    .await?
                    .get(name)
                    .map(|value| Sent {
                        value: value.clone(),
                        leases: Vec::new(),
                    })
            }
        })
    }
}

/// The headers each authenticator issued for one request.
#[derive(Default)]
struct Issued(Vec<(Arc<dyn Authenticator>, HeaderMap)>);

impl Issued {
    async fn headers(
        &mut self,
        authenticator: &Arc<dyn Authenticator>,
    ) -> Result<&HeaderMap, ProviderError> {
        let held = self
            .0
            .iter()
            .position(|(held, _)| Arc::ptr_eq(held, authenticator));
        let index = match held {
            Some(index) => index,
            None => {
                let headers = authenticator.headers().await?;
                self.0.push((Arc::clone(authenticator), headers));
                self.0.len() - 1
            }
        };
        Ok(&self.0[index].1)
    }
}

/// The request headers a provider sends with every request. A list header
/// gathers every value given under its name into one comma-separated value
/// without repeats, and is not sent while it holds none; any other header keeps
/// the last value given. While the provider is built, values may still be
/// pending.
#[derive(Clone, Debug)]
pub(crate) struct Headers<V = Value>(Vec<(HeaderName, Entry<V>)>);

impl<V> Default for Headers<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

#[derive(Clone, Debug)]
enum Entry<V> {
    One(V),
    List(Vec<V>),
}

impl<V> Entry<V> {
    fn into_values(self) -> Vec<V> {
        match self {
            Self::One(value) => vec![value],
            Self::List(values) => values,
        }
    }
}

impl<V> Headers<V> {
    /// Replace a header's value, or join a list header.
    pub(crate) fn insert(&mut self, name: HeaderName, value: V) {
        match self.entry(&name) {
            Some(Entry::List(values)) => values.push(value),
            Some(entry) => *entry = Entry::One(value),
            None => self.0.push((name, Entry::One(value))),
        }
    }

    /// Make `name` a list header holding `values` after any it holds.
    pub(crate) fn list(&mut self, name: HeaderName, values: impl IntoIterator<Item = V>) {
        match self.entry(&name) {
            Some(entry) => {
                let held = std::mem::replace(entry, Entry::List(Vec::new()));
                *entry = Entry::List(held.into_values().into_iter().chain(values).collect());
            }
            None => self
                .0
                .push((name, Entry::List(values.into_iter().collect()))),
        }
    }

    /// Merge `other` in, as though each of its headers were given here in turn.
    pub(crate) fn extend(&mut self, other: Self) {
        for (name, entry) in other.0 {
            match entry {
                Entry::One(value) => self.insert(name, value),
                Entry::List(values) => self.list(name, values),
            }
        }
    }

    /// Each composed value converted, in place.
    pub(crate) fn try_map<W, E>(
        self,
        mut convert: impl FnMut(V) -> Result<W, E>,
    ) -> Result<Headers<W>, E> {
        let entries = self.0.into_iter().map(|(name, entry)| {
            let entry = match entry {
                Entry::One(value) => Entry::One(convert(value)?),
                Entry::List(values) => Entry::List(
                    values
                        .into_iter()
                        .map(&mut convert)
                        .collect::<Result<_, E>>()?,
                ),
            };
            Ok((name, entry))
        });
        Ok(Headers(entries.collect::<Result<_, E>>()?))
    }

    pub(crate) fn contains(&self, name: &HeaderName) -> bool {
        self.0.iter().any(|(held, _)| held == name)
    }

    fn entry(&mut self, name: &HeaderName) -> Option<&mut Entry<V>> {
        self.0
            .iter_mut()
            .find_map(|(held, entry)| (held == name).then_some(entry))
    }
}

impl Headers {
    /// Produce each composed value once, for one request.
    pub(crate) async fn resolve(&self) -> Result<Resolved, ProviderError> {
        let mut issued = Issued::default();
        let mut resolved = Vec::new();
        for (name, entry) in &self.0 {
            let sent = match entry {
                Entry::One(value) => value.resolve(name, &mut issued).await?,
                Entry::List(values) => join(name, values, &mut issued).await?,
            };
            resolved.extend(sent.map(|sent| (name.clone(), sent)));
        }
        Ok(Resolved(resolved))
    }
}

/// A list header's items without repeats. A contribution with no item keeps no
/// leases; with no item at all, nothing is sent.
async fn join(
    name: &HeaderName,
    values: &[Value],
    issued: &mut Issued,
) -> Result<Option<Sent>, ProviderError> {
    let mut items: Vec<Vec<u8>> = Vec::new();
    let mut leases = Vec::new();
    let mut sensitive = false;
    for value in values {
        let Some(sent) = value.resolve(name, issued).await? else {
            continue;
        };
        let parts: Vec<&[u8]> = sent
            .value
            .as_bytes()
            .split(|byte| *byte == b',')
            .map(<[u8]>::trim_ascii)
            .filter(|item| !item.is_empty())
            .collect();
        if parts.is_empty() {
            continue;
        }
        sensitive |= sent.value.is_sensitive();
        leases.extend(sent.leases);
        for item in parts {
            if !items.iter().any(|held| held == item) {
                items.push(item.to_owned());
            }
        }
    }
    if items.is_empty() {
        return Ok(None);
    }
    let mut value = HeaderValue::from_bytes(&items.join(&b", "[..]))
        .expect("parts of header values join into one");
    value.set_sensitive(sensitive);
    Ok(Some(Sent { value, leases }))
}

/// One request's headers, each with the command generations its value came
/// from; the response settles them.
pub(crate) struct Resolved(Vec<(HeaderName, Sent)>);

impl fmt::Debug for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

impl Resolved {
    /// The headers as sent.
    pub(crate) fn map(&self) -> HeaderMap {
        self.0
            .iter()
            .map(|(name, sent)| (name.clone(), sent.value.clone()))
            .collect()
    }

    fn leases(&self) -> impl Iterator<Item = &Lease> {
        self.0.iter().flat_map(|(_, sent)| &sent.leases)
    }

    /// The server accepted the request.
    pub(crate) fn served(&self) {
        for lease in self.leases() {
            lease.generation.served.store(true, Ordering::Release);
        }
    }

    /// The server answered 401. When the request carried command values, the
    /// 401 is about them, whatever the body says: it reports them expired when
    /// any had been accepted before, for the runtime to retry with fresh ones,
    /// and a plain authentication failure otherwise. Each generation carried is
    /// discarded unless a newer one already replaced it.
    pub(crate) fn unauthorized(&self, mut error: ProviderError) -> ProviderError {
        let mut leases = self.leases().peekable();
        if leases.peek().is_none() {
            return error;
        }
        let mut expired = false;
        for lease in leases {
            expired |= lease.generation.served.load(Ordering::Acquire);
            let mut current = lease.current.lock().unwrap_or_else(PoisonError::into_inner);
            if Arc::ptr_eq(&current, &lease.generation) {
                *current = Arc::default();
            }
        }
        error.kind = if expired {
            ProviderErrorKind::CredentialExpired
        } else {
            ProviderErrorKind::Authentication
        };
        error
    }
}

/// What a command value supplies; its failures say which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// `api_key`.
    Credential,
    /// An entry under `headers`.
    Header,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Credential => "credential",
            Self::Header => "header",
        })
    }
}

/// A header value produced by `/bin/sh -c command` on first use; the trimmed
/// stdout, behind `prefix`, is cached only when the command succeeds. Each run
/// starts a generation; a 401 discards the generation its request carried
/// unless a newer one replaced it.
#[derive(Clone)]
pub(crate) struct CommandValue {
    command: String,
    prefix: Option<&'static str>,
    role: Role,
    current: Current,
}

type Current = Arc<Mutex<Arc<Generation>>>;

/// One run's value, shared by every request sent with it.
#[derive(Default)]
struct Generation {
    value: OnceCell<HeaderValue>,
    /// A response has accepted a request carrying this value.
    served: AtomicBool,
}

/// The command may name a secret's location; it is never printed.
impl fmt::Debug for CommandValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CommandValue")
    }
}

impl CommandValue {
    pub(crate) fn new(command: String, prefix: Option<&'static str>, role: Role) -> Self {
        Self {
            command,
            prefix,
            role,
            current: Current::default(),
        }
    }

    /// The current generation's value, running the command once per generation.
    async fn lease(&self) -> Result<(HeaderValue, Lease), ProviderError> {
        let generation = Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner));
        let value = generation
            .value
            .get_or_try_init(|| execute(&self.command, self.prefix, self.role))
            .await?
            .clone();
        let lease = Lease {
            current: Arc::clone(&self.current),
            generation,
        };
        Ok((value, lease))
    }
}

/// The generation one request carried.
struct Lease {
    current: Current,
    generation: Arc<Generation>,
}

fn failure(role: Role, what: &str) -> ProviderError {
    // Never retain the command, output, exit status, or underlying OS error.
    ProviderError {
        kind: ProviderErrorKind::Authentication,
        message: format!("{role} command {what}"),
    }
}

async fn execute(
    command: &str,
    prefix: Option<&'static str>,
    role: Role,
) -> Result<HeaderValue, ProviderError> {
    let failure = |what| failure(role, what);
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| failure("could not be started"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("output could not be read"))?;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    stdout
        .take((MAX_STDOUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| failure("output could not be read"))?;
    if bytes.len() > MAX_STDOUT_BYTES {
        return Err(failure("output exceeded the size limit"));
    }
    let status = child
        .wait()
        .await
        .map_err(|_| failure("could not be awaited"))?;
    if !status.success() {
        return Err(failure("exited unsuccessfully"));
    }
    let key = std::str::from_utf8(&bytes)
        .map_err(|_| failure("output was not valid UTF-8"))?
        .trim();
    if key.is_empty() {
        return Err(failure("output was empty"));
    }
    let value = zeroize::Zeroizing::new(format!("{}{key}", prefix.unwrap_or("")));
    let mut header = HeaderValue::from_str(&value)
        .map_err(|_| failure("output was not a valid header value"))?;
    header.set_sensitive(true);
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::BoxFuture;
    use reqwest::header::AUTHORIZATION;
    use std::{path::Path, time::Duration};

    fn quote(path: &Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
    }

    fn fixed(value: &'static str) -> Value {
        Value::Fixed(HeaderValue::from_static(value))
    }

    impl CommandValue {
        async fn value(&self) -> Result<HeaderValue, ProviderError> {
            Ok(self.lease().await?.0)
        }
    }

    /// A command printing `key-<run>`, counting its runs in `count`.
    fn counting(count: &Path) -> CommandValue {
        let count = quote(count);
        let text = format!("printf x >> {count}; printf key-%s \"$(wc -c < {count})\"");
        CommandValue::new(text, None, Role::Credential)
    }

    fn runs(count: &Path) -> usize {
        std::fs::read(count).map_or(0, |runs| runs.len())
    }

    /// Issues `x-issued-<n>` for every name, counting the requests it answers.
    #[derive(Default)]
    struct Counted(std::sync::atomic::AtomicUsize);

    impl Authenticator for Counted {
        fn headers(&self) -> BoxFuture<'_, Result<HeaderMap, ProviderError>> {
            let run = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                let value = HeaderValue::from_str(&format!("issued-{run}")).unwrap();
                Ok(["x-a", "x-b", "x-replaced"]
                    .into_iter()
                    .map(|name| (HeaderName::from_static(name), value.clone()))
                    .collect())
            })
        }
    }

    /// Later values replace earlier ones and list headers join without
    /// repeats, keeping a value made a list. Only the sources that survive
    /// composition run, each once; a list contribution with no item is not
    /// leased, and an empty list is not sent. A 401 on a request carrying no
    /// command value keeps its kind.
    #[tokio::test]
    async fn later_sources_replace_or_join_and_only_sent_values_run_and_are_leased() {
        let dir = tempfile::tempdir().unwrap();
        let replaced = dir.path().join("replaced");
        let empty = dir.path().join("empty");
        let blank = |count: &Path| {
            let text = format!("printf x >> {}; printf ' , '", quote(count));
            Value::Command(CommandValue::new(text, None, Role::Header))
        };
        let issuer = Arc::new(Counted::default());
        let issued = || Value::Issued(Arc::clone(&issuer) as Arc<dyn Authenticator>);
        let list = HeaderName::from_static("x-list");
        let mut headers = Headers::default();
        headers.insert(AUTHORIZATION, Value::Command(counting(&replaced)));
        headers.insert(HeaderName::from_static("x-a"), issued());
        headers.insert(HeaderName::from_static("x-b"), issued());
        headers.insert(HeaderName::from_static("x-replaced"), issued());
        headers.list(HeaderName::from_static("x-empty"), [blank(&empty)]);
        headers.insert(list.clone(), fixed("first"));
        headers.list(list.clone(), [blank(&empty), fixed("a")]);
        let mut later = Headers::default();
        later.insert(AUTHORIZATION, fixed("Bearer k"));
        later.insert(HeaderName::from_static("x-replaced"), fixed("fixed"));
        later.list(list.clone(), [fixed("b")]);
        headers.extend(later);
        headers.insert(list.clone(), fixed("c, a,, b"));
        let resolved = headers.resolve().await.unwrap();
        let map = resolved.map();
        assert_eq!(map[AUTHORIZATION], "Bearer k");
        assert_eq!(map["x-a"], "issued-1");
        assert_eq!(map["x-b"], "issued-1");
        assert_eq!(map["x-replaced"], "fixed");
        assert_eq!(map[&list], "first, a, b, c");
        assert!(!map.contains_key("x-empty"));
        assert_eq!(issuer.0.load(Ordering::SeqCst), 1);
        assert_eq!((runs(&replaced), runs(&empty)), (0, 2));
        let limited = ProviderErrorKind::RateLimited { retry_after: None };
        let error = ProviderError {
            kind: limited,
            message: "provider HTTP 401 error".into(),
        };
        assert_eq!(resolved.unauthorized(error).kind, limited);
    }

    #[tokio::test]
    async fn command_values_are_lazy_run_once_and_shared_by_clones() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let text = format!(
            "printf x >> {}; printf ' \\t  resolved-key  \\r\\n '",
            quote(&count)
        );
        let value = CommandValue::new(text, Some("Bearer "), Role::Credential);
        assert!(!count.exists(), "nothing runs before the first use");
        let shared: Vec<_> = (0..8).map(|_| value.clone()).collect();
        let resolved = futures_util::future::join_all(shared.iter().map(CommandValue::value)).await;
        for header in resolved {
            let header = header.unwrap();
            assert_eq!(header.to_str().unwrap(), "Bearer resolved-key");
            assert!(header.is_sensitive());
        }
        assert_eq!(std::fs::read(&count).unwrap(), b"x");
    }

    #[tokio::test]
    async fn command_failures_are_sanitized_uncached_and_stdin_is_closed() {
        let cases = [
            (
                Role::Credential,
                "printf private-stdout; printf private-stderr >&2; exit 7",
                "credential command exited unsuccessfully",
            ),
            (
                Role::Credential,
                "printf '\\377'",
                "credential command output was not valid UTF-8",
            ),
            (
                Role::Credential,
                "printf ' \\t\\r\\n '",
                "credential command output was empty",
            ),
            (
                Role::Credential,
                "printf 'private-stdout\\ninvalid-header'",
                "credential command output was not a valid header value",
            ),
            (
                Role::Credential,
                "head -c 65537 /dev/zero",
                "credential command output exceeded the size limit",
            ),
            (
                Role::Header,
                "if read value; then printf stdin-open; else exit 1; fi",
                "header command exited unsuccessfully",
            ),
        ];
        for (role, bad, expected) in cases {
            let dir = tempfile::tempdir().unwrap();
            let marker = quote(&dir.path().join("attempted"));
            let text = format!(
                "# private-command-text\nif test -e {marker}; then printf retry-key; else touch {marker}; {bad}; fi"
            );
            let value = CommandValue::new(text, None, role);
            let error = value.value().await.unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::Authentication);
            assert_eq!(error.message, expected);
            let rendered = format!("{error:?} {error} {value:?}");
            for forbidden in ["private-command-text", "private-std", "retry-key"] {
                assert!(!rendered.contains(forbidden));
            }
            // A failure is not cached: the retry runs the command again and succeeds.
            assert_eq!(value.value().await.unwrap().to_str().unwrap(), "retry-key");
        }
    }

    /// Concurrent 401s from one accepted generation discard it once and fetch
    /// its successor once; a late 401 from the old generation keeps the successor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unauthorized_discards_only_the_generation_it_carried_and_refetches_once() {
        async fn send(value: CommandValue) -> (HeaderValue, Resolved) {
            let mut headers = Headers::default();
            headers.insert(AUTHORIZATION, Value::Command(value));
            let resolved = headers.resolve().await.unwrap();
            (resolved.map()[AUTHORIZATION].clone(), resolved)
        }
        fn refused() -> ProviderError {
            ProviderError {
                kind: ProviderErrorKind::Authentication,
                message: "provider HTTP 401 error".into(),
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let value = counting(&count);
        let run = async {
            let sent = futures_util::future::join_all((0..8).map(|_| send(value.clone()))).await;
            assert!(sent.iter().all(|(header, _)| header == "key-1"));
            sent[0].1.served();
            let renewals = sent.into_iter().map(|(_, leases)| {
                let value = value.clone();
                tokio::spawn(async move {
                    let kind = leases.unauthorized(refused()).kind;
                    (kind, send(value).await.0, leases)
                })
            });
            let mut stale = Vec::new();
            for renewal in futures_util::future::join_all(renewals).await {
                let (kind, header, leases) = renewal.unwrap();
                assert_eq!(kind, ProviderErrorKind::CredentialExpired);
                assert_eq!(header, "key-2");
                stale.push(leases);
            }
            assert_eq!(runs(&count), 2);
            let late = stale[0].unauthorized(refused());
            assert_eq!(late.kind, ProviderErrorKind::CredentialExpired);
            assert_eq!(send(value.clone()).await.0, "key-2");
            assert_eq!(runs(&count), 2);
        };
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_resolution_kills_child_and_allows_retry() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let quoted = quote(&marker);
        let text = format!(
            "if test -e {quoted}; then printf retry-key; else printf '%s' $$ > {quoted}; exec sleep 30; fi"
        );
        let value = CommandValue::new(text, None, Role::Credential);
        let run = async {
            let pending = value.value();
            let pid = tokio::select! {
                _ = pending => panic!("command should still be running"),
                pid = async {
                    loop {
                        if let Ok(text) = tokio::fs::read_to_string(&marker).await
                            && let Ok(pid) = text.parse::<u32>()
                        {
                            break pid;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                } => pid,
            };
            // The select dropped the future: kill_on_drop terminates the child.
            let dead = async {
                while Path::new(&format!("/proc/{pid}")).exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            tokio::time::timeout(Duration::from_secs(5), dead)
                .await
                .expect("child was killed");
            assert_eq!(value.value().await.unwrap().to_str().unwrap(), "retry-key");
        };
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap();
    }
}
