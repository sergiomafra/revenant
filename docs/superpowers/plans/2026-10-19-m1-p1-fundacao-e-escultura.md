# Módulo 1 · Parte 1 — Fundação e motor de escultura (JPEG, MP4/MOV) — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Criar o repositório como software de verdade (workspace Rust, licença, CI, docs, CLAUDE.md) e a primeira fatia do motor:
ler uma imagem de disco só para leitura, achar por assinatura fotos JPEG e vídeos MP4/MOV (mesmo em disco formatado), medir o fim
exato de cada um, classificar em Íntegro/Parcial/Danificado (`Intact`/`Partial`/`Damaged`) e extrair para outra pasta, recusando
salvar no mesmo volume.

**Architecture:** Biblioteca `revenant-core` sem interface, com quatro peças: `Source` (leitura posicional, só leitura), `Format`
(um por tipo de arquivo: reconhece a assinatura e mede o arquivo pela estrutura interna), `scan` (anda pela fonte setor a setor,
emite eventos) e `extract`. Um crate `revenant-testkit` gera imagens de disco sintéticas com conteúdo conhecido; um executável
`revenant-dev` (só para desenvolvimento) expõe o motor por linha de comando.

**Tech Stack:** Rust 2024 (workspace Cargo), `thiserror`, `zune-jpeg` (validação de JPEG), `windows-sys` (só Windows), `serde`/
`serde_json` e `clap` (só no `revenant-dev`), `image` + `tempfile` + `assert_cmd` (só em testes). GitHub Actions + `cargo-deny`.

**Spec:** `docs/superpowers/specs/2026-10-19-revenant-design.md` (§5 motor, §7 licença, §8 software, §9 documentação).

## Roteiro dos módulos (contexto; este plano cobre só a Parte 1.1)

- **Módulo 1 — Motor.** 1.1 (este plano): fundação + escultura JPEG/MP4/MOV. 1.2: formatos RAW (CR2, CR3, NEF, ARW, RAF, DNG, ORF,
  RW2, HEIC) + prévia embutida e EXIF + escultura de arquivos fragmentados. 1.3: exFAT/FAT32 (apagados, nomes e pastas),
  deduplicação, agrupamento por sessão/câmera, cópia do cartão tolerante a setores ruins, sessão de busca salva, leitura do
  aparelho bruto (Mac/Windows) com desmontagem sem ejetar.
- **Módulo 2 — App** (Tauri 2 + React): telas em horizontal aprovadas pelo Sérgio ANTES do plano; ajudante privilegiado; fluxo
  completo; resultados com três visualizações; i18n pt-BR/es/en.
- **Módulo 3 — Lançamento:** release automático, instaladores das 4 arquiteturas, assinatura ad-hoc, atualizador.
- **Módulo 4 — Site de documentação** (Astro Starlight, 3 idiomas, GitHub Pages).

## Global Constraints

- **Language [Sérgio, 19/10/2026]: everything in the repository is in English so more people can contribute**: identifiers,
  comments, error codes, commit messages, crate READMEs, `README.md`, `CHANGELOG.md`. Exceptions: `CLAUDE.md` and
  `docs/superpowers/**` (specs and plans) stay in pt-BR; the end-user docs site (Módulo 4) is pt-BR/es/en.
- **Never write to the source.** `Source` has no write method; `FileSource` opens read-only. No engine code opens the source path
  with write access.
- **Licenses (spec §7):** dependencies only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense,
  CC0-1.0, BSL-1.0. Forbidden: third-party GPL/AGPL/LGPL/MPL and any code copied from PhotoRec/TestDisk. Revenant itself is
  **GPL-3.0-only**, copyright Sérgio Mafra.
- **Errors by code:** every engine error has `code() -> &'static str` in `snake_case`; no UI text in the engine.
- **Rust:** edition 2024, `rust-version = "1.85"`. Dev machine is an **Intel** Mac with Homebrew Rust (no `rustup`): **never**
  `brew install/upgrade` toolchains; extra targets only in CI.
- **No `unsafe`** outside `source/windows.rs` (`windows-sys` calls), each block with a `// SAFETY:` comment.
- **Docs in the same commit [Sérgio]:** every task that changes behavior updates the README of the crate it touches and, if it
  changes what users see, `CHANGELOG.md` (`## [Unreleased]`).
- **Commit trailer, always:** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Never `git add -A`; check
  `git diff --staged --name-only` before each commit. Conventional Commits (`feat(core): …`, `chore: …`).
- **Tests never touch real data** or physical disks: only synthetic images in `tempfile`.
- Every new test is seen failing before it passes.

## Review Focus

1. **File crossing a read-block boundary** (starts at the end of a 4 MiB block, ends in the next): must come out whole and
   `Intact` → test `jpeg_crossing_blocks` in Task 5.
2. **Unreadable sector mid-scan** (failing card): scan doesn't stop, records the unreadable region, finds what comes after → test
   `unreadable_sector_does_not_stop_scan` in Task 5.
3. **Garbage that looks like a header** (stray `FF D8 FF` or `ftyp` in random data): not reported → tests
   `fake_header_is_not_jpeg` (Task 3) and `stray_ftyp_is_not_video` (Task 4).
4. **Tiny or empty image** (0 bytes, 100 bytes, size not a multiple of 512): zero findings, no panic; a file in the last,
   incomplete sector is still found → test `small_and_unaligned_images` in Task 5.
5. **Destination on the source volume**, or same file name already there: refuse with `destination_on_source`; never overwrite,
   add `_2` suffix → tests in Task 6.

---

### Task 1: Fundação do repositório

**Files:**
- Create: `Cargo.toml` (workspace; sem `rust-toolchain.toml`, a máquina de dev usa o Rust do Homebrew),
  `crates/revenant-core/Cargo.toml`, `crates/revenant-core/src/lib.rs`, `crates/revenant-core/src/error.rs`,
  `crates/revenant-core/README.md`
- Create: `LICENSE` (texto oficial da GPL-3.0), `README.md` (substitui o atual), `CHANGELOG.md`, `CLAUDE.md`, `deny.toml`,
  `.github/workflows/ci.yml`; Modify: `.gitignore` (acrescentar `/target/`; manter `/.superpowers/`)

**Interfaces:**
- Produces:
  ```rust
  // crates/revenant-core/src/error.rs
  #[derive(Debug, thiserror::Error)]
  pub enum EngineError {
      #[error("open_failed")]           Open { #[source] cause: std::io::Error },
      #[error("read_failed")]           Read { offset: u64, #[source] cause: std::io::Error },
      #[error("destination_on_source")] DestinationOnSource,
      #[error("write_failed")]          Write { #[source] cause: std::io::Error },
      #[error("cancelled")]             Cancelled,
  }
  impl EngineError { pub fn code(&self) -> &'static str }
  ```
  `lib.rs` declara `pub mod error;` e reexporta `EngineError`.

- [ ] **Step 1: Teste que falha** — em `error.rs`, `#[cfg(test)] mod tests`:
  ```rust
  #[test]
  fn codes_are_stable() {
      assert_eq!(EngineError::DestinationOnSource.code(), "destination_on_source");
      assert_eq!(EngineError::Cancelled.code(), "cancelled");
      let io = || std::io::Error::other("x");
      assert_eq!(EngineError::Read { offset: 512, cause: io() }.code(), "read_failed");
      assert_eq!(EngineError::Open { cause: io() }.code(), "open_failed");
      assert_eq!(EngineError::Write { cause: io() }.code(), "write_failed");
  }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core` → FAIL (não compila: `code` não existe).
- [ ] **Step 3:** Implementar `code()` (um `match`). Workspace: `members = ["crates/*"]`, `resolver = "3"`,
  `[workspace.package] edition = "2024", rust-version = "1.85", license = "GPL-3.0-only", repository =
  "https://github.com/sergiomafra/revenant"`, versão `0.1.0`; todos os crates `publish = false`.
- [ ] **Step 4:** `cargo test -p revenant-core` → PASS; `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check` limpos.
- [ ] **Step 5: Arquivos de projeto.**
  - `deny.toml`: `[licenses] allow = [...]` exatamente a lista das Global Constraints; os crates do workspace ficam fora da
    checagem por `[licenses.private] ignore = true` (o GPL-3.0 é só nosso); `[bans] multiple-versions = "warn"`.
  - `ci.yml`: em `push` e `pull_request`; matriz `macos-latest` (arm64), `macos-15-intel` (x86_64; o `macos-13` foi
    aposentado), `windows-latest` (x64), `windows-11-arm` (ARM64); passos `cargo fmt --check`,
    `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`; job separado
    `EmbarkStudios/cargo-deny-action@v2` em `ubuntu-latest`.
  - `README.md` (inglês): o que é o Revenant (1 parágrafo, da spec §1), estado atual ("under construction: recovery engine; no
    app to download yet"), licença, como contribuir (curto).
  - `CHANGELOG.md` (inglês): formato Keep a Changelog, seção `## [Unreleased]`.
  - `CLAUDE.md` (pt-BR, enxuto, no molde do Clustory): onde o desenvolvimento está; diretrizes fixas (as Global Constraints +
    processo "um módulo por vez; spec → telas → plano → subagentes; o Sérgio decide o merge" + "documentação sempre atualizada e
    explicando como usar" + "código e repositório em inglês"); seção "Armadilhas" vazia com a regra "todo erro vira uma
    instrução aqui"; ambiente do Mac Intel.
  - `crates/revenant-core/README.md` (inglês): o que a versão atual do motor faz (por ora: códigos de erro).
- [ ] **Step 6:** `cargo test --workspace` PASS. Commit: `chore: repository foundation (workspace, license, CI, docs)`.

---

### Task 2: `Source` — leitura posicional só leitura

**Files:**
- Create: `crates/revenant-core/src/source/mod.rs`, `crates/revenant-core/src/source/file.rs`,
  `crates/revenant-core/src/source/unix.rs`, `crates/revenant-core/src/source/windows.rs`
- Test: `crates/revenant-core/tests/source.rs`

**Interfaces:**
- Consumes: `EngineError` (Task 1).
- Produces:
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
  pub struct VolumeId(pub u64);

  pub trait Source: Send + Sync {
      fn len(&self) -> u64;
      /// Reads from `offset`. Fills the whole buffer except at the end of the source (returns less; 0 past the end).
      fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError>;
      /// Volume the source lives on (to refuse extracting onto it). None if unknown.
      fn volume(&self) -> Option<VolumeId>;
  }
  pub struct FileSource { /* File + len + volume */ }
  impl FileSource { pub fn open(path: &std::path::Path) -> Result<Self, EngineError> }
  impl Source for FileSource {}
  /// Volume of an existing path (file or directory).
  pub fn volume_of(path: &std::path::Path) -> Result<VolumeId, EngineError>
  ```
  `read_at` usa leitura posicional sem cursor compartilhado (`FileExt::read_at` no Unix, `seek_read` no Windows), em laço até
  encher o buffer ou chegar ao fim. `volume_of`: Unix = `MetadataExt::dev()`; Windows = `dwVolumeSerialNumber` de
  `GetFileInformationByHandle` (abrir pastas exige `FILE_FLAG_BACKUP_SEMANTICS`). Adicione também `is_empty()` se o clippy pedir.

- [ ] **Step 1: Testes que falham** (`tests/source.rs`, com `tempfile`):
  ```rust
  #[test] fn reads_middle_and_end() {
      // 1000-byte file, byte i = (i % 251) as u8
      let s = FileSource::open(&path).unwrap();
      assert_eq!(s.len(), 1000);
      let mut b = [0u8; 10];
      assert_eq!(s.read_at(500, &mut b).unwrap(), 10); assert_eq!(b[0], (500 % 251) as u8);
      assert_eq!(s.read_at(995, &mut b).unwrap(), 5);
      assert_eq!(s.read_at(1000, &mut b).unwrap(), 0);
      assert_eq!(s.read_at(5000, &mut b).unwrap(), 0);
  }
  #[test] fn never_modifies_the_source() {
      // content and modification time identical after open, reading everything, and dropping the source
  }
  #[test] fn opens_read_only_file() {
      // Unix: chmod 0o444 on the file; FileSource::open still works
  }
  #[test] fn same_volume_for_file_and_its_dir() {
      assert_eq!(volume_of(dir.path()).unwrap(), FileSource::open(&path).unwrap().volume().unwrap());
  }
  #[test] fn opening_missing_file_gives_code() {
      assert_eq!(FileSource::open(Path::new("/does/not/exist")).err().unwrap().code(), "open_failed");
  }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core --test source` → FAIL (tipos inexistentes).
- [ ] **Step 3:** Implementar conforme Interfaces. Varredura de convenção no mesmo arquivo de teste:
  `source_has_no_write_path` lê `src/source/*.rs` como texto e reprova `write(true)`, `.append(`, `create(true)`,
  `create_new(` e `GENERIC_WRITE`.
- [ ] **Step 4:** testes PASS; clippy/fmt limpos.
- [ ] **Step 5:** README do crate (seção "Source"). Commit: `feat(core): read-only positional source`.

---

### Task 3: Testkit e o formato JPEG

**Files:**
- Create: `crates/revenant-testkit/Cargo.toml` (dependências `image` só com o recurso `jpeg`, `tempfile`),
  `crates/revenant-testkit/src/lib.rs`
- Create: `crates/revenant-core/src/formats/mod.rs`, `crates/revenant-core/src/formats/jpeg.rs`
- Test: `crates/revenant-core/tests/jpeg.rs`

**Interfaces:**
- Consumes: `Source`, `FileSource` (Task 2).
- Produces (testkit):
  ```rust
  /// Valid baseline JPEG, deterministic pseudo-random content per `seed`, with an APP1 segment of `app1_extra` bytes
  /// (simulates camera EXIF with an embedded thumbnail; the APP1 payload contains a complete 16×16 JPEG).
  pub fn jpeg(width: u32, height: u32, seed: u64, app1_extra: usize) -> Vec<u8>;
  pub struct DiskImage { bytes: Vec<u8> }
  impl DiskImage {
      pub fn noise(len: usize, seed: u64) -> Self; // deterministic random fill
      pub fn zeros(len: usize) -> Self;
      pub fn put(&mut self, offset: usize, data: &[u8]) -> &mut Self;
      pub fn save(&self) -> tempfile::NamedTempFile;
  }
  ```
- Produces (core):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)] pub enum FileKind { Jpeg, Mp4, Mov }
  impl FileKind { pub fn extension(self) -> &'static str } // "jpg", "mp4", "mov"
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Health { Intact, Partial, Damaged }
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct Measurement { pub len: u64, pub kind: FileKind, pub health: Health }
  pub trait Format: Send + Sync {
      fn name(&self) -> &'static str;
      /// `head`: up to 16 bytes read at a sector start.
      fn recognizes(&self, head: &[u8]) -> bool;
      /// Measures the file starting at `start`. None = not a file of this format.
      fn measure(&self, source: &dyn Source, start: u64) -> Option<Measurement>;
  }
  pub struct Jpeg;
  pub const JPEG_MAX_LEN: u64 = 64 * 1024 * 1024;
  pub fn all_formats() -> Vec<Box<dyn Format>>; // [Jpeg] in this task; Task 4 adds IsoBmff
  ```

Regras do `Jpeg::measure` (o algoritmo que a assinatura não determina):
- `recognizes`: `FF D8 FF` seguido de um marcador válido (`E0..=EF`, `DB`, `C0..=C3`, `C4`, `FE`).
- Anda pelos segmentos pelos comprimentos declarados (assim a miniatura dentro do APP1 é pulada). Marcador inválido ou
  comprimento < 2 antes do SOS → `None` (cabeçalho falso).
- Exige DQT e SOF antes do SOS; sem eles → `None`.
- Depois do SOS, varre os dados comprimidos: `FF 00` e `FF D0..=D7` são dados; `FF D9` é o fim → `len` inclui o `FF D9`.
- Para, sem achar `FF D9`, ao chegar ao fim da fonte, a `JPEG_MAX_LEN`, ou a um **início de setor (512) cujos bytes são uma
  assinatura reconhecida por `all_formats()`** → `len` até ali, `health = Partial`.
- Com `FF D9`: decodifica os bytes com `zune-jpeg`; sucesso → `Intact`; erro → `Damaged`.

- [ ] **Step 1: Testes que falham** (`tests/jpeg.rs`):
  ```rust
  #[test] fn whole_jpeg_measured_exactly() {
      let j = testkit::jpeg(640, 480, 1, 20_000);
      let img = DiskImage::zeros(1 << 20).put(4096, &j).save();
      let m = Jpeg.measure(&FileSource::open(img.path()).unwrap(), 4096).unwrap();
      assert_eq!(m, Measurement { len: j.len() as u64, kind: FileKind::Jpeg, health: Health::Intact });
  }
  #[test] fn app1_thumbnail_does_not_end_the_file() { /* app1_extra = 60_000; len == j.len() */ }
  #[test] fn jpeg_cut_by_next_file_is_partial() {
      // j1 cut in half at 4096; whole j2 at the next multiple of 512 after 4096 + j1.len()/2
      // measure(4096) → Partial, len == (start of j2) - 4096
  }
  #[test] fn jpeg_cut_by_end_of_source_is_partial() {}
  #[test] fn corrupted_scan_data_is_damaged() {
      // whole jpeg with a stretch of the scan data corrupted in a way zune-jpeg rejects (e.g. an invalid FF xx marker in
      // the middle of the scan); same len, Damaged. If no corruption is rejected by the decoder, report DONE_WITH_CONCERNS
      // with proof instead of loosening the rule.
  }
  #[test] fn fake_header_is_not_jpeg() {
      // DiskImage::noise(1<<20, 7) with b"\xFF\xD8\xFF\xE1\x00\x01" at 0 → measure(0) == None
      // and elsewhere FF D8 FF E0 + valid length followed by garbage without DQT/SOF → None
  }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core --test jpeg` → FAIL.
- [ ] **Step 3:** Implementar testkit e `Jpeg` conforme as regras. Leia a fonte em janelas de 1 MiB (nunca o arquivo inteiro antes
  de achar o fim; a decodificação final carrega até `JPEG_MAX_LEN`).
- [ ] **Step 4:** PASS; clippy/fmt limpos.
- [ ] **Step 5:** README do crate (seção "Formats: JPEG"). Commit: `feat(core): JPEG format measured by structure`.

---

### Task 4: Formato ISO BMFF (MP4/MOV)

**Files:**
- Create: `crates/revenant-core/src/formats/isobmff.rs`; Modify: `crates/revenant-core/src/formats/mod.rs` (`all_formats()`)
- Modify: `crates/revenant-testkit/src/lib.rs`
- Test: `crates/revenant-core/tests/isobmff.rs`

**Interfaces:**
- Consumes: `Format`, `Measurement`, `Health`, `FileKind`, `Source` (Tasks 2–3).
- Produces:
  ```rust
  pub struct IsoBmff;            // core; included in all_formats()
  // testkit:
  pub struct Mp4Options { pub mdat_len: usize, pub moov_at_end: bool, pub mov: bool, pub mdat_64bit: bool }
  /// ftyp (major brand "isom", or "qt  " if mov) + mdat + moov (mvhd, 1 trak with stsz and stco pointing into mdat;
  /// co64 if mdat_64bit). Order ftyp, moov, mdat — or ftyp, mdat, moov if moov_at_end (the camera case).
  pub fn mp4(opts: &Mp4Options) -> Vec<u8>;
  ```

Regras do `IsoBmff::measure`:
- `recognizes`: bytes 4..8 == `ftyp` e tamanho declarado do box entre 8 e 4096.
- Anda pelos boxes de nível superior a partir de `start`. Tipos aceitos: `ftyp moov mdat free skip wide uuid meta pdin moof mfra
  styp sidx udta`. Tamanho 1 → `largesize` de 64 bits; tamanho 0 → vai até o fim da fonte (só aceito para `mdat`). Para no
  primeiro box de tipo não aceito, tamanho < 8, ou fim da fonte. `len` = soma dos boxes válidos; box que passa do fim da fonte
  conta só até o fim.
- `kind`: marca principal do `ftyp` `qt  ` → `Mov`; senão `Mp4`.
- Sem `mdat` → `None`. Sem `moov` → `Damaged` (não toca sem o índice; o reparo é a 1.1 do roadmap do produto).
- Com `moov`: lê `stco`/`co64` e `stsz` de cada `trak`; algum chunk começando além de `start + len` ou fim de amostra além →
  `Partial`; senão `Intact`. Box `mdat` declarado maior que o que existe → `Partial`.

- [ ] **Step 1: Testes que falham** (`tests/isobmff.rs`), cada um coloca o MP4 em 8192 de uma `DiskImage::noise(…)`:
  ```rust
  #[test] fn mp4_moov_at_end_is_intact()  { /* len == bytes.len(), kind Mp4, Intact */ }
  #[test] fn mov_is_recognized_as_mov()   { /* mov: true → kind Mov */ }
  #[test] fn largesize_64_bit()           { /* mdat_64bit: true → exact len, Intact */ }
  #[test] fn missing_moov_is_damaged()    { /* moov_at_end mp4 cut before moov, rest noise → Damaged */ }
  #[test] fn truncated_mdat_is_partial()  { /* moov before mdat; bytes cut mid-mdat and source ends → Partial */ }
  #[test] fn stray_ftyp_is_not_video()    { /* only a valid ftyp followed by noise → None */ }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core --test isobmff` → FAIL.
- [ ] **Step 3:** Implementar. Pare a leitura de boxes internos em 16 MiB de `moov` (moov maior → `Damaged`).
- [ ] **Step 4:** PASS + toda a suíte do crate; clippy/fmt.
- [ ] **Step 5:** README (seção "Formats: MP4/MOV"). Commit: `feat(core): MP4/MOV format via box structure`.

---

### Task 5: Varredura

**Files:**
- Create: `crates/revenant-core/src/scan.rs`; Modify: `lib.rs` (reexports)
- Test: `crates/revenant-core/tests/scan.rs`

**Interfaces:**
- Consumes: `Source`, `Format`, `all_formats()`, `Measurement`, `EngineError`.
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct Finding { pub id: u64, pub start: u64, pub len: u64, pub kind: FileKind, pub health: Health }
  pub struct ScanOptions { pub sector: u64, pub block: usize }
  impl Default for ScanOptions { /* sector 512, block 4 MiB */ }
  #[derive(Clone, Default)] pub struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);
  impl CancelToken { pub fn cancel(&self); pub fn is_cancelled(&self) -> bool }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum Event { Found(Finding), Progress { read: u64, total: u64 }, Unreadable { start: u64, len: u64 } }
  #[derive(Clone, Debug, PartialEq, Eq, Default)] pub struct ScanSummary { pub found: u64, pub unreadable_bytes: u64 }
  pub fn scan(source: &dyn Source, formats: &[Box<dyn Format>], opts: &ScanOptions,
              cancel: &CancelToken, on_event: &mut dyn FnMut(Event)) -> Result<ScanSummary, EngineError>;
  ```
Regras:
- Lê a fonte em blocos de `opts.block`; olha a `head` (16 bytes) de cada início de setor; o primeiro formato que reconhece e
  mede ganha. Achou → emite `Found` (`id` sequencial a partir de 1) e pula para o próximo início de setor depois de
  `start + len`. Não achou → próximo setor. A `head` de um setor perto do fim do bloco é lida da fonte, nunca cortada.
- Bloco com erro de leitura → relê setor a setor; cada sequência de setores ilegíveis vira um `Unreadable` e entra em
  `unreadable_bytes`; a busca continua.
- `Progress` no máximo a cada bloco e uma vez no fim com `read == total`.
- `cancel.is_cancelled()` conferido a cada bloco → `Err(EngineError::Cancelled)`.

- [ ] **Step 1: Testes que falham** (`tests/scan.rs`). Uma `FailingSource` de teste embrulha `FileSource` e devolve
  `Err(Read)` para qualquer leitura que toque um intervalo dado.
  ```rust
  #[test] fn finds_photos_and_videos_on_formatted_card() {
      // DiskImage::noise(16 MiB): jpeg at 0, mp4 (moov at end) at 1 MiB, cut jpeg at 5 MiB + jpeg right after it,
      // mov at 9 MiB. Expected findings (start, kind, health) exactly:
      // [(0,Jpeg,Intact),(1MiB,Mp4,Intact),(5MiB,Jpeg,Partial),(next sector,Jpeg,Intact),(9MiB,Mov,Intact)]
  }
  #[test] fn jpeg_crossing_blocks() { /* block = 1 MiB, 300 KB jpeg starting at 1 MiB - 100 KB (512-aligned) → Intact, exact len */ }
  #[test] fn unreadable_sector_does_not_stop_scan() {
      // jpeg at 0, sectors [2 MiB, 2 MiB + 4096) unreadable, jpeg at 3 MiB
      // → 2 Intact findings; one Unreadable { start: 2 MiB, len: 4096 }; summary.unreadable_bytes == 4096
  }
  #[test] fn small_and_unaligned_images() {
      // 0 bytes → summary.found 0; 100 bytes of noise → 0;
      // 10_000-byte image (not a multiple of 512) with a small jpeg at sector 9216 ending at the last byte → 1 Intact finding
  }
  #[test] fn progress_ends_complete() { /* last Progress has read == total */ }
  #[test] fn cancel_interrupts() { /* cancel inside the callback on first Progress → Err with code "cancelled" */ }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core --test scan` → FAIL.
- [ ] **Step 3:** Implementar conforme as regras.
- [ ] **Step 4:** PASS + suíte inteira; clippy/fmt.
- [ ] **Step 5:** README (seção "Scan"). Commit: `feat(core): sector scan with events and unreadable regions`.

---

### Task 6: Extrair

**Files:**
- Create: `crates/revenant-core/src/extract.rs`; Modify: `lib.rs`
- Test: `crates/revenant-core/tests/extract.rs`

**Interfaces:**
- Consumes: `Source`, `Finding`, `volume_of`, `EngineError`.
- Produces:
  ```rust
  /// Copies the finding's bytes into `dest_dir`, named `revenant_<start as 12 hex digits>.<ext>`
  /// (`_2`, `_3`… if it exists; never overwrites). Refuses if the directory is on the source's volume.
  pub fn extract(source: &dyn Source, finding: &Finding, dest_dir: &std::path::Path) -> Result<std::path::PathBuf, EngineError>;
  ```
  Cria com `create_new(true)`; copia em blocos de 1 MiB; erro de leitura no meio → preenche o trecho com zeros e segue (o arquivo
  sai parcial em vez de não sair); erro de escrita → `Write` e apaga o arquivo incompleto.

- [ ] **Step 1: Testes que falham** (`tests/extract.rs`). Para testar o volume sem dois discos, use uma `Source` de teste que
  embrulha `FileSource` e devolve um `volume()` escolhido.
  ```rust
  #[test] fn extracts_identical_bytes() { /* file content == jpeg bytes; name "revenant_000000001000.jpg" for start 4096 */ }
  #[test] fn never_overwrites() { /* extract twice → second is "revenant_000000001000_2.jpg"; first intact */ }
  #[test] fn refuses_destination_on_source_volume() {
      // source with volume() == volume_of(dir) → Err code "destination_on_source", directory stays empty
  }
  #[test] fn accepts_other_volume() { /* volume() == Some(VolumeId(u64::MAX)) → Ok */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implementar. **Step 4:** PASS + suíte; clippy/fmt.
- [ ] **Step 5:** README (seção "Extract"). Commit: `feat(core): extract findings without overwriting, never onto the source`.

---

### Task 7: `revenant-dev` (linha de comando de desenvolvimento) e fechamento da parte

**Files:**
- Create: `crates/revenant-dev/Cargo.toml` (`clap` derive, `serde`, `serde_json`), `crates/revenant-dev/src/main.rs`,
  `crates/revenant-dev/README.md`
- Test: `crates/revenant-dev/tests/cli.rs` (`assert_cmd`, `revenant-testkit`)
- Modify: `README.md`, `CHANGELOG.md`, `CLAUDE.md` ("Onde o desenvolvimento está"), `crates/revenant-core/README.md`

**Interfaces:**
- Consumes: `FileSource`, `all_formats()`, `scan`, `extract`, `Event`.
- Produces: `revenant-dev scan <IMAGE> [--extract <DIR>]`. Saída: uma linha JSON por evento em stdout —
  `{"event":"found","id":1,"start":0,"len":12345,"kind":"jpeg","health":"intact"}`,
  `{"event":"unreadable","start":…,"len":…}`, `{"event":"summary","found":N,"unreadable_bytes":M}`
  (progresso vai para stderr). Com `--extract`, extrai cada achado ao ser achado e acrescenta `"file":"<path>"` na linha.
  Erro → stderr `error: <code>` e saída 1. (`serde` só neste crate; o core não depende de `serde`.)

- [ ] **Step 1: Testes que falham:**
  ```rust
  #[test] fn scan_lists_findings_as_json() { /* image with 1 jpeg + 1 mp4 → 2 "found" lines and "summary" with found 2; exit 0 */ }
  #[test] fn extract_writes_files() { /* --extract tmpdir → 2 files with bytes identical to the originals */ }
  #[test] fn missing_image_exits_with_code() { /* stderr contains "error: open_failed", exit 1 */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implementar. **Step 4:** `cargo test --workspace` PASS; clippy/fmt.
- [ ] **Step 5: Documentação da parte:**
  - `crates/revenant-dev/README.md` (inglês): what it is (developer tool, not the app), how to build, example with a card image
    (how to make one read-only with `dd`/`diskutil` on Mac and the equivalent on Windows, never writing to the card), output format.
  - `README.md`: current state and "trying the engine" for developers.
  - `CHANGELOG.md` `[Unreleased]`: "Engine: finds JPEG photos and MP4/MOV videos in card images, including formatted cards;
    classifies them as intact/partial/damaged; recovers them to another folder."
  - `CLAUDE.md`: Parte 1.1 concluída na branch; próximo: 1.2.
- [ ] **Step 6: Medição final:** `cargo test --workspace 2>&1 | grep -E '^test result'` (todas `ok`), clippy e fmt limpos.
  Commit: `feat(dev): developer command line and part 1.1 docs`.
