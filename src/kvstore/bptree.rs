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

/// The `u16 LE` at `off` of a range already checked to be inside the page.
#[inline]
fn u16_in(page: &[u8], off: usize) -> usize {
    u16::from_le_bytes([page[off], page[off + 1]]) as usize
}

/// The smallest key greater than every key that starts with `prefix`
/// (`None` when no such bound exists: an empty or all-`0xFF` prefix). Keys in
/// `[prefix, prefix_upper(prefix))` are exactly the keys with that prefix.
fn prefix_upper(prefix: &[u8]) -> Option<Vec<u8>> {
    let last = prefix.iter().rposition(|&b| b != 0xFF)?;
    let mut up = prefix[..=last].to_vec();
    up[last] += 1;
    Some(up)
}

/// How a node's entries are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Every entry has the same key length (and, in a leaf, value length) —
    /// every node of a fixed-width tree such as SPO — so entry `i` starts at
    /// `7 + i * stride` and the node can be binary-searched. Detection checked
    /// that all `count` entries lie inside the page.
    Uniform {
        stride: usize,
        klen: usize,
        vlen: usize,
    },
    /// Entries of varying size (dictionary trees): walked in order.
    Variable,
}

/// Classify a node (one pass over the entry lengths, no key comparison).
fn detect_layout(page: &[u8], is_leaf: bool, count: usize) -> Layout {
    if count == 0 || page.len() < 9 {
        return Layout::Variable;
    }
    let klen = u16_in(page, 7);
    let (stride, vlen) = if is_leaf {
        match page.get(9 + klen..11 + klen) {
            Some(b) => {
                let vlen = u16::from_le_bytes([b[0], b[1]]) as usize;
                (4 + klen + vlen, vlen)
            }
            None => return Layout::Variable,
        }
    } else {
        (6 + klen, 4)
    };
    if 7 + count * stride > page.len() {
        return Layout::Variable;
    }
    for i in 1..count {
        let at = 7 + i * stride;
        if u16_in(page, at) != klen || (is_leaf && u16_in(page, at + 2 + klen) != vlen) {
            return Layout::Variable;
        }
    }
    Layout::Uniform { stride, klen, vlen }
}

/// The declared entry shape of a fixed-width tree (see
/// [`BTree::with_fixed_shape`]): every key is `klen` bytes and every leaf
/// value `vlen` bytes, in leaves and internal nodes alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    klen: usize,
    vlen: usize,
}

/// Classify a node of a tree with a declared shape in `O(1)`: the entries
/// must fit the page and the first and last entries must carry the declared
/// lengths; otherwise (a malformed page) fall back to [`detect_layout`].
/// Every binary-search probe re-checks its entry's key length as well.
fn shaped_layout(page: &[u8], is_leaf: bool, count: usize, shape: Shape) -> Layout {
    let (stride, vlen) = if is_leaf {
        (4 + shape.klen + shape.vlen, shape.vlen)
    } else {
        (6 + shape.klen, 4)
    };
    let fits = count > 0 && 7 + count * stride <= page.len();
    let entry_ok = |i: usize| {
        let at = 7 + i * stride;
        u16_in(page, at) == shape.klen
            && (!is_leaf || u16_in(page, at + 2 + shape.klen) == shape.vlen)
    };
    if fits && entry_ok(0) && entry_ok(count - 1) {
        Layout::Uniform {
            stride,
            klen: shape.klen,
            vlen,
        }
    } else {
        detect_layout(page, is_leaf, count)
    }
}

/// A position inside a node: entry index and its byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pos {
    idx: usize,
    off: usize,
}

/// A zero-copy view of one node page: the header and the entry layout are
/// decoded eagerly, entries on access. Every access is bounds-checked, so a
/// malformed page yields `Err`, never a panic. Nodes whose entries all have
/// the same size are binary-searched.
#[derive(Clone, Copy)]
pub struct NodeView<'a> {
    page: &'a [u8],
    is_leaf: bool,
    link: PageId,
    count: usize,
    layout: Layout,
}

impl<'a> NodeView<'a> {
    /// View `page` (normally a whole [`PAGE_SIZE`] page).
    pub fn new(page: &'a [u8]) -> Result<NodeView<'a>> {
        NodeView::with_shape(page, None)
    }

    /// View a page of a tree whose entry shape may be declared.
    fn with_shape(page: &'a [u8], shape: Option<Shape>) -> Result<NodeView<'a>> {
        if page.len() < 7 {
            return Err(corrupt("shorter than a node header"));
        }
        let is_leaf = page[0] != 0;
        let count = u16::from_le_bytes([page[5], page[6]]) as usize;
        let layout = match shape {
            Some(shape) => shaped_layout(page, is_leaf, count, shape),
            None => detect_layout(page, is_leaf, count),
        };
        Ok(NodeView {
            page,
            is_leaf,
            link: u32::from_le_bytes([page[1], page[2], page[3], page[4]]),
            count,
            layout,
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

    /// Key `i` of a uniform node. The entry lies inside the page (layout
    /// detection checked `7 + count * stride`); its recorded key length is
    /// re-checked here, so a page that only looks uniform is an error.
    #[inline]
    fn ukey(&self, stride: usize, klen: usize, i: usize) -> Result<&'a [u8]> {
        let at = 7 + i * stride;
        if u16_in(self.page, at) != klen {
            return Err(corrupt("entry size differs from its node's layout"));
        }
        Ok(&self.page[at + 2..at + 2 + klen])
    }

    /// Entry `i` of a uniform leaf (see [`ukey`](Self::ukey)).
    #[inline]
    fn ukv(&self, stride: usize, klen: usize, vlen: usize, i: usize) -> Result<(&'a [u8], &'a [u8])> {
        let at = 7 + i * stride;
        if u16_in(self.page, at) != klen || u16_in(self.page, at + 2 + klen) != vlen {
            return Err(corrupt("entry size differs from its node's layout"));
        }
        Ok((
            &self.page[at + 2..at + 2 + klen],
            &self.page[at + 4 + klen..at + 4 + klen + vlen],
        ))
    }

    /// The first position whose key is `> key` (`past_equal`) or `>= key`.
    /// Binary search on a uniform node, a walk on a variable one.
    fn search(&self, key: &[u8], past_equal: bool) -> Result<Pos> {
        let goes_left = |k: &[u8]| if past_equal { k <= key } else { k < key };
        match self.layout {
            Layout::Uniform { stride, klen, .. } => {
                let (mut lo, mut hi) = (0, self.count);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if goes_left(self.ukey(stride, klen, mid)?) {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                Ok(Pos {
                    idx: lo,
                    off: 7 + lo * stride,
                })
            }
            Layout::Variable => {
                let mut it = self.entries();
                let mut idx = 0;
                while let Some(k) = it.peek_key()? {
                    if !goes_left(k) {
                        break;
                    }
                    it.skip()?;
                    idx += 1;
                }
                Ok(Pos { idx, off: it.off })
            }
        }
    }

    /// Leaf: the position of the first key `>= key`.
    fn lower_bound(&self, key: &[u8]) -> Result<Pos> {
        self.search(key, false)
    }

    /// Internal node: the child to descend into for `key` (the child after the
    /// last separator `<= key`, or the leftmost child).
    pub fn child_for(&self, key: &[u8]) -> Result<PageId> {
        if self.is_leaf {
            return Err(corrupt("child lookup on a leaf"));
        }
        let at = self.search(key, true)?;
        if at.idx == 0 {
            return Ok(self.link);
        }
        match self.layout {
            Layout::Uniform { stride, klen, .. } => {
                let c = 7 + (at.idx - 1) * stride + 2 + klen;
                let b = &self.page[c..c + 4];
                Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            }
            Layout::Variable => {
                // Re-walk to entry idx-1 (variable nodes keep no offset table).
                let mut it = self.entries();
                let mut child = self.link;
                for _ in 0..at.idx {
                    match it.next_child()? {
                        Some((_, c)) => child = c,
                        None => break,
                    }
                }
                Ok(child)
            }
        }
    }

    /// Leaf: hand entries `from .. to` (`to` an index) to `f` in order.
    fn visit(
        &self,
        from: Pos,
        to: usize,
        f: &mut impl FnMut(&'a [u8], &'a [u8]) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>> {
        match self.layout {
            Layout::Uniform { stride, klen, vlen } => {
                for i in from.idx..to.min(self.count) {
                    let (k, v) = self.ukv(stride, klen, vlen, i)?;
                    if f(k, v).is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
            Layout::Variable => {
                let mut it = Entries {
                    page: self.page,
                    off: from.off,
                    left: self.count.saturating_sub(from.idx),
                    is_leaf: true,
                };
                for _ in from.idx..to {
                    let Some((k, v)) = it.next_kv()? else {
                        break;
                    };
                    if f(k, v).is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
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

    /// Step over one entry of either kind.
    fn skip(&mut self) -> Result<()> {
        if self.is_leaf {
            self.next_kv().map(|_| ())
        } else {
            self.next_child().map(|_| ())
        }
    }

    /// The key of the next entry without consuming it.
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
    layout: Layout,
    /// The tree's declared entry shape, for the leaves still to come.
    shape: Option<Shape>,
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
            layout: Layout::Variable,
            shape: None,
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
            let view = NodeView::with_shape(&page[..], self.shape)?;
            if !view.is_leaf() {
                return Err(corrupt("leaf chain reaches an internal node"));
            }
            self.next_leaf = view.link();
            self.left = view.len();
            self.layout = view.layout;
            self.off = 7;
            self.page = Some(page);
        }
        let Some(page) = self.page.as_deref() else {
            return Ok(None);
        };
        let page = &page[..];
        match self.layout {
            Layout::Uniform { stride, klen, vlen } => {
                let at = self.off;
                if u16_in(page, at) != klen || u16_in(page, at + 2 + klen) != vlen {
                    return Err(corrupt("entry size differs from its node's layout"));
                }
                self.off += stride;
                self.left -= 1;
                Ok(Some((
                    &page[at + 2..at + 2 + klen],
                    &page[at + 4 + klen..at + 4 + klen + vlen],
                )))
            }
            Layout::Variable => {
                let mut it = Entries {
                    page,
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
    }
}

/// A handle to one B+ tree, rooted at a [`Pager`] header slot. The handle is a
/// plain slot index (the tree itself lives in pager pages), so it is `Copy` —
/// cheap to hand a duplicate to e.g. the out-of-core dictionary backend.
#[derive(Clone, Copy)]
pub struct BTree {
    root_slot: usize,
    /// Declared fixed entry shape (see [`with_fixed_shape`](Self::with_fixed_shape)).
    shape: Option<Shape>,
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
        BTree {
            root_slot,
            shape: None,
        }
    }

    /// A tree whose every key is `key_len` bytes and every value `val_len`
    /// bytes (the triple and quad indexes: 12- or 16-byte keys, empty values).
    /// Reads then classify each node in `O(1)` and binary-search it; a page
    /// that does not match the shape is still read correctly (or reported
    /// corrupt), never misread. The on-disk format is the same as for
    /// [`new`](Self::new); the shape is a promise about the data the caller
    /// stores, which [`insert`](Self::insert) enforces.
    pub fn with_fixed_shape(root_slot: usize, key_len: usize, val_len: usize) -> BTree {
        BTree {
            root_slot,
            shape: Some(Shape {
                klen: key_len,
                vlen: val_len,
            }),
        }
    }

    /// A zero-copy view of one of this tree's pages.
    fn view<'p>(&self, page: &'p [u8]) -> Result<NodeView<'p>> {
        NodeView::with_shape(page, self.shape)
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
        if let Some(shape) = self.shape {
            if key.len() != shape.klen || val.len() != shape.vlen {
                return Err(GStoreError::Serialize(format!(
                    "B+ tree entry ({}-byte key + {}-byte value) does not match the tree's \
                     fixed {}-byte keys and {}-byte values",
                    key.len(),
                    val.len(),
                    shape.klen,
                    shape.vlen
                )));
            }
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
                let view = self.view(&page[..])?;
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
        let view = self.view(&page[..])?;
        let at = view.lower_bound(key)?;
        let mut hit = None;
        let _stopped = view.visit(at, at.idx + 1, &mut |k, v| {
            if k == key {
                hit = Some(v);
            }
            ControlFlow::Break(())
        })?;
        Ok(hit.map(f))
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
        let (at, left, layout, next_leaf) = {
            let view = self.view(&page[..])?;
            let at = view.lower_bound(key)?;
            (at, view.len() - at.idx, view.layout, view.link())
        };
        Ok(LeafCursor {
            page: Some(page),
            layout,
            shape: self.shape,
            off: at.off,
            left,
            next_leaf,
            hops: 0,
        })
    }

    /// Walk the leaves holding the keys that start with `prefix`: for each,
    /// `on_leaf(view, from, to)` gets the run's slice of that leaf (entry
    /// `from` up to index `to`), found by two binary searches on a uniform
    /// leaf, so no key in the middle of the run is compared at all.
    fn for_each_run_leaf(
        &self,
        pager: &Pager,
        prefix: &[u8],
        mut on_leaf: impl FnMut(&NodeView<'_>, Pos, usize) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        let upper = prefix_upper(prefix);
        let Some((_, mut page)) = self.leaf_for(pager, prefix)? else {
            return Ok(());
        };
        let mut first = true;
        let mut hops = 0u64;
        loop {
            let next = {
                let view = self.view(&page[..])?;
                if !view.is_leaf() {
                    return Err(corrupt("leaf chain reaches an internal node"));
                }
                let from = if first {
                    view.lower_bound(prefix)?
                } else {
                    Pos { idx: 0, off: 7 }
                };
                let to = match &upper {
                    Some(u) => view.lower_bound(u)?.idx,
                    None => view.len(),
                };
                if from.idx < to && on_leaf(&view, from, to)?.is_break() {
                    return Ok(());
                }
                if to < view.len() || view.link() == 0 {
                    return Ok(());
                }
                view.link()
            };
            hops += 1;
            if hops > u64::from(pager.page_count()) {
                return Err(corrupt("cyclic leaf chain"));
            }
            page = pager.read_page_shared(next)?;
            first = false;
        }
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
        self.for_each_run_leaf(pager, prefix, |view, from, to| {
            view.visit(from, to, &mut |k, v| f(k, v))
        })
    }

    /// The first entry whose key starts with `prefix` (owned copies). Read-only.
    pub fn first_with_prefix(
        &self,
        pager: &Pager,
        prefix: &[u8],
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut first = None;
        self.scan_prefix_with(pager, prefix, |k, v| {
            first = Some((k.to_vec(), v.to_vec()));
            ControlFlow::Break(())
        })?;
        Ok(first)
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
    /// [`scan_prefix`](Self::scan_prefix): on uniform leaves (every fixed-width
    /// tree) it costs two binary searches per leaf of the run and touches no
    /// entry in between, so a "degree" query (e.g. how many objects a
    /// `(sub, pred)` has) costs `O(height + leaves)` and allocates nothing.
    /// Read-only.
    pub fn count_prefix(&self, pager: &Pager, prefix: &[u8]) -> Result<usize> {
        let mut n = 0usize;
        self.for_each_run_leaf(pager, prefix, |_, from, to| {
            n += to - from.idx;
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(n)
    }

    /// Whether *any* entry has a key starting with `prefix`. Descends to the first
    /// key `>= prefix` and checks it — `O(height)`, touching a single leaf (or
    /// its successor) — so it is far cheaper than a scan when the caller only
    /// needs a yes/no (e.g. maintaining a distinct-count on insert/delete).
    /// Read-only.
    pub fn prefix_exists(&self, pager: &Pager, prefix: &[u8]) -> Result<bool> {
        let mut found = false;
        self.for_each_run_leaf(pager, prefix, |_, _, _| {
            found = true;
            Ok(ControlFlow::Break(()))
        })?;
        Ok(found)
    }

    /// The leaf holding the first key `>= key` and that key's index in it.
    fn find_leaf_ge(&self, pager: &Pager, key: &[u8]) -> Result<Option<(PageId, usize)>> {
        let Some((id, page)) = self.leaf_for(pager, key)? else {
            return Ok(None);
        };
        let at = self.view(&page[..])?.lower_bound(key)?;
        Ok(Some((id, at.idx)))
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

    // ---- Phase 6 R1-KV: zero-copy NodeView reads ---------------------------

    /// xorshift64*: a tiny deterministic generator for the property tests.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    /// A random well-formed node (sorted distinct keys) that fits a page.
    fn random_node(rng: &mut Rng) -> Node {
        let is_leaf = rng.below(2) == 0;
        let mut keys: Vec<Vec<u8>> = (0..rng.below(120))
            .map(|_| {
                let n = rng.below(24);
                rng.bytes(n)
            })
            .collect();
        keys.sort();
        keys.dedup();
        let mut node = Node {
            is_leaf,
            link: rng.next() as u32,
            keys: Vec::new(),
            vals: Vec::new(),
            children: Vec::new(),
        };
        for k in keys {
            node.keys.push(k);
            if is_leaf {
                let n = rng.below(16);
                node.vals.push(rng.bytes(n));
            } else {
                node.children.push(rng.next() as u32);
            }
            if node.size() > PAGE_SIZE {
                node.keys.pop();
                node.vals.pop();
                node.children.pop();
                break;
            }
        }
        node
    }

    /// Every entry of `view`, decoded (`Err` if any entry is malformed).
    #[allow(clippy::type_complexity)]
    fn view_entries(view: &NodeView) -> Result<(Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<PageId>)> {
        let (mut ks, mut vs, mut cs) = (Vec::new(), Vec::new(), Vec::new());
        let mut it = view.entries();
        if view.is_leaf() {
            while let Some((k, v)) = it.next_kv()? {
                ks.push(k.to_vec());
                vs.push(v.to_vec());
            }
        } else {
            while let Some((k, c)) = it.next_child()? {
                ks.push(k.to_vec());
                cs.push(c);
            }
        }
        Ok((ks, vs, cs))
    }

    #[test]
    fn node_view_matches_node_parse_on_random_pages() {
        let mut rng = Rng(0x5EED_1234_ABCD_0001);
        for round in 0..400 {
            // A valid page: the view decodes exactly what Node::parse does.
            let node = random_node(&mut rng);
            let page = node.serialize();
            let view = NodeView::new(&page).unwrap();
            let parsed = Node::parse(&page);
            assert_eq!(view.is_leaf(), parsed.is_leaf, "round {round}");
            assert_eq!(view.link(), parsed.link);
            assert_eq!(view.len(), parsed.keys.len());
            let (ks, vs, cs) = view_entries(&view).unwrap();
            assert_eq!(ks, parsed.keys);
            assert_eq!(vs, parsed.vals);
            assert_eq!(cs, parsed.children);
            if !view.is_leaf() {
                // child_for agrees with the Node routing for random probes.
                for _ in 0..8 {
                    let n = rng.below(24);
                    let probe = rng.bytes(n);
                    assert_eq!(view.child_for(&probe).unwrap(), parsed.child_for(&probe));
                }
            }

            // A corrupted copy (random count / length bytes) and a page of pure
            // noise: the view never panics; when it decodes every entry, the
            // page is in bounds and Node::parse agrees.
            let mut bad = page;
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(64.min(PAGE_SIZE));
                bad[at] = rng.next() as u8;
            }
            let noise: Page = {
                let mut p = [0u8; PAGE_SIZE];
                for b in p.iter_mut() {
                    *b = rng.next() as u8;
                }
                p
            };
            for candidate in [&bad, &noise] {
                let view = NodeView::new(&candidate[..]).unwrap();
                if let Ok((ks, vs, cs)) = view_entries(&view) {
                    let parsed = Node::parse(candidate);
                    assert_eq!((ks, vs, cs), (parsed.keys, parsed.vals, parsed.children));
                }
            }
        }
        assert!(NodeView::new(&[1, 0, 0]).is_err(), "short page");
    }

    /// A tree of `n` random composite keys (`a|b|c`, small id spaces).
    fn random_tree(pg: &mut Pager, rng: &mut Rng, n: usize) -> BTree {
        let t = BTree::new(0);
        for _ in 0..n {
            let mut k = Vec::with_capacity(12);
            k.extend_from_slice(&be32(rng.below(40) as u32));
            k.extend_from_slice(&be32(rng.below(8) as u32));
            k.extend_from_slice(&be32(rng.below(500) as u32));
            let n = rng.below(6);
            let v = rng.bytes(n);
            t.insert(pg, &k, &v).unwrap();
        }
        t
    }

    #[test]
    fn scan_prefix_with_equals_scan_prefix() {
        let path = tmp("scan_with");
        let mut pg = Pager::open(&path, 32).unwrap();
        let mut rng = Rng(42);
        let t = random_tree(&mut pg, &mut rng, 6000);
        // Delete a third so merged/borrowed leaves are part of the walk.
        for (k, _) in t.iter_all(&pg).unwrap().into_iter().step_by(3) {
            assert!(t.delete(&mut pg, &k).unwrap());
        }
        let mut prefixes: Vec<Vec<u8>> = vec![Vec::new(), vec![0xff; 5]];
        for _ in 0..300 {
            let a = rng.below(42) as u32;
            let mut p = be32(a).to_vec();
            match rng.below(4) {
                0 => {}
                1 => p.extend_from_slice(&be32(rng.below(9) as u32)),
                2 => p.truncate(1 + rng.below(3)),
                _ => {
                    p.extend_from_slice(&be32(rng.below(9) as u32));
                    p.extend_from_slice(&be32(rng.below(510) as u32));
                }
            }
            prefixes.push(p);
        }
        for p in &prefixes {
            let want = t.scan_prefix(&pg, p).unwrap();
            let mut got = Vec::new();
            t.scan_prefix_with(&pg, p, |k, v| {
                got.push((k.to_vec(), v.to_vec()));
                ControlFlow::Continue(())
            })
            .unwrap();
            assert_eq!(got, want, "prefix {p:?}");
            assert_eq!(t.count_prefix(&pg, p).unwrap(), want.len());
            assert_eq!(t.prefix_exists(&pg, p).unwrap(), !want.is_empty());
            assert_eq!(t.first_with_prefix(&pg, p).unwrap(), want.first().cloned());
            // Early stop after two entries.
            let mut seen = 0;
            t.scan_prefix_with(&pg, p, |_, _| {
                seen += 1;
                if seen == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
            assert_eq!(seen, want.len().min(2));
            if p.len() == 12 {
                let hit = want.first().map(|(_, v)| v.clone());
                assert_eq!(t.get(&pg, p).unwrap(), hit);
                assert_eq!(t.contains(&pg, p).unwrap(), !want.is_empty());
                assert_eq!(t.get_with(&pg, p, |v| v.len()).unwrap(), want.first().map(|e| e.1.len()));
            }
        }
        // The cursor walks the whole tree in order.
        let all = t.scan_prefix(&pg, &[]).unwrap();
        let mut cur = t.cursor_ge(&pg, &[]).unwrap();
        let mut walked = Vec::new();
        while let Some((k, v)) = cur.next_entry(&pg).unwrap() {
            walked.push((k.to_vec(), v.to_vec()));
        }
        assert_eq!(walked, all);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn malformed_pages_are_errors_not_panics() {
        let path = tmp("malformed");
        let mut pg = Pager::open(&path, 32).unwrap();
        let t = BTree::new(0);
        for i in 0..2000u32 {
            t.insert(&mut pg, &be32(i), &be32(i)).unwrap();
        }
        let root = t.root(&pg);
        // An internal root whose entry count runs past the page end.
        let mut page = pg.read_page(root).unwrap();
        assert_eq!(page[0], 0, "2000 keys need an internal root");
        page[5..7].copy_from_slice(&u16::MAX.to_le_bytes());
        pg.write_page(root, &page).unwrap();
        // Entries decode lazily: a descent that stops before the bad region
        // succeeds, one that walks into it is an error — never a panic.
        let _ = t.count_prefix(&pg, &[]);
        assert!(t.get(&pg, &be32(1999)).is_err());
        assert!(t.count_prefix(&pg, &be32(1999)).is_err());
        assert!(t.prefix_exists(&pg, &be32(1999)).is_err());
        assert!(t.cursor_ge(&pg, &be32(1999)).is_err());
        // A child pointer that loops back to the root: the depth guard stops it.
        let node = Node {
            is_leaf: false,
            link: root,
            keys: vec![be32(5).to_vec()],
            vals: Vec::new(),
            children: vec![root],
        };
        pg.write_page(root, &node.serialize()).unwrap();
        assert!(t.get(&pg, &be32(1)).is_err());
        std::fs::remove_file(&path).ok();
    }
}
