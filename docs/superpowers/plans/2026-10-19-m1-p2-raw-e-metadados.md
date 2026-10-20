# Módulo 1 · Parte 2 — Fotos RAW/HEIC e dados da foto — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** O motor passa a achar, medir e classificar fotos RAW de câmera (CR2, CR3, NEF, ARW, RAF, DNG, ORF, RW2), HEIC e TIFF
genérico, e a ler de qualquer achado os dados da foto (câmera, lente, número de série, ISO, data) e a maior prévia JPEG embutida —
o que a tela de resultados (Módulo 2) vai mostrar. Fecha também as pendências da 1.1 que têm dono 1.2.

**Architecture:** Um leitor TIFF compartilhado (`src/tiff.rs`) anda pelos IFDs e serve a três usos: medir os RAW baseados em TIFF,
ler EXIF (de RAW, de JPEG via APP1, de HEIC/CR3) e listar prévias JPEG embutidas. Novos detectores: `TiffRaw` (todos os RAW
baseados em TIFF, a variante decidida pelo conteúdo), `Raf`, e o `IsoBmff` existente ganha despacho por marca (`crx ` → CR3,
`heic/heix/mif1/msf1` → HEIC). Um módulo `inspect` devolve `Metadata` + `Preview` para qualquer `Finding`. O trait `Format` ganha
`confirms` (validação só do cabeçalho), usado na checagem "outro arquivo começa aqui" para não cortar arquivos por falso positivo.

**Tech Stack:** Rust 2024 (MSRV 1.88), crates existentes (`zune-jpeg`, `thiserror`); nenhuma dependência nova no core sem licença
permitida. Amostras reais: raw.pixls.us (CC0), baixadas sob demanda, fora do git.

**Spec:** `docs/superpowers/specs/2026-10-19-revenant-design.md` (§4 prévia e EXIF na tela; §5 formatos, prévia, estado, agrupamento).
**Antes:** `docs/superpowers/history/2026-10-19-m1-p1-pendencias.md` (decisões R1–R11 da 1.1 e pendências com dono 1.2).
**Roteiro:** `docs/superpowers/roadmap.md`.

## Global Constraints

- **Language [Sérgio]:** everything in the repository in English (identifiers, comments, error codes, commits, READMEs,
  CHANGELOG); `CLAUDE.md` and `docs/superpowers/**` stay in pt-BR.
- **Never write to the source.** Nothing in `revenant-core` gets a write path to a `Source`.
- **Licenses (spec §7):** dependencies only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense,
  CC0-1.0, BSL-1.0. No LibRaw, exiv2, libheif, ExifTool or any GPL/LGPL code or code copied from them. Real sample files are
  test data, not code: only CC0/public-domain samples, never committed (downloaded on demand, recorded with URL + SHA-256).
- **Errors by code** (`code() -> &'static str`, snake_case); no UI text in the engine.
- **Rust:** edition 2024, `rust-version = "1.88"`. Dev machine: Intel Mac, Homebrew Rust; never `brew install/upgrade` toolchains.
- **No `unsafe`** outside `source/windows.rs`.
- **Bounded everything:** no read, allocation or loop driven by an on-disk number without a cap (IFD count ≤ 64, entries per IFD
  ≤ 1024, TIFF extent ≤ `TIFF_MAX_LEN` = 1 GiB, `meta`/`moov` in memory ≤ 16 MiB, previews ≤ `JPEG_MAX_LEN`), and cycle
  detection on IFD offsets.
- **Health semantics (from 1.1, binding):** `Intact` = structure complete AND content verified clean; `Partial` = salvageable
  but not clean (cut, lenient-only decode, unreadable sectors, or not fully verifiable); `Damaged` = won't open (index/structure
  unusable or no decoder accepts it). Never `Intact` without evidence.
- **Docs in the same commit [Sérgio]:** each task updates the READMEs it affects and `CHANGELOG.md` `[Unreleased]` for anything
  user-visible.
- **Commit trailer, always:** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Conventional Commits. Never
  `git add -A`. Never push (the controller opens the PR).
- **Tests never touch real data or physical disks**, except the opt-in real-samples test (Task 6), which only reads downloaded
  sample files.
- Every new test is seen failing before it passes (record RED in the report).

## Review Focus

1. **A RAW whose embedded JPEG preview starts at a sector boundary** (common: cameras align previews): the RAW must come out whole
   and `Intact`, not cut at its own preview, and the preview must not be reported as a separate file → test
   `raw_with_sector_aligned_preview_is_one_file` (Task 3).
2. **High-entropy media data (video `mdat`, RAW strips) containing bytes that look like a JPEG/TIFF/ftyp head at a sector start**:
   must not cut the file (only a head that `confirms` cuts) → tests `random_head_inside_mdat_does_not_cut` (Task 1) and
   `random_head_inside_raw_data_does_not_cut` (Task 3).
3. **Garbled IFDs** (cycles, offsets past the end, absurd counts, an IFD pointing into itself): no hang, no huge allocation, a
   defined result (`Damaged` or `None`) → test `hostile_ifds_are_bounded` (Task 2).
4. **Big-endian TIFF** (`MM\0*`, used by some Nikon/older bodies and many DNGs): parsed exactly like little-endian → every
   Task 2/3 test helper runs both endiannesses.
5. **A photo with no EXIF at all, or EXIF with missing/garbage fields** (non-ASCII make, date `0000:00:00 00:00:00`,
   ISO 0): `inspect` returns the fields it can and `None` for the rest, never a panic and never an invented value → test
   `garbage_exif_fields_become_none` (Task 5).

---

### Task 1: Pendências da 1.1, `confirms`, status do projeto

**Files:**
- Modify: `crates/revenant-core/src/formats/{mod.rs,jpeg.rs,isobmff.rs}`, `crates/revenant-dev/src/main.rs`,
  `CLAUDE.md`, `Cargo.toml` (confirm `rust-version = "1.88"`), `crates/revenant-core/README.md`, `CHANGELOG.md`
- Test: `crates/revenant-core/tests/{jpeg.rs,isobmff.rs,scan.rs}`

**Interfaces:**
- Produces:
  ```rust
  pub trait Format: Send + Sync {
      fn name(&self) -> &'static str;
      fn recognizes(&self, head: &[u8]) -> bool;
      /// Header-level validation at `start` (cheap, never recursive, never measures the whole file): true when a real file
      /// of this format plausibly starts here. Used to decide that "another file starts here".
      fn confirms(&self, source: &dyn Source, start: u64) -> bool;
      fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement>;
  }
  impl FileKind { pub fn as_str(self) -> &'static str } // stable lowercase id: "jpeg", "mp4", "mov" (more in later tasks)
  ```
  `Reader::file_starts_at(formats, pos)` returns true only if some format `recognizes` the head AND `confirms` at `pos`.
  JPEG `confirms`: segment walk to SOS with DQT and SOF present (the existing header phase, no scan-data walk). ISO BMFF
  `confirms`: valid `ftyp` box followed by a parseable header of an accepted type. `scan` keeps using `recognizes` + `measure`.

Changes:
- `confirms` as above; `file_starts_at` uses it.
- isobmff: `truncated |= unreadable_header` only when `!complete` (a complete video followed by an unreadable sector stays `Intact`).
- isobmff Motion-JPEG: if the walk was cut inside `mdat`, re-measure without the inner-`mdat` checks; if that measurement has a
  `moov` whose video track sample entry is `mjpa`, `mjpb` or `jpeg` and its index health is `Intact`, use it (the frames are the
  video's own). Otherwise keep the cut.
- jpeg: `is_segment_marker` accepts `0xC8` and `0xCC` (JPG/DAC); fill bytes `FF FF…` before a marker in the header phase are skipped.
- jpeg R5 (ruling R12 of 1.1 revisited): a JPEG above `MAX_DECODE_PIXELS` with complete structure → `Partial` ("not fully
  verifiable"), no longer `Damaged`.
- `FileKind::as_str`; `revenant-dev` uses it instead of its own match.
- `CLAUDE.md`: "Onde o desenvolvimento está" → 1.1 mergeada (PR #1, CI verde nas 4 arquiteturas); 1.2 em andamento na branch
  `feat/m1-p2-raw`; link para `docs/superpowers/roadmap.md`; MSRV 1.88; nova regra fixa "fim de parte = push + PR; o merge é do
  Sérgio".

- [ ] **Step 1: Failing tests:**
  ```rust
  // isobmff.rs
  #[test] fn complete_video_followed_by_unreadable_sector_is_intact() { /* MP4 ending exactly on a sector boundary, next sector unreadable → Intact, len exact */ }
  #[test] fn motion_jpeg_frames_do_not_cut_the_video() { /* moov-at-end MOV, trak stsd entry 'mjpa', mdat holds testkit JPEGs, one starting at a sector start → one finding, Intact, len exact */ }
  #[test] fn h264_video_is_still_cut_by_a_real_jpeg() { /* same layout with stsd 'avc1' → cut as before (control for the test above) */ }
  // scan.rs
  #[test] fn random_head_inside_mdat_does_not_cut() { /* healthy moov-first MP4; at a sector start inside mdat write FF D8 FF E1 + 12 random bytes (no valid segments) → one finding, Intact */ }
  // jpeg.rs
  #[test] fn dac_and_fill_bytes_before_sos_are_accepted() { /* testkit JPEG with an inserted FF CC segment and FF FF fill before DQT → Intact, len exact */ }
  #[test] fn huge_declared_size_is_partial() { /* rename of huge_declared_size_is_damaged_without_decoding: 16384×16384 → Partial */ }
  ```
- [ ] **Step 2:** run them → FAIL. **Step 3:** implement. **Step 4:** `cargo test --workspace`, clippy, fmt.
- [ ] **Step 5:** READMEs (`confirms`, the rules above), CHANGELOG. Commit: `fix(core): confirm heads before cutting, and part 1.1 follow-ups`.

---

### Task 2: Leitor TIFF compartilhado + construtor de RAW de teste

**Files:**
- Create: `crates/revenant-core/src/tiff.rs` (crate-private module, `pub(crate)`), Modify: `lib.rs` (`mod tiff;`)
- Modify: `crates/revenant-testkit/src/lib.rs`
- Test: unit tests inside `tiff.rs` (`#[cfg(test)]`, using testkit as dev-dependency)

**Interfaces:**
- Produces (testkit):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum TiffVariant { Cr2, Nef, Arw, Dng, Orf, Rw2, Plain }
  #[derive(Clone, Debug, Default)] pub struct TestExif {
      pub make: Option<String>, pub model: Option<String>, pub lens: Option<String>, pub serial: Option<String>,
      pub iso: Option<u16>, pub date: Option<String>, // "YYYY:MM:DD HH:MM:SS" as stored in EXIF
  }
  pub struct TiffRawOptions {
      pub variant: TiffVariant, pub big_endian: bool, pub raw_len: usize, pub seed: u64,
      pub preview: Option<(u32, u32)>,          // embedded JPEG (width, height); None = no preview
      pub preview_sector_aligned: bool,         // place the preview at a multiple of 512 from the file start
      pub exif: TestExif,
  }
  pub struct TiffRaw { pub bytes: Vec<u8>, pub preview: Option<std::ops::Range<usize>>, pub raw: std::ops::Range<usize> }
  /// TIFF header (+ variant magic: CR2 "CR\x02\0" at 8; ORF "IIRO"/"MMOR"; RW2 "IIU\0"), IFD0 (Make, Model, ImageWidth/Length,
  /// JPEGInterchangeFormat/Length → preview, ExifIFD pointer, SubIFDs → one IFD with StripOffsets/StripByteCounts → raw data),
  /// Exif IFD (DateTimeOriginal, ISOSpeedRatings, LensModel, BodySerialNumber), DNGVersion in IFD0 for Dng.
  /// Make defaults per variant: Cr2 "Canon", Nef "NIKON CORPORATION", Arw "SONY", Orf "OM Digital Solutions",
  /// Rw2 "Panasonic", Dng "Google" (phone DNG), Plain "Generic".
  pub fn tiff_raw(opts: &TiffRawOptions) -> TiffRaw;
  ```
- Produces (core, crate-private):
  ```rust
  pub(crate) const TIFF_MAX_LEN: u64 = 1 << 30;
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) enum Endian { Little, Big }
  #[derive(Clone, Debug, Default)]
  pub(crate) struct TiffInfo {
      pub end: u64,                       // furthest byte referenced, relative to the TIFF header
      pub garbled: bool,                  // a structural error was hit (cycle, offset past TIFF_MAX_LEN, bad type/count)
      pub dng: bool,                      // DNGVersion (0xC612) present
      pub make: Option<String>, pub model: Option<String>, pub lens: Option<String>, pub serial: Option<String>,
      pub iso: Option<u32>, pub date_original: Option<String>, // raw EXIF strings, trimmed of NULs/spaces; ASCII only, else None
      pub width: Option<u32>, pub height: Option<u32>,          // largest ImageWidth/Length seen in IFD0/SubIFDs
      pub jpegs: Vec<(u64, u64)>,         // (offset, len) relative to the header: JPEGInterchangeFormat ranges and
                                          //  strips/tiles whose Compression is 6/7 and bytes start with FF D8
      pub data: Vec<(u64, u64)>,          // other strip/tile ranges (raw data)
  }
  /// Walks the TIFF whose header is at absolute `base` (offsets are relative to `base`). `limit` caps every offset
  /// (min(TIFF_MAX_LEN, what the caller allows, e.g. an APP1 payload length)). None if the header is not TIFF.
  pub(crate) fn walk(r: &mut Reader, base: u64, limit: u64) -> Option<TiffInfo>;
  ```
  Walks IFD0 → next-IFD chain, SubIFDs (0x14A), Exif IFD (0x8769), GPS IFD (0x8825, extent only), Interop (0xA005), and for
  Nikon the MakerNote preview IFD is NOT required (extent from the MakerNote byte count is enough). Tags read: 0x010F Make,
  0x0110 Model, 0x0100/0x0101 width/height, 0x0103 Compression, 0x0111/0x0117 strips, 0x0144/0x0145 tiles, 0x0201/0x0202
  JPEG, 0x9003 DateTimeOriginal, 0x8827 ISO, 0xA434 LensModel, 0xA431 BodySerialNumber, 0xC612 DNGVersion. Every entry's
  out-of-line value range also extends `end`. Recognized header magics: `II*\0`, `MM\0*`, `IIRO`, `IIRS`, `MMOR`, `IIU\0`.

- [ ] **Step 1: Failing unit tests** (each runs for `big_endian` false and true):
  ```rust
  #[test] fn walks_every_variant_and_finds_the_extent() { /* for each TiffVariant: info.end == bytes.len(), !garbled */ }
  #[test] fn reads_exif_fields() { /* make/model/lens/serial/iso/date_original equal the TestExif values */ }
  #[test] fn lists_the_preview_and_the_raw_data() { /* jpegs == [preview range], data == [raw range] */ }
  #[test] fn dng_flag() { /* Dng → dng true; others false */ }
  #[test] fn hostile_ifds_are_bounded() {
      // IFD0 next-pointer to itself; entry count 60_000; strip offset 2^40; a SubIFD pointing to IFD0; type 0/count 2^31
      // → returns Some with garbled == true, finishes in < 1 s, no allocation over a few MB
  }
  #[test] fn not_tiff_is_none() { /* noise → None */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README ("TIFF reader" section, internal). Commit: `feat(core): shared TIFF reader and RAW test builder`.

---

### Task 3: Detector `TiffRaw` (CR2, NEF, ARW, DNG, ORF, RW2, TIFF)

**Files:**
- Create: `crates/revenant-core/src/formats/tiffraw.rs`; Modify: `formats/mod.rs` (`FileKind` variants, `all_formats()`)
- Test: `crates/revenant-core/tests/tiffraw.rs`, `crates/revenant-core/tests/scan.rs`

**Interfaces:**
- Consumes: `tiff::walk`, `TiffInfo`, `Reader`, `Format`.
- Produces:
  ```rust
  // FileKind gains: Cr2, Nef, Arw, Dng, Orf, Rw2, Tiff
  // extension(): "cr2","nef","arw","dng","orf","rw2","tif";  as_str(): "cr2","nef","arw","dng","orf","rw2","tiff"
  pub struct TiffRaw;  // name() "tiffraw"; in all_formats() after Jpeg and IsoBmff
  ```
Rules:
- `recognizes`: one of the TIFF magics, and IFD0 offset (bytes 4..8 in the header's endianness) between 8 and 64 MiB.
- `confirms`: `walk` with a small limit parses IFD0 with ≥ 3 entries of valid types and is not garbled.
- Kind: CR2 magic → Cr2; ORF magic → Orf; RW2 magic → Rw2; `dng` → Dng; Make starts with "NIKON" → Nef; "SONY" → Arw;
  otherwise → Tiff.
- `len` = `info.end`, but the file is cut (→ `Partial`) at the first 512-aligned sector inside `[start, start+end)` where another
  file starts (`file_starts_at`), **excluding the starts of the file's own `jpegs` ranges**; the end of the source also cuts
  (`Partial`).
- Health: `garbled` → `Damaged` (len = what was walked); cut → `Partial`; unreadable sector inside → `Partial`; the largest
  preview in `jpegs` (by SOF pixel count) strict-decodes → `Intact`; lenient-only or no preview → `Partial` (raw data itself
  can't be verified without a RAW decoder — document it); preview fails both decoders → `Partial` (the raw data may still be fine).
- Inside an `Intact` TiffRaw the scan suppresses heads (1.1 behavior), so embedded previews are not reported separately.

- [ ] **Step 1: Failing tests** (`tests/tiffraw.rs`, both endiannesses where it applies):
  ```rust
  #[test] fn every_variant_is_found_with_its_kind() { /* each TiffVariant at 8192 in noise → (kind, Intact, len == bytes.len()) */ }
  #[test] fn raw_with_sector_aligned_preview_is_one_file() { /* preview_sector_aligned → scan finds exactly 1 finding, Intact */ }
  #[test] fn raw_cut_by_next_file_is_partial() { /* testkit JPEG written at a sector start inside the raw data range → RAW Partial, len up to it; the JPEG found too */ }
  #[test] fn random_head_inside_raw_data_does_not_cut() { /* "II*\0"+garbage and FF D8 FF E1+garbage at sector starts in raw data → 1 finding, Intact */ }
  #[test] fn raw_without_preview_is_partial() {}
  #[test] fn garbled_ifd_is_damaged() { /* IFD0 entry count 0xFFFF → Damaged */ }
  #[test] fn unreadable_sector_in_raw_data_is_partial() { /* through scan with a failing sector */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** README ("Formats: TIFF-based RAW", health rules), CHANGELOG. Commit: `feat(core): TIFF-based RAW photos (CR2, NEF, ARW, DNG, ORF, RW2)`.

---

### Task 4: RAF, CR3 e HEIC

**Files:**
- Create: `crates/revenant-core/src/formats/raf.rs`; Modify: `formats/isobmff.rs`, `formats/mod.rs`
- Modify: `crates/revenant-testkit/src/lib.rs`
- Test: `crates/revenant-core/tests/raf.rs`, `crates/revenant-core/tests/isobmff.rs`

**Interfaces:**
- Produces (core): `FileKind::{Raf, Cr3, Heic}` (extensions "raf","cr3","heic"; as_str same); `pub struct Raf;` (name "raf",
  in `all_formats()`).
- Produces (testkit):
  ```rust
  /// "FUJIFILMCCD-RAW " header; at 0x54/0x58 the JPEG offset/len (big-endian u32), at 0x5C/0x60 the CFA header offset/len,
  /// at 0x64/0x68 the CFA offset/len. The JPEG is a testkit JPEG with an EXIF APP1 built from `exif`.
  pub fn raf(raw_len: usize, seed: u64, exif: &TestExif) -> Vec<u8>;
  /// ftyp major brand "crx " ; moov with uuid 85c0b687-820f-11e0-8111-f4ce462b6a48 holding CMT1 (TIFF IFD0: Make/Model)
  /// and CMT2 (Exif IFD) boxes; one trak whose single chunk (stco) is a full-size testkit JPEG in mdat;
  /// uuid eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16 at top level holding a PRVW box with a smaller JPEG.
  pub fn cr3(seed: u64, exif: &TestExif) -> Vec<u8>;
  /// ftyp "heic" (compatible "mif1"); meta with hdlr "pict", pitm, iinf (item 1 "hvc1", item 2 "Exif"), iloc (version 1,
  /// extents for both items into mdat); mdat with `image_len` random bytes for item 1 and the Exif item (4-byte offset
  /// prefix + TIFF built from `exif`).
  pub fn heic(image_len: usize, seed: u64, exif: &TestExif) -> Vec<u8>;
  ```
Rules:
- RAF: `recognizes` the 16-byte magic; `confirms` the three offset/len pairs are within `TIFF_MAX_LEN`; `len` = max end of the
  three ranges; cut/end-of-source → `Partial`; health from the embedded JPEG like TiffRaw (strict → `Intact`).
- ISO BMFF brand dispatch from `ftyp` major + compatible brands: `crx ` → Cr3; `heic heix heim heis mif1 msf1` (and no video
  `moov`) → Heic; else the existing Mp4/Mov logic.
- CR3 health: the existing `moov`/`stco` index rules apply (CR3 has traks); additionally the PRVW JPEG strict-decodes or the
  file is at best `Partial`.
- HEIC: requires `meta` (≤ 16 MiB) with `iloc`; every iloc extent (construction method 0, offsets absolute in the file) must lie
  inside the measured file → `Intact` (pixel data is HEVC; not decoded in the engine — document); an extent past the end →
  `Partial`; no `iloc`/garbled `meta` → `Damaged`. HEIC needs no `moov`/`mdat` rule from MP4 (mdat may be absent if data is in
  `idat`).

- [ ] **Step 1: Failing tests:** `raf_is_found_intact`, `raf_cut_is_partial`, `cr3_is_found_intact_with_kind_cr3`,
  `cr3_with_broken_preview_is_partial`, `heic_is_found_intact_with_kind_heic`, `heic_extent_past_end_is_partial`,
  `heic_without_iloc_is_damaged`, and `mp4_and_mov_kinds_unchanged` (control: existing testkit mp4/mov still Mp4/Mov).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** README, CHANGELOG. Commit: `feat(core): RAF, CR3 and HEIC photos`.

---

### Task 5: `inspect` — dados da foto e prévia

**Files:**
- Create: `crates/revenant-core/src/inspect.rs`; Modify: `lib.rs` (reexports)
- Test: `crates/revenant-core/tests/inspect.rs`

**Interfaces:**
- Consumes: `Finding`, `Source`, `tiff::walk`, the format modules' parsers (expose `pub(crate)` helpers rather than duplicating
  box/segment walking).
- Produces:
  ```rust
  #[derive(Clone, Debug, Default, PartialEq, Eq)]
  pub struct Metadata {
      pub make: Option<String>, pub model: Option<String>, pub lens: Option<String>, pub serial: Option<String>,
      pub iso: Option<u32>,
      /// Camera-local capture time "YYYY-MM-DDTHH:MM:SS" (EXIF DateTimeOriginal; for MP4/MOV the mvhd creation time in UTC).
      pub captured_at: Option<String>,
      pub width: Option<u32>, pub height: Option<u32>,
  }
  /// An embedded JPEG inside the found file (absolute range in the source), ready to show as the file's preview.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub struct Preview { pub start: u64, pub len: u64, pub width: u32, pub height: u32 }
  #[derive(Clone, Debug, Default, PartialEq, Eq)]
  pub struct Inspection { pub metadata: Metadata, pub preview: Option<Preview> }
  /// Never fails: what can't be read is None.
  pub fn inspect(source: &dyn Source, finding: &Finding) -> Inspection;
  ```
Per kind:
- Jpeg: EXIF from the first APP1 starting `Exif\0\0` (TIFF walk with limit = APP1 payload); width/height from SOF; preview = the
  whole file (it is the photo).
- TIFF-based RAW: fields from `walk`; preview = the largest of `jpegs` by SOF pixel count (dims from its SOF).
- Raf: EXIF from the embedded JPEG's APP1; preview = the embedded JPEG.
- Cr3: Make/Model from CMT1, Exif fields from CMT2; preview = the largest of the PRVW JPEG and trak 1's first chunk if it starts
  with `FF D8 FF`.
- Heic: EXIF from the `Exif` item (skip its 4-byte offset prefix); preview `None` (HEVC; the app will use the OS decoder).
- Mp4/Mov: `captured_at` from `mvhd` creation time (seconds since 1904-01-01 UTC; 0 → None); width/height from the first video
  track's `tkhd`; preview `None`.
- Validation of values: dates must parse as a real date with year ≥ 1990 (else None); strings ASCII-printable, trimmed, non-empty
  (else None); ISO > 0; preview only if its SOF parses and the range lies inside the finding.

- [ ] **Step 1: Failing tests:** `jpeg_exif_and_self_preview`, `each_raw_kind_metadata_and_largest_preview` (loop over
  TiffVariant × endianness, RAF, CR3), `heic_metadata_without_preview`, `mp4_creation_time_and_size`,
  `garbage_exif_fields_become_none` (non-ASCII make, date "0000:00:00 00:00:00", ISO 0 → None), `no_exif_gives_default_metadata`.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** README ("Inspect"), CHANGELOG. Commit: `feat(core): read photo details and the embedded preview`.

---

### Task 6: `revenant-dev`, amostras reais e fechamento da parte

**Files:**
- Modify: `crates/revenant-dev/src/main.rs`, `crates/revenant-dev/README.md`, `crates/revenant-dev/tests/cli.rs`
- Create: `samples/samples.toml` (list: name, url, sha256, expected kind), `scripts/fetch-samples.sh`,
  `crates/revenant-core/tests/real_samples.rs`, `.gitignore` (`/samples/cache/`)
- Modify: `.github/workflows/ci.yml` (job `samples` on `ubuntu-latest`, `actions/cache` keyed on `samples/samples.toml`),
  `README.md`, `CHANGELOG.md`, `CLAUDE.md`, `docs/superpowers/history/` (new `2026-10-19-m1-p2-pendencias.md` at the end)

**Interfaces:**
- CLI: each `found` line gains, when known, `"make"`, `"model"`, `"lens"`, `"serial"`, `"iso"`, `"captured_at"`, `"width"`,
  `"height"` (omitted when None) and `"preview":{"start":…,"len":…,"width":…,"height":…}`. New option `--previews <DIR>`:
  writes each preview as `<finding file stem>.preview.jpg` (same naming as `extract`, `.preview.jpg` suffix), same volume rules as
  `--extract` (refuses the source volume unless `--allow-same-volume`).
- Real samples: `scripts/fetch-samples.sh` downloads every entry of `samples/samples.toml` into `samples/cache/` and checks
  SHA-256. `real_samples.rs` is `#[ignore]` and runs when `REVENANT_SAMPLES` points at that dir: for each sample, embed it in a
  noise disk image at a sector start, scan, and assert exactly one finding of the expected kind, `Intact`, `len` == file size,
  and `inspect` returns make/model and a preview whose JPEG strict-decodes (HEIC: no preview).
- Samples: choose one per kind from raw.pixls.us (CC0) — Canon CR2, Canon CR3, Nikon NEF, Sony ARW, Fujifilm RAF, a DNG,
  Olympus/OM ORF, Panasonic RW2 — and a HEIC only if one with a CC0/public-domain license is found (record the license line in
  `samples.toml`; otherwise leave HEIC out and say so). Record exact URLs and SHA-256. Keep total download ≤ 400 MB.

- [ ] **Step 1: Failing CLI tests:** `found_lines_carry_metadata_and_preview` (testkit CR2 with EXIF → fields present and exact),
  `previews_are_written` (`--previews` → a file that is byte-identical to the embedded preview range).
- [ ] **Step 2:** FAIL. **Step 3:** implement CLI; write the samples list/script/test; run the script and the ignored test locally
  once (`REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --test real_samples -- --ignored`) and paste the output in the
  report. If a real sample fails, that is a real finding: report it with details (don't drop the sample).
- [ ] **Step 4:** suite, clippy, fmt.
- [ ] **Step 5: Docs:** dev README (metadata fields, `--previews`, samples), root README status, CHANGELOG (one bullet per
  capability), CLAUDE.md "Onde o desenvolvimento está" (1.2 concluída na branch; próximo 1.3), new history file with this part's
  decisions and pending items (pt-BR). Commit: `feat(dev): photo details, previews and real camera samples`.
