//! Which protocol version a request speaks, decided from that request alone.
//!
//! The server keeps no session, so nothing an earlier request said is remembered. A request is *modern* when it declares
//! `2026-07-28`, in the `MCP-Protocol-Version` header or in `params._meta`; a modern request must then carry everything
//! that version requires, or it is refused with `400`. A request is *legacy* when it declares `2025-11-25` or one of the
//! two versions before it, or declares nothing at all: its method is then answered as it was before the server knew
//! `2026-07-28`, with no field added. `initialize` is the legacy handshake, so it is legacy whatever else it says.
//!
//! What a request declares is judged whichever version it is. A version that is none of these, in the header or in
//! `params._meta`, is refused with the list of the two the server supports. So is a version header sent twice or not
//! readable, and a header and a body that name different versions. Before `2026-07-28` the server read neither place.

use std::borrow::Cow;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

/// The version `initialize` answers: the last one with a handshake.
pub const PROTOCOL_VERSION: &str = "2025-11-25";
/// The version a request declares for itself, with no handshake before it.
pub const STATELESS_PROTOCOL_VERSION: &str = "2026-07-28";
/// The versions the server supports, newest first: what `server/discover` and an unsupported-version error list.
pub const SUPPORTED_VERSIONS: [&str; 2] = [STATELESS_PROTOCOL_VERSION, PROTOCOL_VERSION];
/// Older versions a client may still name in its header. Their requests are served as legacy ones, as they always were.
const TOLERATED_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];

/// The `_meta` key of a request's protocol version.
pub const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
/// The `_meta` key of the capabilities a client declares on a request.
pub const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
/// The `_meta` key under which a result names the server.
pub const SERVER_INFO_META: &str = "io.modelcontextprotocol/serverInfo";

/// The headers do not match the body, or a required header is missing or unreadable.
pub const HEADER_MISMATCH: i64 = -32020;
/// The request's protocol version is not one the server supports.
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
const INVALID_PARAMS: i64 = -32602;

/// One request header that mirrors a field of the body, as the HTTP layer read it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mirrored<'a> {
    /// The request does not carry the header.
    #[default]
    Absent,
    /// The header's one value.
    Value(&'a str),
    /// The header is sent twice, or its value is not visible ASCII.
    Unreadable,
}

/// The three mirrored headers of one request: `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mirrors<'a> {
    pub version: Mirrored<'a>,
    pub method: Mirrored<'a>,
    pub name: Mirrored<'a>,
}

/// How a request is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    /// As `2025-11-25` and earlier: no header is required and no result gains a field.
    Legacy,
    /// As `2026-07-28`: the request carried its version and its headers, and the result says `resultType`.
    Modern,
}

/// Why a request is refused before its method is looked at. The HTTP status is always `400`.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// The era of the request `method` with `params`, sent with the headers `mirrors`.
pub fn era(mirrors: &Mirrors<'_>, method: &str, params: &Value) -> Result<Era, Refusal> {
    if method == "initialize" {
        return Ok(Era::Legacy);
    }
    let meta = params.get("_meta");
    let in_body = meta.and_then(|meta| meta.get(VERSION_META));
    let in_header = match mirrors.version {
        Mirrored::Absent => None,
        Mirrored::Value(version) if known(version) => Some(version),
        Mirrored::Value(version) => return Err(unsupported(version)),
        Mirrored::Unreadable => {
            return Err(mismatch(
                "the MCP-Protocol-Version header is not one readable value".to_string(),
            ));
        }
    };
    let declared = match (in_header, in_body) {
        (Some(header), Some(body)) if body.as_str() != Some(header) => {
            return Err(mismatch(format!(
                "the MCP-Protocol-Version header does not match params._meta[\"{VERSION_META}\"]"
            )));
        }
        (Some(header), _) => Some(header),
        (None, Some(body)) => match body.as_str() {
            Some(version) if known(version) => Some(version),
            Some(version) => return Err(unsupported(version)),
            None => {
                return Err(Refusal {
                    code: INVALID_PARAMS,
                    message: format!("params._meta[\"{VERSION_META}\"] is a string"),
                    data: None,
                });
            }
        },
        (None, None) => None,
    };
    if declared != Some(STATELESS_PROTOCOL_VERSION) {
        return Ok(Era::Legacy);
    }

    let mut headers = Vec::new();
    if in_header.is_none() {
        headers.push("the MCP-Protocol-Version header is missing".to_string());
    }
    check(
        &mut headers,
        "Mcp-Method",
        mirrors.method,
        Some(method),
        Encoding::Never,
    );
    if method == "tools/call" {
        let name = params.get("name").and_then(Value::as_str);
        check(
            &mut headers,
            "Mcp-Name",
            mirrors.name,
            name,
            Encoding::Allowed,
        );
    }
    let mut body = Vec::new();
    if in_body.is_none() {
        body.push(format!("params._meta lacks \"{VERSION_META}\""));
    }
    if !meta
        .and_then(|meta| meta.get(CAPABILITIES_META))
        .is_some_and(Value::is_object)
    {
        body.push(format!("params._meta lacks \"{CAPABILITIES_META}\""));
    }
    // One refusal names everything the request lacks, so a caller fixes it in one go. A header fault decides the code.
    let code = match (headers.is_empty(), body.is_empty()) {
        (true, true) => return Ok(Era::Modern),
        (false, _) => HEADER_MISMATCH,
        (true, false) => INVALID_PARAMS,
    };
    headers.append(&mut body);
    Err(Refusal {
        code,
        message: format!(
            "not a {STATELESS_PROTOCOL_VERSION} request: {}",
            headers.join("; ")
        ),
        data: None,
    })
}

fn known(version: &str) -> bool {
    SUPPORTED_VERSIONS.contains(&version) || TOLERATED_VERSIONS.contains(&version)
}

/// Whether a header's value may arrive as `=?base64?…?=`. A tool's name may; a method is always sent as it is, so that
/// whoever reads the header alone sees the method the server runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Allowed,
    Never,
}

/// Adds to `faults` what is wrong with the header `name`, which must say what the body says.
fn check(
    faults: &mut Vec<String>,
    name: &str,
    sent: Mirrored<'_>,
    in_body: Option<&str>,
    encoding: Encoding,
) {
    match sent {
        Mirrored::Absent => faults.push(format!("the {name} header is missing")),
        Mirrored::Unreadable => {
            faults.push(format!("the {name} header is not one readable value"));
        }
        Mirrored::Value(value) => {
            let said = match encoding {
                Encoding::Allowed => plain(value),
                Encoding::Never => Some(Cow::Borrowed(value)),
            };
            if said.as_deref() != in_body {
                faults.push(format!("the {name} header does not match the body"));
            }
        }
    }
}

/// A header value as the text it stands for: `=?base64?…?=` carries a value that is not plain ASCII. `None` when the
/// part between the markers is not base64 of UTF-8.
fn plain(value: &str) -> Option<Cow<'_, str>> {
    let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|rest| rest.strip_suffix("?="))
    else {
        return Some(Cow::Borrowed(value));
    };
    let bytes = STANDARD.decode(encoded).ok()?;
    String::from_utf8(bytes).ok().map(Cow::Owned)
}

fn mismatch(message: String) -> Refusal {
    Refusal {
        code: HEADER_MISMATCH,
        message,
        data: None,
    }
}

/// The most characters of the version a caller asked for that an error says back to it.
const REQUESTED_MAX_CHARS: usize = 64;

fn unsupported(requested: &str) -> Refusal {
    let requested: String = requested.chars().take(REQUESTED_MAX_CHARS).collect();
    Refusal {
        code: UNSUPPORTED_PROTOCOL_VERSION,
        message: "unsupported protocol version".to_string(),
        data: Some(json!({ "supported": SUPPORTED_VERSIONS, "requested": requested })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modern_meta() -> Value {
        json!({ VERSION_META: STATELESS_PROTOCOL_VERSION, CAPABILITIES_META: {} })
    }

    fn modern(method: &'static str, name: Mirrored<'static>) -> Mirrors<'static> {
        Mirrors {
            version: Mirrored::Value(STATELESS_PROTOCOL_VERSION),
            method: Mirrored::Value(method),
            name,
        }
    }

    #[test]
    fn a_request_that_declares_nothing_is_legacy() {
        let none = Mirrors::default();
        assert_eq!(era(&none, "tools/list", &json!({})), Ok(Era::Legacy));
        assert_eq!(era(&none, "ping", &Value::Null), Ok(Era::Legacy));
    }

    #[test]
    fn each_older_version_in_the_header_is_legacy() {
        for version in ["2025-11-25", "2025-06-18", "2025-03-26"] {
            let mirrors = Mirrors {
                version: Mirrored::Value(version),
                ..Mirrors::default()
            };
            assert_eq!(
                era(&mirrors, "tools/list", &json!({})),
                Ok(Era::Legacy),
                "{version}"
            );
        }
    }

    #[test]
    fn initialize_is_legacy_whatever_its_header_says() {
        for version in [STATELESS_PROTOCOL_VERSION, "1900-01-01"] {
            let mirrors = Mirrors {
                version: Mirrored::Value(version),
                ..Mirrors::default()
            };
            assert_eq!(
                era(
                    &mirrors,
                    "initialize",
                    &json!({ "protocolVersion": version })
                ),
                Ok(Era::Legacy)
            );
        }
    }

    #[test]
    fn a_complete_modern_request_is_modern() {
        let params = json!({ "_meta": modern_meta() });
        assert_eq!(
            era(
                &modern("tools/list", Mirrored::Absent),
                "tools/list",
                &params
            ),
            Ok(Era::Modern)
        );
        let call = json!({ "name": "arca_status", "_meta": modern_meta() });
        assert_eq!(
            era(
                &modern("tools/call", Mirrored::Value("arca_status")),
                "tools/call",
                &call
            ),
            Ok(Era::Modern)
        );
    }

    #[test]
    fn an_unknown_version_is_refused_with_the_supported_list() {
        let mirrors = Mirrors {
            version: Mirrored::Value("1900-01-01"),
            ..Mirrors::default()
        };
        let refusal = era(&mirrors, "tools/list", &json!({})).expect_err("refused");
        assert_eq!(refusal.code, UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(
            refusal.data,
            Some(json!({ "supported": ["2026-07-28", "2025-11-25"], "requested": "1900-01-01" }))
        );
        // The same when only the body names it.
        let params = json!({ "_meta": { VERSION_META: "1900-01-01" } });
        let refusal = era(&Mirrors::default(), "tools/list", &params).expect_err("refused");
        assert_eq!(refusal.code, UNSUPPORTED_PROTOCOL_VERSION);
    }

    #[test]
    fn a_header_and_a_body_that_disagree_are_a_header_mismatch() {
        let legacy_header = Mirrors {
            version: Mirrored::Value(PROTOCOL_VERSION),
            ..Mirrors::default()
        };
        let params = json!({ "_meta": modern_meta() });
        let refusal = era(&legacy_header, "tools/list", &params).expect_err("refused");
        assert_eq!(refusal.code, HEADER_MISMATCH);
    }

    #[test]
    fn a_modern_body_without_the_version_header_is_a_header_mismatch() {
        let mirrors = Mirrors {
            method: Mirrored::Value("tools/list"),
            ..Mirrors::default()
        };
        let params = json!({ "_meta": modern_meta() });
        let refusal = era(&mirrors, "tools/list", &params).expect_err("refused");
        assert_eq!(refusal.code, HEADER_MISMATCH);
        assert!(
            refusal
                .message
                .contains("the MCP-Protocol-Version header is missing")
        );
    }

    #[test]
    fn a_modern_header_alone_is_refused_and_everything_missing_is_named() {
        let mirrors = Mirrors {
            version: Mirrored::Value(STATELESS_PROTOCOL_VERSION),
            ..Mirrors::default()
        };
        let refusal = era(&mirrors, "tools/list", &json!({})).expect_err("refused");
        assert_eq!(refusal.code, HEADER_MISMATCH);
        for part in [
            "the Mcp-Method header is missing",
            VERSION_META,
            CAPABILITIES_META,
        ] {
            assert!(refusal.message.contains(part), "{}", refusal.message);
        }
    }

    #[test]
    fn modern_headers_without_the_meta_fields_are_invalid_params() {
        let refusal = era(
            &modern("tools/list", Mirrored::Absent),
            "tools/list",
            &json!({}),
        )
        .expect_err("refused");
        assert_eq!(refusal.code, INVALID_PARAMS);
        let half = json!({ "_meta": { VERSION_META: STATELESS_PROTOCOL_VERSION } });
        let refusal =
            era(&modern("tools/list", Mirrored::Absent), "tools/list", &half).expect_err("refused");
        assert_eq!(refusal.code, INVALID_PARAMS);
        assert!(refusal.message.contains(CAPABILITIES_META));
        assert!(!refusal.message.contains(VERSION_META));
    }

    #[test]
    fn a_method_header_that_names_another_method_is_a_header_mismatch() {
        let params = json!({ "_meta": modern_meta() });
        let refusal = era(
            &modern("tools/call", Mirrored::Absent),
            "tools/list",
            &params,
        )
        .expect_err("refused");
        assert_eq!(refusal.code, HEADER_MISMATCH);
    }

    #[test]
    fn a_tool_call_needs_a_name_header_that_matches_plain_or_encoded() {
        let params = json!({ "name": "arca_status", "_meta": modern_meta() });
        let cases = [
            (Mirrored::Value("arca_status"), true),
            // base64 of `arca_status`.
            (Mirrored::Value("=?base64?YXJjYV9zdGF0dXM=?="), true),
            (Mirrored::Value("token_list"), false),
            (Mirrored::Value("=?base64?dG9rZW5fbGlzdA==?="), false),
            (Mirrored::Value("=?base64?not base64?="), false),
            (Mirrored::Absent, false),
            (Mirrored::Unreadable, false),
        ];
        for (name, admitted) in cases {
            let outcome = era(&modern("tools/call", name), "tools/call", &params);
            match outcome {
                Ok(era) => assert!(admitted && era == Era::Modern, "{name:?}"),
                Err(refusal) => {
                    assert!(!admitted, "{name:?}");
                    assert_eq!(refusal.code, HEADER_MISMATCH, "{name:?}");
                }
            }
        }
    }

    #[test]
    fn a_method_header_is_never_read_as_encoded() {
        // base64 of `tools/list`: the encoding is a tool name's alone, so here it is one more value that does not match.
        let params = json!({ "_meta": modern_meta() });
        let mirrors = modern("=?base64?dG9vbHMvbGlzdA==?=", Mirrored::Absent);
        let refusal = era(&mirrors, "tools/list", &params).expect_err("refused");
        assert_eq!(refusal.code, HEADER_MISMATCH);
    }

    #[test]
    fn the_version_an_error_says_back_is_cut_at_64_characters() {
        let long = "9".repeat(500);
        let mirrors = Mirrors {
            version: Mirrored::Value(&long),
            ..Mirrors::default()
        };
        let refusal = era(&mirrors, "tools/list", &json!({})).expect_err("refused");
        assert_eq!(refusal.data.expect("data")["requested"], "9".repeat(64));
        // A version in the body may be any text: the cut falls between characters, never inside one.
        let params = json!({ "_meta": { VERSION_META: "é".repeat(500) } });
        let refusal = era(&Mirrors::default(), "tools/list", &params).expect_err("refused");
        assert_eq!(refusal.data.expect("data")["requested"], "é".repeat(64));
    }

    #[test]
    fn a_version_in_the_body_is_judged_without_any_header() {
        let none = Mirrors::default();
        let not_text = json!({ "_meta": { VERSION_META: 20260728 } });
        assert_eq!(
            era(&none, "tools/list", &not_text)
                .expect_err("refused")
                .code,
            INVALID_PARAMS
        );
        // A legacy version there changes nothing: the request is a legacy one.
        let legacy = json!({ "_meta": { VERSION_META: PROTOCOL_VERSION } });
        assert_eq!(era(&none, "tools/list", &legacy), Ok(Era::Legacy));
    }

    #[test]
    fn a_name_header_carries_a_name_that_is_not_ascii() {
        // base64 of `Hello, 世界`, the specification's own example.
        let params = json!({ "name": "Hello, 世界", "_meta": modern_meta() });
        let name = Mirrored::Value("=?base64?SGVsbG8sIOS4lueVjA==?=");
        assert_eq!(
            era(&modern("tools/call", name), "tools/call", &params),
            Ok(Era::Modern)
        );
    }
}
