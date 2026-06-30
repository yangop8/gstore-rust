//! An out-of-core dictionary backend over the on-disk B+trees.
//!
//! Mirrors gStore resolving terms through its `entity2id`/`id2entity` (and the
//! literal/predicate) trees via the buffer cache, rather than holding the whole
//! string dictionary in RAM. A [`DiskDict`] plugs into [`crate::dict::Dictionary`]
//! as its [`DiskTermSource`]; the query engine then runs over `&Dictionary`
//! unchanged while only the *looked-up* strings ever become resident.
//!
//! ## Residency / memory
//! * **term → id** lookups (query constants) hit the B+tree on every call — no
//!   string is cached for this direction, matching gStore's per-lookup resolve.
//! * **id → str** materialization (result rows, FILTER values) fetches the
//!   string from the B+tree once and retains it, so repeated resolves of the
//!   same id are free and `resident_string_count` reflects exactly the touched
//!   subset.
//!
//! Retained strings are `Box::leak`ed so `id_to_string` can hand back a `&str`
//! valid for the dictionary's lifetime while the type stays `Send + Sync` and
//! `unsafe`-free (a `RefCell`/`Mutex` guard cannot lend a `&str` past the call).
//! Because `DiskStore` keeps one `DiskDict` and reuses it across queries, the
//! leak is *bounded by the number of distinct terms ever materialized* — never
//! more than the full dictionary, i.e. no worse than the old eager load, and
//! typically far less. (Freeing on drop instead would require either `unsafe` —
//! which this codebase avoids — or a stable string arena; see the backlog note.)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::dict::DiskTermSource;
use crate::model::id::{is_literal_id, EntityLiteralId, PredId};

use super::bptree::{be32, de32, BTree};
use super::pager::Pager;

/// Out-of-core dictionary: resolves str↔id from the on-disk B+trees on demand.
pub struct DiskDict {
    /// Shared with the owning `DiskStore`, so both read through one page cache.
    pager: Arc<Mutex<Pager>>,
    entity2id: BTree,
    literal2id: BTree,
    predicate2id: BTree,
    id2entity: BTree,
    id2literal: BTree,
    id2predicate: BTree,
    entity_count: usize,
    literal_count: usize,
    pred_count: usize,
    /// Materialized entity/literal strings, keyed by public id (see module docs).
    ent_lit_cache: Mutex<HashMap<EntityLiteralId, &'static str>>,
    /// Materialized predicate strings, keyed by predicate id.
    pred_cache: Mutex<HashMap<PredId, &'static str>>,
}

impl DiskDict {
    /// Build a backend sharing `pager` with its `DiskStore`. The `BTree` handles
    /// are copies of the store's dictionary trees; `*_count` are the live term
    /// counts captured when the backend is created.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pager: Arc<Mutex<Pager>>,
        entity2id: BTree,
        literal2id: BTree,
        predicate2id: BTree,
        id2entity: BTree,
        id2literal: BTree,
        id2predicate: BTree,
        entity_count: usize,
        literal_count: usize,
        pred_count: usize,
    ) -> DiskDict {
        DiskDict {
            pager,
            entity2id,
            literal2id,
            predicate2id,
            id2entity,
            id2literal,
            id2predicate,
            entity_count,
            literal_count,
            pred_count,
            ent_lit_cache: Mutex::new(HashMap::new()),
            pred_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve `key` to its id through `tree` (a `*2id` forward tree). Hits the
    /// B+tree every call; nothing is cached for the term→id direction.
    fn fetch_id(&self, tree: &BTree, key: &str) -> Option<u32> {
        let mut pager = self.pager.lock().unwrap();
        tree.get(&mut pager, key.as_bytes())
            .ok()
            .flatten()
            .map(|v| de32(&v))
    }

    /// Fetch the string for `id` from `tree` (an `id2*` reverse tree) and retain
    /// it (leaked) so the returned reference is valid for the backend's life.
    fn fetch_str(&self, tree: &BTree, id: u32) -> Option<&'static str> {
        let bytes = {
            let mut pager = self.pager.lock().unwrap();
            tree.get(&mut pager, &be32(id)).ok().flatten()?
        };
        let s = String::from_utf8_lossy(&bytes).into_owned();
        Some(Box::leak(s.into_boxed_str()))
    }
}

impl DiskTermSource for DiskDict {
    fn entity_id(&self, key: &str) -> Option<EntityLiteralId> {
        self.fetch_id(&self.entity2id, key)
    }

    fn literal_id(&self, key: &str) -> Option<EntityLiteralId> {
        // The forward tree already stores the *public* literal id (offset by
        // LITERAL_FIRST_ID), so the fetched value needs no further adjustment.
        self.fetch_id(&self.literal2id, key)
    }

    fn predicate_id(&self, key: &str) -> Option<PredId> {
        self.fetch_id(&self.predicate2id, key)
    }

    fn id_to_string(&self, id: EntityLiteralId) -> Option<&str> {
        if let Some(&s) = self.ent_lit_cache.lock().unwrap().get(&id) {
            return Some(s);
        }
        let tree = if is_literal_id(id) {
            &self.id2literal
        } else {
            &self.id2entity
        };
        let s = self.fetch_str(tree, id)?;
        self.ent_lit_cache.lock().unwrap().insert(id, s);
        Some(s)
    }

    fn predicate_to_string(&self, id: PredId) -> Option<&str> {
        if let Some(&s) = self.pred_cache.lock().unwrap().get(&id) {
            return Some(s);
        }
        let s = self.fetch_str(&self.id2predicate, id)?;
        self.pred_cache.lock().unwrap().insert(id, s);
        Some(s)
    }

    fn entity_num(&self) -> usize {
        self.entity_count
    }
    fn literal_num(&self) -> usize {
        self.literal_count
    }
    fn predicate_num(&self) -> usize {
        self.pred_count
    }

    fn resident_string_count(&self) -> usize {
        self.ent_lit_cache.lock().unwrap().len() + self.pred_cache.lock().unwrap().len()
    }
}

impl std::fmt::Debug for DiskDict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskDict")
            .field("entity_count", &self.entity_count)
            .field("literal_count", &self.literal_count)
            .field("pred_count", &self.pred_count)
            .field("resident_strings", &self.resident_string_count())
            .finish()
    }
}
