//! An append-only vector stored in fixed-size chunks.
//!
//! Result lists (search hits, filtered rows) grow to millions of entries
//! while the UI reads them under the same lock. A plain `Vec` would copy
//! everything on each reallocation — tens of milliseconds with the lock
//! held — and overshoot its capacity by up to 2×. Chunks never move, so an
//! append costs at most one small allocation. A list built in one piece
//! (a sort result) is adopted as a contiguous head without copying.

const CHUNK: usize = 1 << 16;

/// Append-only, chunked vector of `Copy` values.
#[derive(Clone, Debug)]
pub struct ChunkedVec<T> {
    /// Adopted contiguous elements (before the chunks).
    head: Vec<T>,
    chunks: Vec<Vec<T>>,
    len: usize,
}

impl<T> Default for ChunkedVec<T> {
    fn default() -> Self {
        ChunkedVec {
            head: Vec::new(),
            chunks: Vec::new(),
            len: 0,
        }
    }
}

impl<T: Copy> ChunkedVec<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopts `v` without copying it.
    pub fn from_vec(v: Vec<T>) -> Self {
        ChunkedVec {
            len: v.len(),
            head: v,
            chunks: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, v: T) {
        if self.chunks.last().is_none_or(|c| c.len() == CHUNK) {
            self.chunks.push(Vec::with_capacity(CHUNK));
        }
        if let Some(c) = self.chunks.last_mut() {
            c.push(v);
        }
        self.len += 1;
    }

    pub fn extend_from_slice(&mut self, vs: &[T]) {
        for &v in vs {
            self.push(v);
        }
    }

    pub fn get(&self, i: usize) -> Option<T> {
        if i < self.head.len() {
            return Some(self.head[i]);
        }
        let i = i - self.head.len();
        self.chunks
            .get(i / CHUNK)
            .and_then(|c| c.get(i % CHUNK))
            .copied()
    }

    pub fn first(&self) -> Option<T> {
        self.get(0)
    }

    pub fn last(&self) -> Option<T> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    /// Copies `[a, b)` (clipped to the length).
    pub fn slice(&self, a: usize, b: usize) -> Vec<T> {
        let b = b.min(self.len);
        (a.min(b)..b).filter_map(|i| self.get(i)).collect()
    }

    /// Index of the first element for which `pred` is false, assuming the
    /// elements are partitioned (like `slice::partition_point`).
    pub fn partition_point(&self, mut pred: impl FnMut(&T) -> bool) -> usize {
        if !self.head.last().is_none_or(&mut pred) {
            return self.head.partition_point(pred);
        }
        let k = self
            .chunks
            .partition_point(|c| c.last().is_some_and(&mut pred));
        self.head.len()
            + match self.chunks.get(k) {
                Some(c) => k * CHUNK + c.partition_point(pred),
                None => self.len - self.head.len(),
            }
    }

    /// All elements in order.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.head
            .iter()
            .copied()
            .chain(self.chunks.iter().flat_map(|c| c.iter().copied()))
    }

    /// Heap memory in bytes.
    pub fn heap_bytes(&self) -> usize {
        (self.head.capacity() + self.chunks.iter().map(|c| c.capacity()).sum::<usize>())
            * std::mem::size_of::<T>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behaves_like_a_vec() {
        let mut v = ChunkedVec::new();
        let n = CHUNK * 2 + 17;
        for i in 0..n {
            v.push(i as u64 * 2);
        }
        assert_eq!(v.len(), n);
        assert_eq!(v.get(CHUNK), Some(CHUNK as u64 * 2));
        assert_eq!(v.last(), Some((n as u64 - 1) * 2));
        assert_eq!(
            v.slice(CHUNK - 2, CHUNK + 2),
            vec![
                (CHUNK as u64 - 2) * 2,
                (CHUNK as u64 - 1) * 2,
                CHUNK as u64 * 2,
                (CHUNK as u64 + 1) * 2
            ]
        );
        for target in [
            0u64,
            1,
            2,
            CHUNK as u64 * 2,
            CHUNK as u64 * 2 + 1,
            n as u64 * 2,
            u64::MAX,
        ] {
            let want = (0..n).position(|i| i as u64 * 2 >= target).unwrap_or(n);
            assert_eq!(v.partition_point(|&x| x < target), want, "target {target}");
        }
        assert!(ChunkedVec::<u8>::new().partition_point(|_| true) == 0);
    }

    #[test]
    fn adopted_head_then_chunks() {
        let mut v = ChunkedVec::from_vec((0..10u64).collect());
        for i in 10..CHUNK as u64 + 20 {
            v.push(i);
        }
        let n = CHUNK + 20;
        assert_eq!(v.len(), n);
        assert_eq!(v.get(9), Some(9));
        assert_eq!(v.get(10), Some(10));
        assert_eq!(v.last(), Some(n as u64 - 1));
        assert_eq!(v.iter().count(), n);
        assert!(v.iter().enumerate().all(|(i, x)| x == i as u64));
        for target in [0u64, 5, 10, 11, CHUNK as u64 + 3, n as u64, u64::MAX] {
            let want = (target as usize).min(n);
            assert_eq!(v.partition_point(|&x| x < target), want, "target {target}");
        }
    }
}
