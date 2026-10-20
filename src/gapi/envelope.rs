//! The upstream response envelope.
//!
//! Every outcome of an `/api` operation — success or failure — is **HTTP
//! 200** with a JSON body carrying the upstream business code and message:
//!
//! ```json
//! {"StatusCode":0,"StatusMsg":"success", …operation fields…}
//! ```
//!
//! The codes are upstream's `server::StatusCode` (`src/Server/ServerStatusCode.h`),
//! see [`Status`]; an error without its own message carries the code's default
//! text, like upstream's `GRPCResp::Error(code)`.
//!
//! **Key order.** Upstream builds responses as `nlohmann::json` objects — that
//! is, `std::map`s — so every object it emits has its keys sorted byte-wise
//! (`"AnsNum"`, `"OutputLimit"`, …, `"StatusCode"`, …, `"head"`, `"results"`).
//! [`GResp::into_parts`] serializes the envelope the same way
//! ([`Json::sorted`]), so a response is byte-identical to upstream's wherever
//! the field values agree. JSON clients do not depend on the order; it only
//! makes side-by-side comparisons (and naive `grep`-based test scripts) behave.
//!
//! **Compression.** The envelope is a buffered body; the HTTP layer
//! (`ApiInner::handle`) gzip/deflate-compresses it when the client's
//! `Accept-Encoding` allows and the `gzip` feature is enabled.

use crate::json::Json;

/// Upstream gStore's status codes (`server::StatusCode`). The small codes
/// below 1000 are the HTTP-framework ones (file transfer, compression, …);
/// the API codes start at 1000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Ok,
    NotFound,
    CompressError,
    CompressNotSupport,
    NoCompress,
    UncompressError,
    UncompressNotSupport,
    NoUncompress,
    FileRangeInvalid,
    FileReadError,
    FileWriteError,
    FileNotFound,
    JsonInvalid,
    ProxyError,
    RouteVerbNotImplement,
    RouteNotFound,
    /// `1000`
    MethodTypeUnsupport,
    /// `1001` — wrong username / password.
    AuthenticationFailed,
    /// `1002`
    CheckPrivilegeFailed,
    /// `1003` — a missing or malformed parameter.
    ParamIsIllegal,
    /// `1004` — e.g. the database does not exist, or the caller lacks the
    /// operation's privilege (upstream answers a privilege failure with this
    /// code, not `1002`).
    OperationConditionsAreNotSatisfied,
    /// `1005`
    OperationFailed,
    /// `1006`
    AddPrivilegeFailed,
    /// `1007`
    LossOfLock,
    /// `1008`
    TransactionManageFailed,
    /// `1100` — unknown or missing `operation`.
    OperationUndefined,
    /// `1101`
    IpBlocked,
    /// `1102`
    LicenseExpired,
    /// `1103`
    LicenseInvalid,
}

impl Status {
    /// The numeric code.
    pub fn code(self) -> i64 {
        use Status::*;
        match self {
            Ok => 0,
            NotFound => 1,
            CompressError => 2,
            CompressNotSupport => 3,
            NoCompress => 4,
            UncompressError => 5,
            UncompressNotSupport => 6,
            NoUncompress => 7,
            FileRangeInvalid => 8,
            FileReadError => 9,
            FileWriteError => 10,
            FileNotFound => 11,
            JsonInvalid => 12,
            ProxyError => 13,
            RouteVerbNotImplement => 14,
            RouteNotFound => 15,
            MethodTypeUnsupport => 1000,
            AuthenticationFailed => 1001,
            CheckPrivilegeFailed => 1002,
            ParamIsIllegal => 1003,
            OperationConditionsAreNotSatisfied => 1004,
            OperationFailed => 1005,
            AddPrivilegeFailed => 1006,
            LossOfLock => 1007,
            TransactionManageFailed => 1008,
            OperationUndefined => 1100,
            IpBlocked => 1101,
            LicenseExpired => 1102,
            LicenseInvalid => 1103,
        }
    }

    /// The status for a numeric code, if it is one of upstream's.
    pub fn from_code(code: i64) -> Option<Status> {
        ALL.iter().copied().find(|s| s.code() == code)
    }

    /// Upstream's default message (`error_code_to_str`).
    pub fn default_msg(self) -> &'static str {
        use Status::*;
        match self {
            Ok => "Success",
            NotFound => "404 Not Found",
            CompressError => "Compress Error",
            CompressNotSupport => "Compress Not Support",
            NoCompress => "No Comrpession",
            UncompressError => "Uncompress Error",
            UncompressNotSupport => "Uncompress Not Support",
            NoUncompress => "No Uncomrpess",
            FileRangeInvalid => "File Range Invalid",
            FileReadError => "File Read Error",
            FileWriteError => "File Write Error",
            FileNotFound => "unknown", // (absent from upstream's message table)
            JsonInvalid => "Invalid Json Syntax",
            ProxyError => "Http Proxy Error",
            RouteVerbNotImplement => "Route Http Method not implement",
            RouteNotFound => "Route Not Found",
            MethodTypeUnsupport => "The method type is not support",
            AuthenticationFailed => "Authentication Failed",
            CheckPrivilegeFailed => "Check Privilege Failed",
            ParamIsIllegal => "Param is illegal",
            OperationConditionsAreNotSatisfied => "The operation conditions are not satisfied",
            OperationFailed => "Operation failed",
            AddPrivilegeFailed => "Add privilege Failed",
            LossOfLock => "Loss of lock",
            TransactionManageFailed => "Transcation manage Failed",
            OperationUndefined => "The operation is not defined",
            IpBlocked => "IP Blocked",
            LicenseExpired => "The license has expired",
            LicenseInvalid => "The license is invalid",
        }
    }
}

const ALL: [Status; 29] = {
    use Status::*;
    [
        Ok,
        NotFound,
        CompressError,
        CompressNotSupport,
        NoCompress,
        UncompressError,
        UncompressNotSupport,
        NoUncompress,
        FileRangeInvalid,
        FileReadError,
        FileWriteError,
        FileNotFound,
        JsonInvalid,
        ProxyError,
        RouteVerbNotImplement,
        RouteNotFound,
        MethodTypeUnsupport,
        AuthenticationFailed,
        CheckPrivilegeFailed,
        ParamIsIllegal,
        OperationConditionsAreNotSatisfied,
        OperationFailed,
        AddPrivilegeFailed,
        LossOfLock,
        TransactionManageFailed,
        OperationUndefined,
        IpBlocked,
        LicenseExpired,
        LicenseInvalid,
    ]
};

/// The body of a [`GResp`].
#[derive(Debug, Clone, PartialEq)]
pub enum GBody {
    /// A JSON envelope (an object with `StatusCode` / `StatusMsg`), always
    /// sent as HTTP 200 `application/json`.
    Json(Json),
    /// A non-envelope response, e.g. a file download.
    Raw {
        status: u16,
        content_type: String,
        bytes: Vec<u8>,
    },
}

/// The response of one `/api` operation.
#[derive(Debug, Clone, PartialEq)]
pub struct GResp {
    body: GBody,
    headers: Vec<(String, String)>,
}

impl GResp {
    /// `{"StatusCode":0,"StatusMsg":msg}` (`"Success"` when `msg` is empty) —
    /// upstream's `GRPCResp::Success`.
    pub fn success(msg: &str) -> GResp {
        let msg = if msg.is_empty() {
            Status::Ok.default_msg()
        } else {
            msg
        };
        GResp::json(Json::obj().with("StatusCode", 0).with("StatusMsg", msg))
    }

    /// `{"StatusCode":<code>,"StatusMsg":msg}` (the code's default message
    /// when `msg` is empty) — upstream's `GRPCResp::Error`.
    pub fn error(status: Status, msg: impl Into<String>) -> GResp {
        let msg = msg.into();
        let msg = if msg.is_empty() {
            status.default_msg().to_string()
        } else {
            msg
        };
        GResp::json(
            Json::obj()
                .with("StatusCode", status.code())
                .with("StatusMsg", msg),
        )
    }

    /// An envelope from a JSON object. A missing `StatusCode` defaults to `0`
    /// and a missing `StatusMsg` to `"success"`; a non-object is wrapped as
    /// `{"StatusCode":0,"StatusMsg":"success","result":<value>}`.
    pub fn json(body: Json) -> GResp {
        let mut body = match body {
            obj @ Json::Obj(_) => obj,
            other => Json::obj().with("result", other),
        };
        if body.get("StatusCode").is_none() {
            body.set("StatusCode", 0);
        }
        if body.get("StatusMsg").is_none() {
            body.set("StatusMsg", "success");
        }
        GResp {
            body: GBody::Json(body),
            headers: Vec::new(),
        }
    }

    /// A non-envelope response (status, content type, raw bytes).
    pub fn raw(status: u16, content_type: impl Into<String>, bytes: Vec<u8>) -> GResp {
        GResp {
            body: GBody::Raw {
                status,
                content_type: content_type.into(),
                bytes,
            },
            headers: Vec::new(),
        }
    }

    /// Add a response header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> GResp {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set a field on a JSON envelope (no-op for a raw response).
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Json>) -> &mut GResp {
        if let GBody::Json(body) = &mut self.body {
            body.set(key, value);
        }
        self
    }

    /// Builder form of [`set`](Self::set).
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Json>) -> GResp {
        self.set(key, value);
        self
    }

    /// The upstream `StatusCode` (a raw response counts as `0` when its HTTP
    /// status is 2xx, else as `1005`).
    pub fn status_code(&self) -> i64 {
        match &self.body {
            GBody::Json(body) => body.get("StatusCode").and_then(Json::as_i64).unwrap_or(0),
            GBody::Raw { status, .. } if (200..300).contains(status) => 0,
            GBody::Raw { .. } => Status::OperationFailed.code(),
        }
    }

    /// The `StatusMsg` of a JSON envelope (empty for a raw response).
    pub fn status_msg(&self) -> String {
        match &self.body {
            GBody::Json(body) => body
                .get("StatusMsg")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string(),
            GBody::Raw { .. } => String::new(),
        }
    }

    /// Whether the operation succeeded (`StatusCode` 0).
    pub fn is_ok(&self) -> bool {
        self.status_code() == 0
    }

    /// The JSON envelope, if this is one.
    pub fn body_json(&self) -> Option<&Json> {
        match &self.body {
            GBody::Json(body) => Some(body),
            GBody::Raw { .. } => None,
        }
    }

    /// The body.
    pub fn body(&self) -> &GBody {
        &self.body
    }

    /// The extra response headers.
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// The HTTP rendering: `(status, content type, body, headers)`. A JSON
    /// envelope is HTTP 200 `application/json`, serialized compactly with
    /// upstream's key order (see the module docs).
    pub fn into_parts(self) -> (u16, String, Vec<u8>, Vec<(String, String)>) {
        match self.body {
            GBody::Json(body) => (
                200,
                "application/json".to_string(),
                body.sorted().to_string().into_bytes(),
                self.headers,
            ),
            GBody::Raw {
                status,
                content_type,
                bytes,
            } => (status, content_type, bytes, self.headers),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_default_messages_match_upstream() {
        assert_eq!(Status::Ok.code(), 0);
        assert_eq!(Status::FileNotFound.code(), 11);
        assert_eq!(Status::MethodTypeUnsupport.code(), 1000);
        assert_eq!(Status::TransactionManageFailed.code(), 1008);
        assert_eq!(Status::OperationUndefined.code(), 1100);
        assert_eq!(Status::IpBlocked.code(), 1101);
        assert_eq!(Status::from_code(1003), Some(Status::ParamIsIllegal));
        assert_eq!(Status::from_code(999), None);
        for s in ALL {
            assert_eq!(Status::from_code(s.code()), Some(s));
        }
        assert_eq!(
            Status::OperationUndefined.default_msg(),
            "The operation is not defined"
        );
    }

    #[test]
    fn envelopes_are_http_200_and_key_sorted() {
        let (status, ctype, body, _) = GResp::error(Status::ParamIsIllegal, "").into_parts();
        assert_eq!((status, ctype.as_str()), (200, "application/json"));
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"StatusCode":1003,"StatusMsg":"Param is illegal"}"#
        );
        let r = GResp::success("").with("TID", "1").with("AnsNum", 2);
        assert_eq!(r.status_code(), 0);
        assert_eq!(r.status_msg(), "Success");
        let (_, _, body, _) = r.into_parts();
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"AnsNum":2,"StatusCode":0,"StatusMsg":"Success","TID":"1"}"#
        );
        let r = GResp::json(Json::obj().with("x", 1));
        assert!(r.is_ok());
        assert_eq!(r.status_msg(), "success");
    }

    #[test]
    fn raw_responses_pass_through() {
        let r = GResp::raw(200, "application/octet-stream", b"abc".to_vec())
            .with_header("Content-Disposition", "attachment");
        assert!(r.is_ok());
        assert!(r.body_json().is_none());
        let (status, ctype, body, headers) = r.into_parts();
        assert_eq!(
            (status, ctype.as_str(), body.as_slice()),
            (200, "application/octet-stream", &b"abc"[..])
        );
        assert_eq!(headers.len(), 1);
        assert_eq!(GResp::raw(404, "text/plain", vec![]).status_code(), 1005);
    }
}
