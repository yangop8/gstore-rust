//! A disk-backed B+ tree (variable-length byte key → variable-length byte value).
//!
//! Corresponds to gStore's `SITree`/`Tree` B+ trees. Nodes live in [`Pager`]
//! pages. Writes parse a page into an in-memory [`Node`], mutate it, and write
//! it back (correctness-first). Reads never materialize a node: they walk the
//! shared cached page through a zero-copy, bounds-checked [`NodeView`] (a
//! malformed page is an `Err`, never a panic), so a lookup or a range scan
//! copies and allocates nothing per entry. Leaves are linked for ordered range
//! scans, which give the RDF store its prefix lookups (e.g. all `(p, o)` for a
//! subject = prefix scan on SPO).
//!
//! Both keys and values are byte strings: keys carry composite triple keys or
//! dictionary strings; values carry ids (as bytes) or, for `id → string`, the
//! string itself.
//!
//! Page layout (unchanged since the first version):
//! ```text
//! [is_leaf: u8][link: u32 LE][count: u16 LE] then `count` entries:
//!   leaf:     [klen: u16 LE][key][vlen: u16 LE][value]
//!   internal: [klen: u16 LE][key][child: u32 LE]
//! ```
//! A leaf's `link` is the next leaf (0 = none); an internal node's `link` is
//! its leftmost child, for keys below its first separator.

use std::ops::ControlFlow;
use std::sync::Arc;

use crate::error::{GStoreError, Result};

use super::pager::{Page, PageId, Pager, PAGE_SIZE};

/// Deepest descent a read accepts before declaring the tree corrupt (a cycle
/// of child pointers would otherwise loop forever). A 4 KiB-page tree of
/// 12-byte keys is 5 levels deep at a billion entries.
const MAX_DEPTH: usize = 64;

fn corrupt(what: &str) -> GStoreError {
    GStoreError::Database(format!("corrupt B+ tree page: {what}"))
}

/// The `u16 LE` at `off`, bounds-checked.
#[inline]
fn read_u16_at(page: &[u8], off: usize) -> Result<usize> {
    match page.get(off..off + 2) {
        Some(b) => Ok(u16::from_le_bytes([b[0], b[1]]) as usize),
        None => Err(corrupt("entry header past the page end")),
    }
}

/// The `len` bytes at `off`, bounds-checked.
#[inline]
fn slice_at(page: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    page.get(off..off + len)
        .ok_or_else(|| corrupt("entry past the page end"))
}

/// A zero-copy view of one node page: the header is decoded eagerly, entries
/// lazily (every access is bounds-checked, so a malformed page yields `Err`).
#[derive(Clone, Copy)]
pub struct NodeView<'a> {
    page: &'a [u8],
    is_leaf: bool,
    link: PageId,
    count: usize,
}

impl<'a> NodeView<'a> {
    /// View `page` (normally a whole [`PAGE_SIZE`] page).
    pub fn new(page: &'a [u8]) -> Result<NodeView<'a>> {
        if page.len() < 7 {
            return Err(corrupt("shorter than a node header"));
        }
        Ok(NodeView {
            page,
            is_leaf: page[0] != 0,
            link: u32::from_le_bytes([page[1], page[2], page[3], page[4]]),
            count: u16::from_le_bytes([page[5], page[6]]) as usize,
        })
    }

    /// Whether this is a leaf.
    pub fn is_leaf(&self) -> bool {
        self.is_leaf
    }

    /// Leaf: the next leaf (0 = none). Internal: the leftmost child.
    pub fn link(&self) -> PageId {
        self.link
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the node has no entries.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The entries in order.
    pub fn entries(&self) -> Entries<'a> {
        Entries {
            page: self.page,
            off: 7,
            left: self.count,
            is_leaf: self.is_leaf,
        }
    }

    /// Internal node: the child to descend into for `key` (the child after the
    /// last separator `<= key`, or the leftmost child).
    pub fn child_for(&self, key: &[u8]) -> Result<PageId> {
        if self.is_leaf {
            return Err(corrupt("child lookup on a leaf"));
        }
        let mut child = self.link;
        let mut it = self.entries();
        while let Some((k, c)) = it.next_child()? {
            if k > key {
                break;
            }
            child = c;
        }
        Ok(child)
    }
}

/// A cursor over a node's entries (see [`NodeView::entries`]).
#[derive(Clone, Copy)]
pub struct Entries<'a> {
    page: &'a [u8],
    off: usize,
    left: usize,
    is_leaf: bool,
}

impl<'a> Entries<'a> {
    /// Leaf: the next `(key, value)`.
    pub fn next_kv(&mut self) -> Result<Option<(&'a [u8], &'a [u8])>> {
        if self.left == 0 {
            return Ok(None);
        }
        if !self.is_leaf {
            return Err(corrupt("key/value read on an internal node"));
        }
        let klen = read_u16_at(self.page, self.off)?;
        let key = slice_at(self.page, self.off + 2, klen)?;
        let voff = self.off + 2 + klen;
        let vlen = read_u16_at(self.page, voff)?;
        let val = slice_at(self.page, voff + 2, vlen)?;
        self.off = voff + 2 + vlen;
        self.left -= 1;
        Ok(Some((key, val)))
    }

    /// Internal: the next `(separator, child)`.
    pub fn next_child(&mut self) -> Result<Option<(&'a [u8], PageId)>> {
        if self.left == 0 {
            return Ok(None);
        }
        if self.is_leaf {
            return Err(corrupt("child read on a leaf"));
        }
        let klen = read_u16_at(self.page, self.off)?;
        let key = slice_at(self.page, self.off + 2, klen)?;
        let c = slice_at(self.page, self.off + 2 + klen, 4)?;
        self.off += 2 + klen + 4;
        self.left -= 1;
        Ok(Some((key, u32::from_le_bytes([c[0], c[1], c[2], c[3]]))))
    }

    /// Leaf: the key of the next entry without consuming it.
    fn peek_key(&self) -> Result<Option<&'a [u8]>> {
        if self.left == 0 {
            return Ok(None);
        }
        let klen = read_u16_at(self.page, self.off)?;
        slice_at(self.page, self.off + 2, klen).map(Some)
    }
}

/// A forward cursor over a tree's leaf entries, holding the current leaf as a
/// shared page (see [`BTree::cursor_ge`]). Each step is zero-copy; moving to
/// the next leaf costs one cache lookup.
pub struct LeafCursor {
    page: Option<Arc<Page>>,
    off: usize,
    left: usize,
    next_leaf: PageId,
    /// Leaves visited, to detect a cyclic leaf chain in a corrupt file.
    hops: u64,
}

impl LeafCursor {
    fn empty() -> LeafCursor {
        LeafCursor {
            page: None,
            off: 7,
            left: 0,
            next_leaf: 0,
            hops: 0,
        }
    }

    /// The next `(key, value)` in key order, or `None` at the end of the tree.
    pub fn next_entry<'s>(&'s mut self, pager: &Pager) -> Result<Option<(&'s [u8], &'s [u8])>> {
        while self.left == 0 {
            if self.next_leaf == 0 {
                return Ok(None);
            }
            self.hops += 1;
            if self.hops > u64::from(pager.page_count()) {
                return Err(corrupt("cyclic leaf chain"));
            }
            let page = pager.read_page_shared(self.next_leaf)?;
            let view = NodeView::new(&page[..])?;
            if !view.is_leaf() {
                return Err(corrupt("leaf chain reaches an internal node"));
            }
            self.next_leaf = view.link();
            self.left = view.len();
            self.off = 7;
            self.page = Some(page);
        }
        let Some(page) = self.page.as_deref() else {
            return Ok(None);
        };
        let mut it = Entries {
            page: &page[..],
            off: self.off,
            left: self.left,
            is_leaf: true,
        };
        let (k, v) = it.next_kv()?.ok_or_else(|| corrupt("entry count"))?;
        self.off = it.off;
        self.left = it.left;
        Ok(Some((k, v)))
    }
}

/// A handle to one B+ tree, rooted at a [`Pager`] header slot. The handle is a
/// plain slot index (the tree itself lives in pager pages), so it is `Copy` —
/// cheap to hand a duplicate to e.g. the out-of-core dictionary backend.
#[derive(Clone, Copy)]
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
        // A single entry must fit within one page. A leaf node reserves 7 bytes
        // of header (is_leaf + link + count) plus a 2-byte length prefix for the
        // key and another for the value. Reject an oversized entry up front:
        // otherwise `split_leaf` would try to serialise a lone entry that cannot
        // fit and write past the fixed-size page buffer (a panic).
        if 7 + 2 + key.len() + 2 + val.len() > PAGE_SIZE {
            return Err(GStoreError::Serialize(format!(
                "B+ tree entry ({}-byte key + {}-byte value) too large for a {PAGE_SIZE}-byte page",
                key.len(),
                val.len()
            )));
        }
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

    /// Descend from the root to the leaf whose key range holds `key`. `None`
    /// for an empty tree. Zero-copy: every level is a [`NodeView`] over the
    /// shared cached page.
    fn leaf_for(&self, pager: &Pager, key: &[u8]) -> Result<Option<(PageId, Arc<Page>)>> {
        let mut id = self.root(pager);
        if id == 0 {
            return Ok(None);
        }
        for _ in 0..MAX_DEPTH {
            let page = pager.read_page_shared(id)?;
            let next = {
                let view = NodeView::new(&page[..])?;
                if view.is_leaf() {
                    None
                } else {
                    Some(view.child_for(key)?)
                }
            };
            match next {
                None => return Ok(Some((id, page))),
                Some(child) => id = child,
            }
        }
        Err(corrupt("tree deeper than the depth limit (a cycle?)"))
    }

    /// Look up the value for `key` and hand it to `f` without copying it.
    /// Read-only (`&Pager`), so it can run under a shared
    /// [`std::sync::RwLock`] read guard concurrently with other readers.
    pub fn get_with<R>(
        &self,
        pager: &Pager,
        key: &[u8],
        f: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let Some((_, page)) = self.leaf_for(pager, key)? else {
            return Ok(None);
        };
        let mut it = NodeView::new(&page[..])?.entries();
        while let Some((k, v)) = it.next_kv()? {
            match k.cmp(key) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal => return Ok(Some(f(v))),
                std::cmp::Ordering::Greater => break,
            }
        }
        Ok(None)
    }

    /// Look up the value for `key` (an owned copy; see
    /// [`get_with`](Self::get_with) for the zero-copy form). Read-only.
    pub fn get(&self, pager: &Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_with(pager, key, <[u8]>::to_vec)
    }

    /// Whether `key` is present. Read-only.
    pub fn contains(&self, pager: &Pager, key: &[u8]) -> Result<bool> {
        Ok(self.get_with(pager, key, |_| ())?.is_some())
    }

    /// A cursor positioned at the first entry whose key is `>= key`. Read-only.
    pub fn cursor_ge(&self, pager: &Pager, key: &[u8]) -> Result<LeafCursor> {
        let Some((_, page)) = self.leaf_for(pager, key)? else {
            return Ok(LeafCursor::empty());
        };
        let (off, left, next_leaf) = {
            let view = NodeView::new(&page[..])?;
            let mut it = view.entries();
            loop {
                match it.peek_key()? {
                    Some(k) if k < key => {
                        it.next_kv()?;
                    }
                    _ => break,
                }
            }
            (it.off, it.left, view.link())
        };
        Ok(LeafCursor {
            page: Some(page),
            off,
            left,
            next_leaf,
            hops: 0,
        })
    }

    /// Visit every entry whose key starts with `prefix`, in key order, without
    /// copying: `f(key, value)` sees slices of the cached page and may stop
    /// the scan by returning [`ControlFlow::Break`]. Read-only.
    pub fn scan_prefix_with(
        &self,
        pager: &Pager,
        prefix: &[u8],
        mut f: impl FnMut(&[u8], &[u8]) -> ControlFlow<()>,
    ) -> Result<()> {
        let mut cur = self.cursor_ge(pager, prefix)?;
        while let Some((k, v)) = cur.next_entry(pager)? {
            // From the first key >= prefix, the matching keys are contiguous.
            if !k.starts_with(prefix) {
                break;
            }
            if f(k, v).is_break() {
                break;
            }
        }
        Ok(())
    }

    /// The first entry whose key starts with `prefix` (owned copies). Read-only.
    pub fn first_with_prefix(
        &self,
        pager: &Pager,
        prefix: &[u8],
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut cur = self.cursor_ge(pager, prefix)?;
        Ok(match cur.next_entry(pager)? {
            Some((k, v)) if k.starts_with(prefix) => Some((k.to_vec(), v.to_vec())),
            _ => None,
        })
    }

    /// All entries whose key starts with `prefix`, in key order, as owned
    /// copies. Read-only (`&Pager`) so it can run concurrently with other
    /// readers. Prefer [`scan_prefix_with`](Self::scan_prefix_with), which
    /// allocates nothing per entry; this materializing form parses whole
    /// [`Node`]s and is kept as the reference the zero-copy path is tested
    /// against.
    pub fn scan_prefix(&self, pager: &Pager, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
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
    pub fn iter_all(&self, pager: &Pager) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        self.scan_prefix_with(pager, &[], |k, v| {
            out.push((k.to_vec(), v.to_vec()));
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    /// Number of entries whose key starts with `prefix`, without materializing
    /// keys/values. This is the degree-bounded counterpart of
    /// [`scan_prefix`](Self::scan_prefix): it walks the same contiguous leaf run
    /// but only counts, so a "degree" query (e.g. how many objects a `(sub, pred)`
    /// has) costs `O(height + matches)` and allocates nothing. Read-only.
    pub fn count_prefix(&self, pager: &Pager, prefix: &[u8]) -> Result<usize> {
        let mut n = 0usize;
        self.scan_prefix_with(pager, prefix, |_, _| {
            n += 1;
            ControlFlow::Continue(())
        })?;
        Ok(n)
    }

    /// Whether *any* entry has a key starting with `prefix`. Descends to the first
    /// key `>= prefix` and checks it — `O(height)`, touching a single leaf (or
    /// its successor) — so it is far cheaper than a scan when the caller only
    /// needs a yes/no (e.g. maintaining a distinct-count on insert/delete).
    /// Read-only.
    pub fn prefix_exists(&self, pager: &Pager, prefix: &[u8]) -> Result<bool> {
        let mut cur = self.cursor_ge(pager, prefix)?;
        Ok(matches!(cur.next_entry(pager)?, Some((k, _)) if k.starts_with(prefix)))
    }

    /// The leaf holding the first key `>= key` and that key's index in it.
    fn find_leaf_ge(&self, pager: &Pager, key: &[u8]) -> Result<Option<(PageId, usize)>> {
        let Some((id, page)) = self.leaf_for(pager, key)? else {
            return Ok(None);
        };
        let mut it = NodeView::new(&page[..])?.entries();
        let mut idx = 0;
        while let Some(k) = it.peek_key()? {
            if k >= key {
                break;
            }
            it.next_kv()?;
            idx += 1;
        }
        Ok(Some((id, idx)))
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
        let p = std::env::temp_dir().join(format!("gstore_bt_{tag}_{}.kv", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("kv.wal"));
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
        assert_eq!(t.get(&pg, b"bob").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(
            t.get(&pg, b"alice").unwrap().as_deref(),
            Some(&b"1"[..])
        );
        assert_eq!(t.get(&pg, b"dave").unwrap(), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn count_and_exists_prefix_match_scan() {
        // count_prefix / prefix_exists must agree with a full scan_prefix over
        // composite (a, b, c) keys, including empty and absent prefixes.
        let path = tmp("count_exists");
        let mut pg = Pager::open(&path, 128).unwrap();
        let t = BTree::new(0);
        // (a, b, c): a in {1,2}, b in 0..3, c in 0..4 → 2*3*4 = 24 keys.
        for a in 1u32..=2 {
            for b in 0u32..3 {
                for c in 0u32..4 {
                    let mut k = [0u8; 12];
                    k[0..4].copy_from_slice(&be32(a));
                    k[4..8].copy_from_slice(&be32(b));
                    k[8..12].copy_from_slice(&be32(c));
                    t.insert(&mut pg, &k, b"").unwrap();
                }
            }
        }
        let count_via_scan = |prefix: &[u8]| t.scan_prefix(&pg, prefix).unwrap().len();
        for prefix in [
            &[][..],
            &be32(1)[..],           // a = 1  → 12
            &be32(2)[..],           // a = 2  → 12
            &be32(3)[..],           // absent → 0
            &{
                let mut p = [0u8; 8];
                p[0..4].copy_from_slice(&be32(1));
                p[4..8].copy_from_slice(&be32(2));
                p
            }[..], // (1,2) → 4
        ] {
            assert_eq!(
                t.count_prefix(&pg, prefix).unwrap(),
                count_via_scan(prefix),
                "count_prefix disagrees for {prefix:?}"
            );
            assert_eq!(
                t.prefix_exists(&pg, prefix).unwrap(),
                count_via_scan(prefix) > 0,
                "prefix_exists disagrees for {prefix:?}"
            );
        }
        assert_eq!(t.count_prefix(&pg, &[]).unwrap(), 24);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn count_and_exists_track_deletes_across_splits() {
        // After enough inserts to split, and after deleting a whole prefix group,
        // count_prefix / prefix_exists stay exact.
        let path = tmp("count_delete");
        let mut pg = Pager::open(&path, 128).unwrap();
        let t = BTree::new(0);
        let n = 2000u32;
        for i in 0..n {
            // group = i / 100, so 20 groups of 100, keyed (group, i).
            let mut k = [0u8; 8];
            k[0..4].copy_from_slice(&be32(i / 100));
            k[4..8].copy_from_slice(&be32(i));
            t.insert(&mut pg, &k, b"").unwrap();
        }
        assert_eq!(t.count_prefix(&pg, &be32(5)).unwrap(), 100);
        assert!(t.prefix_exists(&pg, &be32(5)).unwrap());
        // Delete group 5 entirely.
        for i in 500..600u32 {
            let mut k = [0u8; 8];
            k[0..4].copy_from_slice(&be32(5));
            k[4..8].copy_from_slice(&be32(i));
            assert!(t.delete(&mut pg, &k).unwrap());
        }
        assert_eq!(t.count_prefix(&pg, &be32(5)).unwrap(), 0);
        assert!(!t.prefix_exists(&pg, &be32(5)).unwrap());
        // A neighbouring group is untouched.
        assert_eq!(t.count_prefix(&pg, &be32(6)).unwrap(), 100);
        assert!(t.prefix_exists(&pg, &be32(6)).unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replace_value() {
        let path = tmp("replace");
        let mut pg = Pager::open(&path, 64).unwrap();
        let t = BTree::new(0);
        t.insert(&mut pg, b"k", b"1").unwrap();
        t.insert(&mut pg, b"k", b"99").unwrap();
        assert_eq!(t.get(&pg, b"k").unwrap().as_deref(), Some(&b"99"[..]));
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
                t.get(&pg, format!("key{i:08}").as_bytes()).unwrap(),
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
        assert_eq!(t.scan_prefix(&pg, &p).unwrap().len(), 3);

        let mut p2 = Vec::new();
        p2.extend_from_slice(&be32(1));
        p2.extend_from_slice(&be32(10));
        assert_eq!(t.scan_prefix(&pg, &p2).unwrap().len(), 2);
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
            let pg = Pager::open(&path, 64).unwrap();
            let t = BTree::new(0);
            assert_eq!(t.get(&pg, b"k00500").unwrap(), Some(be32(500).to_vec()));
            assert_eq!(t.iter_all(&pg).unwrap().len(), 1000);
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
        assert_eq!(t.get(&pg, b"b").unwrap(), None);
        assert_eq!(t.get(&pg, b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(t.get(&pg, b"c").unwrap().as_deref(), Some(&b"3"[..]));
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
            assert_eq!(t.get(&pg, format!("key{i:08}").as_bytes()).unwrap(), None);
        }
        assert!(t.iter_all(&pg).unwrap().is_empty());
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
            let got = t.get(&pg, format!("k{i:08}").as_bytes()).unwrap();
            if i % 2 == 0 {
                assert_eq!(got, None, "even key {i} should be gone");
            } else {
                assert_eq!(got, Some(be32(i).to_vec()), "odd key {i} survives");
            }
        }
        // surviving keys are exactly the odd ones, and the leaf chain is intact
        let all = t.iter_all(&pg).unwrap();
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
            let pg = Pager::open(&path, 64).unwrap();
            let t = BTree::new(0);
            assert_eq!(t.get(&pg, b"k000400").unwrap(), Some(be32(400).to_vec()));
            assert_eq!(t.get(&pg, b"k000700").unwrap(), None);
            assert_eq!(t.get(&pg, b"k001200").unwrap(), Some(be32(1200).to_vec()));
            assert_eq!(t.iter_all(&pg).unwrap().len(), 1000);
        }
        std::fs::remove_file(&path).ok();
    }
}
