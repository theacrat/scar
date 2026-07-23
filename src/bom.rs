//! BOM container (big-endian) reader/writer. See docs/FORMAT.md §1.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};

const HEADER_SIZE: usize = 512;
const MAGIC: &[u8; 8] = b"BOMStore";
const PATHS_HEADER_SIZE: usize = 12; // isLeaf(2) + count(2) + forward(4) + backward(4)
const PATHS_ENTRY_SIZE: usize = 8; // valueBlockId(4) + keyBlockId(4)
// "tree"(4) + version(4) + child(4) + nodeSize(4) + pathCount(4) + isInlineKeys(1); real files add an 8-byte tail, but writing it back breaks large trees, so stop at 21.
// isInlineKeys must be correct: 0 = keys are block refs, 1 = raw inline u32 keys (BITMAPKEYS); wrong value breaks BOM readers.
const TREE_HEADER_SIZE: usize = 21;

fn u32_be(data: &[u8], off: usize) -> Result<u32> {
    let bytes: [u8; 4] = data
        .get(off..off + 4)
        .ok_or_else(|| anyhow!("truncated BOM data at offset {off} (len {})", data.len()))?
        .try_into()
        .unwrap();
    Ok(u32::from_be_bytes(bytes))
}

fn u16_be(data: &[u8], off: usize) -> Result<u16> {
    let bytes: [u8; 2] = data
        .get(off..off + 2)
        .ok_or_else(|| anyhow!("truncated BOM data at offset {off} (len {})", data.len()))?
        .try_into()
        .unwrap();
    Ok(u16::from_be_bytes(bytes))
}

/// Parsed BOM: resolved blocks plus named variables.
pub struct Bom {
    /// Block id -> bytes (id 0 is the null block).
    pub blocks: Vec<Vec<u8>>,
    /// Variable name -> block id.
    pub vars: BTreeMap<String, u32>,
}

impl Bom {
    pub fn parse(data: &[u8]) -> Result<Bom> {
        if data.len() < HEADER_SIZE {
            bail!("BOM data too short for header: {} bytes", data.len());
        }
        if &data[0..8] != MAGIC {
            bail!("bad BOM magic (expected \"BOMStore\")");
        }

        let index_offset = u32_be(data, 16)? as usize;
        let index_length = u32_be(data, 20)? as usize;
        let vars_offset = u32_be(data, 24)? as usize;
        let vars_length = u32_be(data, 28)? as usize;

        let index_end = index_offset
            .checked_add(index_length)
            .ok_or_else(|| anyhow!("index offset/length overflow"))?;
        let index_data = data
            .get(index_offset..index_end)
            .ok_or_else(|| anyhow!("BOM index out of bounds ({index_offset}..{index_end})"))?;

        let block_count = u32_be(index_data, 0)? as usize;
        let mut blocks = Vec::with_capacity(block_count);
        let mut pos = 4usize;
        for i in 0..block_count {
            let addr = u32_be(index_data, pos)? as usize;
            let len = u32_be(index_data, pos + 4)? as usize;
            pos += 8;
            // Null (0,0) and free-slot 0xFFFFFFFF sentinel entries are unreferenced; treat as empty.
            if (addr == 0 && len == 0) || addr == 0xFFFF_FFFF || len == 0xFFFF_FFFF {
                blocks.push(Vec::new());
                continue;
            }
            let end = addr
                .checked_add(len)
                .ok_or_else(|| anyhow!("block {i} address/length overflow"))?;
            let block = data
                .get(addr..end)
                .ok_or_else(|| anyhow!("block {i} out of bounds (addr={addr} len={len})"))?;
            blocks.push(block.to_vec());
        }

        let vars_end = vars_offset
            .checked_add(vars_length)
            .ok_or_else(|| anyhow!("vars offset/length overflow"))?;
        let vars_data = data
            .get(vars_offset..vars_end)
            .ok_or_else(|| anyhow!("BOM vars out of bounds ({vars_offset}..{vars_end})"))?;

        let var_count = u32_be(vars_data, 0)? as usize;
        let mut vars = BTreeMap::new();
        let mut pos = 4usize;
        for _ in 0..var_count {
            let block_id = u32_be(vars_data, pos)?;
            let name_len = *vars_data
                .get(pos + 4)
                .ok_or_else(|| anyhow!("truncated BOM var name length at offset {}", pos + 4))?
                as usize;
            pos += 5;
            let name_bytes = vars_data
                .get(pos..pos + name_len)
                .ok_or_else(|| anyhow!("truncated BOM var name at offset {pos}"))?;
            let name = String::from_utf8(name_bytes.to_vec())
                .context("BOM var name is not valid UTF-8")?;
            pos += name_len;
            vars.insert(name, block_id);
        }

        Ok(Bom { blocks, vars })
    }

    fn block(&self, id: u32) -> Result<&[u8]> {
        self.blocks
            .get(id as usize)
            .map(|b| b.as_slice())
            .ok_or_else(|| {
                anyhow!(
                    "block id {id} out of range (have {} blocks)",
                    self.blocks.len()
                )
            })
    }

    /// Bytes of the block a variable points at.
    pub fn var_block(&self, name: &str) -> Option<&[u8]> {
        let id = *self.vars.get(name)?;
        self.blocks.get(id as usize).map(|b| b.as_slice())
    }

    /// Descend from a paths block id to the leftmost leaf paths block id.
    fn leftmost_leaf(&self, start: u32) -> Result<u32> {
        let mut id = start;
        let mut guard = self.blocks.len() + 2;
        loop {
            if guard == 0 {
                bail!("BOM tree descent exceeded block count; likely a cycle");
            }
            guard -= 1;

            let node = self.block(id)?;
            if node.len() < PATHS_HEADER_SIZE {
                bail!("truncated BOM paths block {id}");
            }
            let is_leaf = u16_be(node, 0)?;
            if is_leaf != 0 {
                return Ok(id);
            }
            let count = u16_be(node, 2)?;
            if count == 0 {
                bail!("empty internal BOM paths block {id}");
            }
            // First entry's valueBlockId is the child to descend into.
            id = u32_be(node, PATHS_HEADER_SIZE)?;
        }
    }

    /// Walk a tree variable, returning (keyBlockId, valueBlockId) pairs in
    /// leaf order, without resolving either through the block index.
    fn tree_pairs(&self, var: &str) -> Result<Vec<(u32, u32)>> {
        let tree_block_id = *self
            .vars
            .get(var)
            .ok_or_else(|| anyhow!("no such BOM variable: {var}"))?;
        let tree_block = self.block(tree_block_id)?;
        if tree_block.len() < TREE_HEADER_SIZE || &tree_block[0..4] != b"tree" {
            bail!("BOM variable {var} does not point at a tree block");
        }
        let version = u32_be(tree_block, 4)?;
        if version != 1 {
            bail!("unsupported BOM tree version {version} for variable {var}");
        }
        let child = u32_be(tree_block, 8)?;
        let path_count = u32_be(tree_block, 16)? as usize;

        let mut pairs = Vec::with_capacity(path_count);
        if path_count == 0 {
            return Ok(pairs);
        }

        let mut node_id = self.leftmost_leaf(child)?;
        let mut guard = self.blocks.len() + 2;
        loop {
            if guard == 0 {
                bail!("BOM tree leaf chain exceeded block count; likely a cycle");
            }
            guard -= 1;

            let node = self.block(node_id)?;
            if node.len() < PATHS_HEADER_SIZE {
                bail!("truncated BOM paths block {node_id}");
            }
            let is_leaf = u16_be(node, 0)?;
            if is_leaf == 0 {
                bail!("expected leaf BOM paths block at {node_id}, found internal node");
            }
            let count = u16_be(node, 2)? as usize;
            let forward = u32_be(node, 4)?;

            let mut pos = PATHS_HEADER_SIZE;
            for _ in 0..count {
                let value_id = u32_be(node, pos)?;
                let key_id = u32_be(node, pos + 4)?;
                pos += PATHS_ENTRY_SIZE;
                pairs.push((key_id, value_id));
            }

            if forward == 0 {
                break;
            }
            node_id = forward;
        }

        Ok(pairs)
    }

    /// Walk a BOM tree variable, returning (key bytes, value bytes) in leaf
    /// order. Errors if the variable is missing or not a tree.
    pub fn tree_entries(&self, var: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.tree_pairs(var)?
            .into_iter()
            .map(|(key_id, value_id)| {
                let key = self.block(key_id)?.to_vec();
                let value = self.block(value_id)?.to_vec();
                Ok((key, value))
            })
            .collect()
    }

    /// Like `tree_entries` but for trees whose keys are inline u32 values
    /// rather than block references (BITMAPKEYS).
    pub fn tree_entries_inline_keys(&self, var: &str) -> Result<Vec<(u32, Vec<u8>)>> {
        self.tree_pairs(var)?
            .into_iter()
            .map(|(key_id, value_id)| {
                let value = self.block(value_id)?.to_vec();
                Ok((key_id, value))
            })
            .collect()
    }
}

/// Build a 21-byte BOM tree descriptor block (see `TREE_HEADER_SIZE`).
fn build_tree_header(
    child_block_id: u32,
    node_size: u32,
    path_count: u32,
    is_inline_keys: bool,
) -> Vec<u8> {
    let mut tree = Vec::with_capacity(TREE_HEADER_SIZE);
    tree.extend_from_slice(b"tree");
    tree.extend_from_slice(&1u32.to_be_bytes());
    tree.extend_from_slice(&child_block_id.to_be_bytes());
    tree.extend_from_slice(&node_size.to_be_bytes());
    tree.extend_from_slice(&path_count.to_be_bytes());
    tree.push(if is_inline_keys { 1 } else { 0 });
    debug_assert_eq!(tree.len(), TREE_HEADER_SIZE);
    tree
}

/// Build a BOM paths block zero-padded to `min_size`; tightly-packed nodes break BOM readers.
fn build_paths_block(
    is_leaf: bool,
    forward: u32,
    backward: u32,
    entries: &[(u32, u32)],
    min_size: usize,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(PATHS_HEADER_SIZE + entries.len() * PATHS_ENTRY_SIZE);
    buf.extend_from_slice(&(if is_leaf { 1u16 } else { 0u16 }).to_be_bytes());
    buf.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    buf.extend_from_slice(&forward.to_be_bytes());
    buf.extend_from_slice(&backward.to_be_bytes());
    for (value_id, key_id) in entries {
        buf.extend_from_slice(&value_id.to_be_bytes());
        buf.extend_from_slice(&key_id.to_be_bytes());
    }
    if buf.len() < min_size {
        buf.resize(min_size, 0);
    }
    buf
}

/// Incremental writer producing a valid BOM file.
pub struct BomWriter {
    blocks: Vec<Vec<u8>>,
    vars: Vec<(String, u32)>,
}

impl Default for BomWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BomWriter {
    pub fn new() -> Self {
        BomWriter {
            blocks: vec![Vec::new()],
            vars: Vec::new(),
        }
    }

    /// Add a block, returning its id.
    pub fn add_block(&mut self, data: Vec<u8>) -> u32 {
        let id = self.blocks.len() as u32;
        self.blocks.push(data);
        id
    }

    /// Point a named variable at a block.
    pub fn set_var(&mut self, name: &str, block: u32) {
        if let Some(entry) = self.vars.iter_mut().find(|(n, _)| n == name) {
            entry.1 = block;
        } else {
            self.vars.push((name.to_string(), block));
        }
    }

    /// Build a BOM tree from entries (MUST be pre-sorted by key bytes
    /// ascending) and point `var` at it. `node_size`: 4096 typical.
    pub fn add_tree(&mut self, var: &str, entries: &[(Vec<u8>, Vec<u8>)], node_size: u32) {
        let max_entries = if (node_size as usize) > PATHS_HEADER_SIZE {
            ((node_size as usize - PATHS_HEADER_SIZE) / PATHS_ENTRY_SIZE).max(1)
        } else {
            1
        };

        let pairs: Vec<(u32, u32)> = entries
            .iter()
            .map(|(key, value)| {
                let key_id = self.add_block(key.clone());
                let value_id = self.add_block(value.clone());
                (value_id, key_id)
            })
            .collect();

        let child_block_id = if pairs.is_empty() {
            // Keep the tree well-formed with a single empty leaf.
            self.add_block(build_paths_block(true, 0, 0, &[], node_size as usize))
        } else {
            // Reserve leaf ids first so forward/backward links can reference siblings.
            let chunks: Vec<&[(u32, u32)]> = pairs.chunks(max_entries).collect();
            let leaf_ids: Vec<u32> = chunks.iter().map(|_| self.add_block(Vec::new())).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let forward = leaf_ids.get(i + 1).copied().unwrap_or(0);
                let backward = if i == 0 { 0 } else { leaf_ids[i - 1] };
                self.blocks[leaf_ids[i] as usize] =
                    build_paths_block(true, forward, backward, chunk, node_size as usize);
            }

            let mut level_ids = leaf_ids;
            // Internal-node separators are the LAST key of each child subtree (docs/FORMAT.md §1.4); first-key separators corrupt multi-leaf trees.
            let mut level_lasts: Vec<u32> = chunks.iter().map(|c| c[c.len() - 1].1).collect();

            while level_ids.len() > 1 {
                let mut next_ids = Vec::new();
                let mut next_lasts = Vec::new();
                for (id_chunk, last_chunk) in level_ids
                    .chunks(max_entries)
                    .zip(level_lasts.chunks(max_entries))
                {
                    let node_entries: Vec<(u32, u32)> = id_chunk
                        .iter()
                        .zip(last_chunk.iter())
                        .map(|(&id, &last)| (id, last))
                        .collect();
                    let node_id = self.add_block(build_paths_block(
                        false,
                        0,
                        0,
                        &node_entries,
                        node_size as usize,
                    ));
                    next_ids.push(node_id);
                    next_lasts.push(*last_chunk.last().unwrap());
                }
                level_ids = next_ids;
                level_lasts = next_lasts;
            }

            level_ids[0]
        };

        let tree = build_tree_header(child_block_id, node_size, entries.len() as u32, false);

        let tree_block_id = self.add_block(tree);
        self.set_var(var, tree_block_id);
    }

    /// Like `add_tree` but keys are raw inline u32s in the paths entries, not
    /// key blocks (BITMAPKEYS). `entries` MUST be pre-sorted by key ascending.
    pub fn add_tree_inline_keys(&mut self, var: &str, entries: &[(u32, Vec<u8>)], node_size: u32) {
        let max_entries = if (node_size as usize) > PATHS_HEADER_SIZE {
            ((node_size as usize - PATHS_HEADER_SIZE) / PATHS_ENTRY_SIZE).max(1)
        } else {
            1
        };

        let pairs: Vec<(u32, u32)> = entries
            .iter()
            .map(|(key, value)| {
                let value_id = self.add_block(value.clone());
                (value_id, *key)
            })
            .collect();

        let child_block_id = if pairs.is_empty() {
            self.add_block(build_paths_block(true, 0, 0, &[], node_size as usize))
        } else {
            let chunks: Vec<&[(u32, u32)]> = pairs.chunks(max_entries).collect();
            let leaf_ids: Vec<u32> = chunks.iter().map(|_| self.add_block(Vec::new())).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let forward = leaf_ids.get(i + 1).copied().unwrap_or(0);
                let backward = if i == 0 { 0 } else { leaf_ids[i - 1] };
                self.blocks[leaf_ids[i] as usize] =
                    build_paths_block(true, forward, backward, chunk, node_size as usize);
            }

            let mut level_ids = leaf_ids;
            // Last-key separators, as in `add_tree`.
            let mut level_lasts: Vec<u32> = chunks.iter().map(|c| c[c.len() - 1].1).collect();

            while level_ids.len() > 1 {
                let mut next_ids = Vec::new();
                let mut next_lasts = Vec::new();
                for (id_chunk, last_chunk) in level_ids
                    .chunks(max_entries)
                    .zip(level_lasts.chunks(max_entries))
                {
                    let node_entries: Vec<(u32, u32)> = id_chunk
                        .iter()
                        .zip(last_chunk.iter())
                        .map(|(&id, &last)| (id, last))
                        .collect();
                    let node_id = self.add_block(build_paths_block(
                        false,
                        0,
                        0,
                        &node_entries,
                        node_size as usize,
                    ));
                    next_ids.push(node_id);
                    next_lasts.push(*last_chunk.last().unwrap());
                }
                level_ids = next_ids;
                level_lasts = next_lasts;
            }

            level_ids[0]
        };

        let tree = build_tree_header(child_block_id, node_size, entries.len() as u32, true);

        let tree_block_id = self.add_block(tree);
        self.set_var(var, tree_block_id);
    }

    /// Serialize the container.
    pub fn finish(self) -> Vec<u8> {
        let BomWriter { blocks, vars } = self;

        // Blocks follow the header, 4-byte aligned; block 0 is the null entry.
        let mut body = Vec::new();
        let mut addrs = vec![(0u32, 0u32); blocks.len()];
        for (id, block) in blocks.iter().enumerate() {
            if id == 0 {
                continue;
            }
            while body.len() % 4 != 0 {
                body.push(0);
            }
            let addr = HEADER_SIZE + body.len();
            body.extend_from_slice(block);
            addrs[id] = (addr as u32, block.len() as u32);
        }

        let index_offset = HEADER_SIZE + body.len();
        let mut index = Vec::new();
        index.extend_from_slice(&(blocks.len() as u32).to_be_bytes());
        for (addr, len) in &addrs {
            index.extend_from_slice(&addr.to_be_bytes());
            index.extend_from_slice(&len.to_be_bytes());
        }
        // Freelist trailer must be a fixed 20 zero bytes, not a bare 4-byte count (docs/FORMAT.md §1.2).
        index.extend_from_slice(&[0u8; 20]);

        let vars_offset = index_offset + index.len();
        let mut vars_bytes = Vec::new();
        vars_bytes.extend_from_slice(&(vars.len() as u32).to_be_bytes());
        for (name, block_id) in &vars {
            vars_bytes.extend_from_slice(&block_id.to_be_bytes());
            vars_bytes.push(name.len() as u8);
            vars_bytes.extend_from_slice(name.as_bytes());
        }

        let number_of_blocks = (blocks.len().saturating_sub(1)) as u32;

        let mut out = vec![0u8; HEADER_SIZE];
        out[0..8].copy_from_slice(MAGIC);
        out[8..12].copy_from_slice(&1u32.to_be_bytes());
        out[12..16].copy_from_slice(&number_of_blocks.to_be_bytes());
        out[16..20].copy_from_slice(&(index_offset as u32).to_be_bytes());
        out[20..24].copy_from_slice(&(index.len() as u32).to_be_bytes());
        out[24..28].copy_from_slice(&(vars_offset as u32).to_be_bytes());
        out[28..32].copy_from_slice(&(vars_bytes.len() as u32).to_be_bytes());

        out.extend_from_slice(&body);
        out.extend_from_slice(&index);
        out.extend_from_slice(&vars_bytes);
        out
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn parses_real_sample() {
        // Catalog-agnostic sanity checks against a real (untracked, Apple-copyrighted) catalog, if present.
        let path = Path::new("/Users/thea/Downloads/Assets.car");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(path).unwrap();
        let bom = Bom::parse(&data).unwrap();

        for name in ["CARHEADER", "RENDITIONS", "KEYFORMAT"] {
            assert!(bom.vars.contains_key(name), "missing core var {name}");
        }

        let header = bom.var_block("CARHEADER").unwrap();
        assert_eq!(&header[0..4], b"RATC", "CARHEADER magic");

        let renditions = bom.tree_entries("RENDITIONS").unwrap();
        assert!(!renditions.is_empty(), "catalog should have renditions");
        for (_key, value) in &renditions {
            assert_eq!(
                &value[0..4],
                b"ISTC",
                "RENDITIONS value should start with ISTC"
            );
        }

        // BITMAPKEYS uses inline keys; if present it must walk without error.
        if bom.vars.contains_key("BITMAPKEYS") {
            bom.tree_entries_inline_keys("BITMAPKEYS").unwrap();
        }
    }

    #[test]
    fn writer_round_trip() {
        let mut w = BomWriter::new();

        let plain = w.add_block(b"hello world".to_vec());
        w.set_var("PLAIN", plain);

        let small_entries = vec![
            (b"a".to_vec(), b"apple".to_vec()),
            (b"b".to_vec(), b"banana".to_vec()),
            (b"c".to_vec(), b"cherry".to_vec()),
        ];
        w.add_tree("SMALL", &small_entries, 4096);

        // Small node_size forces multiple leaves plus an internal node.
        let mut big_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for i in 0..1000u32 {
            let key = format!("{i:06}").into_bytes(); // fixed width sorts lexically like numerically
            let value = vec![(i % 251) as u8; 4 + (i as usize % 37)];
            big_entries.push((key, value));
        }
        w.add_tree("BIG", &big_entries, 128);

        let data = w.finish();
        let bom = Bom::parse(&data).unwrap();

        assert_eq!(bom.var_block("PLAIN").unwrap(), b"hello world");

        let small = bom.tree_entries("SMALL").unwrap();
        assert_eq!(small, small_entries);

        let big = bom.tree_entries("BIG").unwrap();
        assert_eq!(big, big_entries);
        for pair in big.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "entries should come back in sorted order"
            );
        }
    }

    /// Internal nodes must carry LAST-key separators (docs/FORMAT.md §1.4);
    /// first-key separators corrupt any tree bigger than one leaf.
    #[test]
    fn internal_nodes_use_last_key_separators() {
        let mut w = BomWriter::new();
        let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..1000u32)
            .map(|i| (format!("{i:06}").into_bytes(), vec![1]))
            .collect();
        w.add_tree("BIG", &entries, 128); // 14 entries per node -> several levels
        let data = w.finish();
        let bom = Bom::parse(&data).unwrap();

        // Walk down from the root, checking every internal node level.
        let tree_block = bom.block(bom.vars["BIG"]).unwrap();
        let mut node_ids = vec![u32_be(tree_block, 8).unwrap()];
        loop {
            let node = bom.block(node_ids[0]).unwrap();
            if u16_be(node, 0).unwrap() != 0 {
                break; // reached the leaf level
            }
            let mut children = Vec::new();
            for &id in &node_ids {
                let node = bom.block(id).unwrap();
                let count = u16_be(node, 2).unwrap() as usize;
                assert!(count > 0);
                for e in 0..count {
                    let child_id = u32_be(node, PATHS_HEADER_SIZE + e * PATHS_ENTRY_SIZE).unwrap();
                    let key_id =
                        u32_be(node, PATHS_HEADER_SIZE + e * PATHS_ENTRY_SIZE + 4).unwrap();
                    // The separator key must be the last key under `child_id`.
                    let mut last = child_id;
                    loop {
                        let n = bom.block(last).unwrap();
                        let cnt = u16_be(n, 2).unwrap() as usize;
                        let last_entry = PATHS_HEADER_SIZE + (cnt - 1) * PATHS_ENTRY_SIZE;
                        let (val, key) = (
                            u32_be(n, last_entry).unwrap(),
                            u32_be(n, last_entry + 4).unwrap(),
                        );
                        if u16_be(n, 0).unwrap() != 0 {
                            assert_eq!(
                                bom.block(key_id).unwrap(),
                                bom.block(key).unwrap(),
                                "internal entry key must equal the child subtree's last key"
                            );
                            break;
                        }
                        last = val;
                    }
                    children.push(child_id);
                }
            }
            node_ids = children;
        }

        assert_eq!(bom.tree_entries("BIG").unwrap(), entries);
    }

    #[test]
    fn empty_tree_round_trip() {
        let mut w = BomWriter::new();
        w.add_tree("EMPTY", &[], 4096);
        let data = w.finish();
        let bom = Bom::parse(&data).unwrap();
        assert_eq!(
            bom.tree_entries("EMPTY").unwrap(),
            Vec::<(Vec<u8>, Vec<u8>)>::new()
        );
    }

    #[test]
    fn free_slot_sentinel_index_entries_parse_as_empty() {
        // Free/deleted index slots may carry the 0xFFFFFFFF sentinel; the parser must treat them as empty.
        let mut w = BomWriter::new();
        let real = w.add_block(b"payload".to_vec());
        w.set_var("V", real);
        let mut data = w.finish();

        // Flip block 0's index entry to the sentinel in place; it's unreferenced, so parsing must still succeed.
        let index_off = u32::from_be_bytes(data[16..20].try_into().unwrap()) as usize;
        let entry0 = index_off + 4; // first block-pointer entry (block id 0)
        data[entry0..entry0 + 4].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        data[entry0 + 4..entry0 + 8].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());

        let bom = Bom::parse(&data).expect("free-slot sentinel must parse");
        assert_eq!(bom.var_block("V"), Some(b"payload".as_slice()));
    }

    #[test]
    fn inline_key_tree_round_trip() {
        let mut w = BomWriter::new();
        let mut entries: Vec<(u32, Vec<u8>)> = Vec::new();
        for i in 0..500u32 {
            entries.push((i * 7, vec![(i % 251) as u8; 4 + (i as usize % 13)]));
        }
        w.add_tree_inline_keys("BITMAPKEYS", &entries, 1024);

        let data = w.finish();
        let bom = Bom::parse(&data).unwrap();
        let got = bom.tree_entries_inline_keys("BITMAPKEYS").unwrap();
        assert_eq!(got, entries);
        for pair in got.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "entries should come back in sorted order"
            );
        }
    }

    #[test]
    fn empty_inline_key_tree_round_trip() {
        let mut w = BomWriter::new();
        w.add_tree_inline_keys("BITMAPKEYS", &[], 1024);
        let data = w.finish();
        let bom = Bom::parse(&data).unwrap();
        assert_eq!(
            bom.tree_entries_inline_keys("BITMAPKEYS").unwrap(),
            Vec::<(u32, Vec<u8>)>::new()
        );
    }

    /// isInlineKeys must be 1 for inline-key trees (BITMAPKEYS) and 0
    /// otherwise; the wrong value breaks real BOM readers.
    #[test]
    fn tree_header_isinlinekeys_flag_is_correct() {
        let mut w = BomWriter::new();
        let normal_entries: Vec<(Vec<u8>, Vec<u8>)> =
            (0u8..3).map(|i| (vec![i; 24], vec![i])).collect();
        w.add_tree("NORMAL", &normal_entries, 4096);
        w.add_tree_inline_keys("INLINE", &[(7, vec![9, 9])], 1024);

        let data = w.finish();
        let bom = Bom::parse(&data).unwrap();

        for (name, want_inline_flag) in [("NORMAL", 0u8), ("INLINE", 1u8)] {
            let tree_block_id = *bom.vars.get(name).unwrap();
            let tb = &bom.blocks[tree_block_id as usize];
            assert_eq!(tb.len(), 21, "{name}: tree block must be 21 bytes");
            assert_eq!(&tb[0..4], b"tree", "{name}");
            assert_eq!(tb[20], want_inline_flag, "{name}: isInlineKeys flag");
        }

        assert_eq!(bom.tree_entries("NORMAL").unwrap(), normal_entries);
        assert_eq!(
            bom.tree_entries_inline_keys("INLINE").unwrap(),
            vec![(7, vec![9, 9])]
        );
    }
}
