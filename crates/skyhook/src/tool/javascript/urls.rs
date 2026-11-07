//! The host side of `URL` and `URLSearchParams`: WHATWG URL parsing, component
//! access and `application/x-www-form-urlencoded` coding, on the `url` crate.

use url::{Url, form_urlencoded, quirks};

use super::web::{array, string};

/// What the runtime reads or sets on a URL, parsed from its JavaScript name.
#[derive(Clone, Copy)]
enum Component {
    Href,
    Origin,
    Protocol,
    Username,
    Password,
    Host,
    Hostname,
    Port,
    Pathname,
    Search,
    Hash,
}

impl Component {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "href" => Self::Href,
            "origin" => Self::Origin,
            "protocol" => Self::Protocol,
            "username" => Self::Username,
            "password" => Self::Password,
            "host" => Self::Host,
            "hostname" => Self::Hostname,
            "port" => Self::Port,
            "pathname" => Self::Pathname,
            "search" => Self::Search,
            "hash" => Self::Hash,
            _ => return None,
        })
    }

    fn get(self, url: &Url) -> String {
        match self {
            Self::Href => quirks::href(url).to_owned(),
            Self::Origin => quirks::origin(url),
            Self::Protocol => quirks::protocol(url).to_owned(),
            Self::Username => quirks::username(url).to_owned(),
            Self::Password => quirks::password(url).to_owned(),
            Self::Host => quirks::host(url).to_owned(),
            Self::Hostname => quirks::hostname(url).to_owned(),
            Self::Port => quirks::port(url).to_owned(),
            Self::Pathname => quirks::pathname(url).to_owned(),
            Self::Search => quirks::search(url).to_owned(),
            Self::Hash => quirks::hash(url).to_owned(),
        }
    }

    /// Whether the URL took the value: only `href` fails; the other setters ignore
    /// what they cannot apply, as the standard has them.
    fn set(self, url: &mut Url, value: &str) -> bool {
        match self {
            Self::Href => return quirks::set_href(url, value).is_ok(),
            Self::Origin => {}
            Self::Protocol => _ = quirks::set_protocol(url, value),
            Self::Username => _ = quirks::set_username(url, value),
            Self::Password => _ = quirks::set_password(url, value),
            Self::Host => _ = quirks::set_host(url, value),
            Self::Hostname => _ = quirks::set_hostname(url, value),
            Self::Port => _ = quirks::set_port(url, value),
            Self::Pathname => quirks::set_pathname(url, value),
            Self::Search => quirks::set_search(url, value),
            Self::Hash => quirks::set_hash(url, value),
        }
        true
    }
}

fn text(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<'_, v8::Value>) -> String {
    value.to_rust_string_lossy(scope)
}

/// `(input, base?)`: the href, or null when either does not parse.
pub(super) fn parse(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    let input = text(scope, arguments.get(0));
    let base = arguments.get(1);
    let base = match base.is_undefined() {
        true => None,
        false => match Url::parse(&text(scope, base)) {
            Ok(base) => Some(base),
            Err(_) => return returned.set_null(),
        },
    };
    match Url::options().base_url(base.as_ref()).parse(&input) {
        Ok(url) => set_string(scope, returned, url.as_str()),
        Err(_) => returned.set_null(),
    }
}

/// `(href, name)`: the component's value.
pub(super) fn get(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    returned: v8::ReturnValue<v8::Value>,
) {
    let name = text(scope, arguments.get(1));
    if let (Ok(url), Some(component)) = (
        Url::parse(&text(scope, arguments.get(0))),
        Component::parse(&name),
    ) {
        set_string(scope, returned, &component.get(&url));
    }
}

/// `(href, name, value)`: the href after setting the component, or null when
/// `href` was set to what does not parse.
pub(super) fn set(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    let name = text(scope, arguments.get(1));
    let value = text(scope, arguments.get(2));
    let (Ok(mut url), Some(component)) = (
        Url::parse(&text(scope, arguments.get(0))),
        Component::parse(&name),
    ) else {
        return;
    };
    match component.set(&mut url, &value) {
        true => set_string(scope, returned, url.as_str()),
        false => returned.set_null(),
    }
}

/// `(query)`: its name-value pairs, decoded.
pub(super) fn form_parse(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    let query = text(scope, arguments.get(0));
    let pairs = form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| array(scope, [string(scope, &name), string(scope, &value)]));
    if let Some(pairs) = array(scope, pairs) {
        returned.set(pairs);
    }
}

/// `(pairs)`: the pairs, encoded as a query.
pub(super) fn form_serialize(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    returned: v8::ReturnValue<v8::Value>,
) {
    let Ok(pairs) = arguments.get(0).try_cast::<v8::Array>() else {
        return;
    };
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for index in 0..pairs.length() {
        let Some(pair) = pairs
            .get_index(scope, index)
            .and_then(|pair| pair.try_cast::<v8::Array>().ok())
        else {
            return;
        };
        let (Some(name), Some(value)) = (pair.get_index(scope, 0), pair.get_index(scope, 1)) else {
            return;
        };
        let (name, value) = (text(scope, name), text(scope, value));
        serializer.append_pair(&name, &value);
    }
    set_string(scope, returned, &serializer.finish());
}

fn set_string(scope: &v8::PinScope<'_, '_>, mut returned: v8::ReturnValue<v8::Value>, text: &str) {
    if let Some(text) = string(scope, text) {
        returned.set(text);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::tool::javascript::tests::evaluate_wrapper;

    /// `URL` and `URLSearchParams` follow the WHATWG URL standard.
    #[test]
    fn urls_follow_the_whatwg_standard() {
        let source = r#"
const resolved = new URL("../b/c?x=1#h", "https://example.com/a/d/e");
let invalid;
try { new URL("not a url"); } catch (error) { invalid = error.constructor.name; }
const url = new URL("https://example.com/path");
url.pathname = "/other path";
url.port = "8080";
url.hash = "top";
url.searchParams.append("q", "a b&c");
const searched = new URL("https://example.com/?a=1&b=2");
searched.search = "?z=9";
const params = new URLSearchParams([["b", "2"], ["a", "1 +"], ["b", "1"]]);
params.sort();
return {resolved: resolved.href, origin: resolved.origin, invalid, parsed: URL.parse("not a url"),
  canParse: [URL.canParse("/x", "https://example.com"), URL.canParse("/x")], set: url.href,
  live: [searched.searchParams.get("z"), searched.searchParams.has("a")], sorted: params.toString(),
  entries: [...params], keys: [...new URLSearchParams("?x=1&y=2").keys()],
  record: new URLSearchParams({k: "v w"}).toString(), json: JSON.stringify({url: resolved})};
"#;
        let expected = json!({"resolved": "https://example.com/a/b/c?x=1#h", "origin": "https://example.com",
                       "invalid": "TypeError", "parsed": null, "canParse": [true, false],
                       "set": "https://example.com:8080/other%20path?q=a+b%26c#top", "live": ["9", false],
                       "sorted": "a=1+%2B&b=2&b=1", "entries": [["a", "1 +"], ["b", "2"], ["b", "1"]],
                       "keys": ["x", "y"], "record": "k=v+w",
                       "json": r#"{"url":"https://example.com/a/b/c?x=1#h"}"#});
        assert_eq!(evaluate_wrapper(source), expected);
        // Arguments convert as WebIDL has them: a missing required one throws before
        // anything changes, while an explicit undefined is a value.
        let source = r#"
const thrown = make => { try { make(); return null; } catch (error) { return error.name; } };
const params = new URLSearchParams("a=1");
const missing = [() => params.append("a"), () => params.set("a"), () => params.get(), () => params.getAll(),
  () => params.has(), () => params.delete()].map(thrown);
const unchanged = params.toString();
params.append("u", undefined);
return {missing, unchanged, explicit: params.get("u"),
  inits: [() => new URLSearchParams(["ab"]), () => new URLSearchParams([["a"]]),
    () => new URLSearchParams({[Symbol.iterator]: 1})].map(thrown),
  record: new URLSearchParams({[Symbol.iterator]: null, k: "v"}).toString(),
  copied: new URLSearchParams(params).toString(),
  reused: new URLSearchParams((function* () { const pair = ["a", "1"]; yield pair; pair[0] = "b"; pair[1] = "2"; yield pair; })()).toString(),
  element: new URLSearchParams([(function* () { const item = {toString: () => "a"}; yield item; item.toString = () => "1"; yield item; })()]).toString(),
  parse: [thrown(() => URL.parse(Symbol())), URL.parse("not a url"), URL.parse("/x", "https://example.com").href],
  required: [() => URL.parse(), () => URL.canParse(), () => new URL(), () => atob(), () => btoa()].map(thrown)};
"#;
        let expected = json!({"missing": ["TypeError", "TypeError", "TypeError", "TypeError", "TypeError", "TypeError"], "unchanged": "a=1", "explicit": "undefined",
            "inits": ["TypeError", "TypeError", "TypeError"], "record": "k=v", "copied": "a=1&u=undefined", "reused": "a=1&b=2", "element": "a=1",
            "parse": ["TypeError", null, "https://example.com/x"],
            "required": ["TypeError", "TypeError", "TypeError", "TypeError", "TypeError"]});
        assert_eq!(evaluate_wrapper(source), expected);
    }
}
