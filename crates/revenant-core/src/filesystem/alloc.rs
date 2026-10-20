//! Cluster chains and who holds which cluster: shared by the FAT walk (live and deleted files) and exFAT.
//!
//! A live chain claims every cluster it passes; a chain that reaches a cluster another chain already holds stops there
//! (a cross-link) and the cluster becomes contested, so every holder of it can be marked untrusted afterwards.

use std::collections::BTreeSet;

/// Most runs (maximal stretches of consecutive clusters) one chain may have.
pub(crate) const MAX_RUNS: usize = 4096;

/// One allocation-table entry, as the walk sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Free,
    /// The next cluster of the chain (inside the heap).
    Next(u32),
    End,
    Bad,
    /// A reserved value, a cluster outside the heap, or an entry that could not be read.
    Invalid,
}

/// The allocation table of one volume.
pub(crate) trait ClusterMap {
    fn entry(&mut self, c: u32) -> Entry;
    /// Whether cluster `c` is in use; `None` when that cannot be read.
    #[allow(dead_code)] // Used by the deleted-file walk (Task 5) and exFAT (Task 6).
    fn allocated(&mut self, c: u32) -> Option<bool>;
}

/// The clusters live chains hold (a bitmap over the heap) and the contested ones (claimed twice, at most `cap`).
pub(crate) struct Claims {
    bits: Vec<u64>,
    /// Clusters `2 ..= last` exist.
    last: u32,
    contested: BTreeSet<u32>,
    cap: usize,
    overflow: bool,
}

impl Claims {
    /// For clusters `2 ..= cluster_count + 1`, with at most `cap` contested clusters.
    pub(crate) fn new(cluster_count: u32, cap: usize) -> Claims {
        let last = cluster_count.saturating_add(1);
        Claims {
            bits: vec![0; (last as usize + 1).div_ceil(64)],
            last,
            contested: BTreeSet::new(),
            cap,
            overflow: false,
        }
    }

    pub(crate) fn in_heap(&self, c: u32) -> bool {
        (2..=self.last).contains(&c)
    }

    pub(crate) fn is_claimed(&self, c: u32) -> bool {
        self.in_heap(c) && self.bits[c as usize / 64] & (1 << (c % 64)) != 0
    }

    fn claim(&mut self, c: u32) {
        self.bits[c as usize / 64] |= 1 << (c % 64);
    }

    /// Records that a second chain reached `c`. Past the cap the set stays as it is and [`Claims::overflowed`] turns
    /// true (ruling C11: the whole volume is then untrusted).
    fn contest(&mut self, c: u32) {
        if self.contested.contains(&c) {
            return;
        }
        if self.contested.len() >= self.cap {
            self.overflow = true;
        } else {
            self.contested.insert(c);
        }
    }

    /// The contested set filled up: no live chain of the volume can be trusted.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Whether any cluster of `runs` is contested.
    pub(crate) fn touches_contested(&self, runs: &[(u32, u32)]) -> bool {
        runs.iter().any(|&(first, n)| {
            let end = first.saturating_add(n);
            self.contested.range(first..end).next().is_some()
        })
    }
}

/// A followed chain, as runs `(first cluster, cluster count)` in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Chain {
    pub runs: Vec<(u32, u32)>,
    /// Exactly the clusters asked for, the last one's entry `End`.
    pub complete: bool,
    /// The chain reached a cluster another chain holds.
    pub conflict: bool,
    /// The last cluster followed has an `End` entry (what a directory, which has no recorded size, is judged by).
    pub ended: bool,
}

impl Chain {
    fn holds(&self, c: u32) -> bool {
        self.runs
            .iter()
            .any(|&(first, n)| c >= first && c - first < n)
    }
}

/// Follows the chain from `first` for `need` clusters, claiming each; stops at End, an invalid/free/bad entry, a
/// cluster out of range or already claimed (conflict; a cluster of this same chain is a cycle, not a conflict), or
/// [`MAX_RUNS`] runs. Each cluster is examined once, so the work is at most `min(need, clusters of the volume)` steps.
pub(crate) fn follow(
    map: &mut dyn ClusterMap,
    claims: &mut Claims,
    first: u32,
    need: u64,
) -> Chain {
    let mut chain = Chain::default();
    let mut c = first;
    let mut taken = 0u64;
    while taken < need && claims.in_heap(c) {
        if claims.is_claimed(c) {
            if !chain.holds(c) {
                chain.conflict = true;
                claims.contest(c);
            }
            break;
        }
        let extends = chain.runs.last().is_some_and(|&(start, n)| start + n == c);
        if extends {
            chain.runs.last_mut().expect("a last run").1 += 1;
        } else if chain.runs.len() < MAX_RUNS {
            chain.runs.push((c, 1));
        } else {
            break;
        }
        claims.claim(c);
        taken += 1;
        match map.entry(c) {
            Entry::Next(n) => c = n,
            Entry::End => {
                chain.ended = true;
                break;
            }
            Entry::Free | Entry::Bad | Entry::Invalid => break,
        }
    }
    chain.complete = chain.ended && taken == need;
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A map from explicit entries; every other cluster is free.
    struct Map(HashMap<u32, Entry>);

    impl ClusterMap for Map {
        fn entry(&mut self, c: u32) -> Entry {
            self.0.get(&c).copied().unwrap_or(Entry::Free)
        }

        fn allocated(&mut self, c: u32) -> Option<bool> {
            Some(self.entry(c) != Entry::Free)
        }
    }

    #[test]
    fn follow_stops_on_cycles_and_conflicts() {
        let mut map = Map(HashMap::from([(2, Entry::Next(3)), (3, Entry::Next(2))]));
        let mut claims = Claims::new(100, 16);
        let a = follow(&mut map, &mut claims, 2, 10);
        assert_eq!(a.runs, vec![(2, 2)]);
        assert!(!a.complete && !a.conflict && !a.ended);
        // A second chain claiming 3: conflict, and 3 is contested.
        let b = follow(&mut map, &mut claims, 3, 1);
        assert!(b.conflict && !b.complete);
        assert!(b.runs.is_empty());
        assert!(claims.touches_contested(&[(3, 1)]));
        assert!(!claims.touches_contested(&[(2, 1)]));
        assert!(claims.touches_contested(&a.runs));

        // 5000 one-cluster runs: stops at 4096 runs.
        let mut map = Map(HashMap::new());
        for i in 0..5000u32 {
            let c = 10 + 2 * i;
            map.0.insert(
                c,
                if i == 4999 {
                    Entry::End
                } else {
                    Entry::Next(c + 2)
                },
            );
        }
        let mut claims = Claims::new(20_000, 16);
        let long = follow(&mut map, &mut claims, 10, 5000);
        assert_eq!(long.runs.len(), MAX_RUNS);
        assert!(!long.complete);
        // The cluster that would have opened run 4097 is not claimed.
        assert!(!claims.is_claimed(10 + 2 * 4096));
        assert!(claims.is_claimed(10 + 2 * 4095));
    }

    #[test]
    fn follow_complete_only_with_exactly_the_clusters_needed() {
        let mut map = Map(HashMap::from([
            (5, Entry::Next(6)),
            (6, Entry::Next(9)),
            (9, Entry::End),
        ]));
        let mut claims = Claims::new(100, 16);
        let c = follow(&mut map, &mut claims, 5, 3);
        assert_eq!(c.runs, vec![(5, 2), (9, 1)]);
        assert!(c.complete && c.ended && !c.conflict);
        // The chain is longer than the size says: not complete, and nothing past `need` is claimed.
        let mut claims = Claims::new(100, 16);
        let c = follow(&mut map, &mut claims, 5, 2);
        assert_eq!(c.runs, vec![(5, 2)]);
        assert!(!c.complete && !c.ended);
        assert!(!claims.is_claimed(9));
        // Shorter than the size: not complete.
        let mut claims = Claims::new(100, 16);
        let c = follow(&mut map, &mut claims, 5, 4);
        assert!(!c.complete && c.ended);
        // Free, bad and out-of-range entries stop it.
        for stop in [Entry::Free, Entry::Bad, Entry::Invalid, Entry::Next(500)] {
            let mut map = Map(HashMap::from([(5, stop)]));
            let mut claims = Claims::new(100, 16);
            let c = follow(&mut map, &mut claims, 5, 2);
            assert_eq!(c.runs, vec![(5, 1)], "{stop:?}");
            assert!(!c.complete && !c.ended && !c.conflict, "{stop:?}");
        }
        // A first cluster outside the heap gives nothing.
        let mut claims = Claims::new(100, 16);
        assert!(follow(&mut map, &mut claims, 0, 1).runs.is_empty());
        assert!(follow(&mut map, &mut claims, 102, 1).runs.is_empty());
        assert_eq!(map.allocated(5), Some(true));
        assert_eq!(map.allocated(7), Some(false));
    }

    #[test]
    fn contested_set_overflows_at_its_cap() {
        let mut map = Map(HashMap::from([
            (2, Entry::Next(3)),
            (3, Entry::Next(4)),
            (4, Entry::End),
        ]));
        let mut claims = Claims::new(100, 2);
        follow(&mut map, &mut claims, 2, 3);
        for c in [2, 3] {
            follow(&mut map, &mut claims, c, 1);
        }
        assert!(!claims.overflowed());
        follow(&mut map, &mut claims, 3, 1); // already contested: no change
        assert!(!claims.overflowed());
        follow(&mut map, &mut claims, 4, 1);
        assert!(claims.overflowed());
        assert!(!claims.touches_contested(&[(4, 1)]));
    }
}
