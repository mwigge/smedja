/// Standard JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

/// Application-defined error codes (range -32000 to -32099).
/// The operation deadline expired before a terminal state was reached.
/// Distinct from `INTERNAL_ERROR` so callers can distinguish a timeout
/// from an actual server fault.
pub const TIMEOUT: i32 = -32001;

/// The server-side process disconnected unexpectedly (EOF or connection reset).
/// Returned by the client when the transport closes before a response arrives.
pub const SERVER_DISCONNECTED: i32 = -32002;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_codes_match_spec() {
        assert_eq!(PARSE_ERROR, -32700);
        assert_eq!(INVALID_REQUEST, -32600);
        assert_eq!(METHOD_NOT_FOUND, -32601);
        assert_eq!(INVALID_PARAMS, -32602);
        assert_eq!(INTERNAL_ERROR, -32603);
    }

    #[test]
    fn application_codes_are_in_valid_range() {
        const { assert!(TIMEOUT > -32100 && TIMEOUT < -32000) };
        const { assert!(SERVER_DISCONNECTED > -32100 && SERVER_DISCONNECTED < -32000) };
    }
}
