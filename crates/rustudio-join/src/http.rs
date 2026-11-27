//! The one place this crate speaks HTTPS: signing in, and the join-game calls.
//!
//! Ported from rove's `UreqTransport` + `RobloxSession`, with three changes that
//! matter for a shipped client:
//!
//! 1. **The secrets never leave.** The cookie is held in a [`Secret`] and
//!    formatted as `[REDACTED]`; the anti-CSRF token cache is keyed by host.
//!    Nothing here writes a credential into a log, an error, or a `Debug`.
//! 2. **A non-2xx is data, not an exception.** The agent is built with
//!    `http_status_as_error(false)`, which is the idiom the rest of this tree
//!    already uses (`rustudio-account/src/http.rs`): the anti-CSRF retry needs
//!    the status and the headers of a 403, not a wrapped error.
//! 3. **TLS is ureq's default, on purpose.** The same hosts are signed in to by
//!    `rustudio-account` with an unconfigured agent, and that is the trust this
//!    tree already ships: rustls with the compiled-in Mozilla roots, which is
//!    also what works on Android, where the platform store is not readable. A
//!    second, hand-built trust decision here would be one more thing to keep
//!    correct, and the WebTransport side's pinning is separate and exact.

use std::collections::HashMap;

use serde_json::Value;

use crate::allocation::{
    valid_csrf_token, AllocationError, HttpClient, Reply, ALLOWED_HOSTS, MAX_RESPONSE,
};

/// What a join request says it is.
///
/// `gamejoin` validates the `User-Agent`: *any* version it does not recognise
/// is refused with status 12, which is why rove's client fetches the live one
/// (`crate::client_version`). The `rbx-user-agent` header names the kind of
/// client, which is the other half of the admission test — a web identity is
/// refused and a native one admitted (H644b/Roblox-Public-Web-Play-Join
/// measured that on the live endpoint).
const NATIVE_IDENTITY: &str = "Roblox/Darwin";

const MAX_COOKIE: usize = 32 * 1024;
const MAX_REQUEST: usize = 1024 * 1024;
const TIMEOUT_SECS: u64 = 20;

/// The same headers on either request builder.
///
/// ureq 3's `RequestBuilder` is generic over whether a body has been attached,
/// so this cannot be a closure: a closure infers `B` from its first use and
/// then refuses the other one.
fn with_headers<B>(
    mut request: ureq::RequestBuilder<B>,
    headers: &[(String, String)],
) -> ureq::RequestBuilder<B> {
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    request
}

/// A credential whose value is never printed.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// An authenticated Roblox account session: a cookie plus the anti-CSRF tokens
/// hosts have issued for it.
pub struct AccountSession {
    cookie: Secret,
    csrf: HashMap<String, Secret>,
    agent: ureq::Agent,
    /// Set when the last reply carried `rblx-challenge-id`: the account needs a
    /// human challenge before it can join, which this client does not solve.
    challenged: bool,
}

impl std::fmt::Debug for AccountSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSession")
            .field("cookie", &"[REDACTED]")
            .field("csrf_hosts", &self.csrf.len())
            .field("challenged", &self.challenged)
            .finish_non_exhaustive()
    }
}

impl AccountSession {
    /// A session for a pasted `.ROBLOSECURITY` value.
    ///
    /// The value is validated rather than trusted: it goes into a header, and a
    /// value with a newline in it would be a request split.
    pub fn new(raw: &str) -> Result<Self, AllocationError> {
        let mut cookie = raw.trim();
        if let Some(value) = cookie.strip_prefix(".ROBLOSECURITY=") {
            cookie = value.trim_end_matches(';');
        }
        if cookie.len() > MAX_COOKIE
            || cookie.is_empty()
            || !cookie.bytes().all(|b| (33..=126).contains(&b) && b != b';')
        {
            return Err(AllocationError::new(
                "Cookie must contain only the .ROBLOSECURITY value",
            ));
        }
        let config = ureq::Agent::config_builder()
            // A 403 is how roblox.com hands out a CSRF token; it has to be
            // readable, not thrown.
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(TIMEOUT_SECS)))
            // No redirects: the Cookie header must never follow a 3xx to a host
            // that is not on the allowlist above.
            .max_redirects(0)
            // The live client version, not a pinned constant: the launcher
            // refuses an unknown agent (see `crate::client_version`).
            .user_agent(crate::client_version::user_agent())
            .build();
        let agent: ureq::Agent = config.into();
        Ok(Self {
            cookie: Secret::new(cookie),
            csrf: HashMap::new(),
            agent,
            challenged: false,
        })
    }

    /// Whether the account needs a challenge. Nothing else is trusted from a
    /// challenged reply.
    pub fn challenged(&self) -> bool {
        self.challenged
    }

    /// One request, with the anti-CSRF retry roblox.com requires: a 403 that
    /// carries `x-csrf-token` is retried once with that token.
    fn request(
        &mut self,
        host: &str,
        path: &str,
        method: &str,
        payload: Option<&Value>,
    ) -> Result<Reply, AllocationError> {
        if !ALLOWED_HOSTS.contains(&host) || !valid_path(path) {
            return Err(AllocationError::new("Request destination is not allowed"));
        }
        if method != "GET" && method != "POST" {
            return Err(AllocationError::new("Unsupported method"));
        }
        let body = payload
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| AllocationError::new("Request payload could not be encoded"))?;
        if body.as_ref().is_some_and(|body| body.len() > MAX_REQUEST) {
            return Err(AllocationError::new(
                "Request payload exceeded the size limit",
            ));
        }
        for attempt in 0..2u8 {
            let mut headers: Vec<(String, String)> = vec![
                (
                    "Cookie".into(),
                    format!(".ROBLOSECURITY={}", self.cookie.expose()),
                ),
                ("Accept".into(), "application/json".into()),
                // **The launcher admits native clients and refuses web ones.**
                // `gamejoin` answers HTTP 200 with status 12, "You are not
                // allowed to play this game", to a request that identifies as
                // a browser — and status 2 for the same place and cookie once
                // it identifies as a native client (the research in
                // H644b/Roblox-Public-Web-Play-Join measured exactly this,
                // including that the HTTP `User-Agent` alone changes nothing:
                // it is this header).
                ("rbx-user-agent".into(), NATIVE_IDENTITY.into()),
                // Present on real client traffic; some endpoints key off them.
                ("Accept-Language".into(), "en-US,en;q=0.9".into()),
                ("Roblox-Browser-Asset-Request".into(), "false".into()),
                ("Origin".into(), "https://www.roblox.com".into()),
                ("Referer".into(), "https://www.roblox.com/".into()),
            ];
            if body.is_some() {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
            if let Some(token) = self.csrf.get(host) {
                headers.push(("X-CSRF-TOKEN".into(), token.expose().to_owned()));
            }
            let url = format!("https://{host}{path}");
            // GET and POST are different request types in ureq 3
            // (`RequestBuilder<WithoutBody>` / `<WithBody>`), so each arm builds
            // and sends its own. A POST always carries a body — an empty object
            // when the caller passed none — because the endpoints in play read
            // the content type.
            let result = if method == "GET" {
                with_headers(self.agent.get(&url), &headers).call()
            } else {
                let bytes = body.clone().unwrap_or_else(|| b"{}".to_vec());
                with_headers(self.agent.post(&url), &headers).send(bytes.as_slice())
            };
            let mut response = result.map_err(|_| AllocationError::new("HTTPS request failed"))?;
            let status = response.status().as_u16();
            let csrf = header(&response, "x-csrf-token");
            let challenge = header(&response, "rblx-challenge-id").is_some();
            let content_type = header(&response, "content-type").unwrap_or_default();
            let body_bytes = response
                .body_mut()
                .read_to_vec()
                .map_err(|_| AllocationError::new("HTTPS response could not be read"))?;
            if body_bytes.len() > MAX_RESPONSE {
                return Err(AllocationError::new("Response exceeded the size limit"));
            }
            if status == 403 && attempt == 0 && !challenge {
                if let Some(token) = csrf.as_ref().filter(|value| valid_csrf_token(value)) {
                    self.csrf.insert(host.to_owned(), Secret::new(token.clone()));
                    continue;
                }
            }
            self.challenged = challenge;
            return Ok(Reply {
                http_status: status,
                challenge_present: challenge,
                content_type,
                data: serde_json::from_slice(&body_bytes).ok(),
            });
        }
        Err(AllocationError::new("Request retry was exhausted"))
    }

    /// `GET users.roblox.com/v1/users/authenticated`: whose cookie this is.
    ///
    /// This is the sign-in check. A join needs the account's user id, and the
    /// allocation is refused unless it agrees, so a wrong cookie fails here
    /// with a clear message instead of at the join.
    pub fn authenticated_user(&mut self) -> Result<(u64, String, String), AllocationError> {
        let reply = self.request("users.roblox.com", "/v1/users/authenticated", "GET", None)?;
        if reply.challenge_present {
            return Err(AllocationError::new(
                "The account needs a challenge before it can be used",
            ));
        }
        if reply.http_status != 200 {
            return Err(AllocationError::new("The session was refused"));
        }
        let data = reply
            .data
            .as_ref()
            .and_then(Value::as_object)
            .ok_or(AllocationError::new("The session reply was not an account"))?;
        let id = data
            .get("id")
            .and_then(Value::as_u64)
            .ok_or(AllocationError::new("The session reply had no user id"))?;
        let name = data
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let display = data
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or(&name)
            .to_owned();
        Ok((id, name, display))
    }
}

/// The `HttpClient` the allocation path uses, with the anti-CSRF handling
/// inside it.
impl HttpClient for AccountSession {
    fn send(
        &mut self,
        host: &str,
        path: &str,
        method: &str,
        payload: Option<&Value>,
    ) -> Result<Reply, AllocationError> {
        self.request(host, path, method, payload)
    }
}

/// A header as a bounded string: a hostile server cannot make the client hold
/// an unbounded one, and the JSON content type is all the caller reads.
fn header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(256).collect())
}

/// A path this client will request: absolute, ASCII, no fragment, and nothing
/// the URL parser would normalize into a different destination.
fn valid_path(path: &str) -> bool {
    if !path.starts_with('/')
        || path.contains('\\')
        || path.contains('#')
        || path.bytes().any(|b| b < 32 || b == 127 || b == b' ')
    {
        return false;
    }
    if path.contains("..") || path.contains("//") {
        return false;
    }
    path.bytes().all(|b| b.is_ascii())
}
