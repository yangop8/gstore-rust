//! Small decoders the protocol layers share: one implementation of each
//! (review gapi#15, robust#16, design#13: the REST endpoints, the `/api`
//! layer and PFN had their own copies, which had begun to drift).
//!
//! * [`form`] — `application/x-www-form-urlencoded` (and URL query)
//!   decoding: [`form::url_decode`], [`form::parse_form`] (every pair, as
//!   the `/api` layer reads parameters) and [`form::form_lookup`] (one
//!   name, as the REST endpoints read them);
//! * [`base64`] — RFC 4648 base64 decoding with the rules each caller
//!   needs ([`base64::BASIC_AUTH`] for HTTP Basic credentials,
//!   [`base64::LENIENT`] for PFN's WebAssembly modules).

pub mod base64;
pub mod form;
