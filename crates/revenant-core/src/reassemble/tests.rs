use super::*;
use crate::formats::tests::Mem;
use std::ops::Range;

const U: u64 = 4096;
const K: u64 = 1024;
const MIB: u64 = 1 << 20;
const IMG: u64 = 16 * MIB;
const GRID: Grid = (MIB, U, IMG);
const ID: u32 = 0x5EA4;
const NOISE: u32 = 0xBAD0;

fn units(k: u32) -> u64 {
    k as u64 * U
}

fn header(id: u32, i: u32, n: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0..4].copy_from_slice(&id.to_le_bytes());
    h[4..8].copy_from_slice(&i.to_le_bytes());
    h[8..12].copy_from_slice(&n.to_le_bytes());
    h
}

/// Unit `i` of a toy file of `n` units: `[id][i][n]` then bytes seeded by `seed` and `i`.
fn unit(id: u32, i: u32, n: u32, seed: u32) -> Vec<u8> {
    let mut u = vec![0u8; U as usize];
    u[..12].copy_from_slice(&header(id, i, n));
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(i) | 1;
    for b in &mut u[12..] {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *b = x as u8;
    }
    u
}

/// A 16 MiB card, zeros except where toy units are put.
struct Card(Vec<u8>);

impl Card {
    fn new() -> Card {
        Card(vec![0; IMG as usize])
    }

    /// Units `units` of a toy file of `n` units with `id`, from `at`; returns their range.
    fn put(&mut self, at: u64, id: u32, units: Range<u32>, n: u32, seed: u32) -> Fragment {
        for (k, i) in units.clone().enumerate() {
            let p = (at + k as u64 * U) as usize;
            self.0[p..p + U as usize].copy_from_slice(&unit(id, i, n, seed));
        }
        Fragment {
            start: at,
            len: (units.end - units.start) as u64 * U,
        }
    }

    /// `count` units of another file.
    fn noise(&mut self, at: u64, count: u32) {
        self.put(at, NOISE, 0..count, count, 7);
    }

    fn mem(self) -> Mem {
        Mem(self.0)
    }
}

/// The toy format: a file of `n` units of 4096 bytes, unit `i` = `[ID][i][n][seeded bytes]`.
#[derive(Clone, Default)]
struct Toy {
    n: u32,
    /// Units not checked (no evidence either way).
    blind: Range<u32>,
    known_len: Option<u64>,
    anchor: Option<(u64, u64)>,
    /// The prefilter reads 16 bytes at the candidate.
    prefilter_reads: bool,
    /// `prove` ignores blind units (only the search's own rule can lower the proof).
    trusting: bool,
    /// The prefilter rejects every candidate.
    reject_all: bool,
    /// Prefilter calls so far.
    prefilter_calls: u64,
    /// A failed read counts as a good unit (a careless format: only the search's read limit flags it).
    ignore_errors: bool,
}

fn toy(n: u32) -> Toy {
    Toy {
        n,
        ..Toy::default()
    }
}

impl Seams for Toy {
    type State = u32;

    fn begin(&mut self) -> u32 {
        0
    }

    fn walk(&mut self, view: &dyn Source, from: &u32, limit: u64) -> Walked<u32> {
        let mut i = *from;
        let mut checked = units(i);
        let mut resume = Vec::new();
        let limit = limit.min(view.len());
        loop {
            if i >= self.n {
                return Walked {
                    end: WalkEnd::Complete { len: units(self.n) },
                    checked,
                    resume,
                };
            }
            let s = units(i);
            if s + U > limit {
                return Walked {
                    end: WalkEnd::Broken { at: s },
                    checked,
                    resume,
                };
            }
            if !self.blind.contains(&i) {
                let mut h = [0u8; 12];
                let got = view.read_at(s, &mut h);
                if got.is_err() && self.ignore_errors {
                    h = header(ID, i, self.n);
                } else if got.ok() != Some(12) {
                    return Walked {
                        end: WalkEnd::Stopped,
                        checked,
                        resume,
                    };
                }
                if h != header(ID, i, self.n) {
                    return Walked {
                        end: WalkEnd::Broken { at: s },
                        checked,
                        resume,
                    };
                }
                checked = s + U;
                resume.push((s + U, i + 1));
            }
            i += 1;
        }
    }

    fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof {
        let w = self.walk(view, &0, view.len());
        if !matches!(w.end, WalkEnd::Complete { len } if len == view.len()) {
            return Proof::Failed;
        }
        let blind_seam = seams.iter().any(|&p| {
            let k = (p / U) as u32;
            self.blind.contains(&k) || (k > 0 && self.blind.contains(&(k - 1)))
        });
        if blind_seam && !self.trusting {
            Proof::Unproven
        } else {
            Proof::Proven
        }
    }

    fn known_len(&self) -> Option<u64> {
        self.known_len
    }

    fn anchor(&self) -> Option<(u64, u64)> {
        self.anchor
    }

    fn prefilter(&mut self, r: &mut Reader, g: u64, seam: u64) -> bool {
        let _ = seam;
        self.prefilter_calls += 1;
        if self.prefilter_reads {
            let mut b = [0u8; 16];
            r.read(g, &mut b);
        }
        !self.reject_all
    }
}

/// Searches the toy file starting at `start` on the 1 MiB-heap grid, `live` taken, nothing claimed.
fn search_on(
    src: &dyn Source,
    toy: &mut Toy,
    start: u64,
    live: &[Fragment],
    limits: &Limits,
    known: Option<u64>,
) -> (Option<Search>, Budget) {
    let taken = Taken::new(live, &Runs::default());
    let starts = [start];
    let space = Space::new(&[GRID], start, src.len(), &taken, &starts, limits);
    let mut budget = Budget::new(limits);
    let found = search(
        toy,
        src,
        &space,
        start,
        known,
        limits,
        &mut budget,
        &CancelToken::default(),
    );
    (found, budget)
}

/// Places runs of 8 units of a `n`-unit toy file at `places` (run k = units 8k..8k+8), 4 noise units after each.
fn runs_at(card: &mut Card, n: u32, places: &[u64]) -> Vec<Fragment> {
    places
        .iter()
        .enumerate()
        .map(|(k, &p)| {
            let k = k as u32;
            let f = card.put(p, ID, 8 * k..8 * k + 8, n, 1);
            card.noise(p + units(8), 4);
            f
        })
        .collect()
}

const FIVE: [u64; 5] = [MIB, 3 * MIB, 2 * MIB, 5 * MIB, 4 * MIB];

fn complement(keep: &[Fragment]) -> Vec<Fragment> {
    let mut keep = keep.to_vec();
    keep.sort_by_key(|f| f.start);
    let mut out = Vec::new();
    let mut pos = MIB;
    for f in keep {
        if f.start > pos {
            out.push(Fragment {
                start: pos,
                len: f.start - pos,
            });
        }
        pos = pos.max(f.start + f.len);
    }
    if pos < IMG {
        out.push(Fragment {
            start: pos,
            len: IMG - pos,
        });
    }
    out
}

#[test]
fn space_lists_free_cluster_starts_in_prior_order() {
    let live = [Fragment {
        start: 2 * MIB,
        len: MIB,
    }];
    let mut claimed = Runs::default();
    claimed.add(5 * MIB, 5 * MIB + 8 * K, usize::MAX);
    let taken = Taken::new(&live, &claimed);
    let limits = Limits::DEFAULT;
    let space = Space::new(&[GRID], MIB, IMG, &taken, &[], &limits);
    assert!(space.on_grid());
    assert_eq!(space.unit(), U);
    assert_eq!(space.max_pieces(), 16);

    // The unit right at the end is the contiguous case, never a candidate.
    let (c, capped) = space.candidates(4 * MIB, &[], 8, &mut |_| true);
    assert_eq!(c, (1..9).map(|k| 4 * MIB + k * U).collect::<Vec<_>>());
    assert!(capped);

    // The claimed range is stepped over.
    let (c, _) = space.candidates(5 * MIB - 2 * U, &[], 4, &mut |_| true);
    assert_eq!(
        c,
        [
            5 * MIB - U,
            5 * MIB + 2 * U,
            5 * MIB + 3 * U,
            5 * MIB + 4 * U
        ]
    );

    // A used piece is stepped over.
    let used = [Fragment {
        start: 4 * MIB + U,
        len: 2 * U,
    }];
    let (c, _) = space.candidates(4 * MIB, &used, 3, &mut |_| true);
    assert_eq!(c, [4 * MIB + 3 * U, 4 * MIB + 4 * U, 4 * MIB + 5 * U]);

    // Next free forward, then the first hole from the heap start, skipping the live file.
    let end = IMG - 2 * U;
    let (c, capped) = space.candidates(end, &[], 1 + 256 + 5, &mut |_| true);
    let mut want = vec![IMG - U];
    want.extend((0..256).map(|k| MIB + k * U));
    want.extend((0..5).map(|k| 3 * MIB + k * U));
    assert_eq!(c, want);
    assert!(capped);

    // A unit holding another finding's start is not a candidate.
    let starts = [4 * MIB + 512];
    let space = Space::new(&[GRID], MIB, IMG, &taken, &starts, &limits);
    let (c, _) = space.candidates(4 * MIB, &[], 2, &mut |_| true);
    assert_eq!(c, [4 * MIB + U, 4 * MIB + 2 * U]);
}

#[test]
fn space_without_grid_is_forward_only_on_sectors() {
    assert_eq!(Limits::DEFAULT.no_grid_window, 64 << 20);
    let taken = Taken::new(&[], &Runs::default());
    let limits = Limits {
        no_grid_window: 8 * K,
        ..Limits::DEFAULT
    };
    // A start off the cluster grid, no grid at all, a grid with clusters under 4096: sectors.
    for (grids, start) in [
        (vec![GRID], MIB + 512),
        (vec![], MIB),
        (vec![(MIB, 2048, IMG)], MIB),
    ] {
        let space = Space::new(&grids, start, IMG, &taken, &[], &limits);
        assert!(!space.on_grid());
        assert_eq!(space.unit(), 512);
        assert_eq!(space.max_pieces(), 4);
        let (c, capped) = space.candidates(2 * MIB, &[], 100, &mut |_| true);
        assert_eq!(c, (1..16).map(|k| 2 * MIB + k * 512).collect::<Vec<_>>());
        assert!(!capped);
    }
}

#[test]
fn candidate_cap_counts_survivors_not_enumerated_units() {
    let taken = Taken::new(&[], &Runs::default());
    let keep = |g: u64| ((g - MIB) / U) % 8 == 7;
    let space_with = |window: u64| Limits {
        seam_window: window * U,
        ..Limits::DEFAULT
    };
    // From the heap start, so only the forward window is listed.
    let limits = space_with(64);
    let space = Space::new(&[GRID], MIB, IMG, &taken, &[], &limits);
    let (c, capped) = space.candidates(MIB, &[], 8, &mut |g| keep(g));
    assert_eq!(c.len(), 8);
    assert_eq!(c[7], MIB + 63 * U);
    assert!(!capped);

    let limits = space_with(1000);
    let space = Space::new(&[GRID], MIB, IMG, &taken, &[], &limits);
    let (c, capped) = space.candidates(MIB, &[], 8, &mut |g| keep(g));
    assert_eq!(c.len(), 8);
    assert!(capped);

    let limits = space_with(40);
    let space = Space::new(&[GRID], MIB, IMG, &taken, &[], &limits);
    let (c, capped) = space.candidates(MIB, &[], 8, &mut |g| keep(g));
    assert_eq!(c.len(), 5);
    assert!(!capped, "a window cut is not a cap hit");
}

#[test]
fn prefilter_reads_are_charged_and_memoized() {
    let src = Mem(vec![0; IMG as usize]);
    let taken = Taken::new(&[], &Runs::default());
    let limits = Limits::DEFAULT;
    let space = Space::new(&[GRID], MIB, IMG, &taken, &[], &limits);
    let counted = Counted::new(&src);
    let cancel = CancelToken::default();
    let mut t = Toy {
        prefilter_reads: true,
        ..toy(40)
    };
    let mut ctx = Ctx::new(
        &mut t,
        &counted,
        &space,
        &limits,
        &cancel,
        (None, false),
        (FILE_TRIES, FILE_READ),
    );
    // A 16-byte read costs the one sector its reader loads.
    assert!(ctx.keep(2 * MIB, units(8)));
    assert_eq!(counted.read(), SECTOR);
    ctx.keep(2 * MIB + U, units(8));
    assert_eq!(counted.read(), 2 * SECTOR);
    ctx.keep(2 * MIB, units(8));
    assert_eq!(
        counted.read(),
        2 * SECTOR,
        "the same (start, seam) is memoized"
    );
    ctx.keep(2 * MIB, units(16));
    assert_eq!(counted.read(), 3 * SECTOR);

    // Through the search, the prefilter's reads come out of the budget.
    let spent = |prefilter_reads: bool| {
        let mut card = Card::new();
        runs_at(&mut card, 40, &FIVE);
        let src = card.mem();
        let mut t = Toy {
            prefilter_reads,
            ..toy(40)
        };
        let (found, budget) = search_on(&src, &mut t, MIB, &[], &limits, None);
        assert!(found.unwrap().complete);
        Budget::new(&limits).read_left() - budget.read_left()
    };
    let extra = spent(true) - spent(false);
    assert!(extra > 0);
    assert_eq!(extra % SECTOR, 0);
}

#[test]
fn free_run_stops_at_taken_used_and_finding_starts() {
    let g = 4 * MIB;
    let fr =
        |claimed: &[(u64, u64)], used: &[Fragment], starts: &[u64], own: &[Fragment], max: u64| {
            let mut runs = Runs::default();
            for &(s, e) in claimed {
                runs.add(s, e, usize::MAX);
            }
            let live = [Fragment {
                start: g + 40 * K,
                len: U,
            }];
            let taken = Taken::new(&live, &runs);
            let space = Space::new(&[GRID], MIB, IMG, &taken, starts, &Limits::DEFAULT);
            space.free_run(g, used, own, max)
        };
    assert_eq!(fr(&[], &[], &[], &[], 32 * K), 32 * K);
    assert_eq!(
        fr(&[], &[], &[], &[], 5000),
        5000,
        "at most max, even mid-unit"
    );
    assert_eq!(fr(&[], &[], &[], &[], 64 * K), 40 * K, "a live file");
    assert_eq!(
        fr(&[(g + 20 * K, g + 28 * K)], &[], &[], &[], 64 * K),
        20 * K
    );
    assert_eq!(
        fr(&[(g + 20 * K + 100, g + 21 * K)], &[], &[], &[], 64 * K),
        20 * K,
        "the unit it touches"
    );
    let used = [Fragment {
        start: g + 16 * K,
        len: U,
    }];
    assert_eq!(fr(&[], &used, &[], &[], 64 * K), 16 * K);
    assert_eq!(fr(&[], &[], &[g + 12 * K], &[], 64 * K), 12 * K);
    assert_eq!(fr(&[], &[], &[g], &[], 32 * K), 32 * K, "its own start");

    // A claimed range wholly covered by `own` at its exact place is not a stop, nor its start.
    let own = [Fragment {
        start: g + 20 * K,
        len: 8 * K,
    }];
    assert_eq!(
        fr(
            &[(g + 20 * K, g + 28 * K)],
            &[],
            &[g + 20 * K],
            &own,
            32 * K
        ),
        32 * K
    );
    // Two claimed findings touching, merged by `Runs`, each covered by its own range.
    let own2 = [
        Fragment {
            start: g + 20 * K,
            len: 4 * K,
        },
        Fragment {
            start: g + 24 * K,
            len: 4 * K,
        },
    ];
    let touching = [(g + 20 * K, g + 24 * K), (g + 24 * K, g + 28 * K)];
    assert_eq!(fr(&touching, &[], &[], &own2, 32 * K), 32 * K);
    // 512 bytes off, or only partly covered: a stop.
    let off = [Fragment {
        start: g + 20 * K + 512,
        len: 8 * K,
    }];
    assert_eq!(
        fr(&[(g + 20 * K, g + 28 * K)], &[], &[], &off, 32 * K),
        20 * K
    );
    assert_eq!(fr(&touching, &[], &[], &own2[..1], 32 * K), 20 * K);

    // The heap end.
    let taken = Taken::new(&[], &Runs::default());
    let space = Space::new(&[GRID], MIB, IMG, &taken, &[], &Limits::DEFAULT);
    assert_eq!(space.free_run(IMG - U, &[], &[], 64 * K), U);
}

#[test]
fn toy_file_in_five_pieces_is_unique() {
    let mut card = Card::new();
    let runs = runs_at(&mut card, 40, &FIVE);
    let src = card.mem();
    let (found, _) = search_on(&src, &mut toy(40), MIB, &[], &Limits::DEFAULT, None);
    assert_eq!(
        found,
        Some(Search {
            pieces: runs,
            len: units(40),
            complete: true,
            unique: true,
            exhaustive: true,
            proof: Proof::Proven,
        })
    );
}

#[test]
fn toy_twin_continuations_are_ambiguous() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    card.put(6 * MIB, ID, 16..24, 40, 1);
    card.noise(6 * MIB + units(8), 4);
    let src = card.mem();
    let s = search_on(&src, &mut toy(40), MIB, &[], &Limits::DEFAULT, None)
        .0
        .unwrap();
    assert!(s.complete);
    assert!(!s.unique);
    assert!(s.exhaustive);
    assert_eq!(s.pieces[2].start, 6 * MIB, "the copy next free comes first");
}

#[test]
fn toy_budget_hit_is_not_exhaustive() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    let src = card.mem();
    let limits = Limits {
        file_tries: 3,
        ..Limits::DEFAULT
    };
    let s = search_on(&src, &mut toy(40), MIB, &[], &limits, None)
        .0
        .unwrap();
    assert!(!s.exhaustive);
    assert!(!s.complete);
}

#[test]
fn piece_cap_is_respected() {
    let mut card = Card::new();
    let runs = runs_at(
        &mut card,
        48,
        &[MIB, 3 * MIB, 2 * MIB, 5 * MIB, 4 * MIB, 6 * MIB],
    );
    let src = card.mem();
    let limits = Limits {
        pieces: 4,
        ..Limits::DEFAULT
    };
    let s = search_on(&src, &mut toy(48), MIB, &[], &limits, None)
        .0
        .unwrap();
    assert!(!s.complete);
    assert!(!s.exhaustive);
    assert_eq!(s.pieces, runs[..4]);
}

#[test]
fn known_len_must_match() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    let src = card.mem();
    let mut t = Toy {
        known_len: Some(units(41)),
        ..toy(40)
    };
    let s = search_on(&src, &mut t, MIB, &[], &Limits::DEFAULT, None)
        .0
        .unwrap();
    assert!(!s.complete);
}

#[test]
fn entry_size_must_match_when_known() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    let src = card.mem();
    let run = |t: &mut Toy, known| {
        search_on(&src, t, MIB, &[], &Limits::DEFAULT, known)
            .0
            .unwrap()
    };
    assert!(!run(&mut toy(40), Some(units(41))).complete);
    let s = run(&mut toy(40), Some(units(40)));
    assert!(s.complete);
    assert_eq!(s.len, units(40));
    let mut t = Toy {
        known_len: Some(units(41)),
        ..toy(40)
    };
    assert!(
        !run(&mut t, Some(units(40))).complete,
        "the two sizes disagree"
    );
    let mut t = Toy {
        known_len: Some(units(40)),
        ..toy(40)
    };
    assert!(
        !run(&mut t, Some(units(41))).complete,
        "the two sizes disagree"
    );
    assert!(run(&mut t, Some(units(40))).complete);
}

#[test]
fn anchor_fixes_the_last_piece() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    // A copy of unit 32 alone, next free: [.., 7 MiB (unit 32), 4 MiB + 1 unit (33..40)] is a second solution.
    card.put(7 * MIB, ID, 32..33, 40, 1);
    card.noise(7 * MIB + units(1), 4);
    let src = card.mem();
    // Without the anchor the copy next free comes first and the seam is ambiguous.
    let s = search_on(&src, &mut toy(40), MIB, &[], &Limits::DEFAULT, None)
        .0
        .unwrap();
    assert!(s.pieces.iter().any(|p| p.start == 7 * MIB));
    assert!(!s.unique);
    let mut t = Toy {
        anchor: Some((units(32), 4 * MIB)),
        ..toy(40)
    };
    let s = search_on(&src, &mut t, MIB, &[], &Limits::DEFAULT, None)
        .0
        .unwrap();
    assert!(s.complete);
    assert_eq!(s.pieces.last().unwrap().start, 4 * MIB);
    assert!(s.unique);
}

#[test]
fn blind_seam_is_placed_in_prior_order_and_unproven() {
    let mut card = Card::new();
    let a = card.put(MIB, ID, 0..24, 40, 1);
    let b = card.put(MIB + units(28), ID, 24..40, 40, 1);
    let src = card.mem();
    // Q14: the gap between the runs is taken.
    let live = [Fragment {
        start: MIB + units(24),
        len: units(4),
    }];
    let mut t = Toy {
        blind: 16..40,
        ..toy(40)
    };
    let s = search_on(&src, &mut t, MIB, &live, &Limits::DEFAULT, None)
        .0
        .unwrap();
    assert!(s.complete);
    assert_eq!(s.pieces, [a, b]);
    assert_eq!(s.proof, Proof::Unproven);
}

/// Q2's card, start at 4 MiB: A = units 0..8, T = unit 8 alone (blind) on one free cluster, R = units 9..40 two
/// clusters after T, and (when `look_alike`) L = units 8..40 with correct headers but other bytes, in the first hole at
/// the heap start. Everything else is taken. With [`q2_limits`] (a 16-unit window) L's own earlier breaks (Q26) cannot
/// reach R, so only the blind descent through T can make L non-unique.
fn q2_card(look_alike: bool) -> (Mem, Vec<Fragment>, [Fragment; 4]) {
    let mut card = Card::new();
    let a = card.put(4 * MIB, ID, 0..8, 40, 1);
    let t = card.put(4 * MIB + units(9), ID, 8..9, 40, 1);
    let r = card.put(4 * MIB + units(11), ID, 9..40, 40, 1);
    let l = card.put(MIB, ID, 8..40, 40, 99);
    let free: Vec<Fragment> = if look_alike {
        vec![a, t, l, r]
    } else {
        vec![a, t, r]
    };
    (card.mem(), complement(&free), [a, t, l, r])
}

fn q2_limits() -> Limits {
    Limits {
        seam_window: units(16),
        ..Limits::DEFAULT
    }
}

#[test]
fn blind_true_continuation_next_to_an_evident_look_alike_is_not_unique() {
    let (src, live, [a, _, l, _]) = q2_card(true);
    let mut t = Toy {
        blind: 8..9,
        ..toy(40)
    };
    let s = search_on(&src, &mut t, 4 * MIB, &live, &q2_limits(), None)
        .0
        .unwrap();
    assert!(s.complete);
    assert!(!s.unique, "the blind true continuation counts");
    assert!(s.exhaustive);
    assert_eq!(s.pieces, [a, l], "evidence before blind");
    assert!(!(s.unique && s.proof == Proof::Proven));

    // Without the look-alike: unique, but the seam at unit 8 stayed blind.
    let (src, live, [a, tt, _, r]) = q2_card(false);
    for trusting in [false, true] {
        let mut t = Toy {
            blind: 8..9,
            trusting,
            ..toy(40)
        };
        let s = search_on(&src, &mut t, 4 * MIB, &live, &q2_limits(), None)
            .0
            .unwrap();
        assert!(s.complete);
        assert!(s.unique);
        assert!(s.exhaustive);
        assert_eq!(s.pieces, [a, tt, r]);
        assert_eq!(s.proof, Proof::Unproven, "trusting {trusting}");
    }
}

#[test]
fn budget_cut_inside_a_blind_descent_is_not_exhaustive() {
    let (src, live, [a, _, l, _]) = q2_card(true);
    let mut t = Toy {
        blind: 8..9,
        ..toy(40)
    };
    let limits = q2_limits();
    let (s, budget) = search_on(&src, &mut t, 4 * MIB, &live, &limits, None);
    assert!(s.unwrap().exhaustive);
    let tries = Budget::new(&limits).tries_left() - budget.tries_left();
    // The last try is inside the blind descent (evidence first): one fewer cuts it.
    let limits = Limits {
        file_tries: tries - 1,
        ..q2_limits()
    };
    let s = search_on(&src, &mut t, 4 * MIB, &live, &limits, None)
        .0
        .unwrap();
    assert!(s.complete);
    assert_eq!(s.pieces, [a, l]);
    assert!(!s.exhaustive);
}

/// The reviewer's probe: A = units 0..8 at 1 MiB, one taken cluster, T = unit 8 (blind) followed by free noise, R =
/// units 9..40 at 1 MiB + 60 units. With `look_alike` the unit before R is noise too, so (noise) + R is an evident
/// continuation; without it that cluster is taken.
fn probe_card(look_alike: bool) -> (Mem, Vec<Fragment>) {
    let mut card = Card::new();
    card.put(MIB, ID, 0..8, 40, 1);
    card.put(MIB + units(9), ID, 8..9, 40, 1);
    card.noise(MIB + units(10), 50);
    card.put(MIB + units(60), ID, 9..40, 40, 1);
    let mut live = vec![Fragment {
        start: MIB + units(8),
        len: U,
    }];
    if !look_alike {
        live.push(Fragment {
            start: MIB + units(59),
            len: U,
        });
    }
    (card.mem(), live)
}

#[test]
fn blind_continuation_broken_later_in_its_run_is_descended() {
    let limits = Limits {
        seam_window: units(64),
        ..Limits::DEFAULT
    };
    for look_alike in [true, false] {
        let (src, live) = probe_card(look_alike);
        let mut t = Toy {
            blind: 8..9,
            ..toy(40)
        };
        let s = search_on(&src, &mut t, MIB, &live, &limits, None)
            .0
            .unwrap();
        assert!(s.complete, "look-alike {look_alike}");
        assert!(
            !s.unique,
            "look-alike {look_alike}: T then R is another solution"
        );
        assert!(!(s.unique && s.proof == Proof::Proven));
    }
}

#[test]
fn window_bounds_the_distance_not_the_piece() {
    // Q25: a contiguous file longer than the window is one piece.
    let limits = Limits {
        seam_window: units(16),
        ..Limits::DEFAULT
    };
    let mut card = Card::new();
    let f = card.put(4 * MIB, ID, 0..40, 40, 1);
    card.noise(4 * MIB + units(40), 4);
    let src = card.mem();
    let s = search_on(&src, &mut toy(40), 4 * MIB, &[], &limits, None)
        .0
        .unwrap();
    assert_eq!(s.pieces, [f]);
    assert!(s.complete);
    assert!(s.unique);
    assert!(s.exhaustive);
    assert_eq!(s.proof, Proof::Proven);

    // A look-alike of its tail in the first hole is another solution (Q26).
    let mut card = Card::new();
    card.put(4 * MIB, ID, 0..40, 40, 1);
    card.noise(4 * MIB + units(40), 4);
    card.put(MIB, ID, 16..40, 40, 99);
    let src = card.mem();
    let s = search_on(&src, &mut toy(40), 4 * MIB, &[], &limits, None)
        .0
        .unwrap();
    assert!(s.complete);
    assert!(!s.unique);
}

#[test]
fn complete_walk_tries_earlier_breaks_of_the_last_piece() {
    // Q26: T = units 8..16 followed contiguously by a look-alike tail (same headers, other bytes); the true tail R is
    // free elsewhere. T + tail completes, and so does T cut at unit 16 then R: not unique.
    let mut card = Card::new();
    card.put(MIB, ID, 0..8, 40, 1);
    card.noise(MIB + units(8), 4);
    card.put(2 * MIB, ID, 8..16, 40, 1);
    card.put(2 * MIB + units(8), ID, 16..40, 40, 99);
    card.noise(2 * MIB + units(32), 4);
    card.put(3 * MIB, ID, 16..40, 40, 1);
    card.noise(3 * MIB + units(24), 4);
    let src = card.mem();
    let s = search_on(&src, &mut toy(40), MIB, &[], &Limits::DEFAULT, None)
        .0
        .unwrap();
    assert!(s.complete);
    assert!(!s.unique);
    assert!(!(s.unique && s.proof == Proof::Proven));
}

#[test]
fn seam_candidate_cap_hit_is_not_exhaustive() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    let src = card.mem();
    let limits = Limits {
        seam_candidates: 1,
        ..Limits::DEFAULT
    };
    let s = search_on(&src, &mut toy(40), MIB, &[], &limits, None)
        .0
        .unwrap();
    assert!(!s.exhaustive);
}

#[test]
fn break_point_cap_hit_is_not_exhaustive() {
    // The contiguous-continuation card has two valid breaks (7 and 8 units) and is otherwise exhaustive.
    let mut card = Card::new();
    let a = card.put(MIB, ID, 0..8, 40, 1);
    let r = card.put(3 * MIB, ID, 8..40, 40, 1);
    let src = card.mem();
    let live = complement(&[a, r]);
    let limits = Limits {
        break_points: 1,
        ..Limits::DEFAULT
    };
    let mut t = Toy {
        blind: 7..8,
        ..toy(40)
    };
    let s = search_on(&src, &mut t, MIB, &live, &limits, None)
        .0
        .unwrap();
    assert!(!s.exhaustive);
}

/// A source whose reads fail inside `bad`.
struct Holes(Mem, Range<u64>);

impl Source for Holes {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError> {
        if offset < self.1.end && offset + buf.len() as u64 > self.1.start {
            return Err(EngineError::Read {
                offset,
                cause: std::io::Error::other("bad"),
            });
        }
        self.0.read_at(offset, buf)
    }
    fn volume(&self) -> Option<VolumeId> {
        None
    }
}

#[test]
fn stopped_candidate_walk_is_not_exhaustive() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    let src = Holes(card.mem(), 6 * MIB..6 * MIB + U);
    let s = search_on(&src, &mut toy(40), MIB, &[], &Limits::DEFAULT, None)
        .0
        .unwrap();
    assert!(s.complete);
    assert!(!s.exhaustive);
}

#[test]
fn prefilter_read_budget_hit_is_not_exhaustive() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    let src = card.mem();
    let limits = Limits {
        file_read: 8 * K,
        ..Limits::DEFAULT
    };
    let mut t = Toy {
        prefilter_reads: true,
        ..toy(40)
    };
    let s = search_on(&src, &mut t, MIB, &[], &limits, None).0.unwrap();
    assert!(!s.exhaustive);
}

#[test]
fn listing_work_is_charged_to_the_try_budget() {
    // Blind units then noise: 7 break points, each listing ~3800 free units that the prefilter rejects.
    let mut card = Card::new();
    card.put(MIB, ID, 0..8, 40, 1);
    card.noise(MIB + units(8), 4);
    let src = card.mem();
    let limits = Limits {
        file_tries: 50,
        ..Limits::DEFAULT
    };
    let mut t = Toy {
        blind: 1..8,
        reject_all: true,
        ..toy(40)
    };
    let (s, budget) = search_on(&src, &mut t, MIB, &[], &limits, None);
    assert!(!s.unwrap().exhaustive);
    assert!(Budget::new(&limits).tries_left() - budget.tries_left() <= 50);
    assert!(
        t.prefilter_calls <= 50 * LISTED_PER_TRY,
        "{} prefilter calls",
        t.prefilter_calls
    );
}

#[test]
fn cancel_stops_the_search() {
    let mut card = Card::new();
    runs_at(&mut card, 40, &FIVE);
    let src = card.mem();
    let limits = Limits::DEFAULT;
    let taken = Taken::new(&[], &Runs::default());
    let space = Space::new(&[GRID], MIB, IMG, &taken, &[MIB], &limits);
    let mut budget = Budget::new(&limits);
    let cancel = CancelToken::default();
    cancel.cancel();
    let found = search(
        &mut toy(40),
        &src,
        &space,
        MIB,
        None,
        &limits,
        &mut budget,
        &cancel,
    );
    assert_eq!(found, None);
    assert_eq!(budget.tries_left(), limits.scan_tries);
    assert_eq!(budget.read_left(), limits.scan_read);

    // Through the format layer's cancel scope too.
    let mut budget = Budget::new(&limits);
    let scoped = CancelToken::default();
    scoped.cancel();
    let _scope = crate::formats::CancelScope::enter(&scoped);
    let found = search(
        &mut toy(40),
        &src,
        &space,
        MIB,
        None,
        &limits,
        &mut budget,
        &CancelToken::default(),
    );
    assert_eq!(found, None);
    assert_eq!(budget.tries_left(), limits.scan_tries);
}

#[test]
fn contiguous_continuation_is_not_a_seam() {
    // A break point before a blind last unit, continuing at the very next unit, is the same bytes as no seam: it must
    // not count as a second solution.
    let mut card = Card::new();
    let a = card.put(MIB, ID, 0..8, 40, 1);
    let r = card.put(3 * MIB, ID, 8..40, 40, 1);
    let src = card.mem();
    let live = complement(&[a, r]);
    let mut t = Toy {
        blind: 7..8,
        ..toy(40)
    };
    let s = search_on(&src, &mut t, MIB, &live, &Limits::DEFAULT, None)
        .0
        .unwrap();
    assert!(s.complete);
    assert!(s.exhaustive);
    assert!(s.unique);
    assert_eq!(s.pieces, [a, r]);
}

#[test]
fn a_walk_cut_by_the_read_limit_is_not_exhaustive() {
    // One piece allowed, so no later try notices the budget: only the refused read does.
    let mut card = Card::new();
    card.put(MIB, ID, 0..8, 8, 1);
    let src = card.mem();
    let limits = Limits {
        pieces: 1,
        file_read: 24,
        ..Limits::DEFAULT
    };
    let mut t = Toy {
        ignore_errors: true,
        ..toy(8)
    };
    let s = search_on(&src, &mut t, MIB, &[], &limits, None).0.unwrap();
    assert!(s.complete);
    assert!(!s.exhaustive);
}
