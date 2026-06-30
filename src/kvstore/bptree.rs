//! A disk-backed B+ tree (variable-length byte key → variable-length byte value).
//!
//! Corresponds to gStore's `SITree`/`Tree` B+ trees. Nodes live in [`Pager`]
//! pages; this implementation parses a page into an in-memory [`Node`], mutates
//! it, and writes it back (correctness-first; the page cache keeps hot nodes in
//! memory). Leaves are linked for ordered range scans, which give the RDF store
//! its prefix lookups (e.g. all `(p, o)` for a subject = prefix scan on SPO).
//!
//! Both keys and values are byte strings: keys carry composite triple keys or
//! dictionary strings; values carry ids (as bytes) or, for `id → string`, the
//! string itself.

use crate::error::{GStoreError, Result};

use super::pager::{PageId, Pager, PAGE_SIZE};

/// A handle to one B+ tree, rooted at a [`Pager`] header slot.
pub struct BTree {
    root_slot: usize,
}

/// An in-memory view of a node page.
struct Node {
    is_leaf: bool,
    /// Leaf: next leaf page (0 = none). Internal: leftmost child (`child0`).
    link: PageId,
    /// Leaf: keys. Internal: separator keys.
    keys: Vec<Vec<u8>>,
    /// Leaf: values (parallel to `keys`).
    vals: Vec<Vec<u8>>,
    /// Internal: children for keys `>= separator` (parallel to `keys`);
    /// `link` is `child0` for keys `< keys[0]`.
    children: Vec<PageId>,
}

impl Node {
    fn leaf() -> Node {
        Node {
            is_leaf: true,
            link: 0,
            keys: Vec::new(),
            vals: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Serialized byte size of this node.
    fn size(&self) -> usize {
        let mut n = 1 + 4 + 2; // is_leaf + link + count
        for (i, k) in self.keys.iter().enumerate() {
            n += 2 + k.len();
            if self.is_leaf {
                n += 2 + self.vals[i].len();
            } else {
                n += 4;
            }
        }
        n
    }

    fn serialize(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = self.is_leaf as u8;
        buf[1..5].copy_from_slice(&self.link.to_le_bytes());
        buf[5..7].copy_from_slice(&(self.keys.len() as u16).to_le_bytes());
        let mut off = 7;
        for (i, k) in self.keys.iter().enumerate() {
            buf[off..off + 2].copy_from_slice(&(k.len() as u16).to_le_bytes());
            off += 2;
            buf[off..off + k.len()].copy_from_slice(k);
            off += k.len();
            if self.is_leaf {
                let v = &self.vals[i];
                buf[off..off + 2].copy_from_slice(&(v.len() as u16).to_le_bytes());
                off += 2;
                buf[off..off + v.len()].copy_from_slice(v);
                off += v.len();
            } else {
                buf[off..off + 4].copy_from_slice(&self.children[i].to_le_bytes());
                off += 4;
            }
        }
        buf
    }

    fn parse(page: &[u8; PAGE_SIZE]) -> Node {
        let is_leaf = page[0] != 0;
        let link = u32::from_le_bytes(page[1..5].try_into().unwrap());
        let count = u16::from_le_bytes(page[5..7].try_into().unwrap()) as usize;
        let mut keys = Vec::with_capacity(count);
        let mut vals = Vec::new();
        let mut children = Vec::new();
        let mut off = 7;
        for _ in 0..count {
            let klen = u16::from_le_bytes(page[off..off + 2].try_into().unwrap()) as usize;
            off += 2;
            keys.push(page[off..off + klen].to_vec());
            off += klen;
            if is_leaf {
                let vlen = u16::from_le_bytes(page[off..off + 2].try_into().unwrap()) as usize;
                off += 2;
                vals.push(page[off..off + vlen].to_vec());
                off += vlen;
            } else {
                children.push(u32::from_le_bytes(page[off..off + 4].try_into().unwrap()));
                off += 4;
            }
        }
        Node {
            is_leaf,
            link,
            keys,
            vals,
            children,
        }
    }

    /// Internal: child page to descend into for `key`.
    fn child_for(&self, key: &[u8]) -> PageId {
        let i = self.keys.partition_point(|k| k.as_slice() <= key);
        if i == 0 {
            self.link
        } else {
            self.children[i - 1]
        }
    }

    /// Internal: the full child vector `[child0, children…]` (`child0` == `link`),
    /// of length `keys.len() + 1`. Convenient for delete-time rebalancing, where
    /// borrow/merge rotate children through the parent separators.
    fn children_vec(&self) -> Vec<PageId> {
        let mut v = Vec::with_capacity(self.children.len() + 1);
        v.push(self.link);
        v.extend_from_slice(&self.children);
        v
    }

    /// Internal: rebuild `link`/`children` from a full child vector.
    fn set_children_vec(&mut self, all: Vec<PageId>) {
        self.link = all[0];
        self.children = all[1..].to_vec();
    }
}

/// The outcome of inserting into a subtree: a split that must propagate up.
type Split = Option<(Vec<u8>, PageId)>;

/// A non-root node holding fewer than this many serialized bytes is "underfull"
/// after a delete and triggers rebalancing (borrow from a sibling, else merge).
/// 25% fill mirrors the classic half-merge / quarter-borrow B+ tree policy while
/// staying robust to gStore's variable-length keys.
const UNDERFLOW_BYTES: usize = PAGE_SIZE / 4;

impl BTree {
    pub fn new(root_slot: usize) -> BTree {
        BTree { root_slot }
    }

    fn root(&self, pager: &Pager) -> PageId {
        pager.root(self.root_slot) as PageId
    }

    fn set_root(&self, pager: &mut Pager, id: PageId) {
        pager.set_root(self.root_slot, id as u64);
    }

    /// Insert or replace `key → val`.
    pub fn insert(&self, pager: &mut Pager, key: &[u8], val: &[u8]) -> Result<()> {
        let root = self.root(pager);
        if root == 0 {
            let id = pager.alloc()?;
            let mut leaf = Node::leaf();
            leaf.keys.push(key.to_vec());
            leaf.vals.push(val.to_vec());
            pager.write_page(id, &leaf.serialize())?;
            self.set_root(pager, id);
            return Ok(());
        }
        if let Some((sep, right)) = self.insert_rec(pager, root, key, val)? {
            let new_root = pager.alloc()?;
            let node = Node {
                is_leaf: false,
                link: root,
                keys: vec![sep],
                vals: Vec::new(),
                children: vec![right],
            };
            pager.write_page(new_root, &node.serialize())?;
            self.set_root(pager, new_root);
        }
        Ok(())
    }

    fn insert_rec(&self, pager: &mut Pager, page: PageId, key: &[u8], val: &[u8]) -> Result<Split> {
        let mut node = Node::parse(&pager.read_page(page)?);
        if node.is_leaf {
            match node.keys.binary_search_by(|k| k.as_slice().cmp(key)) {
                Ok(i) => node.vals[i] = val.to_vec(),
                Err(i) => {
                    node.keys.insert(i, key.to_vec());
                    node.vals.insert(i, val.to_vec());
                }
            }
            if node.size() <= PAGE_SIZE {
                pager.write_page(page, &node.serialize())?;
                Ok(None)
            } else {
                self.split_leaf(pager, page, node)
            }
        } else {
            let child = node.child_for(key);
            let pos = node.keys.partition_point(|k| k.as_slice() <= key);
            if let Some((sep, right)) = self.insert_rec(pager, child, key, val)? {
                node.keys.insert(pos, sep);
                node.children.insert(pos, right);
                if node.size() <= PAGE_SIZE {
                    pager.write_page(page, &node.serialize())?;
                    Ok(None)
                } else {
                    self.split_internal(pager, page, node)
                }
            } else {
                Ok(None)
            }
        }
    }

    fn split_leaf(&self, pager: &mut Pager, page: PageId, mut node: Node) -> Result<Split> {
        let mid = node.keys.len() / 2;
        let right_keys = node.keys.split_off(mid);
        let right_vals = node.vals.split_off(mid);
        let sep = right_keys[0].clone();
        let right_id = pager.alloc()?;
        let right = Node {
            is_leaf: true,
            link: node.link,
            keys: right_keys,
            vals: right_vals,
            children: Vec::new(),
        };
        node.link = right_id;
        pager.write_page(right_id, &right.serialize())?;
        pager.write_page(page, &node.serialize())?;
        Ok(Some((sep, right_id)))
    }

    fn split_internal(&self, pager: &mut Pager, page: PageId, mut node: Node) -> Result<Split> {
        // An internal split pushes exactly one separator up, so a node with a
        // single separator can't be split into two key-bearing internal nodes:
        // pushing the lone key up leaves both halves with zero keys (a
        // degenerate node that no longer routes between children). We only reach
        // here when the node already overflows a page, so a single separator
        // means that one key plus its child link is too large to ever fit.
        if node.keys.len() < 2 {
            return Err(GStoreError::Serialize(format!(
                "B+ tree separator key too large to fit in a {PAGE_SIZE}-byte page"
            )));
        }
        let mid = node.keys.len() / 2;
        let sep = node.keys[mid].clone();
        let right_keys = node.keys.split_off(mid + 1);
        let right_children = node.children.split_off(mid + 1);
        node.keys.pop(); // median separator moves up
        let right_child0 = node.children.pop().unwrap();
        let right_id = pager.alloc()?;
        let right = Node {
            is_leaf: false,
            link: right_child0,
            keys: right_keys,
            vals: Vec::new(),
            children: right_children,
        };
        pager.write_page(right_id, &right.serialize())?;
        pager.write_page(page, &node.serialize())?;
        Ok(Some((sep, right_id)))
    }

    /// Look up the value for `key`.
    pub fn get(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut page = self.root(pager);
        if page == 0 {
            return Ok(None);
        }
        loop {
            let node = Node::parse(&pager.read_page(page)?);
            if node.is_leaf {
                return Ok(node
                    .keys
                    .binary_search_by(|k| k.as_slice().cmp(key))
                    .ok()
                    .map(|i| node.vals[i].clone()));
            }
            page = node.child_for(key);
        }
    }

    /// All entries whose key starts with `prefix`, in key order.
    pub fn scan_prefix(&self, pager: &mut Pager, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        let Some((mut page, mut idx)) = self.find_leaf_ge(pager, prefix)? else {
            return Ok(out);
        };
        loop {
            let node = Node::parse(&pager.read_page(page)?);
            while idx < node.keys.len() {
                let k = &node.keys[idx];
                if k.starts_with(prefix) {
                    out.push((k.clone(), node.vals[idx].clone()));
                } else if k.as_slice() > prefix {
                    return Ok(out);
                }
                idx += 1;
            }
            if node.link == 0 {
                return Ok(out);
            }
            page = node.link;
            idx = 0;
        }
    }

    /// All entries in key order.
    pub fn iter_all(&self, pager: &mut Pager) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_prefix(pager, &[])
    }

    fn find_leaf_ge(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<(PageId, usize)>> {
        let mut page = self.root(pager);
        if page == 0 {
            return Ok(None);
        }
        loop {
            let node = Node::parse(&pager.read_page(page)?);
            if node.is_leaf {
                let idx = node.keys.partition_point(|k| k.as_slice() < key);
                return Ok(Some((page, idx)));
            }
            page = node.child_for(key);
        }
    }

    // ---- deletion (gStore tree node merge / redistribution) ---------------

    /// Remove `key`. Returns `true` if it was present. Empty leaves and
    /// single-child internal nodes are merged away, shrinking the tree and
    /// returning freed pages to the pager's free list. The leaf-link chain and
    /// every separator stay consistent, so subsequent `get`/`scan_prefix` see a
    /// well-formed tree.
    pub fn delete(&self, pager: &mut Pager, key: &[u8]) -> Result<bool> {
        let root = self.root(pager);
        if root == 0 {
            return Ok(false);
        }
        let removed = self.delete_rec(pager, root, key)?;
        if !removed {
            return Ok(false);
        }
        // Shrink the root: an empty leaf clears the tree; an internal node that
        // lost its last separator is replaced by its sole remaining child.
        let node = Node::parse(&pager.read_page(root)?);
        if node.is_leaf {
            if node.keys.is_empty() {
                pager.free(root)?;
                self.set_root(pager, 0);
            }
        } else if node.keys.is_empty() {
            let only_child = node.link;
            self.set_root(pager, only_child);
            pager.free(root)?;
        }
        Ok(true)
    }

    fn delete_rec(&self, pager: &mut Pager, page: PageId, key: &[u8]) -> Result<bool> {
        let mut node = Node::parse(&pager.read_page(page)?);
        if node.is_leaf {
            return match node.keys.binary_search_by(|k| k.as_slice().cmp(key)) {
                Ok(i) => {
                    node.keys.remove(i);
                    node.vals.remove(i);
                    pager.write_page(page, &node.serialize())?;
                    Ok(true)
                }
                Err(_) => Ok(false),
            };
        }
        let ci = node.keys.partition_point(|k| k.as_slice() <= key);
        let child_page = if ci == 0 { node.link } else { node.children[ci - 1] };
        let removed = self.delete_rec(pager, child_page, key)?;
        if removed {
            let child = Node::parse(&pager.read_page(child_page)?);
            if child.size() < UNDERFLOW_BYTES {
                self.rebalance(pager, &mut node, page, ci)?;
            }
        }
        Ok(removed)
    }

    /// Repair an underfull child at index `ci` (in the parent's full child
    /// vector) by borrowing one entry from a sibling, or merging with one. The
    /// parent is mutated in place and written back here; sibling/child pages are
    /// rewritten and any emptied page is freed. Projected sizes are checked
    /// exactly so a node never overflows its page.
    fn rebalance(
        &self,
        pager: &mut Pager,
        parent: &mut Node,
        parent_page: PageId,
        ci: usize,
    ) -> Result<()> {
        let all = parent.children_vec();
        let child = Node::parse(&pager.read_page(all[ci])?);

        // Borrow from the left sibling.
        if ci > 0 {
            let left = Node::parse(&pager.read_page(all[ci - 1])?);
            if left.keys.len() >= 2 {
                let entry = boundary_bytes(&left, left.keys.len() - 1);
                let sep_len = parent.keys[ci - 1].len();
                let child_gain = if child.is_leaf { entry } else { 6 + sep_len };
                if left.size() - entry >= UNDERFLOW_BYTES
                    && child.size() + child_gain <= PAGE_SIZE
                {
                    self.borrow_from_left(pager, parent, ci)?;
                    pager.write_page(parent_page, &parent.serialize())?;
                    return Ok(());
                }
            }
        }
        // Borrow from the right sibling.
        if ci + 1 < all.len() {
            let right = Node::parse(&pager.read_page(all[ci + 1])?);
            if right.keys.len() >= 2 {
                let entry = boundary_bytes(&right, 0);
                let sep_len = parent.keys[ci].len();
                let child_gain = if child.is_leaf { entry } else { 6 + sep_len };
                if right.size() - entry >= UNDERFLOW_BYTES
                    && child.size() + child_gain <= PAGE_SIZE
                {
                    self.borrow_from_right(pager, parent, ci)?;
                    pager.write_page(parent_page, &parent.serialize())?;
                    return Ok(());
                }
            }
        }
        // Merge with the left sibling.
        if ci > 0 {
            let left = Node::parse(&pager.read_page(all[ci - 1])?);
            let merged = if child.is_leaf {
                left.size() + child.size() - 7
            } else {
                left.size() + child.size() - 1 + parent.keys[ci - 1].len()
            };
            if merged <= PAGE_SIZE {
                self.merge_into_left(pager, parent, ci)?;
                pager.write_page(parent_page, &parent.serialize())?;
                return Ok(());
            }
        }
        // Merge the right sibling into the child.
        if ci + 1 < all.len() {
            let right = Node::parse(&pager.read_page(all[ci + 1])?);
            let merged = if child.is_leaf {
                child.size() + right.size() - 7
            } else {
                child.size() + right.size() - 1 + parent.keys[ci].len()
            };
            if merged <= PAGE_SIZE {
                self.merge_right_into(pager, parent, ci)?;
                pager.write_page(parent_page, &parent.serialize())?;
                return Ok(());
            }
        }
        // No feasible rebalance (e.g. lone huge entries): leave the node
        // underfull. The tree stays correct, only slightly less compact.
        Ok(())
    }

    /// Move the left sibling's last entry into the front of child `ci`.
    fn borrow_from_left(&self, pager: &mut Pager, parent: &mut Node, ci: usize) -> Result<()> {
        let all = parent.children_vec();
        let (lp, cp) = (all[ci - 1], all[ci]);
        let mut left = Node::parse(&pager.read_page(lp)?);
        let mut child = Node::parse(&pager.read_page(cp)?);
        if child.is_leaf {
            let k = left.keys.pop().unwrap();
            let v = left.vals.pop().unwrap();
            child.keys.insert(0, k);
            child.vals.insert(0, v);
            parent.keys[ci - 1] = child.keys[0].clone();
        } else {
            let sep = parent.keys[ci - 1].clone();
            let mut left_all = left.children_vec();
            let moved = left_all.pop().unwrap();
            let lk = left.keys.pop().unwrap();
            left.set_children_vec(left_all);
            child.keys.insert(0, sep);
            let mut child_all = child.children_vec();
            child_all.insert(0, moved);
            child.set_children_vec(child_all);
            parent.keys[ci - 1] = lk;
        }
        pager.write_page(lp, &left.serialize())?;
        pager.write_page(cp, &child.serialize())?;
        Ok(())
    }

    /// Move the right sibling's first entry onto the end of child `ci`.
    fn borrow_from_right(&self, pager: &mut Pager, parent: &mut Node, ci: usize) -> Result<()> {
        let all = parent.children_vec();
        let (cp, rp) = (all[ci], all[ci + 1]);
        let mut child = Node::parse(&pager.read_page(cp)?);
        let mut right = Node::parse(&pager.read_page(rp)?);
        if child.is_leaf {
            let k = right.keys.remove(0);
            let v = right.vals.remove(0);
            child.keys.push(k);
            child.vals.push(v);
            parent.keys[ci] = right.keys[0].clone();
        } else {
            let sep = parent.keys[ci].clone();
            child.keys.push(sep);
            let mut right_all = right.children_vec();
            let moved = right_all.remove(0);
            let rk = right.keys.remove(0);
            right.set_children_vec(right_all);
            let mut child_all = child.children_vec();
            child_all.push(moved);
            child.set_children_vec(child_all);
            parent.keys[ci] = rk;
        }
        pager.write_page(cp, &child.serialize())?;
        pager.write_page(rp, &right.serialize())?;
        Ok(())
    }

    /// Merge child `ci` into its left sibling (`ci-1`); free child `ci`'s page.
    fn merge_into_left(&self, pager: &mut Pager, parent: &mut Node, ci: usize) -> Result<()> {
        let all = parent.children_vec();
        let (lp, cp) = (all[ci - 1], all[ci]);
        let mut left = Node::parse(&pager.read_page(lp)?);
        let child = Node::parse(&pager.read_page(cp)?);
        if child.is_leaf {
            left.keys.extend(child.keys);
            left.vals.extend(child.vals);
            left.link = child.link;
        } else {
            let sep = parent.keys[ci - 1].clone();
            let mut left_all = left.children_vec();
            left_all.extend(child.children_vec());
            left.keys.push(sep);
            left.keys.extend(child.keys);
            left.set_children_vec(left_all);
        }
        parent.keys.remove(ci - 1);
        parent.children.remove(ci - 1);
        pager.write_page(lp, &left.serialize())?;
        pager.free(cp)?;
        Ok(())
    }

    /// Merge the right sibling (`ci+1`) into child `ci`; free the right page.
    fn merge_right_into(&self, pager: &mut Pager, parent: &mut Node, ci: usize) -> Result<()> {
        let all = parent.children_vec();
        let (cp, rp) = (all[ci], all[ci + 1]);
        let mut child = Node::parse(&pager.read_page(cp)?);
        let right = Node::parse(&pager.read_page(rp)?);
        if child.is_leaf {
            child.keys.extend(right.keys);
            child.vals.extend(right.vals);
            child.link = right.link;
        } else {
            let sep = parent.keys[ci].clone();
            let mut child_all = child.children_vec();
            child_all.extend(right.children_vec());
            child.keys.push(sep);
            child.keys.extend(right.keys);
            child.set_children_vec(child_all);
        }
        parent.keys.remove(ci);
        parent.children.remove(ci);
        pager.write_page(cp, &child.serialize())?;
        pager.free(rp)?;
        Ok(())
    }
}

/// Serialized byte size of the entry at index `i` (a key+value for a leaf, or a
/// separator key + one child pointer for an internal node) — used to project a
/// node's size before borrowing.
fn boundary_bytes(node: &Node, i: usize) -> usize {
    if node.is_leaf {
        2 + node.keys[i].len() + 2 + node.vals[i].len()
    } else {
        2 + node.keys[i].len() + 4
    }
}

/// Encode a `u32` big-endian (byte order == numeric order, for range scans).
pub fn be32(x: u32) -> [u8; 4] {
    x.to_be_bytes()
}

/// Decode a 4-byte big-endian slice to `u32`.
pub fn de32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_bt_{tag}.kv"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn insert_get_basic() {
        let path = tmp("basic");
        let mut pg = Pager::open(&path, 64).unwrap();
        let t = BTree::new(0);
        t.insert(&mut pg, b"alice", b"1").unwrap();
        t.insert(&mut pg, b"bob", b"2").unwrap();
        t.insert(&mut pg, b"carol", b"3").unwrap();
        assert_eq!(t.get(&mut pg, b"bob").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(
            t.get(&mut pg, b"alice").unwrap().as_deref(),
            Some(&b"1"[..])
        );
        assert_eq!(t.get(&mut pg, b"dave").unwrap(), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replace_value() {
        let path = tmp("replace");
        let mut pg = Pager::open(&path, 64).unwrap();
        let t = BTree::new(0);
        t.insert(&mut pg, b"k", b"1").unwrap();
        t.insert(&mut pg, b"k", b"99").unwrap();
        assert_eq!(t.get(&mut pg, b"k").unwrap().as_deref(), Some(&b"99"[..]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn many_inserts_force_splits_and_stay_correct() {
        let path = tmp("splits");
        let mut pg = Pager::open(&path, 64).unwrap();
        let t = BTree::new(0);
        let n = 5000u32;
        for i in 0..n {
            t.insert(&mut pg, format!("key{i:08}").as_bytes(), &be32(i))
                .unwrap();
        }
        for i in 0..n {
            assert_eq!(
                t.get(&mut pg, format!("key{i:08}").as_bytes()).unwrap(),
                Some(be32(i).to_vec()),
                "key {i}"
            );
        }
        assert!(pg.page_count() > 2, "expected splits to allocate pages");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn prefix_scan() {
        let path = tmp("prefix");
        let mut pg = Pager::open(&path, 64).unwrap();
        let t = BTree::new(0);
        let key = |s: u32, p: u32, o: u32| {
            let mut k = Vec::with_capacity(12);
            k.extend_from_slice(&be32(s));
            k.extend_from_slice(&be32(p));
            k.extend_from_slice(&be32(o));
            k
        };
        t.insert(&mut pg, &key(1, 10, 100), b"").unwrap();
        t.insert(&mut pg, &key(1, 10, 200), b"").unwrap();
        t.insert(&mut pg, &key(1, 20, 300), b"").unwrap();
        t.insert(&mut pg, &key(2, 10, 100), b"").unwrap();

        let mut p = Vec::new();
        p.extend_from_slice(&be32(1));
        assert_eq!(t.scan_prefix(&mut pg, &p).unwrap().len(), 3);

        let mut p2 = Vec::new();
        p2.extend_from_slice(&be32(1));
        p2.extend_from_slice(&be32(10));
        assert_eq!(t.scan_prefix(&mut pg, &p2).unwrap().len(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn persists_across_reopen() {
        let path = tmp("persist");
        {
            let mut pg = Pager::open(&path, 64).unwrap();
            let t = BTree::new(0);
            for i in 0..1000u32 {
                t.insert(&mut pg, format!("k{i:05}").as_bytes(), &be32(i))
                    .unwrap();
            }
            pg.flush().unwrap();
        }
        {
            let mut pg = Pager::open(&path, 64).unwrap();
            let t = BTree::new(0);
            assert_eq!(t.get(&mut pg, b"k00500").unwrap(), Some(be32(500).to_vec()));
            assert_eq!(t.iter_all(&mut pg).unwrap().len(), 1000);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_basic_and_missing() {
        let path = tmp("del_basic");
        let mut pg = Pager::open(&path, 64).unwrap();
        let t = BTree::new(0);
        t.insert(&mut pg, b"a", b"1").unwrap();
        t.insert(&mut pg, b"b", b"2").unwrap();
        t.insert(&mut pg, b"c", b"3").unwrap();
        assert!(t.delete(&mut pg, b"b").unwrap());
        assert_eq!(t.get(&mut pg, b"b").unwrap(), None);
        assert_eq!(t.get(&mut pg, b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(t.get(&mut pg, b"c").unwrap().as_deref(), Some(&b"3"[..]));
        // deleting an absent key is a no-op false
        assert!(!t.delete(&mut pg, b"zz").unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_all_clears_tree_and_frees_pages() {
        let path = tmp("del_all");
        let mut pg = Pager::open(&path, 64).unwrap();
        let t = BTree::new(0);
        let n = 2000u32;
        for i in 0..n {
            t.insert(&mut pg, format!("key{i:08}").as_bytes(), &be32(i))
                .unwrap();
        }
        let peak = pg.page_count();
        for i in 0..n {
            assert!(
                t.delete(&mut pg, format!("key{i:08}").as_bytes()).unwrap(),
                "delete {i}"
            );
        }
        // every key is gone and the tree is empty
        for i in 0..n {
            assert_eq!(t.get(&mut pg, format!("key{i:08}").as_bytes()).unwrap(), None);
        }
        assert!(t.iter_all(&mut pg).unwrap().is_empty());
        assert_eq!(t.root(&pg), 0, "root reset after deleting everything");
        // re-inserting reuses freed pages rather than growing unbounded
        for i in 0..n {
            t.insert(&mut pg, format!("key{i:08}").as_bytes(), &be32(i))
                .unwrap();
        }
        assert!(
            pg.page_count() <= peak + 1,
            "freed pages should be reused on re-insert (peak {peak}, now {})",
            pg.page_count()
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_half_keeps_other_half_correct() {
        let path = tmp("del_half");
        let mut pg = Pager::open(&path, 64).unwrap();
        let t = BTree::new(0);
        let n = 3000u32;
        for i in 0..n {
            t.insert(&mut pg, format!("k{i:08}").as_bytes(), &be32(i))
                .unwrap();
        }
        // delete every even key
        for i in (0..n).step_by(2) {
            assert!(t.delete(&mut pg, format!("k{i:08}").as_bytes()).unwrap());
        }
        for i in 0..n {
            let got = t.get(&mut pg, format!("k{i:08}").as_bytes()).unwrap();
            if i % 2 == 0 {
                assert_eq!(got, None, "even key {i} should be gone");
            } else {
                assert_eq!(got, Some(be32(i).to_vec()), "odd key {i} survives");
            }
        }
        // surviving keys are exactly the odd ones, and the leaf chain is intact
        let all = t.iter_all(&mut pg).unwrap();
        assert_eq!(all.len() as u32, n / 2);
        // ordered + ascending (leaf links consistent after merges)
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_then_reinsert_persists() {
        let path = tmp("del_persist");
        {
            let mut pg = Pager::open(&path, 64).unwrap();
            let t = BTree::new(0);
            for i in 0..1500u32 {
                t.insert(&mut pg, format!("k{i:06}").as_bytes(), &be32(i))
                    .unwrap();
            }
            for i in 500..1000u32 {
                t.delete(&mut pg, format!("k{i:06}").as_bytes()).unwrap();
            }
            pg.flush().unwrap();
        }
        {
            let mut pg = Pager::open(&path, 64).unwrap();
            let t = BTree::new(0);
            assert_eq!(t.get(&mut pg, b"k000400").unwrap(), Some(be32(400).to_vec()));
            assert_eq!(t.get(&mut pg, b"k000700").unwrap(), None);
            assert_eq!(t.get(&mut pg, b"k001200").unwrap(), Some(be32(1200).to_vec()));
            assert_eq!(t.iter_all(&mut pg).unwrap().len(), 1000);
        }
        std::fs::remove_file(&path).ok();
    }
}
