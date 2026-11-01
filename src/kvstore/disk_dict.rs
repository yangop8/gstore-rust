//! An out-of-core dictionary backend over the on-disk B+trees.
//!
//! Mirrors gStore resolving terms through its `entity2id`/`id2entity` (and the
//! literal/predicate) trees via the buffer cache, rather than holding the whole
//! string dictionary in RAM. A [`DiskDict`] plugs into [`crate::dict::Dictionary`]
//! as its [`DiskTermSource`]; the query engine then runs over `&Dictionary`
//! unchanged while only the *looked-up* strings ever become resident.
//!
//! Resolved strings are handed out as shared `Arc<str>` (no `Box::leak`).

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, RwLock};

use crate::dict::{DiskTermSource, Space};
use crate::error::Result;
use crate::model::id::LITERAL_FIRST_ID;

use super::bptree::{de32, BTree};
use super::ivarray::IvArray;
use super::pager::Pager;

/// Out-of-core dictionary: resolves str↔id from the on-disk B+trees on demand.
pub struct DiskDict {
    /// Shared with the owning `DiskStore`, so both read through one page cache.
    pager: Arc<RwLock<Pager>>,
    entity2id: BTree,
    literal2id: BTree,
    predicate2id: BTree,
    id2entity: IvArray,
    id2literal: IvArray,
    id2predicate: IvArray,
    entity_count: usize,
    literal_count: usize,
    pred_count: usize,
    /// Materialized strings, keyed by `(space, id)`.
    resolved: Mutex<HashMap<(Space, u32), Arc<str>>>,
}

impl DiskDict {
    /// Build a backend sharing `pager` with its `DiskStore`. The `BTree` handles
    /// are copies of the store's dictionary trees; `*_count` are the live term
    /// counts captured when the backend is created.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pager: Arc<RwLock<Pager>>,
        entity2id: BTree,
        literal2id: BTree,
        predicate2id: BTree,
        id2entity: IvArray,
        id2literal: IvArray,
        id2predicate: IvArray,
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
            resolved: Mutex::new(HashMap::new()),
        }
    }

    fn tree(&self, space: Space) -> &BTree {
        match space {
            Space::Entity => &self.entity2id,
            Space::Literal => &self.literal2id,
            Space::Predicate => &self.predicate2id,
        }
    }

    /// The `id2*` array of `space` and the array key of public id `id`.
    fn array(&self, space: Space, id: u32) -> Option<(&IvArray, u32)> {
        match space {
            Space::Entity => Some((&self.id2entity, id)),
            Space::Literal => Some((&self.id2literal, id.checked_sub(LITERAL_FIRST_ID)?)),
            Space::Predicate => Some((&self.id2predicate, id)),
        }
    }
}

impl DiskTermSource for DiskDict {
    fn lookup(&self, space: Space, key: &str) -> Result<Option<u32>> {
        let pager = self.pager.read().unwrap();
        // The literal tree stores the *public* id (offset by LITERAL_FIRST_ID).
        self.tree(space).get_with(&pager, key.as_bytes(), de32)
    }

    fn resolve(&self, space: Space, id: u32) -> Result<Option<Arc<str>>> {
        if let Some(s) = self.resolved.lock().unwrap().get(&(space, id)) {
            return Ok(Some(Arc::clone(s)));
        }
        let Some((arr, key)) = self.array(space, id) else {
            return Ok(None);
        };
        if key >= self.id_limit(space) - space.first_id() {
            return Ok(None);
        }
        let s = {
            let pager = self.pager.read().unwrap();
            arr.get_with(&pager, key, |b| {
                Arc::<str>::from(String::from_utf8_lossy(b))
            })?
        };
        if let Some(s) = &s {
            self.resolved
                .lock()
                .unwrap()
                .insert((space, id), Arc::clone(s));
        }
        Ok(s)
    }

    fn for_each(
        &self,
        space: Space,
        from: u32,
        f: &mut dyn FnMut(u32, &str) -> ControlFlow<()>,
    ) -> Result<()> {
        let first = space.first_id();
        let Some((arr, start)) = self.array(space, from.max(first)) else {
            return Ok(());
        };
        let end = self.id_limit(space) - first;
        let pager = self.pager.read().unwrap();
        arr.for_each_range(&pager, start, end, |k, v| {
            f(first + k, &String::from_utf8_lossy(v))
        })
    }

    fn count(&self, space: Space) -> usize {
        match space {
            Space::Entity => self.entity_count,
            Space::Literal => self.literal_count,
            Space::Predicate => self.pred_count,
        }
    }

    fn id_limit(&self, space: Space) -> u32 {
        space.first_id() + self.count(space) as u32
    }

    fn resident_string_count(&self) -> usize {
        self.resolved.lock().unwrap().len()
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
