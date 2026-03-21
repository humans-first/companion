use agent_client_protocol as acp;

// Gateway-specific error codes in the JSON-RPC server error range (-32000 to -32099).
// Avoid -32000 (AuthRequired) and -32002 (ResourceNotFound) which the ACP SDK uses.

pub const UNKNOWN_SESSION: i32 = -32001;
pub const PROMPT_IN_FLIGHT: i32 = -32003;
pub const AUTHZ_DENIED: i32 = -32004;
pub const POOL_EXHAUSTED: i32 = -32005;

pub fn unknown_session(sid: &str) -> acp::Error {
    acp::Error::new(UNKNOWN_SESSION, format!("unknown session: {sid}"))
}

pub fn prompt_in_flight(sid: &str) -> acp::Error {
    acp::Error::new(PROMPT_IN_FLIGHT, format!("prompt already in flight for session: {sid}"))
}

pub fn authz_denied(reason: &str) -> acp::Error {
    acp::Error::new(AUTHZ_DENIED, format!("authorization denied: {reason}"))
}

pub fn pool_exhausted(max: usize) -> acp::Error {
    acp::Error::new(POOL_EXHAUSTED, format!("pool exhausted: all {max} slots in use"))
}
