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

use crate::error::Result;

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
}

/// The outcome of inserting into a subtree: a split that must propagate up.
type Split = Option<(Vec<u8>, PageId)>;

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
}
