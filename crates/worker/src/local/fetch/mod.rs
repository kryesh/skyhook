//! One HTTP exchange from this machine. Redirects are deliberately followed here,
//! never by reqwest, so each hop's method, headers and body are rebuilt.
mod client;
mod redirects;
mod request;
mod response;

use std::time::Instant;

use reqwest::{Method, header::HeaderMap};

use crate::diagnostic::{Failure, Fault};
use crate::op::{
    Events, Fetch, FetchEvent, FetchOutput, FetchStep, Fetched, HttpRequestUrl, Redirect,
};
use redirects::{FollowableRedirectStatus, redirect_headers};
use request::{Upload, prepare_body, transport_error};

fn os(error: std::io::Error) -> Fault {
    error.into()
}

impl crate::op::Operation for Fetch {
    type Event = FetchEvent;
    type End = Fetched;
    type Error = Failure;

    fn input(&mut self) -> Option<&mut crate::op::Input> {
        match &mut self.body {
            Some(crate::op::FetchBody::Bytes { input, .. }) => Some(input),
            Some(crate::op::FetchBody::File { .. }) | None => None,
        }
    }

    async fn perform(
        self,
        local: &crate::local::Local,
        events: Events<FetchEvent>,
    ) -> Result<Fetched, Failure> {
        let started = Instant::now();
        let Some(timeout) = self.timeout else {
            return exchange(self, local, &events, started).await;
        };
        match tokio::time::timeout(timeout, exchange(self, local, &events, started)).await {
            Ok(result) => result,
            Err(_) => Err(Fault::Timeout(timeout).into()),
        }
    }
}

async fn step(events: &Events<FetchEvent>, step: FetchStep) {
    // A closed receiver means the fetch is being dropped.
    let _ = events.send(FetchEvent::Step(step)).await;
}

/// A redirect consumes the complete request state, so method/header/body rewrites
/// cannot be applied independently or reused after the next hop is formed.
struct RequestState {
    url: HttpRequestUrl,
    method: Method,
    headers: HeaderMap,
    body: Option<Upload>,
}

impl RequestState {
    fn transition(
        mut self,
        status: FollowableRedirectStatus,
        next: HttpRequestUrl,
    ) -> (Self, Redirect) {
        let hop = Redirect {
            status: status.get(),
            location: next.as_str().into(),
            method: self.method.to_string(),
        };
        let drop_body = status.rewrites_to_get(&self.method);
        redirect_headers(&mut self.headers, &self.url, &next, drop_body);
        if drop_body {
            self.method = Method::GET;
            self.body = None;
        }
        self.url = next;
        (self, hop)
    }
}

async fn exchange(
    fetch: Fetch,
    local: &crate::local::Local,
    events: &Events<FetchEvent>,
    started: Instant,
) -> Result<Fetched, Failure> {
    if let FetchOutput::Save { path, overwrite } = &fetch.output {
        response::check_destination(path, *overwrite, events).await?;
    }
    step(events, FetchStep::Client).await;
    let client = client::client(&fetch.client).await?;
    step(events, FetchStep::Body).await;
    let body = prepare_body(fetch.body, events).await?;
    let mut state = RequestState {
        url: fetch.url,
        method: fetch.method.0,
        headers: fetch.headers.0,
        body,
    };
    let mut redirects = Vec::new();
    let response = loop {
        step(
            events,
            FetchStep::Send {
                method: state.method.to_string(),
                origin: state.url.origin(),
            },
        )
        .await;
        let request = client
            .request(state.method.clone(), state.url.url().clone())
            .headers(state.headers.clone());
        let request = request::apply_body(request, state.body.as_ref(), &state.headers);
        let response = request.send().await.map_err(transport_error)?;
        let Some(status) = FollowableRedirectStatus::new(response.status().as_u16()) else {
            break response;
        };
        if !fetch.redirects.follows(&state.method) {
            break response;
        }
        step(events, FetchStep::Redirect).await;
        let Some(location) = response.headers().get("location") else {
            break response;
        };
        if redirects.len() as u64 >= fetch.max_redirects {
            return Err(Fault::TooManyRedirects.into());
        }
        let location = location.to_str().map_err(|_| Fault::InvalidLocation)?;
        let next = state.url.join(location)?;
        if state.url.url().scheme() == "https" && next.url().scheme() == "http" {
            return Err(Fault::Downgrade.into());
        }
        let (next, hop) = state.transition(status, next);
        state = next;
        redirects.push(hop);
    };
    let status = response.status().as_u16();
    let headers = (fetch.include_headers).then(|| collect_headers(response.headers()));
    let content_type = (response.headers().get("content-type"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let limits = (fetch.max_bytes, &state.method);
    let (size, spilled) =
        response::read_body(response, fetch.output, limits, local, events).await?;
    Ok(Fetched {
        status,
        headers,
        redirects,
        content_type,
        url: state.url,
        size,
        spilled,
        duration: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
    })
}

fn collect_headers(headers: &HeaderMap) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut result = std::collections::BTreeMap::<String, Vec<String>>::new();
    for (name, value) in headers {
        result
            .entry(name.to_string())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    result
}
