//! How a database directory is opened (contract C7 of docs/PHASE6_PLAN.md):
//! [`OpenOptions`], the process-wide defaults
//! ([`set_default_open_options`] / [`default_open_options`]) and their
//! environment overrides.
//!
//! The one decision that matters most is the [`DiskQueryMode`] of an on-disk
//! (`kvstore.kv`) database:
//!
//! * [`Stream`](DiskQueryMode::Stream) evaluates straight off the B+ trees
//!   ([`Backend::Disk`](crate::store::Backend::Disk)) with an out-of-core
//!   dictionary: an open reads nothing proportional to the data except the
//!   named graphs and the O(P) predicate statistics, and memory stays bounded
//!   by the page cache and the dictionary caches.
//! * [`Materialize`](DiskQueryMode::Materialize) loads the dictionary and the
//!   default graph into memory at open (the pre-Phase-6 working set) and
//!   writes every committed update through to `kvstore.kv` by dictionary key.
//!
//! Both read and write the same version-1 `kvstore.kv` (no feature bit), so a
//! directory can be opened in either mode, and by older binaries, in turn.
//!
//! ## Precedence
//!
//! [`Database::open`](crate::Database::open) uses [`default_open_options`]:
//! the built-in defaults (`DEFAULT_*` below), replaced by whatever a program
//! installed with [`set_default_open_options`] (`gserver` installs
//! `conf.ini`'s `[storage]` section, [`GStoreConfig::open_options`](crate::config::GStoreConfig::open_options)),
//! with the environment on top:
//!
//! | variable | overrides | values |
//! |---|---|---|
//! | `GSTORE_DISK_QUERY_MODE` | [`disk_query_mode`](OpenOptions::disk_query_mode) | `stream` / `memory` (also `materialize`) |
//! | `GSTORE_DISK_CACHE_MB` | [`disk_cache_pages`](OpenOptions::disk_cache_pages) | MiB of 4 KiB pages, at least 1 |
//! | `GSTORE_VSTREE_ON_OPEN` | [`vstree_on_open`](OpenOptions::vstree_on_open) | `1`/`true`/`yes`/`on` or `0`/`false`/`no`/`off` |
//!
//! [`Database::open_with`](crate::Database::open_with) takes its options as
//! given: neither the installed defaults nor the environment change them (a
//! test or a tool that needs one mode says so).

use std::sync::RwLock;

use super::persist::StorageKind;
use crate::dict::cache::{DEFAULT_KEY_CACHE_BYTES, DEFAULT_TERM_CACHE_BYTES};

/// How an on-disk (`kvstore.kv`) database answers queries (see the module
/// docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiskQueryMode {
    /// Evaluate over the on-disk B+ trees and an out-of-core dictionary
    /// (`Backend::Disk`); writes go to the store by id.
    Stream,
    /// Load the dictionary and the default graph into memory at open and
    /// write every committed update through to the store.
    Materialize,
}

impl DiskQueryMode {
    /// Parse `stream` / `disk`, or `memory` / `materialize` / `materialise`
    /// (case-insensitive).
    pub fn parse(s: &str) -> Option<DiskQueryMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stream" | "disk" => Some(DiskQueryMode::Stream),
            "memory" | "materialize" | "materialise" => Some(DiskQueryMode::Materialize),
            _ => None,
        }
    }

    /// The canonical spelling: `stream` or `memory` (as `GSTORE_DISK_QUERY_MODE`
    /// and `[storage] disk_query_mode` take it).
    pub fn name(self) -> &'static str {
        match self {
            DiskQueryMode::Stream => "stream",
            DiskQueryMode::Materialize => "memory",
        }
    }
}

impl std::fmt::Display for DiskQueryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The disk query mode used when nothing else chooses one. **The one switch
/// for the default**: Stream mode becomes the default by changing this
/// constant, which Phase 6 does only once every R3-STREAM gate passes at univ
/// 38 (docs/PHASE6_PLAN.md §2 G1d, §6 R3-STREAM acceptance).
pub const DEFAULT_DISK_QUERY_MODE: DiskQueryMode = DiskQueryMode::Materialize;

/// The default page cache of an on-disk store: 4096 pages of 4 KiB (16 MiB).
pub const DEFAULT_DISK_CACHE_PAGES: usize = 4096;

/// The default byte budget of a Stream-mode dictionary's caches (id → string
/// plus string → id: 64 + 16 MiB).
pub const DEFAULT_DICT_CACHE_BYTES: usize = DEFAULT_TERM_CACHE_BYTES + DEFAULT_KEY_CACHE_BYTES;

/// The environment variable that overrides [`OpenOptions::disk_query_mode`].
pub const ENV_DISK_QUERY_MODE: &str = "GSTORE_DISK_QUERY_MODE";
/// The environment variable that overrides [`OpenOptions::disk_cache_pages`]
/// (in MiB).
pub const ENV_DISK_CACHE_MB: &str = "GSTORE_DISK_CACHE_MB";
/// The environment variable that overrides [`OpenOptions::vstree_on_open`]
/// (Round 1's switch for RocksDB, kept as an override).
pub const ENV_VSTREE_ON_OPEN: &str = "GSTORE_VSTREE_ON_OPEN";

/// 4 KiB pages per MiB.
const PAGES_PER_MIB: usize = (1 << 20) / crate::kvstore::PAGE_SIZE;

/// How [`Database::open_with`](crate::Database::open_with) opens a database
/// directory (contract C7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOptions {
    /// How an on-disk database answers queries (default
    /// [`DEFAULT_DISK_QUERY_MODE`]).
    pub disk_query_mode: DiskQueryMode,
    /// Pages (4 KiB) of an on-disk store's page cache (default
    /// [`DEFAULT_DISK_CACHE_PAGES`], 16 MiB).
    pub disk_cache_pages: usize,
    /// Bytes a Stream-mode dictionary may keep in its caches (default
    /// [`DEFAULT_DICT_CACHE_BYTES`]): four fifths for id → string, one fifth
    /// for string → id.
    pub dict_cache_bytes: usize,
    /// Build the VS-tree at open when no fresh one is persisted: a RocksDB
    /// database (which has none) builds it in memory; an on-disk database in
    /// Stream mode rebuilds a stale `vstree.kv` (a streaming pass). Off by
    /// default: such a database evaluates without the VS-tree until
    /// [`rebuild_index`](crate::Database::rebuild_index). (A Materialize-mode
    /// open always rebuilds a stale `vstree.kv`, as before.)
    pub vstree_on_open: bool,
    /// The layout new databases are built in, if configured (`[storage]
    /// default_layout`; consumed by the servers' build paths, R4-SNAP).
    pub default_layout: Option<StorageKind>,
}

impl Default for OpenOptions {
    fn default() -> OpenOptions {
        OpenOptions {
            disk_query_mode: DEFAULT_DISK_QUERY_MODE,
            disk_cache_pages: DEFAULT_DISK_CACHE_PAGES,
            dict_cache_bytes: DEFAULT_DICT_CACHE_BYTES,
            vstree_on_open: false,
            default_layout: None,
        }
    }
}

impl OpenOptions {
    /// The built-in defaults (no installed defaults, no environment).
    pub fn new() -> OpenOptions {
        OpenOptions::default()
    }

    /// These options with `mode` as the disk query mode.
    pub fn with_disk_query_mode(mut self, mode: DiskQueryMode) -> OpenOptions {
        self.disk_query_mode = mode;
        self
    }

    /// The built-in defaults in Stream mode.
    pub fn stream() -> OpenOptions {
        OpenOptions::new().with_disk_query_mode(DiskQueryMode::Stream)
    }

    /// The built-in defaults in Materialize mode.
    pub fn materialize() -> OpenOptions {
        OpenOptions::new().with_disk_query_mode(DiskQueryMode::Materialize)
    }

    /// These options with a page cache of `pages` 4 KiB pages (at least 16).
    pub fn with_disk_cache_pages(mut self, pages: usize) -> OpenOptions {
        self.disk_cache_pages = pages.max(16);
        self
    }

    /// These options with `bytes` for the Stream-mode dictionary caches.
    pub fn with_dict_cache_bytes(mut self, bytes: usize) -> OpenOptions {
        self.dict_cache_bytes = bytes;
        self
    }

    /// These options with [`vstree_on_open`](Self::vstree_on_open) set.
    pub fn with_vstree_on_open(mut self, on: bool) -> OpenOptions {
        self.vstree_on_open = on;
        self
    }

    /// The id → string and string → id budgets the dictionary caches get
    /// from [`dict_cache_bytes`](Self::dict_cache_bytes).
    pub fn dict_cache_split(&self) -> (usize, usize) {
        let keys = self.dict_cache_bytes / 5;
        (self.dict_cache_bytes - keys, keys)
    }

    /// Apply the environment overrides (see the module docs) and return a
    /// warning for each variable whose value is not understood (it is then
    /// ignored).
    pub fn apply_env(&mut self) -> Vec<String> {
        self.apply_vars(|name| std::env::var(name).ok())
    }

    /// [`apply_env`](Self::apply_env) over an arbitrary variable source.
    pub(crate) fn apply_vars(&mut self, var: impl Fn(&str) -> Option<String>) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(v) = var(ENV_DISK_QUERY_MODE).filter(|v| !v.trim().is_empty()) {
            match DiskQueryMode::parse(&v) {
                Some(m) => self.disk_query_mode = m,
                None => warnings.push(format!(
                    "{ENV_DISK_QUERY_MODE}={v:?} is not 'stream' or 'memory'; ignored"
                )),
            }
        }
        if let Some(v) = var(ENV_DISK_CACHE_MB).filter(|v| !v.trim().is_empty()) {
            match v.trim().parse::<usize>() {
                Ok(mb) if mb >= 1 => {
                    self.disk_cache_pages = mb.saturating_mul(PAGES_PER_MIB);
                }
                _ => warnings.push(format!(
                    "{ENV_DISK_CACHE_MB}={v:?} is not a positive number of MiB; ignored"
                )),
            }
        }
        if let Some(v) = var(ENV_VSTREE_ON_OPEN).filter(|v| !v.trim().is_empty()) {
            match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => self.vstree_on_open = true,
                "0" | "false" | "no" | "off" => self.vstree_on_open = false,
                _ => warnings.push(format!(
                    "{ENV_VSTREE_ON_OPEN}={v:?} is not on or off; ignored"
                )),
            }
        }
        warnings
    }
}

/// The options installed with [`set_default_open_options`].
static INSTALLED: RwLock<Option<OpenOptions>> = RwLock::new(None);

/// Install `options` as this process's defaults for
/// [`Database::open`](crate::Database::open) (the environment still overrides
/// them, see [`default_open_options`]). `gserver` installs `conf.ini`'s
/// `[storage]` section at startup.
pub fn set_default_open_options(options: OpenOptions) {
    *INSTALLED.write().unwrap_or_else(|e| e.into_inner()) = Some(options);
}

/// The options [`Database::open`](crate::Database::open) uses: the installed
/// defaults ([`set_default_open_options`]) or the built-in ones, with the
/// environment overrides applied (an unreadable value is logged and ignored).
pub fn default_open_options() -> OpenOptions {
    let mut options = INSTALLED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_default();
    for w in options.apply_env() {
        crate::gs_warn!("open options: {w}");
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_and_print() {
        for (text, mode) in [
            ("stream", DiskQueryMode::Stream),
            (" Disk ", DiskQueryMode::Stream),
            ("memory", DiskQueryMode::Materialize),
            ("MATERIALIZE", DiskQueryMode::Materialize),
            ("materialise", DiskQueryMode::Materialize),
        ] {
            assert_eq!(DiskQueryMode::parse(text), Some(mode), "{text}");
            assert_eq!(DiskQueryMode::parse(mode.name()), Some(mode));
        }
        assert_eq!(DiskQueryMode::parse("fast"), None);
        assert_eq!(DiskQueryMode::Stream.to_string(), "stream");
    }

    #[test]
    fn defaults_are_the_documented_constants() {
        let o = OpenOptions::default();
        assert_eq!(o.disk_query_mode, DEFAULT_DISK_QUERY_MODE);
        assert_eq!(o.disk_cache_pages, DEFAULT_DISK_CACHE_PAGES);
        assert_eq!(o.dict_cache_bytes, DEFAULT_DICT_CACHE_BYTES);
        assert_eq!(
            o.dict_cache_split(),
            (DEFAULT_TERM_CACHE_BYTES, DEFAULT_KEY_CACHE_BYTES)
        );
        assert!(!o.vstree_on_open && o.default_layout.is_none());
        assert_eq!(
            OpenOptions::stream().disk_query_mode,
            DiskQueryMode::Stream
        );
        assert_eq!(
            OpenOptions::materialize().disk_query_mode,
            DiskQueryMode::Materialize
        );
    }

    #[test]
    fn the_environment_overrides_and_bad_values_warn() {
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let mut o = OpenOptions::materialize();
        let w = o.apply_vars(vars(&[
            (ENV_DISK_QUERY_MODE, "stream"),
            (ENV_DISK_CACHE_MB, "64"),
            (ENV_VSTREE_ON_OPEN, "on"),
        ]));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(o.disk_query_mode, DiskQueryMode::Stream);
        assert_eq!(o.disk_cache_pages, 64 * 256);
        assert!(o.vstree_on_open);

        let mut o = OpenOptions::stream();
        let w = o.apply_vars(vars(&[
            (ENV_DISK_QUERY_MODE, "fastest"),
            (ENV_DISK_CACHE_MB, "0"),
            (ENV_VSTREE_ON_OPEN, "maybe"),
        ]));
        assert_eq!(w.len(), 3, "{w:?}");
        assert_eq!(o, OpenOptions::stream(), "bad values are ignored");

        // Empty values count as unset.
        let mut o = OpenOptions::stream();
        assert!(o.apply_vars(vars(&[(ENV_DISK_QUERY_MODE, " ")])).is_empty());
        assert_eq!(o.disk_query_mode, DiskQueryMode::Stream);
    }
}
