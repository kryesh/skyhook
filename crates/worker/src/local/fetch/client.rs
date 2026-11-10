//! Fetch's HTTP clients, each shared by every call with the same client settings,
//! and opt-in insecure TLS.

use std::sync::{Mutex, MutexGuard, PoisonError};

use reqwest::Client;

use crate::diagnostic::Fault;
use crate::op::ClientIdentity;

/// How many clients of distinct settings are kept before the cache starts over.
const CLIENTS: usize = 8;

static CLIENTS_BUILT: Mutex<Vec<(ClientIdentity, Client)>> = Mutex::new(Vec::new());

fn clients() -> MutexGuard<'static, Vec<(ClientIdentity, Client)>> {
    CLIENTS_BUILT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The client for `identity`, with the connections it keeps open. A miss builds one
/// off the async workers, since the first build reads this machine's trusted roots;
/// calls that miss together each build one, and the first built is kept.
pub(super) async fn client(identity: &ClientIdentity) -> Result<Client, Fault> {
    let built = |clients: &[(ClientIdentity, Client)]| {
        let found = clients.iter().find(|(built, _)| built == identity);
        found.map(|(_, client)| client.clone())
    };
    if let Some(client) = built(&clients()) {
        return Ok(client);
    }
    let building = identity.clone();
    let client = skyhook_base::blocking(move || build(&building)).await?;
    let mut clients = clients();
    if let Some(kept) = built(&clients) {
        return Ok(kept);
    }
    if clients.len() == CLIENTS {
        clients.clear();
    }
    clients.push((identity.clone(), client.clone()));
    Ok(client)
}

fn build(identity: &ClientIdentity) -> Result<Client, Fault> {
    // Insecure verifies nothing, so it needs no trusted roots.
    let builder = match identity.insecure {
        true => Client::builder().danger_accept_invalid_certs(true),
        false => crate::tls::client_builder(),
    };
    let mut builder = builder
        .user_agent(concat!("Skyhook/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        // The fetch's outer deadline is the only total limit: an equal client
        // timeout would race it and could report the timeout without its limit.
        .connect_timeout(identity.connect_timeout);
    // Proxy schemes and URL credentials intentionally differ from request URL admission.
    if let Some(proxy) = &identity.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| Fault::InvalidProxy)?);
    }
    builder.build().map_err(|_| Fault::Client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::bounded;

    /// Insecure TLS is opt-in per client: strict clients before and after an
    /// insecure one still refuse a self-signed certificate.
    #[tokio::test]
    async fn insecure_is_opt_in_and_does_not_leak_between_clients() {
        let (url, mut server, _) = crate::tls::tests::self_signed_server(5).await;
        for insecure in [false, false, true, false, false] {
            let identity = ClientIdentity {
                connect_timeout: std::time::Duration::from_secs(20),
                proxy: None,
                insecure,
            };
            let client = client(&identity).await.unwrap();
            let sent = bounded(client.get(&url).send()).await;
            match sent {
                Ok(response) if insecure => {
                    assert_eq!(bounded(response.text()).await.unwrap(), "tls-ok");
                }
                Err(error) if !insecure => {
                    std::assert_matches!(super::super::request::transport_error(error), Fault::Tls)
                }
                sent => panic!("{insecure}: {sent:?}"),
            }
        }
        let (rejected, requests) = bounded(&mut server.0).await.unwrap();
        assert_eq!((rejected, requests.len()), (4, 1), "{requests:#?}");
        assert!(requests[0].starts_with("GET /tls HTTP/1.1\r\n"));
    }
}
