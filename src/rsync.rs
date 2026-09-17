//! The rsync delta algorithm in pure Rust: rolling-checksum block
//! signatures, delta generation against a signature, and patch application.
//!
//! We control both ends of the wire (the local client and the remote agent
//! are the same binary), so compatibility with real rsync is not needed.

use std::collections::HashMap;

use blake2::{Blake2b512, Digest};

/// Default block size used for signatures and deltas.
pub const BLOCK_LEN: usize = 32 * 1024;
/// Length of the strong (BLAKE2b, truncated) per-block hash.
pub const STRONG_LEN: usize = 16;

/// Pick a block size for a file of `len` bytes, like rsync does (~√len,
/// clamped): small files get fine-grained deltas, big files stay cheap.
pub fn block_len_for(len: u64) -> usize {
    let sqrt = (len as f64).sqrt() as usize;
    sqrt.clamp(2048, 1 << 20)
}

/// Per-block signature: rolling checksum (a, b) + strong hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSig {
    pub a: u64,
    pub b: u64,
    pub strong: [u8; STRONG_LEN],
}

/// rsync-style rolling checksum over a window: a = Σ wᵢ, b = Σ (n−i)·wᵢ.
fn rollsum(w: &[u8]) -> (u64, u64) {
    let n = w.len() as u64;
    let mut a = 0u64;
    let mut b = 0u64;
    for (i, &x) in w.iter().enumerate() {
        a += x as u64;
        b += (n - i as u64) * x as u64;
    }
    (a, b)
}

/// Slide a window one byte: drop `out_b`, append `in_b`, window length `n`.
fn roll(a: &mut u64, b: &mut u64, out_b: u8, in_b: u8, n: u64) {
    *a = *a - out_b as u64 + in_b as u64;
    *b = *b - n * out_b as u64 + *a;
}

fn strong(w: &[u8]) -> [u8; STRONG_LEN] {
    let h = Blake2b512::digest(w);
    let mut out = [0u8; STRONG_LEN];
    out.copy_from_slice(&h[..STRONG_LEN]);
    out
}

/// Signature of `data`: one entry per `block_len` block (last may be short).
pub fn signature(data: &[u8], block_len: usize) -> Vec<BlockSig> {
    data.chunks(block_len)
        .map(|c| {
            let (a, b) = rollsum(c);
            BlockSig {
                a,
                b,
                strong: strong(c),
            }
        })
        .collect()
}

/// Serialize a signature for the wire.
pub fn sig_to_bytes(sigs: &[BlockSig]) -> Vec<u8> {
    const REC: usize = 8 + 8 + STRONG_LEN;
    let mut out = Vec::with_capacity(sigs.len() * REC);
    for s in sigs {
        out.extend_from_slice(&s.a.to_le_bytes());
        out.extend_from_slice(&s.b.to_le_bytes());
        out.extend_from_slice(&s.strong);
    }
    out
}

/// Parse a signature received over the wire.
pub fn sig_from_bytes(mut buf: &[u8]) -> anyhow::Result<Vec<BlockSig>> {
    const REC: usize = 8 + 8 + STRONG_LEN;
    if buf.len() % REC != 0 {
        anyhow::bail!("corrupt signature: {} bytes", buf.len());
    }
    let mut out = Vec::with_capacity(buf.len() / REC);
    while !buf.is_empty() {
        let a = u64::from_le_bytes(buf[..8].try_into().expect("len checked"));
        let b = u64::from_le_bytes(buf[8..16].try_into().expect("len checked"));
        let mut strong = [0u8; STRONG_LEN];
        strong.copy_from_slice(&buf[16..16 + STRONG_LEN]);
        out.push(BlockSig { a, b, strong });
        buf = &buf[REC..];
    }
    Ok(out)
}

const OP_COPY: u8 = 1;
const OP_LITERAL: u8 = 2;

fn emit_literal(out: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    out.push(OP_LITERAL);
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Generate a delta that transforms the file described by `sigs` into `new`.
pub fn delta(new: &[u8], sigs: &[BlockSig], block_len: usize) -> Vec<u8> {
    let mut by_roll: HashMap<(u64, u64), Vec<u32>> = HashMap::with_capacity(sigs.len());
    for (i, s) in sigs.iter().enumerate() {
        by_roll.entry((s.a, s.b)).or_default().push(i as u32);
    }

    let mut out = Vec::new();
    let mut lit_start = 0usize;
    let n = block_len as u64;

    let mut i = 0usize;
    if new.len() >= block_len {
        let (mut a, mut b) = rollsum(&new[..block_len]);
        while i + block_len <= new.len() {
            let hit = by_roll.get(&(a, b)).and_then(|cands| {
                let s = strong(&new[i..i + block_len]);
                cands
                    .iter()
                    .find(|&&ix| sigs[ix as usize].strong == s)
                    .copied()
            });
            if let Some(ix) = hit {
                emit_literal(&mut out, &new[lit_start..i]);
                out.push(OP_COPY);
                out.extend_from_slice(&ix.to_le_bytes());
                i += block_len;
                lit_start = i;
                if i + block_len <= new.len() {
                    let (na, nb) = rollsum(&new[i..i + block_len]);
                    a = na;
                    b = nb;
                }
            } else {
                if i + block_len < new.len() {
                    roll(&mut a, &mut b, new[i], new[i + block_len], n);
                }
                i += 1;
            }
        }
    }
    emit_literal(&mut out, &new[lit_start..]);
    out
}

/// Take `n` bytes from `delta` at `p`, bounds-checked (delta is remote input).
fn take<'a>(delta: &'a [u8], p: &mut usize, n: usize) -> anyhow::Result<&'a [u8]> {
    let end = p
        .checked_add(n)
        .filter(|&e| e <= delta.len())
        .ok_or_else(|| anyhow::anyhow!("truncated delta"))?;
    let s = &delta[*p..end];
    *p = end;
    Ok(s)
}

/// Apply `delta` to `old`, producing the new file content.
pub fn patch(old: &[u8], delta: &[u8], block_len: usize) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < delta.len() {
        match take(delta, &mut p, 1)?[0] {
            OP_COPY => {
                let ix = u32::from_le_bytes(take(delta, &mut p, 4)?.try_into().expect("4 bytes"))
                    as usize;
                let start = ix
                    .checked_mul(block_len)
                    .ok_or_else(|| anyhow::anyhow!("delta copy offset overflow"))?;
                anyhow::ensure!(start < old.len(), "delta copy out of range");
                let end = (start + block_len).min(old.len());
                out.extend_from_slice(&old[start..end]);
            }
            OP_LITERAL => {
                let len = u32::from_le_bytes(take(delta, &mut p, 4)?.try_into().expect("4 bytes"))
                    as usize;
                out.extend_from_slice(take(delta, &mut p, len)?);
            }
            op => anyhow::bail!("unknown delta op {op}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift for reproducible pseudo-random data without a rand dep.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            out.push((seed & 0xff) as u8);
        }
        out
    }

    fn roundtrip(old: &[u8], new: &[u8], block_len: usize) {
        let sig = signature(old, block_len);
        let wire = sig_to_bytes(&sig);
        let sig2 = sig_from_bytes(&wire).unwrap();
        assert_eq!(sig, sig2);
        let d = delta(new, &sig2, block_len);
        let got = patch(old, &d, block_len).unwrap();
        assert_eq!(got, new, "patch(old, delta) != new");
    }

    #[test]
    fn identical_files() {
        let data = noise(100_000, 1);
        roundtrip(&data, &data.clone(), BLOCK_LEN);
    }

    #[test]
    fn small_edit_in_large_file() {
        let mut new = noise(3 * BLOCK_LEN + 1000, 7);
        let old = new.clone();
        // change a few bytes in the middle
        for i in 0..10 {
            new[BLOCK_LEN + 100 + i] ^= 0xff;
        }
        let bl = block_len_for(old.len() as u64);
        let sig = signature(&old, bl);
        let d = delta(&new, &sig, bl);
        // with adaptive block size, only the edited block region is literal
        assert!(d.len() < bl + 2048, "delta too big: {}", d.len());
        assert_eq!(patch(&old, &d, bl).unwrap(), new);
    }

    #[test]
    fn insertion_shifts_blocks() {
        let mut new = noise(2 * BLOCK_LEN, 42);
        let old = new.clone();
        new.splice(1000..1000, vec![7u8; 500]); // insert 500 bytes early
        roundtrip(&old, &new, BLOCK_LEN);
    }

    #[test]
    fn complete_rewrite() {
        let old = noise(50_000, 3);
        let new = noise(50_000, 4);
        roundtrip(&old, &new, BLOCK_LEN);
    }

    #[test]
    fn edge_cases() {
        roundtrip(b"", b"", BLOCK_LEN);
        roundtrip(b"", b"hello", BLOCK_LEN);
        roundtrip(b"hello", b"", BLOCK_LEN);
        roundtrip(b"short", b"short2", BLOCK_LEN);
        roundtrip(&noise(BLOCK_LEN - 1, 5), &noise(BLOCK_LEN - 1, 6), BLOCK_LEN);
        roundtrip(&noise(BLOCK_LEN, 5), &noise(BLOCK_LEN + 1, 5), BLOCK_LEN);
    }

    #[test]
    fn corrupt_delta_is_an_error() {
        assert!(patch(b"abc", &[99, 1, 2], BLOCK_LEN).is_err());
        assert!(patch(b"abc", &[OP_COPY, 5, 0, 0, 0], BLOCK_LEN).is_err());
        assert!(patch(b"abc", &[OP_LITERAL, 10, 0, 0, 0, 1], BLOCK_LEN).is_err());
    }
}
