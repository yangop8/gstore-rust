//! Access policy for SPARQL `LOAD <iri>`.
//!
//! `LOAD` makes the *server* read a document. Over HTTP that is a file-read
//! primitive available to anyone holding the update privilege: without a
//! policy, `LOAD <file:///etc/passwd>` (or any local path) reads whatever the
//! server process can. A [`LoadPolicy`] decides which sources a database may
//! load from; set it with
//! [`Database::set_load_policy`](crate::Database::set_load_policy).
//!
//! * [`LoadPolicy::permissive`] (the [`Default`]) keeps the historical library
//!   behaviour — any local path, unchanged — so CLI tools and embedders that
//!   `LOAD` their own files keep working.
//! * [`LoadPolicy::restricted`] denies every source; network-facing servers
//!   should start from it and opt specific directories back in with
//!   [`LoadPolicy::with_allowed_dir`].
//!
//! The policy also covers the other ways a client can make the server reach
//! outside its own data: federated `SERVICE` patterns (outbound HTTP) follow
//! [`LoadPolicy::allow_remote`], and the HTTP API's `/build?db_path=` and
//! `/restore` sources follow its local-file rules.
//!
//! When a policy lists allowed directories, a local source is
//! **canonicalized** (`..` components and every symlink resolved, see
//! [`std::fs::canonicalize`]) and must then lie inside one of the canonicalized
//! allowed directories, and must be a regular file. A `../` escape or a symlink
//! inside an allowed directory that points outside it is therefore refused.
//! The canonical path is what gets opened, so the check and the read agree on
//! the file. (An attacker able to replace directory components *inside* an
//! allowed directory between the check and the open could still race it; the
//! allowed directories are assumed to be writable only by trusted parties.)
//!
//! Denials use one uniform message that does not distinguish "outside the
//! allowed directories" from "does not exist", so a client cannot probe for
//! the existence of files outside the sandbox.

use std::path::{Path, PathBuf};

use crate::error::{GStoreError, Result};

/// Which sources a SPARQL `LOAD` may read. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadPolicy {
    /// Whether local files (`file:` IRIs and bare paths) may be loaded at all.
    pub allow_local_files: bool,
    /// `None` ⇒ any local path the process can read (legacy behaviour).
    /// `Some(dirs)` ⇒ only regular files whose canonical path lies inside one
    /// of `dirs` (an empty list therefore denies every local path).
    pub allowed_dirs: Option<Vec<PathBuf>>,
    /// Whether a remote (`http://`, `https://`, … scheme) source may be named.
    /// SPARQL `LOAD` never fetches remote documents, so an allowed remote
    /// `LOAD` still fails with "not supported"; a disallowed one is refused by
    /// policy before anything else happens. The bulk loaders do download
    /// `http(s)://` sources (`/api` `build` / `batchInsert` / `batchRemove`
    /// with `remote=true`, [`crate::parser::import::expand_sources`]) — only
    /// when this is set. It also governs federated `SERVICE`
    /// patterns, which do make outbound HTTP requests: a database whose policy
    /// disallows remote sources refuses any query using `SERVICE` — so a
    /// server on [`restricted`](Self::restricted) cannot be made to contact
    /// internal hosts on a client's behalf.
    pub allow_remote: bool,
    /// Whether a download the bulk loaders make on a client's behalf (`/api`
    /// `build` / `batchInsert` / `batchRemove` with `remote=true`) may reach
    /// a host-local address — loopback, link-local (cloud metadata), …, see
    /// [`crate::http_client::is_local_addr`] — as asynchronous callbacks may
    /// not either. Allowed on [`permissive`](Self::permissive), refused on
    /// [`restricted`](Self::restricted), whatever `allow_remote` says.
    pub allow_local_hosts: bool,
}

impl Default for LoadPolicy {
    fn default() -> LoadPolicy {
        LoadPolicy::permissive()
    }
}

/// A `LOAD` source after the policy approved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadSource {
    /// A local file, to be read from this path (canonical when the policy
    /// restricts directories).
    Local(PathBuf),
    /// A remote IRI (`scheme://…`, not `file:`).
    Remote(String),
}

impl LoadPolicy {
    /// The historical library behaviour: any local path, remote sources not
    /// refused by policy (they still fail as unsupported).
    pub fn permissive() -> LoadPolicy {
        LoadPolicy {
            allow_local_files: true,
            allowed_dirs: None,
            allow_remote: true,
            allow_local_hosts: true,
        }
    }

    /// Deny every `LOAD` source — the starting point for network-facing
    /// servers. Re-enable specific directories with
    /// [`with_allowed_dir`](Self::with_allowed_dir).
    pub fn restricted() -> LoadPolicy {
        LoadPolicy {
            allow_local_files: false,
            allowed_dirs: Some(Vec::new()),
            allow_remote: false,
            allow_local_hosts: false,
        }
    }

    /// Allow local files inside `dir` (and its subdirectories). Enables local
    /// loading and, on a policy that allowed any path, narrows it to the
    /// listed directories.
    pub fn with_allowed_dir(mut self, dir: impl Into<PathBuf>) -> LoadPolicy {
        self.allow_local_files = true;
        self.allowed_dirs
            .get_or_insert_with(Vec::new)
            .push(dir.into());
        self
    }

    /// Check `source` (the IRI of a `LOAD`) against the policy, returning where
    /// to read it from, or a [`GStoreError::Query`] when it is not allowed.
    pub fn resolve(&self, source: &str) -> Result<LoadSource> {
        match classify(source) {
            Classified::Remote => {
                if self.allow_remote {
                    Ok(LoadSource::Remote(source.to_string()))
                } else {
                    Err(GStoreError::Query(format!(
                        "LOAD of remote source '{source}' is not allowed by the load policy"
                    )))
                }
            }
            Classified::Local(path) => {
                if !self.allow_local_files {
                    return Err(GStoreError::Query(format!(
                        "LOAD from local files is disabled by the load policy ('{source}')"
                    )));
                }
                match &self.allowed_dirs {
                    None => Ok(LoadSource::Local(path)),
                    Some(dirs) => sandboxed(&path, dirs)
                        .map(LoadSource::Local)
                        .ok_or_else(|| {
                            GStoreError::Query(format!(
                                "LOAD source '{source}' is not a readable file inside the \
                                 directories allowed by the load policy"
                            ))
                        }),
                }
            }
        }
    }

    /// Whether a client may make the server read the local *directory* `path`
    /// (e.g. a backup to restore from): the policy's rules for local files,
    /// applied to a directory. Returns its canonical path when allowed, `None`
    /// when not — uniformly, whether it is outside the allowed directories, is
    /// not a directory, or does not exist.
    pub fn allowed_local_dir(&self, path: &Path) -> Option<PathBuf> {
        if !self.allow_local_files {
            return None;
        }
        let canonical = std::fs::canonicalize(path).ok()?;
        let inside = match &self.allowed_dirs {
            None => true,
            Some(dirs) => dirs
                .iter()
                .filter_map(|d| std::fs::canonicalize(d).ok())
                .any(|d| canonical.starts_with(&d)),
        };
        (inside && canonical.is_dir()).then_some(canonical)
    }
}

enum Classified {
    Local(PathBuf),
    Remote,
}

/// Split a `LOAD` IRI into a local path or a remote source.
///
/// `file://localhost/p` and `file:///p` name `/p`; `file:/p` names `/p`; any
/// other `file://rest` keeps the historical reading of `rest` as a path. A
/// non-`file` `scheme://` is remote. Anything else is a bare local path.
fn classify(source: &str) -> Classified {
    let lower = source.get(..7).map(str::to_ascii_lowercase);
    if lower.as_deref() == Some("file://") {
        let rest = &source[7..];
        let rest = match rest.strip_prefix("localhost") {
            Some(r) if r.starts_with('/') => r,
            _ => rest,
        };
        return Classified::Local(PathBuf::from(rest));
    }
    if source.get(..5).map(str::to_ascii_lowercase).as_deref() == Some("file:")
        && source[5..].starts_with('/')
    {
        return Classified::Local(PathBuf::from(&source[5..]));
    }
    if has_url_scheme(source) {
        return Classified::Remote;
    }
    Classified::Local(PathBuf::from(source))
}

/// `scheme://…` with an RFC 3986 scheme (`ALPHA *( ALPHA / DIGIT / + - . )`).
fn has_url_scheme(s: &str) -> bool {
    let Some((scheme, _)) = s.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The canonical form of `path` if it is a regular file inside one of `dirs`
/// (each canonicalized; directories that do not exist are ignored).
fn sandboxed(path: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let inside = dirs
        .iter()
        .filter_map(|d| std::fs::canonicalize(d).ok())
        .any(|d| canonical.starts_with(&d));
    if inside && std::fs::metadata(&canonical).ok()?.is_file() {
        Some(canonical)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_file_iris_paths_and_remote() {
        let local = |s: &str| match classify(s) {
            Classified::Local(p) => p,
            Classified::Remote => panic!("{s} classified remote"),
        };
        assert_eq!(local("file:///tmp/x.ttl"), PathBuf::from("/tmp/x.ttl"));
        assert_eq!(
            local("file://localhost/tmp/x.ttl"),
            PathBuf::from("/tmp/x.ttl")
        );
        assert_eq!(local("FILE:///tmp/x.ttl"), PathBuf::from("/tmp/x.ttl"));
        assert_eq!(local("file:/tmp/x.ttl"), PathBuf::from("/tmp/x.ttl"));
        assert_eq!(local("file://data/x.ttl"), PathBuf::from("data/x.ttl"));
        assert_eq!(local("/abs/x.ttl"), PathBuf::from("/abs/x.ttl"));
        assert_eq!(local("rel/x.ttl"), PathBuf::from("rel/x.ttl"));
        for remote in [
            "http://e.org/x",
            "https://e.org/x",
            "ftp://h/x",
            "s3+x.y-z://b/k",
        ] {
            assert!(matches!(classify(remote), Classified::Remote), "{remote}");
        }
        // Not a scheme: a path that merely contains "://" later on.
        assert!(matches!(classify("dir/a://b"), Classified::Local(_)));
    }

    #[test]
    fn permissive_is_the_default_and_passes_paths_through() {
        let p = LoadPolicy::default();
        assert_eq!(p, LoadPolicy::permissive());
        assert_eq!(
            p.resolve("file:///nonexistent/x.ttl").unwrap(),
            LoadSource::Local(PathBuf::from("/nonexistent/x.ttl"))
        );
        assert_eq!(
            p.resolve("http://e.org/x").unwrap(),
            LoadSource::Remote("http://e.org/x".into())
        );
    }

    #[test]
    fn restricted_denies_everything() {
        let p = LoadPolicy::restricted();
        assert!(p.resolve("/etc/hostname").is_err());
        assert!(p.resolve("file:///etc/hostname").is_err());
        assert!(p.resolve("http://e.org/x").is_err());
    }

    #[test]
    fn allowed_local_dir_follows_the_file_rules() {
        let base = std::env::temp_dir().join(format!("gstore_lp_dir_{}", std::process::id()));
        let (inside, outside) = (base.join("allowed/sub"), base.join("other"));
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(base.join("allowed/f"), b"x").unwrap();
        let p = LoadPolicy::restricted().with_allowed_dir(base.join("allowed"));
        assert_eq!(p.allowed_local_dir(&inside), Some(std::fs::canonicalize(&inside).unwrap()));
        assert_eq!(p.allowed_local_dir(&base.join("allowed/sub/../../other")), None);
        assert_eq!(p.allowed_local_dir(&outside), None);
        assert_eq!(p.allowed_local_dir(&base.join("allowed/f")), None, "not a directory");
        assert_eq!(p.allowed_local_dir(&base.join("allowed/missing")), None);
        assert_eq!(LoadPolicy::restricted().allowed_local_dir(&inside), None);
        assert!(LoadPolicy::permissive().allowed_local_dir(&outside).is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn with_allowed_dir_enables_local_and_narrows_permissive() {
        let p = LoadPolicy::restricted().with_allowed_dir("/srv/data");
        assert!(p.allow_local_files);
        assert_eq!(p.allowed_dirs, Some(vec![PathBuf::from("/srv/data")]));
        assert!(!p.allow_remote);
        let q = LoadPolicy::permissive().with_allowed_dir("/srv/data");
        assert_eq!(q.allowed_dirs, Some(vec![PathBuf::from("/srv/data")]));
    }
}
