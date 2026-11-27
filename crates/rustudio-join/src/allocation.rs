//! The join-game allocation: what the server issues, and what is parsed out of
//! it.
//!
//! This module is deliberately split into two halves:
//!
//! - **Parsing** ([`parse_allocation`] and the helpers under it) is pure. It
//!   takes the JSON a `POST gamejoin.roblox.com/v1/join-game` returned and
//!   yields an [`IssuedJoinAllocation`], or says exactly why it refused. Every
//!   branch is reachable from a recorded body, which is what the tests do.
//! - **Asking** ([`request_allocation`]) is the one part that needs a network:
//!   it speaks to the [`HttpClient`] it is given. A test passes a client that
//!   replays recorded responses, so the request shape (path, payload,
//!   anti-CSRF retry) is checked too.
//!
//! The fields that matter to the transport are the three the current gamejoin
//! shape moved around, and each has a fallback because the servers are not
//! uniform:
//!
//! | what | where it comes from |
//! |---|---|
//! | the address to dial | `UdmuxEndpoints[0].Address`, else `MachineAddress` |
//! | the port | `NetStackConfig.port`, else `NetStackPort`, else `ServerPort` |
//! | the certificate pins | `NetStackConfig.certHashes` (base64 SHA-256) |
//! | the routing token | `NetStackTokenValue`, or sixteen zero bytes when absent |
//!
//! Ported from `rove/rust-client/src/allocation.rs`, which is the reference for
//! the current wire shape. The differences are noted at each site.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use base64::Engine;
use serde_json::{json, Value};

/// A cookie, a ticket, a session id: something whose value is never printed.
#[derive(Clone, PartialEq, Eq)]
struct Secret(String);

impl Secret {
    fn expose(&self) -> &str {
        &self.0
    }

    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Why an allocation was refused, or a request could not be made.
///
/// A `&'static str` like rove's, because every message is a fixed decision the
/// caller acts on; the interesting part of a failure is which stage it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationError(&'static str);

impl AllocationError {
    pub(crate) fn new(message: &'static str) -> Self {
        Self(message)
    }

    /// The reason, for a log or a diagnostics overlay. Never contains a value
    /// from the response.
    pub fn message(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for AllocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for AllocationError {}

/// Response and token bounds shared with the live client. Unused when the crate
/// is built without `live`, which is the build the offline tests run in.
#[cfg_attr(not(feature = "live"), allow(dead_code))]
pub(crate) const MAX_RESPONSE: usize = 2 * 1024 * 1024;
#[cfg_attr(not(feature = "live"), allow(dead_code))]
const MAX_CSRF_TOKEN: usize = 8 * 1024;
const MAX_CERT_PINS: usize = 16;

/// The hosts this crate will ever address. A name not on this list is refused
/// before any request is built, so a response cannot redirect the join.
pub const ALLOWED_HOSTS: &[&str] = &[
    "assetdelivery.roblox.com",
    "users.roblox.com",
    "games.roblox.com",
    "gamejoin.roblox.com",
    "apis.roblox.com",
    "friends.roblox.com",
    "presence.roblox.com",
    "thumbnails.roblox.com",
    "avatar.roblox.com",
    "inventory.roblox.com",
    "privatemessages.roblox.com",
    "trades.roblox.com",
    "groups.roblox.com",
    "catalog.roblox.com",
    "economy.roblox.com",
];

/// One reply, as the parser sees it. No socket types: this is what a recorded
/// exchange produces and what the live client fills in.
#[derive(Clone, Debug)]
pub struct Reply {
    pub http_status: u16,
    /// `rblx-challenge-id` was present: the account needs a challenge, not a
    /// join. Nothing about the body is trusted.
    pub challenge_present: bool,
    pub content_type: String,
    pub data: Option<Value>,
}

/// What the caller of [`request_allocation`] must be able to do: make one
/// HTTPS request and get one reply.
///
/// The live implementation is behind the `live` feature; tests implement it to
/// replay a recording. Keeping it this narrow is what makes the request shape
/// checkable without a network.
pub trait HttpClient {
    fn send(
        &mut self,
        host: &str,
        path: &str,
        method: &str,
        payload: Option<&Value>,
    ) -> Result<Reply, AllocationError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// A public place the account owns or selected, played as itself.
    AltOwnedPublicPlace,
    /// A specific public server (`GameId` is the job id).
    UserSelectedPlace,
    /// A private server, which needs an invite credential.
    UserSelectedPrivateServer,
}

#[derive(Clone, Debug)]
pub struct JoinTarget {
    pub place_id: u64,
    pub is_playable: bool,
    pub selection: Selection,
    /// The job id for `UserSelectedPlace`; `None` for the matchmaker.
    pub job_id: Option<String>,
}

impl JoinTarget {
    /// The matchmaker's choice for a place: the server decides which server.
    pub fn place(place_id: u64) -> Self {
        Self {
            place_id,
            is_playable: true,
            selection: Selection::UserSelectedPlace,
            job_id: None,
        }
    }

    /// A specific server of a place.
    pub fn server(place_id: u64, job_id: impl Into<String>) -> Self {
        Self {
            place_id,
            is_playable: true,
            selection: Selection::UserSelectedPlace,
            job_id: Some(job_id.into()),
        }
    }
}

#[derive(Clone)]
pub struct PrivateInvite {
    place_id: u64,
    owner_user_id: u64,
    private_server_id: u64,
    credential: Secret,
    credential_kind: InviteKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InviteKind {
    LinkCode,
    OwnedAccessCode,
}

impl PrivateInvite {
    pub fn new(
        place_id: u64,
        owner_user_id: u64,
        private_server_id: u64,
        link_code: &str,
    ) -> Result<Self, AllocationError> {
        if link_code.is_empty()
            || link_code.len() > 4096
            || link_code.bytes().any(|b| b <= 32 || b == 127)
        {
            return Err(AllocationError::new("Invalid private invite"));
        }
        Ok(Self {
            place_id,
            owner_user_id,
            private_server_id,
            credential: Secret::new(link_code),
            credential_kind: InviteKind::LinkCode,
        })
    }

    pub fn new_owned_access_code(
        place_id: u64,
        owner_user_id: u64,
        private_server_id: u64,
        access_code: &str,
    ) -> Result<Self, AllocationError> {
        let mut invite = Self::new(place_id, owner_user_id, private_server_id, access_code)?;
        invite.credential_kind = InviteKind::OwnedAccessCode;
        Ok(invite)
    }
}

impl fmt::Debug for PrivateInvite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateInvite")
            .field("place_id", &self.place_id)
            .field("owner_user_id", &self.owner_user_id)
            .field("private_server_id", &self.private_server_id)
            .field("credential", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SessionTicketMetadata {
    client_ticket: Option<Secret>,
    join_ticket: Option<Secret>,
}

impl fmt::Debug for SessionTicketMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionTicketMetadata")
            .field(
                "client_ticket",
                &self.client_ticket.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "join_ticket",
                &self.join_ticket.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl SessionTicketMetadata {
    pub fn client_ticket(&self) -> Option<&str> {
        self.client_ticket.as_ref().map(Secret::expose)
    }

    pub fn join_ticket(&self) -> Option<&str> {
        self.join_ticket.as_ref().map(Secret::expose)
    }
}

/// What the server issued, parsed and checked against the request.
///
/// The secrets — the session id, the routing token, the tickets — are private
/// and absent from `Debug`. What is public is what a caller needs to dial and
/// to display, and what is validated is what a caller must be able to trust
/// before dialling.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedJoinAllocation {
    pub place_id: u64,
    pub user_id: u64,
    pub user_name: String,
    pub display_name: String,
    pub has_verified_badge: bool,
    /// Where to dial, if the response carried a usable address and port.
    pub endpoint: Option<SocketAddr>,
    /// The machine the server says it is, which goes into the routing header.
    pub backend: Option<Ipv4Addr>,
    /// The SHA-256 of each certificate the server may present.
    pub certificate_sha256: Vec<[u8; 32]>,
    /// Local feature offer; this path enables no optional bits.
    pub capabilities: u64,
    pub game_id: Option<String>,
    pub private_server_owner_id: Option<u64>,
    pub tickets: SessionTicketMetadata,
    session_id: Secret,
    token: Option<[u8; 16]>,
    private_server_routing_id: Option<Secret>,
    /// The issued join document, kept because packets 138/144 are built from
    /// it rather than from anything reconstructed here.
    pub(crate) join_fields: Value,
    pub(crate) join_ticket_value: Option<Value>,
}

impl IssuedJoinAllocation {
    pub fn session_id(&self) -> &str {
        self.session_id.expose()
    }

    pub fn token(&self) -> Option<&[u8; 16]> {
        self.token.as_ref()
    }

    pub fn private_server_routing_id(&self) -> Option<&str> {
        self.private_server_routing_id.as_ref().map(Secret::expose)
    }

    /// Everything the transport needs, present and sane.
    pub fn is_transport_ready(&self) -> bool {
        self.endpoint.is_some()
            && self.backend.is_some()
            && self.token.is_some()
            && !self.certificate_sha256.is_empty()
    }

    /// What a diagnostics overlay shows: nothing secret, nothing long.
    pub fn summary(&self) -> String {
        match self.endpoint {
            Some(endpoint) => format!(
                "place {} user {} at {} ({} pin(s), token {})",
                self.place_id,
                self.user_id,
                endpoint,
                self.certificate_sha256.len(),
                if self.token.is_some() { "yes" } else { "no" }
            ),
            None => format!(
                "place {} user {} with no dialable endpoint ({} pin(s))",
                self.place_id,
                self.user_id,
                self.certificate_sha256.len()
            ),
        }
    }
}

impl fmt::Debug for IssuedJoinAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedJoinAllocation")
            .field("place_id", &self.place_id)
            .field("user_id", &"[VERIFIED]")
            .field("user_name", &"[REDACTED]")
            .field("display_name", &"[REDACTED]")
            .field("has_verified_badge", &self.has_verified_badge)
            .field("endpoint", &self.endpoint)
            .field("backend", &self.backend)
            .field("session_id", &"[REDACTED]")
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("certificate_pin_count", &self.certificate_sha256.len())
            .field("capabilities", &self.capabilities)
            .field("game_id", &self.game_id)
            .field(
                "private_server_routing_id",
                &self.private_server_routing_id.as_ref().map(|_| "[REDACTED]"),
            )
            .field("private_server_owner_id", &self.private_server_owner_id)
            .field("tickets", &self.tickets)
            .finish()
    }
}

/// The four things a join-game POST can mean.
#[derive(Debug)]
pub enum AllocationResult {
    /// Status 0 or 1 with no join script: the server is still placing us. Ask
    /// again — that is [`request_allocation_until_ready`]'s job, not the
    /// parser's.
    Pending { status: i64, http_status: u16 },
    /// The account needs a challenge before it can join.
    Challenge { http_status: u16 },
    Rejected {
        http_status: u16,
        status: Option<i64>,
        /// The launcher's own sentence, when it sent one. Roblox says *why*
        /// in the body — `"You are not allowed to play this game"`,
        /// `"Unable to join Game 211"`, `"Playtesters Only"` — and a status
        /// number alone cannot distinguish an admission refusal from a place
        /// that does not exist.
        message: Option<String>,
    },
    Ready(IssuedJoinAllocation),
}

/// Build the request for a target, honouring the three endpoints.
fn request_shape(
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    attempt_id: &str,
) -> Result<(&'static str, Value), AllocationError> {
    if !target.is_playable {
        return Err(AllocationError::new("Target is not verified playable"));
    }
    if attempt_id.len() != 36 || !canonical_uuid(attempt_id) {
        return Err(AllocationError::new("Invalid join attempt identifier"));
    }
    let private = target.selection == Selection::UserSelectedPrivateServer;
    if private && invite.map(|i| i.place_id) != Some(target.place_id) {
        return Err(AllocationError::new(
            "Private invite does not match selected place",
        ));
    }
    if !private && invite.is_some() {
        return Err(AllocationError::new("Unexpected private invite"));
    }
    let mut payload = json!({
        "placeId": target.place_id,
        "gameJoinAttemptId": attempt_id,
        "isTeleport": false
    });
    let mut path = if private {
        "/v1/join-private-game"
    } else {
        "/v1/join-game"
    };
    if private {
        let invite = invite.expect("validated above");
        match invite.credential_kind {
            InviteKind::LinkCode => payload["linkCode"] = json!(invite.credential.expose()),
            InviteKind::OwnedAccessCode => payload["accessCode"] = json!(invite.credential.expose()),
        }
    }
    if let Some(job) = target.job_id.as_deref() {
        if private || !canonical_uuid(job) {
            return Err(AllocationError::new("Invalid public server selection"));
        }
        payload["gameId"] = json!(job);
        path = "/v1/join-game-instance";
    }
    Ok((path, payload))
}

/// One join-game exchange: build the POST, read the reply, decide.
pub fn request_allocation(
    http: &mut dyn HttpClient,
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    attempt_id: &str,
    expected_user_id: u64,
) -> Result<AllocationResult, AllocationError> {
    let (path, payload) = request_shape(target, invite, attempt_id)?;
    let reply = http.send("gamejoin.roblox.com", path, "POST", Some(&payload))?;
    classify(reply, target, invite, expected_user_id)
}

/// The parser, with no I/O: what a reply means.
///
/// Split out of [`request_allocation`] so a recorded body can be replayed
/// straight into it, and so the pending/rejected/ready decisions are one
/// readable block.
pub fn classify(
    reply: Reply,
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    expected_user_id: u64,
) -> Result<AllocationResult, AllocationError> {
    if reply.challenge_present {
        return Ok(AllocationResult::Challenge {
            http_status: reply.http_status,
        });
    }
    let data = reply.data.as_ref().and_then(Value::as_object);
    let status = data.and_then(|v| v.get("status")).and_then(Value::as_i64);
    let no_script = data
        .and_then(|d| d.get("joinScript"))
        .map_or(true, Value::is_null)
        && data
            .and_then(|d| d.get("joinScriptUrl"))
            .map_or(true, Value::is_null);
    if reply.http_status == 200 && matches!(status, Some(0 | 1)) && no_script {
        return Ok(AllocationResult::Pending {
            status: status.expect("matched 0 or 1"),
            http_status: reply.http_status,
        });
    }
    if reply.http_status != 200 || status != Some(2) {
        return Ok(AllocationResult::Rejected {
            http_status: reply.http_status,
            status,
            message: data
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    parse_allocation(reply.http_status, data, target, invite, expected_user_id, &reply.content_type)
}

/// Parse a ready response. `data` is the whole body object; the join document
/// is `data.joinScript`.
///
/// Every field that decides where the client dials is checked here, against
/// what was asked for: a response that names another place, another account or
/// another server is refused rather than dialled.
pub fn parse_allocation(
    http_status: u16,
    data: Option<&serde_json::Map<String, Value>>,
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    expected_user_id: u64,
    content_type: &str,
) -> Result<AllocationResult, AllocationError> {
    if http_status != 200 {
        return Ok(AllocationResult::Rejected {
            http_status,
            status: data.and_then(|d| d.get("status")).and_then(Value::as_i64),
            message: data
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    // A ready allocation is JSON; a challenge page is HTML carrying a
    // challenge id we already checked for. Refusing here means a captive
    // portal or a login redirect cannot be read as a join.
    if !content_type.is_empty() && !content_type.contains("application/json") {
        return Err(AllocationError::new(
            "Allocation reply was not JSON (a challenge or a redirect?)",
        ));
    }
    let join = data
        .and_then(|d| d.get("joinScript"))
        .and_then(Value::as_object)
        .ok_or(AllocationError::new("Ready allocation omitted join script"))?;
    let user_id = join
        .get("UserId")
        .and_then(value_u64)
        .ok_or(AllocationError::new(
            "Issued allocation omitted account identity",
        ))?;
    if expected_user_id == 0 || user_id != expected_user_id {
        return Err(AllocationError::new(
            "Issued allocation did not match authenticated account",
        ));
    }
    let session_id = secret_field(join, "SessionId")
        .ok_or(AllocationError::new("Issued allocation omitted session identity"))?;
    let user_name = bounded_issued_text(join, "UserName")
        .ok_or(AllocationError::new("Issued allocation omitted player name"))?;
    let display_name = bounded_issued_text(join, "DisplayName").unwrap_or_else(|| user_name.clone());
    if join.get("PlaceId").and_then(value_u64) != Some(target.place_id) {
        return Err(AllocationError::new(
            "Issued allocation did not match requested place",
        ));
    }
    if let Some(job) = target.job_id.as_deref() {
        let actual = join
            .get("GameId")
            .and_then(Value::as_str)
            .ok_or(AllocationError::new(
                "Issued allocation did not match requested server",
            ))?;
        if !canonical_uuid(actual) || !actual.eq_ignore_ascii_case(job) {
            return Err(AllocationError::new(
                "Issued allocation did not match requested server",
            ));
        }
    }
    let (private_server_routing_id, requested_vip_server_id) =
        parse_private_routing_id(join.get("PrivateServerID"), invite)?;
    let owner_id = join.get("PrivateServerOwnerID").and_then(value_u64);
    if let Some(invite) = invite {
        if owner_id != Some(invite.owner_user_id)
            || join.get("PlaceId").and_then(value_u64) != Some(target.place_id)
        {
            return Err(AllocationError::new(
                "Issued allocation did not match requested private server",
            ));
        }
    }
    let _ = requested_vip_server_id;

    let address = parse_endpoint(join);
    let backend = parse_backend(join);
    // Current allocations omit the routing token entirely; only an absent or
    // empty field falls back to zeros. A malformed value still fails, because
    // the header is built from it either way.
    let token = match join.get("NetStackTokenValue").and_then(Value::as_str) {
        None | Some("") => Some([0u8; 16]),
        Some(text) => base64::engine::general_purpose::STANDARD
            .decode(text)
            .ok()
            .and_then(|v| v.try_into().ok()),
    };
    let (pins, netstack_port) = parse_net_stack(join);
    let direct_port = join
        .get("NetStackPort")
        .and_then(value_u64)
        .and_then(|p| u16::try_from(p).ok())
        .or_else(|| {
            join.get("ServerPort")
                .and_then(value_u64)
                .and_then(|p| u16::try_from(p).ok())
        });
    let endpoint = address.and_then(|ip| {
        netstack_port
            .or(direct_port)
            .map(|port| SocketAddr::new(IpAddr::V4(ip), port))
    });
    let tickets = SessionTicketMetadata {
        client_ticket: secret_field(join, "ClientTicket"),
        join_ticket: data
            .and_then(|d| d.get("joinTicket"))
            .and_then(Value::as_str)
            .map(Secret::new),
    };
    Ok(AllocationResult::Ready(IssuedJoinAllocation {
        place_id: target.place_id,
        user_id,
        user_name,
        display_name,
        has_verified_badge: join
            .get("HasVerifiedBadge")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        endpoint,
        backend,
        certificate_sha256: pins,
        capabilities: 0,
        game_id: join.get("GameId").and_then(Value::as_str).map(str::to_owned),
        private_server_owner_id: owner_id,
        tickets,
        session_id,
        token,
        private_server_routing_id,
        join_fields: crate::login::retain_issued_game_fields(join),
        join_ticket_value: data.and_then(|d| d.get("joinTicket")).cloned(),
    }))
}

fn parse_private_routing_id(
    wire_value: Option<&Value>,
    invite: Option<&PrivateInvite>,
) -> Result<(Option<Secret>, Option<u64>), AllocationError> {
    let Some(value) = wire_value.filter(|value| !value.is_null()) else {
        if invite.is_some() {
            return Err(AllocationError::new(
                "Issued allocation omitted private server routing id",
            ));
        }
        return Ok((None, None));
    };
    if let Some(text) = value.as_str() {
        if text.is_empty() && invite.is_none() {
            return Ok((None, None));
        }
        if !canonical_non_nil_uuid(text) {
            return Err(AllocationError::new(
                "Issued private server routing id had an unsupported format",
            ));
        }
        return Ok((
            Some(Secret::new(text)),
            invite.map(|i| i.private_server_id),
        ));
    }
    if value.as_u64() == Some(0) && invite.is_none() {
        return Ok((None, None));
    }
    if let Some(numeric_id) = value.as_u64().filter(|id| *id > 0) {
        match invite {
            Some(invite)
                if invite.credential_kind == InviteKind::LinkCode
                    && numeric_id == invite.private_server_id =>
            {
                return Ok((Some(Secret::new(numeric_id.to_string())), Some(numeric_id)));
            }
            Some(_) => {
                return Err(AllocationError::new(
                    "Issued private server routing id did not match the selected server",
                ))
            }
            None => return Ok((Some(Secret::new(numeric_id.to_string())), None)),
        }
    }
    Err(AllocationError::new(
        "Issued private server routing id had an unsupported format",
    ))
}

/// The address to dial: the udmux front, else the machine itself.
fn parse_endpoint(join: &serde_json::Map<String, Value>) -> Option<Ipv4Addr> {
    if let Some(endpoints) = join.get("UdmuxEndpoints").and_then(Value::as_array) {
        if let Some(addr) = endpoints
            .first()
            .and_then(Value::as_object)
            .and_then(|first| first.get("Address").or_else(|| first.get("address")))
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<Ipv4Addr>().ok())
            .filter(|addr| is_global_ipv4(*addr))
        {
            return Some(addr);
        }
    }
    // Servers without udmux fronting expose the game host directly; the join
    // is reachable at that address on NetStackPort/ServerPort.
    join.get("MachineAddress")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<Ipv4Addr>().ok())
        .filter(|addr| is_global_ipv4(*addr))
}

/// The machine the server reports, which the routing header names.
///
/// This is *not* the dial address: with udmux in front the two differ, and the
/// header has to carry the backend even when the packet goes elsewhere. An
/// unusable value is `None`, which the transport refuses.
fn parse_backend(join: &serde_json::Map<String, Value>) -> Option<Ipv4Addr> {
    join.get("MachineAddress")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
}

/// The pins and the port from `NetStackConfig`, which is a percent-encoded
/// JSON document inside the join script.
///
/// The port is accepted only when `NetStackPort` agrees with it: a config that
/// disagrees with the script beside it is not something to dial. A missing or
/// unusable config yields no pins, which the transport refuses — the absence is
/// visible rather than silently filled.
fn parse_net_stack(join: &serde_json::Map<String, Value>) -> (Vec<[u8; 32]>, Option<u16>) {
    let Some(encoded) = join.get("NetStackConfig").and_then(Value::as_str) else {
        return (vec![], None);
    };
    let Some(decoded) = percent_decode(encoded) else {
        return (vec![], None);
    };
    let Ok(config) = serde_json::from_str::<Value>(&decoded) else {
        return (vec![], None);
    };
    let port = config
        .get("port")
        .and_then(value_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0);
    if port
        != join
            .get("NetStackPort")
            .and_then(value_u64)
            .and_then(|p| u16::try_from(p).ok())
    {
        return (vec![], None);
    }
    let Some(values) = config.get("certHashes").and_then(Value::as_array) else {
        return (vec![], port);
    };
    if values.is_empty() || values.len() > MAX_CERT_PINS {
        return (vec![], port);
    }
    let pins: Option<Vec<[u8; 32]>> = values
        .iter()
        .map(|v| {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(v.as_str()?)
                .ok()?;
            decoded.try_into().ok()
        })
        .collect();
    (pins.unwrap_or_default(), port)
}

/// An address this client will dial: globally routable, so a join script
/// cannot point it at the loopback interface or a private network.
fn is_global_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip == Ipv4Addr::BROADCAST
        || a == 0
        || a == 100 && (64..=127).contains(&b) // shared address space
        || a == 192 && b == 0 && c == 0
        || a == 192 && b == 0 && c == 2
        || a == 192 && b == 88 && c == 99
        || a == 198 && (b == 18 || b == 19 || b == 51 && c == 100)
        || a == 203 && b == 0 && c == 113
        || a >= 240
        || d == 255 && a == 255 && b == 255 && c == 255)
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return None;
            }
            let hi = (b[i + 1] as char).to_digit(16)? as u8;
            let lo = (b[i + 2] as char).to_digit(16)? as u8;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn value_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

fn secret_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<Secret> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(Secret::new)
}

/// A player name: bounded, printable, non-empty. Anything else is refused
/// rather than shown, because it is drawn in the UI.
fn bounded_issued_text(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    let value = obj.get(key).and_then(Value::as_str)?;
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_owned())
}

fn canonical_uuid(value: &str) -> bool {
    let mut nonzero = false;
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => {
                let valid = b.is_ascii_hexdigit();
                nonzero |= valid && b != b'0';
                valid
            }
        })
        && nonzero
}

fn canonical_non_nil_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    let mut nonzero = false;
    for (index, byte) in value.bytes().enumerate() {
        if [8, 13, 18, 23].contains(&index) {
            if byte != b'-' {
                return false;
            }
        } else if byte.is_ascii_hexdigit() {
            nonzero |= byte != b'0';
            if index == 14 && !matches!(byte.to_ascii_lowercase(), b'1'..=b'8') {
                return false;
            }
            if index == 19 && !matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b') {
                return false;
            }
        } else {
            return false;
        }
    }
    nonzero
}

/// rove's anti-CSRF token sanity check: printable, non-empty, bounded. Used by
/// the live client; the offline build only needs the parser.
#[cfg_attr(not(feature = "live"), allow(dead_code))]
pub(crate) fn valid_csrf_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_CSRF_TOKEN
        && token.bytes().all(|b| (33..=126).contains(&b))
}

/// At most eight POSTs, three seconds apart, and only while the server keeps
/// saying "pending". A challenge or a rejection stops at once, because neither
/// becomes a join by waiting.
pub fn request_allocation_until_ready(
    http: &mut dyn HttpClient,
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    attempt_id: &str,
    expected_user_id: u64,
) -> Result<AllocationResult, AllocationError> {
    request_allocation_until_ready_with(http, target, invite, attempt_id, expected_user_id, |_| {
        std::thread::sleep(std::time::Duration::from_secs(3))
    })
}

/// [`request_allocation_until_ready`] with the wait supplied, so a test can
/// run the whole poll sequence instantly and count the attempts.
pub fn request_allocation_until_ready_with(
    http: &mut dyn HttpClient,
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    attempt_id: &str,
    expected_user_id: u64,
    mut wait: impl FnMut(usize),
) -> Result<AllocationResult, AllocationError> {
    #[cfg_attr(not(feature = "live"), allow(unused_mut, unused_variables))]
    let mut refreshed_version = false;
    for attempt in 0..8 {
        let result = request_allocation(http, target, invite, attempt_id, expected_user_id)?;
        if matches!(result, AllocationResult::Pending { .. }) && attempt < 7 {
            wait(attempt);
            continue;
        }
        // Status 12 is the launcher's refusal of the client identity, and a
        // stale `User-Agent` version is the common cause (rove learned this:
        // "gamejoin rejects unknown agents with status 12"). Refetch the
        // version once and ask again, rather than reporting a refusal that was
        // only ever about the version string.
        #[cfg(feature = "live")]
        if !refreshed_version
            && matches!(result, AllocationResult::Rejected { status: Some(12), .. })
            && attempt < 7
        {
            refreshed_version = true;
            crate::client_version::invalidate();
            log::info!("allocate: status 12; refetching the client version and retrying");
            continue;
        }
        return Ok(result);
    }
    Err(AllocationError::new("Allocation retry was exhausted"))
}

/// A fresh UUID v4 for `gameJoinAttemptId`, so a repeated join is a new
/// attempt rather than a replay of the last one.
pub fn new_join_attempt_id() -> Result<String, AllocationError> {
    let mut bytes = [0u8; 16];
    rove_client_core::entropy::fill(&mut bytes)
        .map_err(|_| AllocationError::new("Could not generate join attempt identifier"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    ))
}
