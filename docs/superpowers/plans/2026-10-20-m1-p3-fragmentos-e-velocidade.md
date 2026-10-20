# Módulo 1 · Parte 3 — Arquivos fragmentados e velocidade — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A busca passa a correr no ritmo do leitor de cartão (≥ 300 MB/s medidos, do cache de páginas, em imagens densas de
JPEG e de RAW), nunca lê duas vezes o mesmo setor nem relê um setor ruim já conhecido, e remonta os dois casos de fragmentação
mais comuns em cartão de câmera: o JPEG partido em dois pedaços e o vídeo cujo `moov` ficou separado do `mdat`. Fecha as
pendências com dono 1.3.

**Architecture:** Primeiro um `revenant-dev bench` mede tudo (antes e depois). Um `CachedSource` interno fica entre a origem e
o motor: janelas de 1 MiB lidas uma vez, mapa de setores ruins compartilhado e registro do que já foi lido. A busca vira um
pipeline: um estágio lê blocos em ordem e despacha os candidatos para threads de trabalho (`std`), e um estágio de confirmação
aplica, em ordem, exatamente o algoritmo sequencial de hoje, então os eventos saem idênticos para qualquer número de threads.
`Finding`/`Measurement`/`Preview` passam a descrever um arquivo como 1+ pedaços (`Fragment`). O JPEG ganha um decodificador
de entropia próprio (só Huffman, sem IDCT) que conta MCUs e retoma de um ponto salvo, base da "bifragment gap carving". Vídeos
`Damaged` sem `moov` são pareados, depois da varredura, com `moov` órfãos achados por padrão de bytes, e saem como `Updated`.

**Tech Stack:** Rust 2024 (MSRV 1.88), `std::thread::scope` + `mpsc`/`Mutex`/`Condvar` (sem rayon), `memchr` (nova dependência
do core, Unlicense OR MIT), crates existentes (`zune-jpeg`, `thiserror`, `image` no testkit).

**Spec:** `docs/superpowers/specs/2026-10-19-revenant-design.md` (§5: "escultura com fragmentação", "desempenho", "somente
leitura", estado Íntegro/Parcial/Danificado).
**Antes:** `docs/superpowers/history/2026-10-19-m1-p1-pendencias.md` (R1–R11) e `docs/superpowers/history/2026-10-19-m1-p2-pendencias.md`
(P1–P18). **Roteiro:** `docs/superpowers/roadmap.md`.

## Global Constraints

- **Language [Sérgio]:** everything in the repository in English (identifiers, comments, error codes, commits, READMEs,
  CHANGELOG); `CLAUDE.md` and `docs/superpowers/**` stay in pt-BR.
- **Never write to the source.** Nothing in `revenant-core` gets a write path to a `Source`; `CachedSource` and `Fragmented`
  only read.
- **Licenses (spec §7):** dependencies only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense,
  CC0-1.0, BSL-1.0. New in this part: `memchr` in `revenant-core`; `revenant-testkit` becomes a normal dependency of
  `revenant-dev` (bench). Nothing else. No code copied from PhotoRec/TestDisk/libjpeg or any GPL/LGPL project.
- **Errors by code** (`code() -> &'static str`, snake_case); no UI text in the engine.
- **Rust:** edition 2024, `rust-version = "1.88"`. Dev machine: Intel Mac (8 cores/16 threads), Homebrew Rust; never
  `brew install/upgrade` toolchains. No `unsafe` outside `source/windows.rs`.
- **Bounded everything:** cache ≤ `ScanOptions::cache_bytes` (default 256 MiB), lookahead ≤ 16 blocks, concurrent decodes ≤ the
  512 MiB decode budget, JPEG gap window ≤ 64 MiB and ≤ 64 join attempts, orphan `moov` candidates ≤ 4096, `moov` in memory
  ≤ `MOOV_MAX_LEN` (16 MiB). No loop or allocation driven by an on-disk number without a cap.
- **Determinism:** for a given source, the event sequence is identical for any `threads` and any `cache_bytes`.
- **Health semantics (binding, from 1.1/1.2):** `Intact` = structure complete AND content verified clean; `Partial` = salvageable
  but not clean; `Damaged` = won't open. Never `Intact` without evidence. A reassembled file is `Intact` only with the evidence
  its task lists; otherwise `Partial`.
- **Docs in the same commit [Sérgio]:** each task updates the READMEs it affects and `CHANGELOG.md` `[Unreleased]` for anything
  user-visible.
- **Commit trailer, always:** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Conventional Commits. Never
  `git add -A` (add files by name). Never push (the controller opens the PR).
- **Tests never touch real data or physical disks**: synthetic images in `tempfile` only (the bench writes its images to a temp
  dir too). Every new test is seen failing before it passes (record RED in the report).
- Speed claims are measured with `revenant-dev bench` in a release build, never estimated.

## Review Focus

1. **Hostile data making the JPEG gap search explode** (64 MiB where every sector looks like a valid continuation ending in
   `FF D9`): bounded time, bounded reads, never `Intact` → `hostile_gap_search_is_bounded` (Task 8); the same for `moov`
   candidates → `hostile_moov_candidates_are_bounded` (Task 9).
2. **Two different photos glued into one "Intact" JPEG** (the real continuation is gone and another photo's data sits after
   the gap): must not come out `Intact` → `glued_halves_of_two_photos_are_not_intact`, `seam_check_rejects_a_glued_dc_grid`
   (Task 8).
3. **Parallelism reordering events or racing the cancel**: identical events for 1, 2, 8 and 16 threads; no event after the one
   during which `cancel()` was called; workers joined before `scan` returns → `parallel_scan_matches_sequential_events`,
   `no_event_after_cancel`, `cancel_from_another_thread_stops_the_scan` (Task 6).
4. **The cache serving an unreadable sector as good data (zeros), or a different answer after eviction** →
   `known_bad_sector_is_never_served_as_data` and `cache_size_does_not_change_events` (Task 4).
5. **A `moov` paired with the wrong video** (two recordings from the same camera, or two copies of the same first half) →
   `moov_pairs_with_its_own_mdat_not_a_sibling`, `moov_matching_two_videos_is_not_paired` (Task 9).

## Pendências herdadas com dono 1.3

| Pendência | Destino |
|---|---|
| 1.1: setores ruins relidos a cada `measure` (mapa compartilhado) | Tarefa 4 |
| 1.1: ~62 MB/s, `mdat` lido duas vezes | Tarefas 1, 4, 5, 6 |
| 1.1: `dd bs=4m conv=noerror,sync` zera 4 MiB por setor ruim | Tarefa 10 (documentação: `ddrescue` com mapa; cópia própria na 1.5) |
| 1.1: `FileSource` com aparelho (tamanho 0, `dev()`, leituras alinhadas) e `volume()` None com aparelho | **Adiada para 1.5**: ler aparelho bruto é o escopo da 1.5 |
| 1.2: `moov` carregado várias vezes por medida; `all_formats()` por medida; custo do P13; releituras do MJPEG | Tarefa 5 |
| 1.2: saúde depende da posição perto do fim da origem (revisão final, item 4) | Tarefa 2 |
| 1.2: `confirms` do ISO BMFF aceita o que `measure` recusa (revisão final, item 8) | Tarefa 2 |
| 1.2: comentários de `claim()`/`TiffInfo.data` (o corte vale para toda faixa) | Tarefa 2 |
| 1.1 (revisão): JPEG cortado logo depois do cabeçalho sai Parcial sem nenhum pixel | Tarefa 7 |
| 1.2: JPEGs próprios de um achado Parcial relatados (JpgFromRaw de RW2) | **Adiada para 1.4**: o P17, decidido depois, entrega isso à deduplicação, que resolve o caso geral |
| 1.2: COUNT/tamanho corrompido num offset válido é cortado no limite | **Adiada**: sem decodificador de RAW não há como distinguir; os tamanhos do sistema de arquivos (1.4) resolvem |
| 1.2: ARW da DSLR-A100, empacotamentos de RW2 não conferidos, MakerNote `OLYMP\0`, HEIC real | **Adiadas** (sem dono de parte): pedem amostras reais com licença que não temos ou tratamento por modelo de câmeras de 2000–2006; voltam com o uso real |

Fora desta parte: exFAT/FAT32 e deduplicação (1.4); aparelho bruto, desmontar, cópia do cartão, sessão salva (1.5);
**fragmentação de RAW** (sem decodificador de RAW não há como validar a emenda dos dados brutos; um RAW cortado segue Parcial);
JPEG progressivo ou aritmético (o decodificador próprio só cobre o sequencial Huffman, que é o que câmera grava); arquivos em 3+
pedaços; segundo pedaço antes do primeiro no disco (JPEG); reparo de vídeo com arquivo de referência (pós-1.0).

---

### Task 1: `revenant-dev bench` e a medida de partida

**Files:**
- Create: `crates/revenant-dev/src/bench.rs`; Modify: `crates/revenant-dev/src/main.rs`, `crates/revenant-dev/Cargo.toml`
  (testkit as a normal dependency, `tempfile` too)
- Modify: `crates/revenant-testkit/src/lib.rs`
- Test: `crates/revenant-dev/tests/cli.rs`
- Docs: `crates/revenant-dev/README.md` ("Benchmark" section)

**Interfaces:**
- Produces (testkit):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum BenchImage { Noise, DenseJpeg, DenseRaw, Video, CutJpeg }
  impl BenchImage {
      pub const ALL: [BenchImage; 5];
      pub fn name(self) -> &'static str;              // "noise", "dense-jpeg", "dense-raw", "video", "cut-jpeg"
      pub fn parse(s: &str) -> Option<BenchImage>;
  }
  /// Streams a synthetic disk image of exactly `len` bytes into `out`; returns how many files it placed. Files go back to
  /// back, each at the next 512-byte boundary, the rest is seeded noise. Big = `len >= 512 MiB`:
  /// DenseJpeg: 8 distinct `jpeg(w, h, seed, 30_000)` cycled, (w, h) = (3000, 2000) if big else (320, 240);
  /// CutJpeg: the same with the second half of each file's bytes replaced by noise;
  /// DenseRaw: 4 distinct `tiff_raw` (Arw, Nef, Cr2, Dng), raw_len 24 MiB / 1 MiB, preview (1616, 1080) / (160, 120);
  /// Video: `mp4` moov at end, mdat_len 256 MiB / 8 MiB; Noise: no files.
  pub fn write_bench_image(kind: BenchImage, len: u64, out: &mut dyn std::io::Write) -> std::io::Result<u64>;
  ```
- Produces (CLI): `revenant-dev bench [--size-mib N=1024] [--runs N=3] [--images a,b,..=all] [--dir DIR=temp]`. Writes each
  image into a fresh temp dir under `--dir`, then scans it `--runs` times (the first write leaves it in the page cache) and
  prints one JSON line per image:
  `{"event":"bench","image":"dense-jpeg","bytes":1073741824,"placed":102,"found":102,"runs":3,"best_seconds":2.91,"mb_per_s":368.9}`
  (MB = 10^6 bytes; `found` = `ScanSummary::found` of the best run). Temp files are removed at the end. An unknown image
  name is rejected by clap (exit 2) through a `value_parser` over `BenchImage::parse`.

- [ ] **Step 1: Failing test** (`cli.rs`):
  ```rust
  #[test] fn bench_reports_each_image() {
      // revenant-dev bench --size-mib 8 --runs 1 --images noise,dense-jpeg --dir <tempdir>
      // → exit 0; exactly 2 JSON lines; image names in order; bytes == 8 * 1024 * 1024; mb_per_s > 0.0;
      //   noise: placed == 0; dense-jpeg: placed >= 4 and found == placed; the temp dir is empty afterwards
  }
  ```
- [ ] **Step 2:** run → FAIL (no `bench` subcommand). **Step 3:** implement. **Step 4:** `cargo test --workspace`, clippy, fmt.
- [ ] **Step 5: Baseline.** `cargo run --release -p revenant-dev -- bench` on the dev Mac; paste the 5 lines in the report and in
  the commit body (they are the "before" numbers Task 10 compares against). README: how to run it, what each image is, that
  numbers are page-cache throughput (an upper bound for a card reader). Commit: `feat(dev): scan throughput benchmark`.

---

### Task 2: Pendências da 1.2 — posição, `confirms` do ISO BMFF

**Files:**
- Modify: `crates/revenant-core/src/tiff.rs`, `crates/revenant-core/src/formats/{tiffraw.rs,isobmff.rs}`
- Test: `crates/revenant-core/tests/{tiffraw.rs,isobmff.rs,scan.rs}`
- Docs: core README (TIFF-based RAW health, MP4/MOV `confirms`)

Rules:
- **TIFF, sem depender da posição:** `walk` is always run with limit `TIFF_MAX_LEN` relative to the header. A referenced range
  that starts at or past `TIFF_MAX_LEN` is a structural error (`Damaged`) wherever the source ends. A range below the cap that
  runs past the end of the source is a cut (`Partial`; P11 unchanged: one starting past the end is not counted). So the same
  bytes give the same health whether the source ends right after the file or 3 GiB later. Comments of `claim()` and
  `TiffInfo.data` say the clamp applies to every range.
- **ISO BMFF `confirms` = what `measure` accepts:** a header-only walk from `start` (≤ 64 top-level box headers, no payload
  reads, never calls `file_starts_at`, so not recursive) confirms iff it meets an `mdat`, or the `ftyp` has a HEIC brand and a
  `meta` is met, or a box header falls in an unreadable sector. A lone `ftyp` + `free` no longer cuts another file.

- [ ] **Step 1: Failing tests:**
  ```rust
  // tiffraw.rs — SparseSource { data: Vec<u8>, len: u64 }: bytes from `data`, zeros past it, up to `len`
  #[test] fn corrupt_offset_past_the_cap_is_damaged_wherever_the_source_ends() {
      // testkit Arw with the raw StripOffsets patched to 0x5000_0000 (1.25 GiB):
      // TiffRaw.measure(source of len bytes.len() + 4096, 0).health == Damaged; same with len 3 GiB → Damaged
  }
  #[test] fn offset_past_the_source_end_below_the_cap_is_partial() {
      // StripOffsets patched to bytes.len() + 1 MiB, source len bytes.len() + 4096 → Partial
  }
  // isobmff.rs
  #[test] fn confirms_agrees_with_measure() {
      // for each layout at AT in noise: (ftyp+free) → (false, None); (ftyp+free+mdat) → (true, Some);
      // (ftyp+moov, no mdat) → (false, None); testkit mp4 → (true, Some); testkit heic → (true, Some);
      // (ftyp "heic" + free only) → (false, None)
      // assert_eq!(IsoBmff.confirms(&src, AT), IsoBmff.measure(&src, AT).is_some())
  }
  // scan.rs
  #[test] fn stray_ftyp_does_not_cut_a_jpeg() {
      // testkit jpeg(640, 480, 31, 0) at 4096; at a sector start inside its scan data: a 20-byte ftyp box + an 8-byte free
      // box (no mdat). The bytes overwrite real scan data, so the health is not asserted: exactly one finding, at 4096,
      // with len == jpeg.len() (not cut at the ftyp)
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** README, CHANGELOG ("Changed": health of a RAW with a corrupt offset no longer depends on where the card
  ends; a stray MP4 signature no longer cuts a photo). Commit: `fix(core): position-independent RAW health, confirm only measurable videos`.

---

### Task 3: Um arquivo em pedaços — `Fragment` na API

**Files:**
- Modify: `crates/revenant-core/src/formats/mod.rs` (`Fragment`, `Measurement`), `src/scan.rs` (`Finding`), `src/inspect.rs`
  (`Preview`, fragmented inspection), `src/extract.rs`, `src/lib.rs`
- Create: `crates/revenant-core/src/source/fragmented.rs`; Modify: `src/source/mod.rs`
- Modify: `crates/revenant-dev/src/main.rs`
- Test: `crates/revenant-core/tests/{source.rs,extract.rs,inspect.rs}`, unit test in `revenant-dev/src/main.rs`, and every
  existing `Measurement {..}`/`Finding {..}` literal in tests (add `fragments: Vec::new()`)

**Interfaces:**
- Produces:
  ```rust
  // formats/mod.rs, re-exported as revenant_core::Fragment
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
  pub struct Fragment { pub start: u64, pub len: u64 }
  // Measurement loses `Copy` (keeps Clone, Debug, PartialEq, Eq):
  pub struct Measurement { pub len: u64, pub kind: FileKind, pub health: Health,
      /// Empty for a contiguous file. Otherwise ≥ 2 pieces in file order: the first starts at the measured start, the lens
      /// sum to `len`, no two overlap.
      pub fragments: Vec<Fragment> }
  // Finding gains the same field with the same contract (`start` = first piece's start, `len` = total length):
  pub fragments: Vec<Fragment>,
  impl Finding { /// `[Fragment { start, len }]` for a contiguous file, else `fragments`.
      pub fn ranges(&self) -> Vec<Fragment> }
  // Preview loses `Copy`, gains `pub fragments: Vec<Fragment>` (same contract, absolute pieces) and `pub fn ranges(&self) -> Vec<Fragment>`.

  // source/fragmented.rs, re-exported as revenant_core::source::Fragmented
  /// Read-only logical view of pieces of a source, in the given order: offset 0 is the first byte of the first piece.
  pub struct Fragmented<'a> { .. }
  impl<'a> Fragmented<'a> {
      pub fn new(source: &'a dyn Source, pieces: Vec<Fragment>) -> Self;
      /// Absolute pieces of the logical range [start, start + len), adjacent pieces merged.
      pub fn absolute(&self, start: u64, len: u64) -> Vec<Fragment>;
  }
  impl Source for Fragmented<'_> // len = sum of lens; a read error reports the ABSOLUTE offset; volume = the inner volume
  ```
- `extract` copies `finding.ranges()` in order, each clamped to the source end (unreadable stretches → zeros, as today).
- `inspect` of a finding with fragments inspects `Finding { start: 0, fragments: vec![], ..f }` over a `Fragmented` view and maps
  the preview back with `absolute()` (one piece → `fragments` empty).
- CLI: a `found` line gets `"fragments":[{"start":…,"len":…},…]` only when the finding has pieces; `preview` gets the same key
  under the same rule; `--previews` writes all preview pieces in order.

- [ ] **Step 1: Failing tests:**
  ```rust
  // source.rs
  #[test] fn fragmented_view_maps_reads_and_errors() {
      // pieces [(100, 10), (50, 20)] over a 4 KiB image: len() == 30; read_at(5, 10 bytes) == src[105..110] ++ src[50..55];
      // read_at(30, ..) == Ok(0); absolute(5, 10) == [Fragment{start:105,len:5}, Fragment{start:50,len:5}];
      // over a source failing reads in [60, 61): read_at(0, 30 bytes) → Err(Read { offset: 60, .. })
  }
  // extract.rs
  #[test] fn extracts_a_fragmented_finding_in_file_order() {
      // Finding { start: 8192, len: 2500, fragments: [(8192, 1000), (4096, 1500)], .. } → file bytes == img[8192..9192] ++ img[4096..5596]
  }
  // inspect.rs
  #[test] fn fragmented_jpeg_inspects_like_the_whole_file() {
      // jpeg_with_exif(640, 480, 3, &exif) split at the first 512-multiple after len/2; piece 1 at 8192, piece 2 at 1 MiB;
      // metadata == inspect of the same JPEG contiguous; preview.len == jpeg len; preview.ranges() == the two pieces
  }
  // revenant-dev main.rs #[cfg(test)]
  #[test] fn fragments_key_only_for_pieces() { /* found_line of a 2-piece finding has "fragments" with 2 entries; a contiguous one has no key */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (no producer of pieces yet; `scan` fills `fragments` from the measurement).
  **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README ("Findings in pieces", Extract, Inspect), dev README (`fragments` key), CHANGELOG. Commit:
  `feat(core): describe a found file as one or more pieces`.

---

### Task 4: `CachedSource` — ler cada setor uma vez, mapa de setores ruins

**Files:**
- Create: `crates/revenant-core/src/cache.rs` (crate-private); Modify: `src/lib.rs`, `src/scan.rs`
- Test: unit tests in `cache.rs`; `crates/revenant-core/tests/scan.rs` (update the 5 `ScanOptions {..}` literals with
  `..ScanOptions::default()`)

**Interfaces:**
- Produces:
  ```rust
  pub(crate) const CACHE_WINDOW: usize = 1 << 20;
  pub(crate) struct CachedSource<'a> { .. }   // Send + Sync
  impl<'a> CachedSource<'a> {
      /// `sector`: bad-sector granularity (the scan's normalized sector size); `capacity`: bytes of windows kept (LRU).
      pub(crate) fn new(inner: &'a dyn Source, sector: u64, capacity: usize) -> Self;
      /// Every byte of [start, end) has been read, or found unreadable, through this cache.
      pub(crate) fn covered(&self, start: u64, end: u64) -> bool;
      /// Unreadable runs `(start, len)` inside [start, end), merged, ascending.
      pub(crate) fn bad_runs(&self, start: u64, end: u64) -> Vec<(u64, u64)>;
  }
  impl Source for CachedSource<'_>
  // ScanOptions gains: pub cache_bytes: usize  (default 256 MiB; values below CACHE_WINDOW are raised to it)
  ```
Behavior (exactly that of the inner source, memoized):
- Window = aligned `CACHE_WINDOW` bytes, loaded with one inner read; if that fails, resolved once sector by sector: failing
  sectors become zeros in the window and enter the shared bad map (merged runs). Loaded windows are kept up to `capacity` bytes
  (LRU). A window being loaded by one thread is waited for (Condvar) by the others, never read twice concurrently.
- `read_at` over a range that overlaps a known bad sector returns `Err(EngineError::Read { offset: <first bad sector>, cause:
  io::Error::other("unreadable sector") })` **every time**, without touching the inner source again; otherwise copies.
  `Reader`'s per-sector fallback then gets the good sectors from the cache and the bad ones as errors.
- Coverage = merged runs of windows fully resolved; runs below the scan position may be pruned (keep ≤ 65,536 runs).
- `scan` wraps its source in a `CachedSource` (granularity = normalized `opts.sector`) and passes `&cached` to every
  `measure`. Its block read goes through the cache. A block that is entirely inside the skip range of an `Intact` finding and
  `covered` is **not read again**: its sectors' bad flags come from `bad_runs`, and its `Unreadable` events are the same as if it
  had been read.

- [ ] **Step 1: Failing tests:**
  ```rust
  // cache.rs (unit)
  #[test] fn serves_identical_bytes() { /* 1000 seeded random (offset, len ≤ 3 MiB) reads over a 16 MiB noise source == inner */ }
  #[test] fn known_bad_sector_is_never_served_as_data() {
      // inner fails reads touching [1 MiB + 4096, + 512); counting inner attempts that touch it.
      // 3 × read_at over [1 MiB, 2 MiB) → Err each time; read_at(1 MiB + 4096, 512 bytes) → Err; read_at(1 MiB + 4608, 512) → Ok, == inner bytes;
      // bad_runs(0, 4 MiB) == [(1 MiB + 4096, 512)]; inner attempts touching the bad sector ≤ 2 in total
  }
  #[test] fn concurrent_readers_load_each_window_once() { /* 8 threads read all of a 16 MiB source → inner window reads == 16 */ }
  #[test] fn memory_is_bounded() { /* capacity 8 MiB, read 64 MiB sequentially → resident bytes ≤ 8 MiB (cfg(test) accessor) */ }
  // scan.rs
  #[test] fn each_byte_is_read_once() { /* counting source; 32 MiB noise with 6 JPEGs, a RAW, an MP4 → inner bytes read ≤ 32 MiB + 1 MiB */ }
  #[test] fn big_intact_video_is_read_once() { /* cache_bytes 4 MiB; testkit mp4 mdat_len 40 MiB in a 48 MiB image → inner bytes ≤ 50 MiB; finding Intact */ }
  #[test] fn bad_sector_is_read_once_across_measures() { /* JPEG + RAW + video around one bad sector inside the JPEG → inner attempts touching it ≤ 3 */ }
  #[test] fn cache_size_does_not_change_events() {
      // the busy image of `each_byte_is_read_once` plus 3 bad ranges (inside the JPEG, inside mdat, between files):
      // events with cache_bytes 1 MiB == events with the default
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; `bench` (release): paste the 5 lines.
- [ ] **Step 5:** core README ("Scan": the cache, `cache_bytes`, what is never read twice), CHANGELOG ("Changed": known
  unreadable sectors are never retried). Commit: `perf(core): read each sector once and remember unreadable ones`.

---

### Task 5: Custos dos detectores

**Files:**
- Modify: `crates/revenant-core/src/formats/{mod.rs,jpeg.rs,isobmff.rs,tiffraw.rs,raf.rs}`, `Cargo.toml` (`memchr`)
- Test: unit tests in `formats/mod.rs`, `formats/jpeg.rs`, `formats/isobmff.rs`

Changes (behavior identical; every existing test keeps passing unchanged):
- `pub(crate) fn builtin() -> &'static [Box<dyn Format>]` (a `OnceLock` of `all_formats()`), used by every detector instead of
  calling `all_formats()` per measure.
- `Reader` takes its 1 MiB window from a per-thread pool (at most 4 buffers kept per thread) and returns it on drop: no
  allocation per `measure`/`confirms` after warm-up.
- `Reader::find_ff(&mut self, from: u64, end: u64) -> Option<u64>` (memchr on window slices); the JPEG entropy walk jumps from
  `FF` to `FF` and still checks `file_starts_at` at every sector start it crosses.
- `OwnJpegs`: merge only ranges that overlap (not ones that merely touch), and `vouch_for` looks only at the members of the merged
  range that contains `pos` (binary search), so N touching tiles cost O(log N) per head, not O(N).
- ISO BMFF: within one `measure`, each `(offset, len)` box payload (`moov`, `meta`) is loaded at most once (memoized in the
  measure), across `judge`, `own_starts`, `cr3_jpegs` and the re-walks.

- [ ] **Step 1: Failing tests:**
  ```rust
  // formats/mod.rs
  #[test] fn reader_reuses_its_window() { /* 1000 × Reader::new + read + drop on one thread → fresh window allocations == 1 (cfg(test) counter) */ }
  #[test] fn builtin_formats_are_built_once() { /* std::ptr::eq(builtin(), builtin()) */ }
  // formats/jpeg.rs
  #[test] fn vouch_for_scales_with_touching_ranges() { /* 65,536 touching 512-byte ranges over zeros, 65,536 queries at positions between range starts → < 2 s in debug, and the members examined per query ≤ 2 (cfg(test) counter) */ }
  // formats/isobmff.rs
  #[test] fn moov_is_loaded_once_per_measure() {
      // counting source; cut Motion-JPEG MOV (moov at end, 4 MiB moov via `extra` padding boxes, mdat cut by a real JPEG at a
      // non-chunk sector) → inner read_at calls overlapping the moov range ≤ 5
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; `bench` (release): paste the 5 lines.
- [ ] **Step 5:** core README (only where it described internals that changed). Commit: `perf(core): no per-measure allocations, rebuilds or reloads`.

---

### Task 6: Análise em paralelo, eventos na mesma ordem

**Files:**
- Move `crates/revenant-core/src/scan.rs` → `src/scan/mod.rs` (public types, `scan`, the in-order commit loop); Create
  `src/scan/pipeline.rs` (read stage, job queue, workers) and `src/budget.rs` (decode budget)
- Modify: `src/formats/jpeg.rs` (`decode_health` acquires the budget), `crates/revenant-dev/src/{main.rs,bench.rs}` (`--threads`)
- Test: `crates/revenant-core/tests/scan.rs`, unit test in `budget.rs`, `crates/revenant-dev/tests/cli.rs`

**Interfaces:**
- Consumes: `CachedSource` (Task 4), `Measurement::fragments`/`Finding::fragments` (Task 3), `builtin()` (Task 5).
- Produces:
  ```rust
  // ScanOptions gains: pub threads: usize  (0 = available_parallelism().clamp(1, 16); default 0)
  pub(crate) struct Budget { .. }
  impl Budget {
      pub(crate) const fn new(limit: u64) -> Self;
      /// Blocks until `n` more bytes fit under the limit (a request above the limit waits until nothing is held, then runs
      /// alone); released when the guard drops.
      pub(crate) fn acquire(&self, n: u64) -> BudgetGuard<'_>;
  }
  pub(crate) static DECODE_BUDGET: Budget = Budget::new(512 << 20); // decode_health acquires len + width × height
  ```
  CLI: `scan --threads N` and `bench --threads N` (default 0 = auto); bench lines gain `"threads":N` (the resolved count).

Pipeline (the algorithm the tests do not determine):
```
scan = std::thread::scope {
  workers × threads: loop { job = queue.recv(); if cancelled { drop job; continue }
                            result = formats.iter().filter(|f| f.recognizes(head)).find_map(|f| f.measure(&cached, at));
                            job.slot.fill(result) }
  read stage (scan thread) keeps `ahead`: VecDeque<Block> of at most LOOKAHEAD_BLOCKS = 16:
    Block { pos, len, bad: per-sector flags, heads: Vec<(at, Slot)>, deferred: bool }
    fill: if the block is `covered` and entirely inside a COMMITTED Intact skip range → deferred (flags from bad_runs, no heads)
          else read it through the cache, flag bad sectors, and dispatch one job per sector start whose 16-byte head some
          format recognizes and that is not inside a committed skip range and not bad.
  commit stage (scan thread): pop the oldest block and run the current per-sector loop unchanged in meaning (pending FIFO by
    start, Unreadable runs, downgrades, skip ranges); "measure at `at`" = wait for that head's slot, or, for a head that was
    not dispatched (deferred block outside a skip range at commit time), read the block now and measure on the scan thread.
  loop { fill `ahead`; commit one block; emit Progress }
}
```
- Skip ranges replace `skip_until`: an `Intact` finding adds one range per piece (rounded out to sectors); heads in the gap
  between pieces are examined. A pending finding is emitted once the scan passes the end of its last piece. An unreadable
  sector downgrades a pending `Intact` finding only if it lies inside one of its pieces.
- A measurement whose `fragments` break their contract (Task 3) is ignored (no finding).
- Cancel: checked before each block and after every emitted event; once seen, no further event is emitted, workers drop
  their remaining jobs, and `scan` returns `Err(Cancelled)` after the scope has joined every worker.
- Events, ids and `ScanSummary` are identical to the sequential algorithm for any `threads`/`cache_bytes`: `measure` is a pure
  function of the source bytes and its unreadable sectors.

- [ ] **Step 1: Failing tests:**
  ```rust
  // scan.rs — MultiFailingSource { inner, bad: Vec<(u64, u64)> }
  #[test] fn parallel_scan_matches_sequential_events() {
      // 48 MiB noise (seed 42): 6 JPEGs, 2 cut JPEGs, an MP4 (moov at end), a Motion-JPEG MOV, an ARW with a sector-aligned
      // preview, a RAF, a CR3, a HEIC, a JPEG inside a Damaged video; bad ranges inside a JPEG, inside an mdat, between files.
      // events(threads 1) == events(threads 2) == events(threads 8) == events(threads 16) == events(threads 8, cache 1 MiB);
      // and threads 8 run 3 times gives the same events each time
  }
  #[test] fn no_event_after_cancel() { /* threads 8; callback records events and cancels on the first Found → Err(Cancelled), last recorded event is that Found */ }
  #[test] fn cancel_from_another_thread_stops_the_scan() { /* SlowSource (2 ms per read_at) over 64 MiB noise; another thread cancels after 100 ms → Err(Cancelled) within 2 s of the cancel */ }
  #[test] fn fragmented_measurements_skip_only_their_pieces() {
      // test format TwoPieces: recognizes b"TWOPIECES-HEAD!!", measures len 2048, Intact, fragments [(at, 1024), (at + 1 MiB, 1024)].
      // formats [TwoPieces, Jpeg]; head at 4096; testkit JPEGs at 4096+512 (inside piece 1), 4096+64 KiB (gap), 4096+1 MiB+512 (inside piece 2)
      // → found == [two-piece at 4096 Intact with those fragments, Jpeg at 4096+64 KiB Intact]
      // with a bad sector at 4096+1 MiB+512: events == [Unreadable(that sector), Found(two-piece, Partial), Found(gap JPEG)];
      // with a bad sector at 4096+128 KiB instead: the two-piece finding stays Intact
  }
  #[test] fn inconsistent_fragments_are_ignored() { /* TwoPieces variant whose lens sum to 2047 → no finding at 4096 */ }
  // budget.rs
  #[test] fn budget_never_exceeds_its_limit() { /* 16 threads × 200 acquires of random n ≤ limit/3, a shared counter of held bytes never > limit; one request > limit completes alone */ }
  // cli.rs
  #[test] fn threads_option_gives_the_same_output() { /* scan --threads 1 and --threads 8 over the cli test image → identical stdout */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; `bench` (release, default threads and
  `--threads 1`): paste both sets. Target: dense-jpeg and dense-raw ≥ 300 MB/s at default threads. If a target is missed,
  stop and report the bench lines for `--threads 1, 2, 4, 8, 16` to the controller instead of tuning blindly.
- [ ] **Step 5:** core README ("Scan": threads, ordering guarantee, cancel), dev README (`--threads`), CHANGELOG ("Changed":
  scanning uses every core; results are the same). Commit: `perf(core): measure candidates in parallel, keep events in order`.

---

### Task 7: Decodificador de entropia do JPEG e o JPEG "só cabeçalho"

**Files:**
- Create: `crates/revenant-core/src/formats/jpeg_entropy.rs` (crate-private); Modify: `formats/mod.rs`, `formats/jpeg.rs`
- Modify: `crates/revenant-testkit/src/lib.rs`
- Test: unit tests in `jpeg_entropy.rs`; `crates/revenant-core/tests/jpeg.rs`

**Interfaces:**
- Produces (testkit):
  ```rust
  /// Grayscale baseline JPEG of a flat gray image, standard luminance Huffman tables, DRI = `interval` MCUs, built
  /// byte by byte (every block: DC diff 0, EOB; RSTn every `interval` MCUs, byte-aligned with 1-bits). Strict-decodable.
  pub fn jpeg_flat_with_restarts(width: u32, height: u32, interval: u16) -> Vec<u8>;
  ```
- Produces (core, crate-private):
  ```rust
  pub(crate) struct Frame { pub mcus_x: u32, pub mcus_y: u32, pub restart_interval: u16, .. } // Huffman tables, components, sampling
  impl Frame {
      pub(crate) fn total_mcus(&self) -> u64;
      pub(crate) fn luma_blocks_per_mcu(&self) -> (u32, u32);   // (h, v) of component 0
      pub(crate) fn start(&self, scan_start: u64) -> Cursor;
  }
  /// From the JPEG's header segments (DHT, SOF0/SOF1, DRI, SOS). None unless: sequential DCT with Huffman coding (SOF0/1),
  /// 8-bit, 1–4 components all in the single scan, sampling factors ≤ 2, every referenced table present, width × height
  /// ≤ MAX_DECODE_PIXELS.
  pub(crate) fn frame(r: &mut Reader, start: u64, scan_start: u64) -> Option<Frame>;
  #[derive(Clone, Debug)] pub(crate) struct Cursor { pub pos: u64, pub mcu: u64, .. } // bit buffer, DC predictors, restart count
  impl Cursor { pub(crate) fn shifted_back(&self, by: u64) -> Cursor; } // same state, positions minus `by`
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) enum End { Eoi { at: u64 }, Error { at: u64 }, OutOfData }
  pub(crate) struct Run {
      pub mcus: u64,          // complete MCUs (index reached)
      pub end: End,
      pub resume: Vec<Cursor>, // cursors at the last RESUME_POINTS MCU boundaries, oldest first
      pub dc: Vec<i32>,        // with keep_dc: luma DC of every block decoded in this run, in coding order
  }
  pub(crate) const RESUME_POINTS: usize = 4096;
  /// Decodes MCUs from `from`, reading only bytes in [from.pos, limit).
  pub(crate) fn decode(r: &mut Reader, frame: &Frame, from: Cursor, limit: u64, keep_dc: bool) -> Run;
  ```
Decode rules: canonical Huffman codes from DHT; a code with no symbol within 16 bits, a DC category > 11, an AC size > 10 or
a coefficient index past 63 is an `Error` at the current byte. `FF 00` is a data `FF`; with DRI, after every `restart_interval`
MCUs (but not after the last) the next bytes, after the 1-bit padding, must be `FF D0+((n) mod 8)` in sequence (DC predictors
reset), any other marker or a wrong number is an `Error`. After the last MCU, the padding and optional fill `FF`s must be
followed by `FF D9`: `Eoi { at }` (position of that `FF`); EOI before the last MCU, or data after it, is an `Error`. A cursor's
`pos` is the first byte not yet consumed into the bit buffer; `resume[i]` is the state right before MCU `resume[i].mcu`.

**JPEG "só cabeçalho" (pendência da 1.1):** when a JPEG ends without EOI (cut by another file, the end of the source or
`JPEG_MAX_LEN`) and `frame` is `Some`, it is `Partial` only if `decode` completes at least `mcus_x` MCUs (one full MCU row)
before its first error or the cut; otherwise `Damaged` (nothing of the photo would show). Progressive/unsupported: unchanged.

- [ ] **Step 1: Failing tests:**
  ```rust
  // jpeg_entropy.rs (unit)
  #[test] fn counts_every_mcu_of_testkit_jpegs() { /* jpeg(64,64), (640,480), (33,17), jpeg_with_exif(320,200), jpeg_flat_with_restarts(256,128,4): End::Eoi at jpeg.len() - 2, mcus == total_mcus() */ }
  #[test] fn truncated_data_runs_out() { /* jpeg(640,480,1,0) with limit = scan_start + (len - scan_start) / 2 → OutOfData, 0 < mcus < total */ }
  #[test] fn corrupt_entropy_data_is_an_error() { /* 64 noise bytes over the middle of jpeg(640,480,2,0) → End::Error { at } with at ≥ that offset */ }
  #[test] fn restart_markers_are_checked() { /* flat(256,128,4) with its 2nd and 3rd RST markers swapped → Error; unchanged → Eoi */ }
  #[test] fn resuming_from_a_cursor_gives_the_same_result() { /* decode to the end; resume from resume[len/2] → same End and mcus; dc of the resumed run == the tail of the full run's dc */ }
  #[test] fn unsupported_frames_are_none() { /* SOF2 (progressive), SOF9 (arithmetic), 12-bit SOF1, missing DHT → None */ }
  // tests/jpeg.rs
  #[test] fn cut_inside_the_first_mcu_row_is_damaged() {
      // jpeg(640, 480, 5, 0): keep the header + 200 bytes of scan data, next testkit JPEG at the next sector → measure: Damaged, len unchanged (up to that sector)
  }
  ```
  `jpeg_cut_by_next_file_is_partial` and `jpeg_cut_by_end_of_source_is_partial` must still pass unchanged.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README (JPEG health: the cut-file rule), CHANGELOG ("Changed": a JPEG cut before its first row of
  pixels is damaged, not partial). Commit: `feat(core): JPEG entropy decoder; a JPEG with no decodable row is damaged`.

---

### Task 8: JPEG em dois pedaços (bifragment gap carving)

**Files:**
- Create: `crates/revenant-core/src/formats/jpeg_join.rs` (crate-private); Modify: `formats/jpeg.rs`, `formats/mod.rs`
- Modify: `crates/revenant-testkit/src/lib.rs`
- Test: unit tests in `jpeg_join.rs`; `crates/revenant-core/tests/{jpeg.rs,scan.rs}`

**Interfaces:**
- Consumes: `jpeg_entropy::{frame, decode, Frame, Cursor, Run, End}` (Task 7), `Fragmented` (Task 3), `builtin()` (Task 5).
- Produces (testkit): `pub fn jpeg_smooth(width: u32, height: u32, seed: u64) -> Vec<u8>` — RGB smooth content (sum of two
  low-frequency sinusoids whose orientation and phase come from `seed`), quality 90, no APP1.
- Produces (core):
  ```rust
  pub(crate) const JPEG_GAP_MAX: u64 = 64 << 20;
  pub(crate) struct Joined { pub fragments: [Fragment; 2], pub len: u64 }
  /// Bifragment gap carving for the baseline JPEG at `start` (entropy data from `scan_start`) whose contiguous walk ended at
  /// `cut` without an Intact result.
  pub(crate) fn join(r: &mut Reader, start: u64, scan_start: u64, cut: u64) -> Option<Joined>;
  /// DC seam test (below). `dc`: luma DC of every block of the whole joined image in coding order; `seam_mcu`: first MCU
  /// decoded with bytes of the second piece.
  pub(crate) fn seam_ok(frame: &Frame, dc: &[i32], seam_mcu: u64) -> bool;
  ```
`Jpeg::measure`: when the contiguous result is not `Intact`, `frame` is `Some` and the contiguous `decode` does not reach
`Eoi` with every MCU, call `join`. On `Some`, the measurement is the two pieces, with health = `decode_health` over a
`Fragmented` view (strict → `Intact`, lenient → `Partial`; neither → ignore the join and keep the contiguous result). No unreadable
sector may lie in either piece for `Intact` (as for any file). On `None`, the contiguous result stands (Task 7 rules).

Algorithm of `join` (constants: `BREAK_BACK = 4` sectors, `PER_EOI = 4`, `MAX_JOIN_TRIES = 64`, continuation bytes decoded
≤ 256 MiB in total, all candidate positions absolute and 512-aligned):
```
1. run0 = decode(r, frame, frame.start(scan_start), cut, keep_dc = true).
   x = match run0.end { Error{at} => at, _ => cut }.        // first sign that the data stopped being this photo
   C1 = sector boundaries c with scan_start < c ≤ x and c > x − BREAK_BACK × 512, latest first
        (a cut by another file's head gives x == cut, so C1 = {cut, cut − 512, cut − 1024, cut − 1536}).
2. Window: sector starts g in (min(C1), min(min(C1) + JPEG_GAP_MAX, source end)). One forward pass records, per g,
   outcome(g) = the first event at or after g: Eoi(e) at an `FF D9`; Bad at an `FF` followed by a byte other than
   00, D0–D7, D9, FF; Bad at a sector made of one repeated byte value; Bad at a sector start where `file_starts_at`.
   (Bounded memory: one small record per sector, ≤ 131,072.)
3. Candidates: for each distinct Eoi(e), the first PER_EOI sector starts g (ascending) with outcome(g) == Eoi(e).
   Try pairs (c1, g) ordered by g ascending then c1 latest first, with g > c1 and (c1 − start) + (e + 2 − g) ≤ JPEG_MAX_LEN,
   until MAX_JOIN_TRIES or the byte budget:
     cur = the last cursor in run0.resume with pos ≤ c1 (none → skip this c1); seam_mcu = cur.mcu.
     view = Fragmented[(start, c1 − start), (g, e + 2 − g)];  run = decode(view reader, frame, cur.shifted_back(start), view.len(), true)
     pass ⇔ run.end == Eoi { at: view.len() − 2 } (so every MCU, exact count, EOI right after)
            ∧ seam_ok(frame, run0.dc[..seam_mcu × blocks_per_mcu] ++ run.dc, seam_mcu)
     first pass → Some(Joined { fragments: [(start, c1 − start), (g, e + 2 − g)], len: view.len() }).
4. None.
```
`seam_ok`: build the luma DC grid (blocks_x × blocks_y). With seam MCU at MCU row R, column C: seam pairs are, for block columns
of MCU columns ≥ C, (top block row of MCU row R, the block row above it) and, for MCU columns < C, (top block row of MCU row
R + 1, the block row above it), skipping pairs off the image. `V_seam` = mean |ΔDC| over seam pairs; `V_base` = median over
every other block-row boundary of its mean |ΔDC|. Pass iff the image has ≥ 4 block rows and `V_seam ≤ 3 × V_base + 4`.
(Fewer than 4 block rows → false: such a join is never `Intact`.)

- [ ] **Step 1: Failing tests:**
  ```rust
  // jpeg_join.rs (unit)
  #[test] fn seam_check_rejects_a_glued_dc_grid() {
      // synthetic Frame 64×48 blocks: a smooth gradient grid → true at any seam; rows < 24 from gradient A and ≥ 24 from a
      // different gradient B (seam_mcu at row 24) → false; a seeded noise grid → true; a 64×3-block grid → false
  }
  // tests/jpeg.rs — a = jpeg_smooth(1024, 768, 1); c = first 512-multiple ≥ (a.len() / 2); start = 64 KiB
  #[test] fn bifragment_jpeg_is_reassembled_intact() {
      // noise 8 MiB (seed 5); a[..c] at start; jpeg(64, 64, 9, 0) at start + c (cuts it); a[c..] at 3 MiB
      // Jpeg.measure(start) == Measurement { len: a.len(), kind: Jpeg, health: Intact,
      //     fragments: vec![Fragment { start, len: c }, Fragment { start: 3 MiB, len: a.len() − c }] }
  }
  #[test] fn fragment_followed_by_noise_is_reassembled() { /* as above without the small JPEG → same Measurement */ }
  #[test] fn fragment_followed_by_zeros_is_reassembled() { /* gap filled with zeros (image zeros, a's pieces put) → same Measurement */ }
  #[test] fn glued_halves_of_two_photos_are_not_intact() {
      // b = jpeg_smooth(1024, 768, 2); a[..c] at start; b whole at start + c; noise after (a's second piece is gone)
      // → measure(start).health != Intact; scan finds b at start + c, Intact, contiguous
  }
  #[test] fn foreign_tail_after_the_gap_is_not_glued_intact() { /* a[..c] at start; noise; b[c..] at 3 MiB → measure(start).health != Intact */ }
  #[test] fn three_pieces_are_not_reassembled() { /* a split at c and c + 4096 into 3 pieces apart → fragments empty, health Partial */ }
  #[test] fn hostile_gap_search_is_bounded() {
      // a = jpeg(640, 480, 3, 0): a[..c] at 0, then 65 MiB of sectors each [0x00; 510] ++ [FF, D9]; counting source
      // → Some, health != Intact, elapsed < 10 s (debug), inner bytes read ≤ 68 MiB
  }
  // tests/scan.rs
  #[test] fn scan_reports_the_reassembled_jpeg_and_the_file_in_the_gap() {
      // the image of bifragment_jpeg_is_reassembled_intact → found == [(64 KiB, Jpeg, Intact, 2 pieces), (64 KiB + c, Jpeg, Intact)];
      // extract of the first == a (byte-identical)
  }
  ```
  The existing `lenient_only_decode_is_partial` and `corrupted_scan_data_is_damaged` (in-place corruption) must still pass
  unchanged: a join that skips bytes never gives the exact MCU count.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; `bench` (release): paste the 5 lines (watch
  `cut-jpeg`).
- [ ] **Step 5:** core README ("Formats: JPEG" → "Fragmented JPEG": the algorithm, bounds, the `Intact` evidence, what is not
  covered: progressive, 3+ pieces, second piece before the first), dev README (example with `fragments`), CHANGELOG ("Added":
  a photo split in two pieces on the card is put back together). Commit: `feat(core): reassemble JPEGs split in two pieces`.

---

### Task 9: Vídeo com o `moov` separado do `mdat`

**Files:**
- Create: `crates/revenant-core/src/pairing.rs` (crate-private); Modify: `src/formats/isobmff.rs`, `src/scan/{mod.rs,pipeline.rs}`,
  `src/lib.rs`
- Modify: `crates/revenant-testkit/src/lib.rs`, `crates/revenant-dev/src/main.rs`
- Test: `crates/revenant-core/tests/pairing.rs` (new), `crates/revenant-dev/tests/cli.rs`

**Interfaces:**
- Produces (testkit):
  ```rust
  pub struct AvcOptions { pub samples: usize, pub seed: u64, pub mov: bool, /** bytes of padding at the end of mdat */ pub mdat_pad: usize }
  pub struct AvcVideo { pub bytes: Vec<u8>, pub mdat_payload: std::ops::Range<usize>, pub moov: std::ops::Range<usize> }
  /// ftyp, mdat, moov (moov at the end, mdat with a 32-bit size). One video trak: `avc1` entry with an `avcC`
  /// (lengthSizeMinusOne = 3), `stsz` per sample, `stsc` 4 samples per chunk, `stco`. Each sample: 1–3 NAL units
  /// (4-byte big-endian length, header byte 0x65 for the first sample of a chunk else 0x41, seeded random payload),
  /// sample sizes in [2 KiB, 24 KiB] from `seed`.
  pub fn avc_video(opts: &AvcOptions) -> AvcVideo;
  ```
- Produces (core):
  ```rust
  // scan: Event gains Updated(Finding): replaces the earlier Found with the same id; emitted after every Found, before
  // `scan` returns. ScanSummary gains `pub updated: u64`.
  // isobmff.rs
  pub(crate) struct MdatLayout { pub kind: FileKind, pub mdat_at: u64, pub header_len: u64, pub declared: u64 }
  /// Header-only walk from `start`: Some only for ftyp → mdat with a declared (non-zero) size and no moov.
  pub(crate) fn moovless_layout(r: &mut Reader, start: u64) -> Option<MdatLayout>;
  pub(crate) struct Sample { pub offset: u64, pub size: u64 }  // offset relative to the file start
  pub(crate) enum Check { Nal { length_size: u8, hevc: bool }, Mjpeg }
  /// Per track: its check (None = not checkable: audio, other codecs) and its samples sorted by offset, from stsz/stsc/stco|co64,
  /// ≤ 4,000,000 samples per moov (more → None for the moov).
  pub(crate) fn track_samples(moov: &[u8]) -> Option<Vec<(Option<Check>, Vec<Sample>)>>;
  // pairing.rs
  /// Damaged moov-less videos × orphan moov box starts → the reassembled findings (same ids), each with 2 pieces.
  pub(crate) fn pair_videos(source: &dyn Source, videos: &[Finding], moovs: &[u64], cancel: &CancelToken) -> Vec<Finding>;
  ```
- CLI: an `updated` line `{"event":"updated", …same fields as found…}`; with `--extract`/`--previews` the updated finding is
  written again (new name with `_2`); the summary line gains `"updated":n`.

Collection during the scan: the read stage searches every block it reads (and every deferred block read lazily at commit) for
`moov` with `memchr::memmem` (with a 15-byte overlap across blocks); a hit at box start `q` is a candidate when
`be32(q) ∈ [24, MOOV_MAX_LEN + 8]` and bytes `q+12..q+16 == "mvhd"`. At the end, candidates inside any `Intact` finding's piece or
inside a non-`Damaged` MP4/MOV/CR3/HEIC finding are dropped; at most 4096 are kept (the first ones; the rest are dropped). Videos
= `Damaged` findings of kind Mp4/Mov (≤ 4096).

Algorithm of `pair_videos`:
```
For each video V (start s): L = moovless_layout(s)? ; P = (L.mdat_at − s) + L.declared   // where moov would sit in the file
  payload = L.mdat_at − s + L.header_len
For each candidate M at q: moov = load(q+8 .. q+be32(q)) (≤ MOOV_MAX_LEN), not garbled, has mvhd and ≥ 1 trak; tracks = track_samples(moov)?
  S = all samples of checkable tracks, sorted by offset; need |S| ≥ 8; c_min = min offset over all tracks; c_end = max(offset+size).
Pair filter (arithmetic only): (q − P) ≡ 0 (mod 512) [signed]; payload ≤ c_min < payload + 64 KiB; c_end ≤ P.
At most 256 pairs pass the filter (the rest are dropped). For each:
  A(i): sample i read at s + off_i passes its check; B(i): read at q − P + off_i (must be ≥ 0 and inside the source).
  NAL check: walk ≤ 512 NAL units: length ℓ ≥ 1; header byte has the forbidden bit 0 and, AVC, type in 1..=23 / HEVC, type ≤ 40
    and temporal_id_plus1 ≠ 0; the (length_size + ℓ) steps sum exactly to the sample size. MJPEG: starts FF D8 FF, ends FF D9.
  k = n if A(n−1) else the smallest i with !A(i) (binary search; A assumed true on a prefix).
  Verify up to 32 evenly spaced samples in [0, k) by A and up to 32 in (k, n) by B; any failure → no pair.
  L1 candidates: multiples of 512 in [max(payload, end of sample k−1), min(start of sample k+1, P)] (k = n: up to P), ≤ 4096;
    keep those where sample k (if any), read piecewise (bytes < L1 at A, ≥ L1 at B), passes. None → no pair.
  Pieces: [s, s + L1) and [g, g + (P − L1) + moov box size) with g = q − P + L1; both inside the source, not overlapping.
Uniqueness: a video with 2+ passing moovs, or a moov passing for 2+ videos, pairs with none of them.
Health: index_health(moov, P + moov box size) (Damaged/Partial as it says); an unreadable sector in either piece → Partial;
  more than one passing L1 → the smallest, Partial; else Intact. Kind from the ftyp (as the walk decides).
```
Cancel is checked between pairs; `scan` returns `Err(Cancelled)` without the remaining `Updated` events.

- [ ] **Step 1: Failing tests** (`tests/pairing.rs`; `v = avc_video(samples 120, seed 1)`, `l1` = first 512-multiple ≥
  `mdat_payload.start + mdat_payload.len() × 2 / 5`):
  ```rust
  #[test] fn split_video_is_reassembled_intact() {
      // noise 24 MiB (seed 7); v.bytes[..l1] at 1 MiB; v.bytes[l1..] at 12 MiB
      // → Found { id: 1, start: 1 MiB, kind: Mp4, health: Damaged } and, as the last non-progress event,
      //   Updated(Finding { id: 1, start: 1 MiB, len: v.bytes.len(), kind: Mp4, health: Intact,
      //            fragments: [(1 MiB, l1), (12 MiB, len − l1)] }); extract(updated) == v.bytes; summary.updated == 1
  }
  #[test] fn split_just_before_moov_is_reassembled() { /* l1 = moov.start rounded down to 512 → Updated, Intact */ }
  #[test] fn moov_pairs_with_its_own_mdat_not_a_sibling() {
      // v1 (seed 1), v2 (seed 2, mdat_pad chosen so both moov positions P are equal mod 512); first pieces at 1 and 4 MiB,
      // second pieces at 10 MiB (v2) and 16 MiB (v1) → 2 Updated; extracted bytes == v1.bytes and v2.bytes respectively
  }
  #[test] fn moov_matching_two_videos_is_not_paired() { /* v.bytes[..l1] at 1 MiB and at 5 MiB, v.bytes[l1..] at 12 MiB → no Updated; 2 Found Damaged */ }
  #[test] fn video_split_in_three_is_not_paired() { /* 3 pieces at 1, 6, 12 MiB → no Updated */ }
  #[test] fn orphan_moov_alone_is_not_reported() { /* only v.bytes[l1..] at 1 MiB → no Mp4 Found, no Updated */ }
  #[test] fn reassembled_video_with_unreadable_sector_is_partial() { /* split_video layout, a bad sector inside the second piece's mdat part → Updated health Partial */ }
  #[test] fn hostile_moov_candidates_are_bounded() {
      // 32 MiB noise; 10,000 fake headers [00 00 10 00]"moov"[00 00 00 6C]"mvhd" at seeded offsets; one real orphan moov
      // whose stsz and stco counts are 0xFFFF_FFFF with truncated tables; one Damaged moov-less video
      // → Ok, no Updated, elapsed < 10 s (debug)
  }
  // cli.rs
  #[test] fn updated_lines_follow_found_lines() { /* split_video image → a "found" line id 1 damaged, then an "updated" line id 1 intact with 2 fragments; summary has "updated":1 */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; `bench` (release): paste the 5 lines.
- [ ] **Step 5:** core README ("Formats: MP4/MOV" → "Video with a separated index": the algorithm, bounds, `Updated`), dev
  README (`updated` line), CHANGELOG ("Added": a camera video whose index ended up elsewhere on the card is put back
  together). Commit: `feat(core): pair camera videos with their separated moov`.

---

### Task 10: Medida final, documentação e fechamento da parte

**Files:**
- Modify: `README.md`, `crates/revenant-core/README.md` ("Performance" section), `crates/revenant-dev/README.md` (imaging),
  `CHANGELOG.md`, `CLAUDE.md`, `docs/superpowers/roadmap.md`
- Create: `docs/superpowers/history/2026-10-20-m1-p3-pendencias.md`

- [ ] **Step 1:** `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`,
  `cargo deny check licenses` (if installed; else say so), and `REVENANT_SAMPLES=samples/cache cargo test -p revenant-core
  --test real_samples -- --ignored` if `samples/cache` exists (the real samples must still be one `Intact` finding each).
- [ ] **Step 2:** `cargo run --release -p revenant-dev -- bench` and `-- bench --threads 1`; table "antes (Tarefa 1) × depois"
  per image in the core README ("Performance": MB/s, machine, page cache, what it means for a card reader) and in the history
  file. If dense-jpeg or dense-raw is below 300 MB/s, say so plainly in both places with the measured value.
- [ ] **Step 3: Docs:** root README status (fragmented JPEG and separated-`moov` video, speed); dev README "Making a card
  image": replace `dd bs=4m conv=noerror,sync` (zeroes 4 MiB per bad sector) by GNU `ddrescue -d /dev/rdiskN card.img card.map`
  (an external tool, only recommended; built-in imaging comes in 1.5); `CLAUDE.md` "Onde o desenvolvimento está" (pt-BR,
  20/10/2026: 1.2 mergeada (PRs #2 e #3); 1.3 concluída na branch `feat/m1-p3-fragments-speed`, aguardando PR/merge; decisões
  em `docs/superpowers/history/2026-10-20-m1-p3-pendencias.md`; próxima: 1.4); roadmap 1.3 marked done with what it covers
  (JPEG and video in two pieces; 3+ pieces and RAW fragmentation not covered).
- [ ] **Step 4:** history file (pt-BR, same shape as the 1.2 one): decisions taken during execution (each with its cost if
  wrong), the before/after table, the pending items with owner — including those this plan deferred (table "Pendências
  herdadas") and anything new.
- [ ] **Step 5:** Commit: `docs: part 1.3 performance numbers, status and pending items`.
