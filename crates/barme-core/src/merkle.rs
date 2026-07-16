//! A Merkle tree over an object's ordered chunk hashes.
//!
//! The manifest already lists an object's chunks, so the tree is a pure
//! function of that list: we don't store interior nodes, we recompute them when
//! a proof or a sync delta needs them. What the manifest keeps is the `root`,
//! a single hash that commits to the whole ordered sequence.
//!
//! Two things this buys that a flat list can't:
//!   - an inclusion proof: show a chunk belongs to an object in log(n) hashes,
//!     without shipping the whole chunk list;
//!   - a cheap identity for the data itself, independent of manifest metadata
//!     like timestamps or the active policy.
//!
//! Hashing is domain-separated so a leaf can never be read as an interior node
//! (the classic second-preimage trap): leaves are prefixed 0x00, interior nodes
//! 0x01, the empty tree 0x02. An odd node at a level is promoted unchanged.

use crate::Hash;
use serde::{Deserialize, Serialize};

const LEAF: u8 = 0x00;
const NODE: u8 = 0x01;
const EMPTY: u8 = 0x02;

fn leaf(chunk: &Hash) -> Hash {
    let mut buf = [0u8; 33];
    buf[0] = LEAF;
    buf[1..].copy_from_slice(chunk.as_bytes());
    Hash::of(&buf)
}

fn node(left: &Hash, right: &Hash) -> Hash {
    let mut buf = [0u8; 65];
    buf[0] = NODE;
    buf[1..33].copy_from_slice(left.as_bytes());
    buf[33..].copy_from_slice(right.as_bytes());
    Hash::of(&buf)
}

/// Collapse one level into the next: pair neighbours, promote a lone tail.
fn reduce(level: &[Hash]) -> Vec<Hash> {
    let mut next = Vec::with_capacity((level.len() + 1) / 2);
    let mut i = 0;
    while i < level.len() {
        if i + 1 < level.len() {
            next.push(node(&level[i], &level[i + 1]));
            i += 2;
        } else {
            next.push(level[i]); // odd tail promoted unchanged
            i += 1;
        }
    }
    next
}

/// The Merkle root over an ordered chunk list. Empty list -> a fixed sentinel.
pub fn root(chunks: &[Hash]) -> Hash {
    if chunks.is_empty() {
        return Hash::of(&[EMPTY]);
    }
    let mut level: Vec<Hash> = chunks.iter().map(leaf).collect();
    while level.len() > 1 {
        level = reduce(&level);
    }
    level[0]
}

/// Which side of the pairing a proof sibling sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

/// One hop up the tree: the sibling hash and which side it's on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub side: Side,
    pub hash: Hash,
}

/// An inclusion proof for one chunk: the audit path from its leaf to the root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proof {
    pub index: usize,
    pub total: usize,
    pub steps: Vec<Step>,
}

/// Build the inclusion proof for the chunk at `index`. `None` if out of range.
pub fn prove(chunks: &[Hash], index: usize) -> Option<Proof> {
    if index >= chunks.len() {
        return None;
    }
    let total = chunks.len();
    let mut level: Vec<Hash> = chunks.iter().map(leaf).collect();
    let mut i = index;
    let mut steps = Vec::new();
    while level.len() > 1 {
        if i % 2 == 0 {
            if i + 1 < level.len() {
                steps.push(Step {
                    side: Side::Right,
                    hash: level[i + 1],
                });
            }
            // no right neighbour: this node is promoted, no sibling this level
        } else {
            steps.push(Step {
                side: Side::Left,
                hash: level[i - 1],
            });
        }
        level = reduce(&level);
        i /= 2;
    }
    Some(Proof { index, total, steps })
}

/// Check that `chunk` sits under `root` via `proof`. Folds the audit path and
/// compares to the trusted root; a wrong chunk, tampered path, or wrong root
/// all fail.
pub fn verify(root_hash: &Hash, chunk: &Hash, proof: &Proof) -> bool {
    let mut cur = leaf(chunk);
    for step in &proof.steps {
        cur = match step.side {
            Side::Left => node(&step.hash, &cur),
            Side::Right => node(&cur, &step.hash),
        };
    }
    &cur == root_hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Hash {
        Hash::of(&[n])
    }

    fn chunks(n: usize) -> Vec<Hash> {
        (0..n as u8).map(h).collect()
    }

    #[test]
    fn empty_is_a_fixed_sentinel() {
        assert_eq!(root(&[]), root(&[]));
        assert_ne!(root(&[]), root(&[h(0)]));
    }

    #[test]
    fn single_root_is_its_leaf() {
        assert_eq!(root(&[h(7)]), leaf(&h(7)));
    }

    #[test]
    fn root_is_deterministic_and_order_sensitive() {
        assert_eq!(root(&chunks(5)), root(&chunks(5)));
        assert_ne!(root(&[h(1), h(2)]), root(&[h(2), h(1)]));
    }

    #[test]
    fn changing_one_chunk_changes_the_root() {
        let a = chunks(9);
        let mut b = a.clone();
        b[4] = h(200);
        assert_ne!(root(&a), root(&b));
    }

    #[test]
    fn proofs_verify_for_every_index_across_sizes() {
        for n in [1usize, 2, 3, 4, 5, 7, 8, 16, 17, 31] {
            let cs = chunks(n);
            let r = root(&cs);
            for i in 0..n {
                let p = prove(&cs, i).expect("in range");
                assert!(verify(&r, &cs[i], &p), "n={n} i={i}");
            }
            assert!(prove(&cs, n).is_none());
        }
    }

    #[test]
    fn a_proof_rejects_the_wrong_chunk_or_root() {
        let cs = chunks(6);
        let r = root(&cs);
        let p = prove(&cs, 2).unwrap();
        assert!(verify(&r, &cs[2], &p));
        assert!(!verify(&r, &cs[3], &p)); // wrong chunk
        assert!(!verify(&root(&chunks(7)), &cs[2], &p)); // wrong root
    }

    #[test]
    fn a_proof_for_one_index_does_not_prove_another() {
        let cs = chunks(8);
        let r = root(&cs);
        let p = prove(&cs, 1).unwrap();
        assert!(!verify(&r, &cs[5], &p));
    }
}
