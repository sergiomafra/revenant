//! Cluster chains and who holds which cluster: shared by the FAT walk (live and deleted files) and exFAT.
//!
//! A live chain claims every cluster it passes; a chain that reaches a cluster another chain already holds stops there
//! (a cross-link) and the cluster becomes contested, so every holder of it can be marked untrusted afterwards. A deleted
//! file has no chain left: where its clusters were is guessed ([`guess`], ruling D4) from the allocation alone, and it
//! claims nothing.

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
    fn allocated(&mut self, c: u32) -> Option<bool>;

    /// How many clusters in a row from `from` (up to `to`, inclusive, at most the last cluster of the heap) are free
    /// (`free`), or not free (`!free`: allocated or unreadable). A map may answer faster than one
    /// [`ClusterMap::allocated`] call per cluster.
    fn count(&mut self, from: u32, to: u32, free: bool) -> u64 {
        let mut n = 0;
        let mut c = from as u64;
        while c <= to as u64 && (self.allocated(c as u32) == Some(false)) == free {
            n += 1;
            c += 1;
        }
        n
    }
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

    /// How many clusters in a row from `from` (up to `to`, inclusive, at most the last cluster) are claimed
    /// (`claimed`) or not (`!claimed`), 64 at a time.
    pub(crate) fn run_len(&self, from: u32, to: u32, claimed: bool) -> u64 {
        let (from, to) = (from as u64, to.min(self.last) as u64);
        let mut c = from;
        while c <= to {
            let w = self.bits[(c / 64) as usize];
            let differs = (if claimed { !w } else { w }) >> (c % 64);
            if differs != 0 {
                return (c + differs.trailing_zeros() as u64).min(to + 1) - from;
            }
            c = (c / 64 + 1) * 64;
        }
        to + 1 - from
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

/// The allocation as the deleted-file guesses must see it once the live walk is done (ruling R10): a cluster any live
/// chain claims is in use, even when its FAT entry reads free (a chain broken there, or cross-linked into it).
pub(crate) struct Unclaimed<'m> {
    pub map: &'m mut dyn ClusterMap,
    pub claims: &'m Claims,
}

impl ClusterMap for Unclaimed<'_> {
    fn entry(&mut self, c: u32) -> Entry {
        self.map.entry(c)
    }

    fn allocated(&mut self, c: u32) -> Option<bool> {
        if self.claims.is_claimed(c) {
            return Some(true);
        }
        self.map.allocated(c)
    }

    fn count(&mut self, from: u32, to: u32, free: bool) -> u64 {
        if free {
            match self.map.count(from, to, true) {
                0 => 0,
                n => self.claims.run_len(from, from + (n - 1) as u32, false),
            }
        } else {
            let mut c = from as u64;
            while c <= to as u64 {
                c += self.map.count(c as u32, to, false);
                if c > to as u64 {
                    break;
                }
                let claimed = self.claims.run_len(c as u32, to, true);
                if claimed == 0 {
                    break;
                }
                c += claimed;
            }
            c - from as u64
        }
    }
}

/// Per-volume budget of cluster examinations ([`super::MAX_GUESS_STEPS`]), shared by [`guess`], the old exFAT chain
/// and the bitmap check; once exhausted the caller lists no more deleted files of that volume (walk order, ruling C7).
pub(crate) struct Budget {
    left: u64,
    exhausted: bool,
}

impl Budget {
    pub(crate) fn new(steps: u64) -> Budget {
        Budget {
            left: steps,
            exhausted: false,
        }
    }

    /// Takes `n` steps; false, and exhausted from then on, when fewer are left.
    pub(crate) fn spend(&mut self, n: u64) -> bool {
        if self.exhausted || n > self.left {
            self.exhausted = true;
            return false;
        }
        self.left -= n;
        true
    }

    pub(crate) fn exhausted(&self) -> bool {
        self.exhausted
    }

    pub(crate) fn left(&self) -> u64 {
        if self.exhausted { 0 } else { self.left }
    }
}

/// Where a deleted file's clusters probably are (ruling D4): runs `(first cluster, count)` in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Guess {
    pub runs: Vec<(u32, u32)>,
    /// Allocated clusters were stepped over (the file is then fragmented, so untrusted).
    pub skipped: bool,
    /// Fewer clusters than the size needs were found.
    pub short: bool,
}

/// The clusters of a deleted file, a run at a time, as ruling D4 places them: free clusters from the first one up,
/// stepping over allocated (or unreadable) ones, until `last` or `2 × need + 1024` clusters were examined. Each
/// examined cluster spends one step of the budget, exactly as if they were looked at one by one.
pub(crate) struct Cursor {
    next: u64,
    last: u32,
    examined: u64,
    bound: u64,
    found: bool,
    skipped: bool,
}

impl Cursor {
    /// `None` when `first` is outside `2 ..= last` or not free today (the file was overwritten).
    pub(crate) fn new(
        map: &mut dyn ClusterMap,
        first: u32,
        need: u64,
        last: u32,
    ) -> Option<Cursor> {
        if first < 2 || first > last || map.allocated(first) != Some(false) {
            return None;
        }
        Some(Cursor {
            next: first as u64,
            last,
            examined: 0,
            bound: need.saturating_mul(2).saturating_add(1024),
            found: false,
            skipped: false,
        })
    }

    /// The next run of free clusters `(first, count)`, at most `max` long (`max ≥ 1`); `Ok(None)` at the end (past
    /// `last`, or the examination bound reached); `Err(())` when the budget ran out first.
    pub(crate) fn run(
        &mut self,
        map: &mut dyn ClusterMap,
        budget: &mut Budget,
        max: u64,
    ) -> Result<Option<(u32, u32)>, ()> {
        loop {
            if self.next > self.last as u64 || self.examined >= self.bound {
                return Ok(None);
            }
            // Clusters that may still be examined, and those the budget pays for.
            let room = (self.last as u64 - self.next + 1).min(self.bound - self.examined);
            let paid = room.min(budget.left());
            if paid == 0 {
                budget.spend(1);
                return Err(());
            }
            let at = self.next as u32;
            let end = (self.next + paid - 1) as u32;
            let busy = map.count(at, end, false);
            self.take(budget, busy);
            if busy > 0 && self.found {
                self.skipped = true;
            }
            if busy == paid {
                continue;
            }
            let len = map.count(self.next as u32, end, true).min(max);
            if len == 0 {
                // The map disagrees with itself: count the cluster as not free.
                self.take(budget, 1);
                self.skipped |= self.found;
                continue;
            }
            let first = self.next as u32;
            self.take(budget, len);
            self.found = true;
            return Ok(Some((first, len as u32)));
        }
    }

    fn take(&mut self, budget: &mut Budget, n: u64) {
        self.examined += n;
        self.next += n;
        budget.spend(n);
    }

    /// Allocated clusters were stepped over after the first one.
    pub(crate) fn skipped(&self) -> bool {
        self.skipped
    }
}

/// Ruling D4: `need` clusters for a deleted file starting at `first`, in at most [`MAX_RUNS`] runs (past that the guess
/// is short). `None` when `first` is outside `2 ..= last` or allocated (overwritten), or the budget runs out.
pub(crate) fn guess(
    map: &mut dyn ClusterMap,
    budget: &mut Budget,
    first: u32,
    need: u64,
    last: u32,
) -> Option<Guess> {
    let mut cursor = Cursor::new(map, first, need, last)?;
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut taken = 0u64;
    while taken < need {
        // A run past the cap is examined only as far as its first cluster, as the rule reads cluster by cluster.
        let full = runs.len() == MAX_RUNS;
        let max = if full { 1 } else { need - taken };
        let Some((c, n)) = cursor.run(map, budget, max).ok()? else {
            break;
        };
        match runs.last_mut() {
            Some((start, len)) if *start + *len == c => *len += n,
            _ if full => break,
            _ => runs.push((c, n)),
        }
        taken += n as u64;
    }
    Some(Guess {
        runs,
        skipped: cursor.skipped(),
        short: taken < need,
    })
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

    /// Counts `allocated` calls.
    struct Counted(Map, u64);

    impl ClusterMap for Counted {
        fn entry(&mut self, c: u32) -> Entry {
            self.0.entry(c)
        }

        fn allocated(&mut self, c: u32) -> Option<bool> {
            self.1 += 1;
            self.0.allocated(c)
        }
    }

    fn used(clusters: impl IntoIterator<Item = u32>) -> Map {
        Map(clusters.into_iter().map(|c| (c, Entry::End)).collect())
    }

    #[test]
    fn guess_follows_the_rules() {
        let steps = || Budget::new(1 << 26);
        // A free map: one run, exactly the clusters needed.
        let g = guess(&mut used([]), &mut steps(), 10, 5, 100).unwrap();
        assert_eq!(
            g,
            Guess {
                runs: vec![(10, 5)],
                skipped: false,
                short: false
            }
        );
        // One allocated cluster in the middle is stepped over.
        let g = guess(&mut used([12]), &mut steps(), 10, 5, 100).unwrap();
        assert_eq!(g.runs, vec![(10, 2), (13, 3)]);
        assert!(g.skipped && !g.short);
        // Allocated clusters before the end do not count as skipped once enough clusters were taken.
        let g = guess(&mut used([15]), &mut steps(), 10, 5, 100).unwrap();
        assert!(!g.skipped);
        // The first cluster allocated (overwritten), unreadable or outside the heap: nothing.
        assert_eq!(guess(&mut used([10]), &mut steps(), 10, 5, 100), None);
        for first in [0, 1, 101] {
            assert_eq!(
                guess(&mut used([]), &mut steps(), first, 5, 100),
                None,
                "{first}"
            );
        }
        // The heap ends first: short.
        let g = guess(&mut used([]), &mut steps(), 98, 5, 100).unwrap();
        assert_eq!(g.runs, vec![(98, 3)]);
        assert!(g.short);
        // Everything allocated after the first cluster: at most 2 × need + 1024 clusters are examined (one step
        // each), then short.
        let mut map = Counted(used(11..200_000), 0);
        let mut budget = steps();
        let g = guess(&mut map, &mut budget, 10, 3, 199_999).unwrap();
        assert_eq!(g.runs, vec![(10, 1)]);
        assert!(g.short && g.skipped);
        assert_eq!(budget.left(), (1 << 26) - 1030);
        assert!(map.1 <= 2 * 1031, "{}", map.1);
        // The budget: each examination is one step, shared across calls, and once spent nothing more is guessed.
        let mut budget = Budget::new(7);
        assert!(guess(&mut used([]), &mut budget, 10, 5, 100).is_some());
        assert!(!budget.exhausted());
        assert_eq!(guess(&mut used([]), &mut budget, 20, 5, 100), None);
        assert!(budget.exhausted());
        assert_eq!(guess(&mut used([]), &mut budget, 30, 1, 100), None);
        let mut exact = Budget::new(5);
        assert!(guess(&mut used([]), &mut exact, 10, 5, 100).is_some());
        // At most 4096 runs: every other cluster allocated.
        let mut map = used((0..10_000u32).map(|i| 11 + 2 * i));
        let g = guess(&mut map, &mut steps(), 10, 5000, 30_000).unwrap();
        assert_eq!(g.runs.len(), MAX_RUNS);
        assert!(g.runs.iter().all(|&(_, n)| n == 1));
        assert!(g.short && g.skipped);
    }

    #[test]
    fn claimed_clusters_are_in_use_for_the_guesses() {
        // FAT: 20..=29 free but 23 (End); claims: 25 and 130..=200 (a live chain whose FAT entries read free).
        let mut map = used([23]);
        let mut claims = Claims::new(1000, 16);
        let mut fat = Map(HashMap::from([(25, Entry::End)]));
        follow(&mut fat, &mut claims, 25, 1);
        let mut long = Map((130..=200)
            .map(|c| {
                (
                    c,
                    if c == 200 {
                        Entry::End
                    } else {
                        Entry::Next(c + 1)
                    },
                )
            })
            .collect());
        follow(&mut long, &mut claims, 130, 71);
        assert_eq!(claims.run_len(130, 1000, true), 71);
        assert_eq!(claims.run_len(126, 1000, false), 4);
        assert_eq!(claims.run_len(26, 60, false), 35);
        assert_eq!(claims.run_len(131, 140, true), 10);
        let mut u = Unclaimed {
            map: &mut map,
            claims: &claims,
        };
        assert_eq!(u.allocated(25), Some(true));
        assert_eq!(u.allocated(24), Some(false));
        assert_eq!(u.count(20, 100, true), 3);
        assert_eq!(u.count(23, 100, false), 1);
        assert_eq!(u.count(24, 100, true), 1);
        assert_eq!(u.count(25, 100, false), 1);
        assert_eq!(u.count(129, 300, false), 0);
        assert_eq!(u.count(130, 300, false), 71);
        // Against one cluster at a time.
        struct One<'a, 'm>(&'a mut Unclaimed<'m>);
        impl ClusterMap for One<'_, '_> {
            fn entry(&mut self, c: u32) -> Entry {
                self.0.entry(c)
            }
            fn allocated(&mut self, c: u32) -> Option<bool> {
                self.0.allocated(c)
            }
        }
        for from in 2..260 {
            for to in [from, from + 3, from + 70, 300] {
                for free in [true, false] {
                    let fast = u.count(from, to, free);
                    assert_eq!(
                        fast,
                        One(&mut u).count(from, to, free),
                        "{from}..={to} {free}"
                    );
                }
            }
        }
        // A guess steps over the claimed clusters.
        let g = guess(&mut u, &mut Budget::new(1000), 24, 3, 1000).unwrap();
        assert_eq!(g.runs, vec![(24, 1), (26, 2)]);
        assert!(g.skipped);
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
