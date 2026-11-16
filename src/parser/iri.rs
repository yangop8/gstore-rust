//! IRI reference resolution (RFC 3986 §5.2), shared by every RDF reader.
//!
//! Turtle / TriG (`@base`, `BASE`), RDF/XML (`xml:base`, `rdf:about`,
//! `rdf:ID`, `rdf:resource`, `rdf:datatype`), JSON-LD (`@base`, `@id`) and
//! RDFa (`<base href>`, `xml:base`, `about` / `resource` / `href` / `src`)
//! all resolve a relative reference with [`resolve`], so one document base
//! gives one IRI whatever the format (review parser#4: RDF/XML, JSON-LD and
//! RDFa used to join the reference to the base after its last `/`, so
//! `../c` stayed in the IRI and `/x`, `//host/x` and `?q` were wrong).
//!
//! The resolver follows the reference-resolution algorithm of RFC 3986
//! §5.2.2 for a reference without a scheme. A reference with a scheme is an
//! absolute IRI and is returned unchanged (RDF does not normalise IRIs, so
//! an absolute IRI is stored exactly as written). `remove_dot_segments`
//! (§5.2.4) walks the path once with a stack of output segments, so
//! resolution is linear in the length of the reference (review parser#15:
//! the previous version moved the rest of the path once per segment).

/// Whether `s` is an absolute IRI: it starts with a valid scheme and `:`
/// (RFC 3986 §3.1, `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`).
pub fn is_absolute(s: &str) -> bool {
    match s.find(':') {
        Some(i) => is_valid_scheme(&s[..i]),
        None => false,
    }
}

/// `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` (RFC 3986 §3.1).
/// The first character must be a letter; an empty scheme is invalid.
pub fn is_valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        _ => false,
    }
}

/// Resolve the IRI reference `reference` against `base` (RFC 3986 §5.2).
///
/// * An absolute `reference` (one with a scheme) is returned as written.
/// * An empty `base` means no base is known: `reference` is returned as
///   written (it stays a relative IRI, as the readers always did).
/// * Otherwise the reference is resolved: `""` is the base without its
///   fragment, `#f` replaces the base's fragment, `?q` its query, `/p` its
///   path, `//host/p` its authority and path, and a relative path is
///   merged with the base path and has its `.` / `..` segments removed.
pub fn resolve(base: &str, reference: &str) -> String {
    if is_absolute(reference) || base.is_empty() {
        return reference.to_string();
    }
    let b = split(base);
    let r = split(reference);
    let (authority, path, query) = if r.authority.is_some() {
        (r.authority, remove_dot_segments(r.path), r.query)
    } else if r.path.is_empty() {
        (b.authority, b.path.to_string(), r.query.or(b.query))
    } else if r.path.starts_with('/') {
        (b.authority, remove_dot_segments(r.path), r.query)
    } else {
        let merged = merge_paths(b.authority, b.path, r.path);
        (b.authority, remove_dot_segments(&merged), r.query)
    };
    recompose(b.scheme, authority, &path, query, r.fragment)
}

/// The five components of an IRI reference (RFC 3986 §3). `scheme` and
/// `path` are always present (possibly empty); `authority`, `query` and
/// `fragment` only when their delimiters (`//`, `?`, `#`) appear.
struct Parts<'a> {
    scheme: &'a str,
    authority: Option<&'a str>,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

fn split(s: &str) -> Parts<'_> {
    let mut rest = s;
    let mut fragment = None;
    if let Some(i) = rest.find('#') {
        fragment = Some(&rest[i + 1..]);
        rest = &rest[..i];
    }
    let mut query = None;
    if let Some(i) = rest.find('?') {
        query = Some(&rest[i + 1..]);
        rest = &rest[..i];
    }
    let mut scheme = "";
    if let Some(i) = rest.find(':') {
        if is_valid_scheme(&rest[..i]) {
            scheme = &rest[..i];
            rest = &rest[i + 1..];
        }
    }
    let mut authority = None;
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find('/').unwrap_or(after.len());
        authority = Some(&after[..end]);
        rest = &after[end..];
    }
    Parts {
        scheme,
        authority,
        path: rest,
        query,
        fragment,
    }
}

/// Merge a relative-path reference onto the base path (RFC 3986 §5.2.3).
fn merge_paths(base_authority: Option<&str>, base_path: &str, ref_path: &str) -> String {
    if base_authority.is_some() && base_path.is_empty() {
        format!("/{ref_path}")
    } else if let Some(i) = base_path.rfind('/') {
        format!("{}{}", &base_path[..=i], ref_path)
    } else {
        ref_path.to_string()
    }
}

/// Remove `.` and `..` segments (RFC 3986 §5.2.4), in one pass.
fn remove_dot_segments(path: &str) -> String {
    let mut steps = 0;
    remove_dot_segments_counted(path, &mut steps)
}

/// [`remove_dot_segments`], adding to `steps` the bytes it examines (a work
/// counter for the complexity test). The output buffer of the RFC's
/// algorithm is kept as a stack of segments (`/seg`, or `seg` first), so
/// "remove the last segment and its preceding `/`" is one pop, and the
/// input is a slice that only ever advances.
fn remove_dot_segments_counted(path: &str, steps: &mut usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut input = path;
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../") {
            input = rest;
        } else if let Some(rest) = input.strip_prefix("./") {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..];
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") {
            input = &input[3..];
            out.pop();
        } else if input == "/.." {
            input = "/";
            out.pop();
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..].find('/').map_or(input.len(), |i| start + i);
            out.push(&input[..end]);
            input = &input[end..];
            *steps += end;
        }
        *steps += 1;
    }
    out.concat()
}

/// Recompose the components into an IRI (RFC 3986 §5.3).
fn recompose(
    scheme: &str,
    authority: Option<&str>,
    path: &str,
    query: Option<&str>,
    fragment: Option<&str>,
) -> String {
    let mut out = String::with_capacity(
        scheme.len()
            + authority.map_or(0, str::len)
            + path.len()
            + query.map_or(0, str::len)
            + fragment.map_or(0, str::len)
            + 6,
    );
    if !scheme.is_empty() {
        out.push_str(scheme);
        out.push(':');
    }
    if let Some(a) = authority {
        out.push_str("//");
        out.push_str(a);
    }
    out.push_str(path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 3986 §5.4.1 (normal) and §5.4.2 (abnormal) examples.
    #[test]
    fn rfc3986_reference_resolution_examples() {
        let base = "http://a/b/c/d;p?q";
        let cases = [
            ("g:h", "g:h"),
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q#s"),
            ("g#s", "http://a/b/c/g#s"),
            ("g?y#s", "http://a/b/c/g?y#s"),
            (";x", "http://a/b/c/;x"),
            ("g;x", "http://a/b/c/g;x"),
            ("g;x?y#s", "http://a/b/c/g;x?y#s"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../", "http://a/"),
            ("../../g", "http://a/g"),
            ("../../../g", "http://a/g"),
            ("../../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            (".g", "http://a/b/c/.g"),
            ("g..", "http://a/b/c/g.."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("./g/.", "http://a/b/c/g/"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
            ("g;x=1/../y", "http://a/b/c/y"),
            ("g?y/./x", "http://a/b/c/g?y/./x"),
            ("g?y/../x", "http://a/b/c/g?y/../x"),
            ("g#s/./x", "http://a/b/c/g#s/./x"),
            ("g#s/../x", "http://a/b/c/g#s/../x"),
            ("http:g", "http:g"),
        ];
        for (reference, want) in cases {
            assert_eq!(resolve(base, reference), want, "reference {reference:?}");
        }
    }

    #[test]
    fn no_base_keeps_the_reference_and_a_fragment_of_the_base_is_dropped() {
        assert_eq!(resolve("", "../x"), "../x");
        assert_eq!(resolve("http://ex.org/doc#top", ""), "http://ex.org/doc");
        assert_eq!(
            resolve("http://ex.org/doc#top", "#id"),
            "http://ex.org/doc#id"
        );
        assert_eq!(resolve("http://ex.org", "foo"), "http://ex.org/foo");
        assert_eq!(resolve("http://ex.org", "../up"), "http://ex.org/up");
        assert_eq!(resolve("urn:x", "y"), "urn:y");
    }

    /// Review parser#15: dot-segment removal examines each byte of the path
    /// a bounded number of times, however many segments it has.
    #[test]
    fn dot_segment_removal_is_linear_in_the_path() {
        for segments in [1_000usize, 10_000, 100_000] {
            for unit in ["a/", "../", "./", "a/../"] {
                let path = format!("/{}x", unit.repeat(segments));
                let mut steps = 0;
                let out = remove_dot_segments_counted(&path, &mut steps);
                assert!(
                    steps <= 2 * path.len() + 2,
                    "{unit:?} x {segments}: {steps} steps for {} bytes",
                    path.len()
                );
                let want = if unit == "a/" {
                    format!("/{}x", unit.repeat(segments))
                } else {
                    "/x".to_string()
                };
                assert_eq!(out, want, "{unit:?} x {segments}");
            }
        }
    }
}
