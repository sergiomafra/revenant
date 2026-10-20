# Módulo 1 · Parte 4 — Sistemas de arquivos — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A busca passa a ler exFAT, FAT32, FAT16 e FAT12 (em MBR, GPT ou sem tabela, inclusive com o setor de boot
estragado): cada foto ou vídeo que o sistema de arquivos registra, vivo ou apagado, sai **uma vez**, com nome, pasta,
datas e os pedaços exatos da cadeia de clusters, e a saúde continua vindo só dos detectores. Um cartão formatado rápido
recupera os nomes pelas pastas antigas. Os arquivos recuperados mantêm o nome original (ou ganham um nome por data e
câmera) e há uma API de agrupamento por sessão, dia, câmera e pasta. Fecha as pendências com dono 1.4.

**Architecture:** Um módulo novo `filesystem` acha os volumes (tabelas de partição, boot primário ou cópia) e lista os
arquivos de foto/vídeo com os pedaços em bytes, só lendo metadados. `scan` roda essa leitura antes da escultura, emite
um `Event::Volume` por volume e, no laço de commit em ordem, trata cada arquivo do sistema de arquivos no setor onde ele
começa: o detector mede o arquivo por uma visão `Fragmented` dos pedaços (numa thread de trabalho), e uma regra única
junta essa medida com a da escultura no mesmo início. `Finding` ganha `origin: Option<FsOrigin>`. Nada é pulado antes da busca: um
arquivo do sistema de arquivos só vira faixa pulada quando o achado fundido mede `Intact` (R10); o resto da escultura segue igual. Depois da busca, pastas antigas
achadas por assinatura dão nome aos achados sem origem (`Updated`). `naming` e `group` são funções puras sobre
`Finding` + `Metadata`.

**Tech Stack:** Rust 2024 (MSRV 1.88), só `std` e as dependências atuais (CRC32 próprio, sem crate nova). Teste opcional
contra o driver real do macOS com `hdiutil` (já no sistema).

**Spec:** `docs/superpowers/specs/2026-10-19-revenant-design.md` (§3 passo 6: nomes; §4: agrupar por data ou por pasta,
"Por pasta" só quando a estrutura sobreviveu, seleção por grupo; §5: leitura do sistema de arquivos exFAT e FAT32,
deduplicação, agrupamento por sessão e câmera, somente leitura).
**Antes:** `docs/superpowers/history/2026-10-19-m1-p1-pendencias.md`, `2026-10-19-m1-p2-pendencias.md`,
`2026-10-20-m1-p3-pendencias.md`. **Roteiro:** `docs/superpowers/roadmap.md`. **Plano anterior (estilo):**
`docs/superpowers/plans/2026-10-20-m1-p3-fragmentos-e-velocidade.md`.

## Global Constraints

- **Language [Sérgio]:** everything in the repository in English (identifiers, comments, error codes, commits, READMEs,
  CHANGELOG); `CLAUDE.md` and `docs/superpowers/**` stay in pt-BR.
- **Never write to the source.** The file-system code only reads, through `&dyn Source`; nothing gets a write path. The
  opt-in macOS test writes only to image files it created in a temp dir and attaches only its own images.
- **Licenses (spec §7):** dependencies only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense,
  CC0-1.0, BSL-1.0. **No new dependency in this part** (CRC32 and UTF-16 handling are written here). No code copied from
  PhotoRec/TestDisk, the Linux/BSD FAT drivers or any GPL/LGPL project; the on-disk formats come from the public Microsoft
  specifications (FAT: "FAT32 File System Specification" 1.03; exFAT: "exFAT file system specification") and the UEFI GPT
  chapter.
- **Errors by code** (`code() -> &'static str`, snake_case); no UI text in the engine. A broken file system is never an
  error: it yields fewer volumes or files.
- **Rust:** edition 2024, `rust-version = "1.88"`. Dev machine: Intel Mac, Rust from `/usr/local/bin` (no `rustup`); **never
  run `brew`**; check that a binary exists (`which`, `/sbin`, `/usr/bin`) before using it. No `unsafe` outside
  `source/windows.rs`.
- **Bounded everything** (constants in `filesystem/mod.rs`, each with a `cfg(test)`-lowerable twin where a test needs it):
  ≤ 16 volumes; ≤ 64 EBR links; ≤ 128 GPT entries; ≤ 262,144 photo/video files listed per scan; ≤ 65,536 directories and
  ≤ 4,000,000 directory entries parsed per volume; depth ≤ 32; a FAT directory ≤ 2 MiB, an exFAT directory ≤ 256 MiB;
  ≤ 4096 pieces per file; volumes with more than 2^28 clusters are reported but not walked; ≤ 65,536 contested clusters;
  ≤ 65,536 high-word probes of 512 B per scan (the 16 B check of the low cluster counts as a probe); ≤ 65,536
  old-directory candidates; ≤ 2^20 pieces in the whole listing (`MAX_PIECES`, a file past it is not listed); ≤ 2^26
  cluster examinations per volume, shared by `guess`, the old exFAT chain and the bitmap check (`MAX_GUESS_STEPS`,
  exhausted ⇒ the remaining deleted files are not listed); when the contested set (65,536) fills, every live file of that
  volume becomes untrusted. No loop or allocation driven by an on-disk number without a cap.
- **Read once, precisely:** file-system metadata is read through the scan's `CachedSource`; the high-word probes (Task 5)
  read 512 B each through a `Probe` over the raw source, outside the cache, and the `Probe` refuses (without reading) any
  range the cache's bad-sector map already knows; at most 65,536 reads of 512 B (32 MiB) per scan. The later pieces of a
  fragmented file may be read again when the cache is small, and the old folders of Task 10 are read after the search.
  READMEs say exactly that.
- **Determinism:** for a given source, the event sequence is identical for any `threads` and any `cache_bytes`; listings,
  merges, names and groups never depend on hash-map order or timing. The job of a file-system file is never dropped
  because of a skip range, and every commit path without a worker result (`NotMeasured`, deferred block, `threads 1`)
  runs the same `fs_job(at)` function as the worker, so the result does not depend on who measured.
- **Hostile names at parse:** `/` and NUL inside a path component read from the disk become `_` (FAT LFN and exFAT
  names alike; exFAT names are decoded with `String::from_utf16_lossy`), so `FsOrigin::name()`/`folder()` never split
  wrongly.
- **Health semantics (binding):** `Intact` = structure complete AND content verified clean; `Partial` = salvageable but not
  clean; `Damaged` = won't open. **The file system is evidence of placement, never of integrity:** health always comes
  from a detector over the bytes; file-system facts can only lower it (D3, D5, D6). Never `Intact` without evidence.
- **Docs in the same commit [Sérgio]:** each task updates the READMEs it affects and `CHANGELOG.md` `[Unreleased]` for
  anything user-visible.
- **Commit trailer, always:** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Conventional Commits. Never
  `git add -A` (add files by name). Never push (the controller opens the PR).
- **Tests never touch real data or physical disks:** synthetic images in `tempfile` only. The single new exception is the
  opt-in `os_filesystems` / `os_builders` tests (Task 2): images that `hdiutil` creates in a temp dir, gated by
  `REVENANT_OS_FS=1`, macOS and the binaries existing. Every new test is seen failing before it passes (record RED in the
  report).

## Review Focus

1. **Hostile directories and cluster chains** (a subdirectory pointing at the root or at itself, a FAT chain that cycles,
   two files sharing clusters, a directory chain longer than its cap, an exFAT root of 2^40 bytes): bounded time, each
   directory walked once, no panic, the files involved never `Intact` → `hostile_directory_loop_and_chain_cycle_are_bounded`
   (Task 4), `hostile_exfat_is_bounded` (Task 6), `hostile_partition_tables_are_bounded` (Task 3),
   `hostile_deleted_entries_are_bounded` and `high_word_probes_are_capped` (Task 5), `piece_cap_bounds_the_listing` and
   `contested_overflow_makes_the_volume_untrusted` (Task 4).
2. **A deleted file rebuilt from clusters now owned by another file and called `Intact`** →
   `deleted_file_never_takes_live_clusters` (Task 5), `reused_clusters_make_a_deleted_file_partial` (Task 6),
   `carved_file_over_live_clusters_is_not_intact`, `forged_live_file_does_not_hide_a_deleted_photo` and
   `photo_written_over_a_raw_preview_is_found` (Task 7).
3. **Deduplication hiding a real, distinct file** (a deleted file's guessed clusters hold another photo written later; a
   foreign photo inside a `Partial` RAW) → `dedup_keeps_distinct_files` (Task 7), `partial_raw_hides_its_own_jpegs_only`
   (Task 9).
4. **Hostile names used when extracting** (`../`, absolute, `C:\`, NUL, `..`, `CON` and its variants, lone UTF-16
   surrogates on FAT and exFAT, 300 chars, bidi controls, trailing dots, case-only collisions) → `sanitize_rules`,
   `hostile_names_stay_inside_the_destination` and `case_insensitive_names_do_not_overwrite` (Task 11).
5. **Nondeterminism** (events across threads and caches with file-system findings; groups depending on input order) →
   `fs_events_identical_across_threads_and_caches`, `fs_job_is_independent_of_who_measures` and
   `cancel_inside_the_first_volume_callback` (Task 7), `orphan_events_identical_across_threads_and_caches` (Task 10),
   `grouping_is_order_independent` (Task 12).

## Decisões do pre-flight (C1–C20, K1–K4)

Cada linha: ruling aceito pelo controlador — custo se estiver errado. O texto de cada tarefa já reflete o ruling.

- **C1** Jobs de arquivo do sistema de arquivos nunca são descartados por `skipped`; `fill` cria um `Head` para cada início mantido; todo caminho de commit sem resultado chama `fs_job(at)` (Task 7) — custo: nenhum (torna explícito o que o texto supunha).
- **C2** `read(source, cancel)` chama `read_with(meta, probe, cancel)`; o `Probe` lê 512 B da fonte crua e recusa faixas ruins conhecidas; a leitura de 16 B do `lo` conta no teto de 65.536 (Tasks 5, 7) — custo: amplificação de leitura (até 64 GiB) ou releitura de setor ruim num cartão doente.
- **C3** Candidatos do D10 = `{lo}` (se reconhece e está livre) ∪ `{h ≥ 1 …}`; exatamente um ⇒ usa; o teste usa uma cabeça JPEG livre em 4.564 (Task 5) — custo: um apagado do Windows ambíguo vira nome errado (a saúde segue do detector).
- **C4** Resolvido pelo K1 (sem pré-pulo de vivos) — custo: ver K1.
- **C5** `Listing.walked` (faixas de todo diretório percorrido, vivo ou apagado) é criado e preenchido na Task 10 — custo: reprocessar diretórios já lidos (só tempo).
- **C6** O teste do `ClusterCount 2^30` remenda `ClusterCount`, `FatLength`, `ClusterHeapOffset`, `VolumeLength` e refaz o checksum (Task 6) — custo: o implementador afrouxa a regra da Task 3 para passar.
- **C7** `MAX_GUESS_STEPS = 2^26` por volume, partilhado por `guess`, cadeia antiga do exFAT e conferência do bitmap (Tasks 5, 6) — custo: num cartão gigante alguns apagados finais perdem o nome (a escultura os acha).
- **C8** `MAX_PIECES = 1 << 20` na `Listing` (Task 4; frase de memória em `ScanOptions`, Task 7) — custo: OOM num cartão forjado.
- **C9** `notes.txt` recebe os bytes exatos do JPEG no último cluster, para só o teto D7 poder dar `Partial` (Task 7) — custo: teste verde sem a regra.
- **C10** Diretório confiável = cadeia termina em `End`, sem ciclo/conflito, ≤ teto (exFAT `NoFatChain`: `DataLength` inteiro reivindicado e marcado no bitmap); a raiz fixa de FAT12/16 não entra em `live` (Task 4) — custo: pular ou deixar de pular um diretório quebrado.
- **C11** Conjunto contestado cheio ⇒ todo arquivo vivo do volume vira não confiável (Task 4) — custo: num cartão muito cruzado tudo sai no máximo `Partial`.
- **C12** `exfat-card` entra em `default_values` e na ajuda do `--images`; o pico de memória (= tamanho da imagem) é documentado (Task 8) — custo: `bench` padrão usa 1 GiB de RAM a mais.
- **C13** `own_files` sai da mesma função de medição em todo caminho; um achado escondido que mede `Intact` ainda adiciona a sua faixa pulada; o `OrphanFinder` copia as regras do `keep_moovs`; teste de eventos iguais na Task 10 (Tasks 9, 10) — custo: `Updated` de órfão dependente de timing.
- **C14** O builder grava a entrada 0x83 (exFAT) e a de rótulo (FAT); `fat_delete_styles` usa o cluster 70.000 (Task 1) — custo: nenhum.
- **C15** `/` e NUL de um componente viram `_` no parse; nomes exFAT por `from_utf16_lossy` (Tasks 4, 6) — custo: nenhum.
- **C16** Pastas de origem `Orphaned` contam como estrutura sobrevivente: "Por pasta" fica ligado quando as únicas pastas são antigas recuperadas (Task 12) — custo: se o Sérgio preferir desligar após formatação, a condição vira "algum item `Live`/`Deleted`" (uma linha).
- **C17** Testes da Task 12 corrigidos (item 6 às 08:15; `by_camera` ordenado pela chave ⇒ `[B, A, UnknownCamera]`; raiz = `Folder("")`) — custo: nenhum.
- **C18** O RAW nomeado do teste de nomes se chama `DSC01235.ARW` (Task 11) — custo: nenhum.
- **C19** `rank(Health)` (Intact 2, Partial 1, Damaged 0) em `scan/fs.rs`; `modified` ausente perde para presente (Tasks 7, 10) — custo: nenhum.
- **C20** Lista de arquivos da Task 7 completa; `ScanOptions::sector > 512` descarta em silêncio arquivos cujo início não cai no setor (documentado); "lido uma vez" cita as pastas órfãs (Tasks 7, 10); D14 vs spec §3 segue como pendência do Módulo 2 (Task 13) — custo: nenhum.
- **K1** Sem pré-pulo de vivos: a faixa pulada só entra quando o achado fundido mede `Intact` (R10); vivo de outro tipo: só a cabeça no primeiro cluster é consumida; `Listing.skip` some (Tasks 4, 7) — custo: o portão de desempenho; se `exfat-card` < 0,8 × `dense-jpeg`, reavaliar só para tipos não-foto (Task 8).
- **K2** Coberto pelo C2 mais `high_word_probes_are_capped` com teto `cfg(test)` em 4 (Task 5) — custo: leitura dobrada ou releitura de setor ruim.
- **K3** Três testes de determinismo: cancelamento no callback do primeiro `Volume`, eventos iguais da Task 10, `threads 1` com cache de 1 MiB (Tasks 7, 10) — custo: eventos diferentes entre máquinas.
- **K4** `sanitize` cobre reservados completos (haste até o primeiro `.`, `COM0`/`LPT0`, `¹²³`, `CONIN$`/`CONOUT$`), extensão ≤ 16 B, Cc/Cf → `_`, pontos/espaços finais; testes novos e caso exFAT com surrogate isolado (Task 11) — custo: no pior caso um nome que o Windows recusa (erro de escrita, nunca fuga da pasta).

## Decisões do pre-flight (D1–D18)

Cada linha: decisão — custo se estiver errada. Decididas pelo Claude, pelo Sérgio (ele delegou as escolhas técnicas).

- **D1** A leitura do sistema de arquivos roda dentro do `scan`, antes da escultura (desligável por
  `ScanOptions::filesystems`), só metadados; cada arquivo sai na ordem do commit, no setor do seu primeiro pedaço;
  `Event::Volume` vem antes de tudo — custo: num volume enorme e quebrado, o primeiro `Progress` atrasa (limitado pelos tetos).
- **D2** `Finding` ganha `origin: Option<FsOrigin>` (em vez de um evento novo); a linha JSON só ganha chaves quando há
  origem — custo: todo literal de `Finding` muda.
- **D3** Confiança na posição (`FsFile::trusted`): vivo = cadeia completa, sem ciclo nem cruzamento, `ValidDataLength ==
  DataLength` (diretório: cadeia termina em `End`, sem ciclo/conflito, ≤ teto; conjunto contestado cheio ⇒ nada do volume é confiável); apagado = **um só trecho** de clusters hoje livres (ou o trecho `NoFatChain` gravado no exFAT com todos os
  clusters livres). Sem confiança ⇒ no máximo `Partial` — custo: um arquivo apagado fragmentado, mesmo inteiro, sai `Partial`.
- **D4** Heurística de contiguidade do apagado: clusters livres a partir do primeiro, **pulando** os alocados (o
  alocador do FAT gravou em volta dos arquivos que já existiam); pular ⇒ sem confiança; primeiro cluster hoje alocado ⇒
  o apagado não é listado (foi sobrescrito) — custo: um apagado cuja cabeça foi sobrescrita não volta pelo nome.
- **D5** Mesmo início, mesma foto: vivo ⇒ vale a medida pela cadeia; apagado ⇒ vale a da escultura só se for de saúde
  estritamente melhor (Intact > Partial > Damaged), do mesmo tipo e com `len == size` gravado — custo: um apagado com trailer (MPF) cuja escultura é
  melhor fica com a heurística.
- **D6** Tamanho coerente: o que sobra entre o fim da estrutura e o tamanho gravado tem de ser enchimento (`00`/`FF`) ou
  até 8 arquivos medíveis seguidos (MPF, foto com vídeo anexado); senão no máximo `Partial` — custo: um arquivo com
  trailer desconhecido sai `Partial`.
- **D7** Nada é pulado antes da busca (R10: só um achado `Intact` impede a busca de olhar o que há dentro dele): o
  arquivo vivo de foto/vídeo é medido pela visão no commit e a faixa dele só entra se o achado fundido sair `Intact`; vivo
  de outro tipo (`.THM`, `.XML`) consome só a cabeça no primeiro cluster, e a faixa dela entra se a medição sair `Intact`;
  qualquer achado sem origem viva que toca clusters vivos sai no máximo `Partial` — custo: o desempenho (um `.LRV`/`.XML`
  grande é varrido por cabeças); um sistema de arquivos forjado não esconde mais fotos apagadas.
- **D8** FAT32 é reconhecido pela BPB (`FATSz16 == 0`, `RootEntCnt == 0`), não só pela contagem de clusters — custo: nenhum.
- **D9** Primeira letra de um nome curto apagado: do nome longo (conferida pelo checksum); senão a letra mais comum entre
  nomes curtos com os mesmos bytes 1..4, primeiro na mesma pasta, depois no volume; senão `_` — custo: uma letra errada.
- **D10** FAT32 apagado pelo Windows (palavra alta do cluster zerada): tenta cada palavra alta, aceita o único candidato
  cuja cabeça um formato reconhece com o tipo da extensão (candidatos: o `lo` e cada `h ≥ 1`, desde que livres); até
  65.536 sondas de 512 B (32 MiB, incluída a conferência de 16 B do `lo`) pela fonte crua, fora do cache, recusando faixas
  ruins conhecidas — custo: os ambíguos se perdem, e um apagado com cabeça sobrescrita por ruído pode ganhar o nome de outra
  foto no único candidato; as sondas são exceção ao "lido uma vez".
- **D11** Pastas antigas (formatação rápida) só **dão nome** a achados da escultura (`Updated`), nunca posicionam dados —
  custo: arquivo sem cabeça reconhecível não volta pelas pastas antigas.
- **D12** P17 por `Format::own_files`: um RAW `Partial` (TIFF, RAF, CR3) esconde só os JPEGs que ele mesmo referencia (e um achado escondido que mede `Intact` ainda adiciona a sua faixa pulada); um
  `Damaged` não esconde nada; quadros de MJPEG ficam de fora — custo: quadros de um vídeo MJPEG `Partial` ainda aparecem.
- **D13** Pareamento de vídeo na grade de clusters quando o vídeo começa num cluster de um volume achado; nenhum ponto de
  quebra na grade ⇒ volta à grade de 512 B (regras da 1.3) — custo: um cartão reformatado com outro cluster pareia como antes.
- **D14** Nome: o original (saneado); senão `AAAA-MM-DD_HH-MM-SS_<modelo>.<EXT>` com o modelo do EXIF limpo (sem nomes
  comerciais: `ILCE-7M4`, não `A7IV`); senão `revenant_<início em hex>.<ext>`; a extração é plana (sem subpastas), nomes
  iguais ganham `_2` — custo: nome de câmera técnico; pastas não são recriadas.
- **D15** Sessão = mesma câmera e intervalo ≤ 2 h entre fotos seguidas; item sem número de série entra na câmera se a
  marca+modelo tem um só número de série — custo: dois trabalhos a menos de 2 h se juntam; um almoço longo divide.
- **D16** Hora do vídeo: a do sistema de arquivos (hora local) quando há origem; senão a do `mvhd` (UTC) — custo: vídeo
  só esculpido cai fora da sessão quando o fuso da câmera fica longe do UTC.
- **D17** Conferência com dados reais: teste opcional no macOS com `hdiutil` (imagens em pasta temporária, só as
  próprias; as imagens do testkit montadas só leitura) — custo: o Windows não é conferido contra um driver real.
- **D18** Arquivo de extensão que não é foto/vídeo não vira achado, mas os clusters dele contam como vivos (D7) — custo:
  um JPEG dentro de um arquivo vivo de outro tipo não aparece (o arquivo vivo pode ser copiado normalmente).

## Pendências herdadas com dono 1.4

| Pendência | Destino |
|---|---|
| 1.2: foto gravada exatamente no início de uma prévia/strip de RAW, chunk de CR3 ou extensão de HEIC | Tarefa 7 (arquivo do sistema de arquivos tratado mesmo dentro de faixa pulada; D7); sem metadados continua indistinguível (sem dono) |
| 1.2/1.3: JpgFromRaw de um RAW `Partial` relatado à parte (P17) | Tarefa 9 (D12) |
| 1.2: COUNT/tamanho corrompido num offset válido cortado no limite | Tarefa 7 (a visão termina no tamanho gravado); achado só esculpido segue igual (sem dono: sem decodificador de RAW) |
| 1.3 (C4): vídeos reais remontados saem quase sempre `Partial` | Tarefa 7 (cadeia viva exata) e Tarefa 9 (grade de clusters, D13) |
| 1.3 (Q7, Q7b): foto partida em volta de outra, fotos intercaladas | Tarefa 7 (cadeia viva); apagado fragmentado fica `Partial` (D3) → 1.6 |
| 1.3: foto cortada seguida da cauda de uma quase igual que decodifica até o EOI | Tarefas 7 e 10 (tamanho coerente, D6); sem metadados → 1.6 |
| 1.3 (Q8): primeiro pedaço < 16 KiB, segundo antes do primeiro ou além da janela | Tarefa 7 (cadeia viva) |
| 1.3: cauda órfã de foto da rajada emendada `Intact` | Tarefa 7 (D5: escultura só vale com `len == size`) |
| 1.3: cauda alheia num marcador de restart | Já resolvida pelo Q16; nada a fazer |

Fora desta parte: aparelho bruto, desmontar, cópia do cartão, sessão salva (1.5); 3+ pedaços e fragmentação de RAW (1.6,
que passa a ter o tamanho de cluster de `Volume`); NTFS/APFS/HFS+ (a escultura segue cobrindo); a interface (Módulo 2).

---

### Task 1: Imagens sintéticas de FAT e exFAT no testkit

**Files:**
- Create: `crates/revenant-testkit/src/fs.rs`; Modify: `crates/revenant-testkit/src/lib.rs` (`pub mod fs;`)
- Test: `crates/revenant-testkit/tests/fs_builder.rs` (new; no new dependency)

**Interfaces:**
- Produces (`revenant_testkit::fs`):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum FsType { Fat12, Fat16, Fat32, Exfat }
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum DeleteStyle { Camera, WindowsFat32 }
  /// FAT/exFAT time stamp (2 s resolution); `utc_offset_min` is written only on exFAT (multiple of 15).
  #[derive(Clone, Copy, Debug)] pub struct FsStamp { pub year: u16, pub month: u8, pub day: u8, pub hour: u8,
      pub min: u8, pub sec: u8, pub utc_offset_min: Option<i16> }
  pub struct FsBuilder { .. }
  impl FsBuilder {
      /// Sector 512. Label "REVENANT" (FAT: a volume-label entry in the root and the BPB label; exFAT: a 0x83 entry in the
      /// root), serial 0x1234_5678. FAT12/16: 1 reserved sector, 2 FATs, 512 root entries;
      /// FAT32: 32 reserved sectors, FSInfo at 1, backup boot at 6, 2 FATs, root at cluster 2. exFAT: FAT at sector 128,
      /// 1 FAT, heap aligned to the cluster, bitmap then a 128-entry identity up-case table then the root, boot region
      /// checksums and backup region (sectors 12–23). Panics unless the cluster count fits the type (FAT12 < 4085,
      /// FAT16 4085..=65524, FAT32 ≥ 65525, as the Microsoft spec and macOS require).
      pub fn new(fs: FsType, volume_len: u64, cluster: u32) -> FsBuilder;
      /// Parents created as needed. A directory's cluster chain grows without a cap other than the free clusters (Task 4's
      /// `DCIM/BIG` holds 70,000 entries).
      pub fn mkdir(&mut self, path: &str) -> &mut Self;
      /// `data` in the next free clusters (first fit, ascending); returns its clusters in file order. On exFAT a
      /// contiguous file is NoFatChain. A name that is not an upper-case 8.3 name gets LFN entries (FAT; SFN "XXXXXX~1.EXT").
      pub fn add(&mut self, path: &str, data: &[u8]) -> Vec<u32>;
      /// `data` in exactly `clusters` (file order); FAT chain written; on exFAT FAT-chained unless consecutive.
      pub fn add_at(&mut self, path: &str, data: &[u8], clusters: &[u32]) -> &mut Self;
      /// Like `add` with a raw UTF-16 last component (lone surrogates allowed), in directory `dir`.
      pub fn add_utf16(&mut self, dir: &str, name: &[u16], data: &[u8]) -> Vec<u32>;
      pub fn stamp(&mut self, path: &str, created: FsStamp, modified: FsStamp) -> &mut Self; // default 2025-12-14 07:12:04 both
      /// Camera: FAT ← 0xE5 on byte 0 of the SFN and its LFN entries, chain entries ← 0; exFAT ← InUse bits cleared,
      /// bitmap bits cleared, FAT entries kept. WindowsFat32: as Camera and the SFN's high cluster word ← 0.
      /// A directory: its entries first (recursively), then itself.
      pub fn delete(&mut self, path: &str, style: DeleteStyle) -> &mut Self;
      pub fn raw_entry(&mut self, dir: &str, entry: [u8; 32]) -> &mut Self;  // appended to `dir`'s entries
      pub fn set_fat(&mut self, cluster: u32, value: u32) -> &mut Self;     // FAT entry override (all FAT copies)
      pub fn build(&self) -> Vec<u8>;                      // the whole volume in memory: peak memory = volume length
      /// As after a quick format with the same geometry: fresh boot region(s), FATs zeroed but for the root chain,
      /// empty root at the same first cluster (exFAT: fresh bitmap and up-case table), every other byte untouched.
      pub fn build_quick_formatted(&self) -> Vec<u8>;   // = build() + quick_format_in_place
      pub fn cluster_offset(&self, cluster: u32) -> u64;   // from the volume start
      pub fn cluster_count(&self) -> u32;
      pub fn clusters(&self, path: &str) -> Vec<u32>;      // file order
  }
  pub enum Partitioning { Superfloppy, Mbr { first_lba: u32 }, Gpt { first_lba: u64 } }
  /// Zeros of `disk_len` bytes holding `volume` at the partitioning's offset. MBR: one primary entry, type 0x01 (FAT12),
  /// 0x0E (FAT16), 0x0C (FAT32), 0x07 (exFAT), read from the volume's own boot sector. GPT: protective MBR (0xEE),
  /// header at LBA 1 and entries at LBA 2 (128 × 128 B), backup entries and header at the end, CRC32s set, one Basic
  /// Data partition.
  pub fn disk(p: Partitioning, volume: &[u8], disk_len: usize) -> Vec<u8>;
  /// MBR with `primary` entries `(first_lba, volume, type)` and, when `logical` is not empty, an extended partition
  /// (type 0x0F) after them holding one EBR per logical volume, chained.
  pub fn mbr_disk(primary: &[(u32, &[u8], u8)], logical: &[(u32, &[u8], u8)], disk_len: usize) -> Vec<u8>;
  pub fn crc32(data: &[u8]) -> u32;   // IEEE 802.3, reflected, init/xorout 0xFFFF_FFFF
  /// Quick-formats a FAT or exFAT volume in place, reading its type and geometry from its own boot sector (also used
  /// by Task 10's opt-in test on an image macOS made).
  pub fn quick_format_in_place(volume: &mut [u8]);
  ```
  Field offsets come from the Microsoft specs; the builder is cross-checked against macOS in Task 2, **before** any
  parser is written, so builder and parser cannot share a misreading.

- [ ] **Step 1: Failing tests** (`fs_builder.rs`):
  ```rust
  #[test] fn fat32_layout_is_as_documented() {
      // b = FsBuilder::new(Fat32, 48 << 20, 512); cs = b.add("DCIM/100TEST/DSC00001.JPG", &[7u8; 1500]) → 3 consecutive
      // clusters; v = b.build(): v[510..512] == [0x55, 0xAA]; v[82..90] == *b"FAT32   "; v[6*512..7*512] == v[..512];
      // FAT entry (u32 LE at 32*512 + 4*c): cs[0] → cs[1], cs[1] → cs[2], cs[2] & 0x0FFF_FFFF ≥ 0x0FFF_FFF8;
      // v[b.cluster_offset(cs[0]) as usize..][..512] == [7; 512]
  }
  #[test] fn fat_delete_styles() {
      // Fat32 48 MiB / 512; add_at "DSC00002.JPG" (1500 B) at clusters 70_000..70_003 (high word 1, non-zero), delete(Camera):
      // root entry byte 0 == 0xE5, bytes 1..11 == *b"SC00002JPG ", the 3 FAT entries == 0, bytes 20..22 (high word)
      // == 1u16 LE unchanged; same file with WindowsFat32: bytes 20..22 == [0, 0]
  }
  #[test] fn labels_are_written() {
      // Fat32: a root entry with attr 0x08 and name "REVENANT   "; Exfat: a 0x83 entry in the root with label "REVENANT"
  }
  #[test] fn long_names_get_lfn_entries() {
      // "Long name photo.jpg" → 2 LFN entries (ord 0x42 then 0x01), attr 0x0F, byte 13 == checksum of the SFN's 11 bytes,
      // then SFN "LONGNA~1JPG"; after delete(Camera) all three entries start with 0xE5
  }
  #[test] fn exfat_boot_region_checksums() {
      // Exfat 16 MiB / 32 KiB: v[3..11] == *b"EXFAT   "; every u32 of sector 11 == boot checksum of sectors 0..=10
      // (bytes 106, 107, 112 skipped); v[12*512..24*512] == v[..12*512]
  }
  #[test] fn exfat_entry_sets() {
      // "DCIM/A.JPG" 1500 B: set types [0x85, 0xC0, 0xC1], SetChecksum valid, stream flags == 0x03 (AllocationPossible |
      // NoFatChain), DataLength == ValidDataLength == 1500; after delete(Camera): types [0x05, 0x40, 0x41], the
      // bitmap bit of its cluster 0, FAT entry unchanged
  }
  #[test] fn quick_format_keeps_data_clusters() {
      // 3 files in DCIM/100TEST (Fat32 and Exfat): q = build_quick_formatted(): the root's first cluster holds no file
      // entry; the clusters of DCIM, DCIM/100TEST and of the 3 files are byte-identical in q and build()
  }
  #[test] fn disk_layouts() {
      // Mbr{first_lba: 2048} with a FAT32 volume: byte 446+4 == 0x0C, LBA start 2048, volume at 1 MiB;
      // Gpt{first_lba: 2048}: v[512..520] == *b"EFI PART", header CRC == crc32(header with the CRC field zeroed), entries
      // CRC matches, backup header "EFI PART" at the last LBA; Superfloppy: volume at 0; crc32(b"123456789") == 0xCBF4_3926
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** `cargo test -p revenant-testkit`, clippy, fmt.
- [ ] **Step 5:** Commit: `test(testkit): synthetic FAT and exFAT volumes, MBR and GPT disks`.

---

### Task 2: Conferência opcional com o driver do macOS

**Files:**
- Create: `crates/revenant-testkit/src/os_fs.rs`; Modify: `crates/revenant-testkit/src/lib.rs`
- Test: `crates/revenant-testkit/tests/os_builders.rs` (new)
- Docs: `crates/revenant-dev/README.md` (new section "Checking against real file systems (macOS, opt-in)")

**Interfaces:**
- Produces (`revenant_testkit::os_fs`):
  ```rust
  #[derive(Clone, Copy, Debug)] pub enum OsFs { Fat16, Fat32, Exfat }      // hdiutil -fs "MS-DOS FAT16" | "MS-DOS FAT32" | ExFAT
  #[derive(Clone, Copy, Debug)] pub enum OsLayout { Mbr, Gpt, None }        // -layout MBRSPUD | GPTSPUD | NONE
  /// true only on macOS with REVENANT_OS_FS=1 and /usr/bin/hdiutil present. Tests return early (printing "skipped:
  /// <reason>") when false. Nothing is ever installed.
  pub fn available() -> bool;
  pub struct OsImage { pub path: std::path::PathBuf, _dir: tempfile::TempDir }
  impl OsImage {
      /// `hdiutil create -size <n>m -fs .. -volname RVT -layout .. <tmp>/card.dmg` (a raw UDRW image: plain sector bytes,
      /// checked on the dev Mac). None when `available()` is false or the command fails.
      pub fn create(fs: OsFs, layout: OsLayout, size_mib: u32) -> Option<OsImage>;
      /// Attaches read-write (`hdiutil attach -nobrowse -noverify`), runs `f` with the mount point, then always detaches
      /// (drop guard). Panics unless the mount point printed by hdiutil starts with "/Volumes/RVT".
      pub fn with_mounted<R>(&self, f: impl FnOnce(&std::path::Path) -> R) -> R;
  }
  /// Attaches an existing raw image file read-only (`-readonly -imagekey diskimage-class=CRawDiskImage`), runs `f` with
  /// the mount point, detaches.
  pub fn with_raw_mounted<R>(image: &std::path::Path, f: impl FnOnce(&std::path::Path) -> R) -> R;
  /// `fsck_msdos -n` / `fsck_exfat -n` on the attached (not mounted) image's partition. None if the binary is missing or
  /// the check cannot run without privileges (reported, not failed).
  pub fn fsck(image: &std::path::Path, fs: OsFs) -> Option<bool>;
  ```
  Safety rules (in the module doc and asserted): the only device passed to `hdiutil detach` is the one `hdiutil attach`
  printed for an image under the test's temp dir; no `diskutil`, no `sudo`, no other disk. Ignore names starting with `.`
  when listing (macOS adds `.fseventsd` and friends).

- [ ] **Step 1: Failing test** (`os_builders.rs`, `#[ignore]`):
  ```rust
  #[test] #[ignore] fn builder_images_mount_on_macos() {
      // if !available() { eprintln!("skipped: …"); return }
      // for (fs, len, cluster) in [(Fat12, 2 MiB, 1024), (Fat16, 16 MiB, 2048), (Fat32, 48 MiB, 512), (Exfat, 16 MiB, 32 KiB)]:
      //   mkdir DCIM/100TEST; add DSC00001.JPG (testkit jpeg 320×240), "Long name photo.jpg", DSC00003.JPG via add_at in
      //   2 runs, DSC00004.JPG then delete(Camera); stamp DSC00001.JPG modified 2025-12-14 07:12:06;
      //   image = disk(Mbr{first_lba: 2048}, ..) saved to a temp file; with_raw_mounted:
      //   read_dir(DCIM/100TEST) names (sorted, '.'-names ignored) == ["DSC00001.JPG", "DSC00003.JPG", "Long name photo.jpg"];
      //   each file's bytes == the data added; DSC00001.JPG's modified time (local) == 2025-12-14 07:12:06;
      //   fsck(image, ..) is Some(true) or None (FAT12 is checked with the FAT16 tool)
  }
  ```
- [ ] **Step 2:** FAIL to compile (no `os_fs`); once it compiles, without the env var it passes as "skipped" (record it).
  **Step 3:** implement. **Step 4:** `REVENANT_OS_FS=1 cargo test -p revenant-testkit --test os_builders -- --ignored`
  → PASS on the dev Mac; any mismatch is a builder bug (fix Task 1's builder and its test, not this one). clippy, fmt.
- [ ] **Step 5:** dev README section (how to run, what it checks, that it only touches its own temp images, why the
  CLAUDE.md "no real disks" rule still holds). Commit: `test(testkit): opt-in check of the FAT/exFAT builders against macOS`.

---

### Task 3: Volumes — partições, setor de boot e cópia

**Files:**
- Create: `crates/revenant-core/src/filesystem/{mod.rs,partitions.rs}`; Modify: `src/lib.rs` (`pub mod filesystem;`)
- Test: `crates/revenant-core/tests/fs_volumes.rs` (new), `crates/revenant-core/tests/os_filesystems.rs` (new, opt-in)
- Docs: core README (new section "File systems: volumes")

**Interfaces:**
- Produces (`revenant_core::filesystem`):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)] pub enum FsKind { Fat12, Fat16, Fat32, Exfat }  // as_str: "fat12"…"exfat"
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Scheme { None, Mbr, Gpt }                    // as_str: "none" | "mbr" | "gpt"
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct Volume {
      pub index: u32,           // position in offset order, from 0
      pub fs: FsKind,
      pub offset: u64,          // absolute byte offset of the volume's first sector
      pub len: u64,             // bytes, as the boot sector says (may run past the source's end)
      pub sector_size: u32,
      pub cluster_size: u64,
      pub heap_offset: u64,     // absolute byte offset of cluster 2
      pub cluster_count: u32,   // clusters 2 ..= cluster_count + 1; heap end = heap_offset + cluster_count × cluster_size
      pub label: Option<String>,
      pub serial: u32,
      pub from_backup: bool,    // the primary boot sector was unusable; the backup was used
      pub scheme: Scheme,
  }
  impl Volume { pub fn cluster_offset(&self, c: u32) -> Option<u64>; }  // None outside 2..=cluster_count + 1
  /// Every FAT/exFAT volume of the source, in offset order (at most 16). Reads at most a few hundred sectors.
  pub fn volumes(source: &dyn Source) -> Vec<Volume>;
  ```
  Also crate-private `pub(crate) fn crc32(data: &[u8]) -> u32` in `filesystem/mod.rs`.

Algorithm (the tests do not pin the order of attempts):
```
s0 = sector 0. cands = []
if s0 is a valid boot sector (FAT or exFAT rules below)       → cands += (0, None)
else if s0[510..512] == 55 AA:
    if an entry has type 0xEE → GPT: header "EFI PART" at LBA 1 (else at byte 4096, for 4Kn), header CRC32 checked; on
       failure the backup header at its AlternateLBA (or the last LBA). Entries: count ≤ 128, size a multiple of 128 in
       [128, 1024], entries CRC32 checked; every entry with a non-zero type GUID and first ≤ last LBA → cands (Gpt)
    else MBR: each of the 4 entries with type ≠ 0, start ≠ 0, size ≠ 0; types 0x05/0x0F/0x85 → EBR chain (≤ 64 links,
       links relative to the extended start, a link already seen or outside the source stops it) → cands (Mbr)
for each cand offset o (inside the source): parse(o)
if no volume came out: probe LBAs [0, 63, 128, 2048, 8192, 32768, 65536] (inside the source) → parse (Scheme::None)
parse(o): primary boot at o; if invalid → exFAT backup at o + 12 × S, FAT backup at o + 6 × S, for S in [512, 4096]
          (from_backup = true); an exFAT primary whose fields are valid but whose boot checksum (sector 11) does not
          match is replaced by a valid backup when there is one.
dedup by offset, sort, keep 16, index = position.
```
Valid FAT boot: `55 AA`; jump `EB ?? 90` or `E9`; BytsPerSec ∈ {512, 1024, 2048, 4096}; SecPerClus a power of 2 ≤ 128;
RsvdSecCnt ≥ 1; NumFATs ∈ {1, 2}; FAT32 by BPB (D8: `FATSz16 == 0`, `RootEntCnt == 0`, `FATSz32 > 0`, `RootClus ≥ 2`),
else FAT12/FAT16 by cluster count (< 4085 / < 65525); data region start < total sectors; cluster count ≥ 1. Valid exFAT
boot: `"EXFAT   "` at 3, bytes 11..64 zero, BytesPerSectorShift 9..=12, SectorsPerClusterShift ≤ 25 − it, NumberOfFats ∈
{1, 2}, FatOffset ≥ 24, ClusterHeapOffset ≥ FatOffset + FatLength × NumberOfFats, ClusterCount ≥ 1 and ≤ 2^32 − 11,
heap end ≤ VolumeLength, root cluster in range, `55 AA` (a test that needs a huge `ClusterCount`, as Task 6's, patches
`FatLength`, `ClusterHeapOffset` and `VolumeLength` coherently; the rules are not loosened for it). Label: exFAT `0x83` entry in the root's first cluster; FAT: the
volume-label entry in the root's first 16 entries, else the BPB label; `"NO NAME"` → None.

- [ ] **Step 1: Failing tests** (`fs_volumes.rs`):
  ```rust
  #[test] fn finds_the_volume_of_each_layout() {
      // for (fs, len, cluster) in [(Fat12, 2 MiB, 1024), (Fat16, 16 MiB, 2048), (Fat32, 48 MiB, 512), (Exfat, 16 MiB, 32 KiB)]
      // × [Superfloppy, Mbr{8192}, Gpt{2048}] (disk = volume + 8 MiB): volumes(&src) == [Volume { index: 0, fs, offset:
      // 0 | 8192*512 | 2048*512, len, sector_size: 512, cluster_size: cluster, heap_offset: offset + b.cluster_offset(2),
      // cluster_count: b.cluster_count(), label: Some("REVENANT"), serial: 0x1234_5678, from_backup: false, scheme }]
  }
  #[test] fn damaged_primary_boot_sector_uses_the_backup() {
      // FAT32 and exFAT in Mbr{8192}: volume's first 512 bytes zeroed → same Volume with from_backup: true;
      // exFAT with byte 512*3 + 7 flipped (checksum mismatch only) → from_backup: true
  }
  #[test] fn destroyed_partition_table_is_probed() { /* Mbr{8192} with sector 0 zeroed → one volume at 8192*512, scheme None */ }
  #[test] fn several_partitions_and_an_extended_chain() {
      // mbr_disk(primary: [(2048, fat32, 0x0C), (2048 + fat32 sectors, exfat, 0x07)], logical: [fat16 a, fat16 b]) →
      // 4 volumes in offset order, index 0..=3, kinds [Fat32, Exfat, Fat16, Fat16]
  }
  #[test] fn hostile_partition_tables_are_bounded() {
      // each case < 1 s, no panic, ≤ 16 volumes: an EBR linking to itself; an EBR chain of 1000 links (≤ 64 followed);
      // GPT with 0xFFFF_FFFF entries of size 128 and of size 0; MBR entries starting past the source end; boot sectors
      // with SecPerClus 0 and 3, FatOffset past the volume, ClusterCount 0xFFFF_FFFF, BytesPerSectorShift 31 → no volume
      // from those; 20 valid FAT16 volumes in logical partitions → exactly 16
  }
  #[test] fn noise_and_photos_are_not_volumes() { /* 16 MiB noise (seed 3) with testkit jpeg at 0 → volumes == [] */ }
  // os_filesystems.rs (opt-in, #[ignore], returns early unless testkit::os_fs::available())
  #[test] #[ignore] fn os_made_volumes_are_found() {
      // OsImage::create(Fat32, Mbr, 64), (Exfat, Gpt, 64), (Fat16, None, 32): exactly one volume each, fs as made, offset ==
      // 512 × the first LBA the test reads itself from the MBR entry / GPT entry (0 for None), label Some("RVT")
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; the opt-in test with `REVENANT_OS_FS=1`.
- [ ] **Step 5:** core README ("File systems: volumes": layouts, backups, probes, bounds), CHANGELOG ("Added": FAT and
  exFAT volumes are found on MBR, GPT and unpartitioned media, also with a damaged boot sector). Commit:
  `feat(core): find FAT and exFAT volumes, with backup boot sectors`.

---

### Task 4: FAT — árvore viva, nomes longos, cadeias

**Files:**
- Create: `crates/revenant-core/src/filesystem/{fat.rs,alloc.rs}`; Modify: `filesystem/mod.rs`, `src/formats/mod.rs`
  (`FileKind::from_extension`)
- Test: `crates/revenant-core/tests/fs_fat.rs` (new), unit tests in `alloc.rs`
- Docs: core README ("File systems: FAT")

**Interfaces:**
- Consumes: `Volume`, `volumes` (Task 3).
- Produces (`revenant_core::filesystem`, used by every later task):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum FsState { Live, Deleted, Orphaned }     // as_str: "live" | "deleted" | "orphaned"
  /// How the pieces were placed: Chain = FAT chain or exFAT NoFatChain of a live file; Recorded = exFAT NoFatChain run
  /// of a deleted file; Guessed = deleted-file heuristic (D4); Carved = the scan's own measurement (D5, Task 10).
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Placement { Chain, Recorded, Guessed, Carved } // as_str lowercase
  /// "YYYY-MM-DDTHH:MM:SS" as recorded (local time); Display appends "+HH:MM"/"-HH:MM" when the offset is known.
  #[derive(Clone, Debug, PartialEq, Eq)] pub struct FsTime { pub local: String, pub utc_offset_minutes: Option<i16> }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct FsOrigin {
      pub volume: u32,
      /// Components joined by '/', root-relative, no leading '/'; "?" for a directory whose name is unknown.
      pub path: String,
      pub state: FsState,
      pub placement: Placement,
      pub size: u64,                 // as recorded in the entry
      pub created: Option<FsTime>,
      pub modified: Option<FsTime>,
  }
  impl FsOrigin { pub fn name(&self) -> &str; /* last component */ pub fn folder(&self) -> &str; /* "" for the root */ }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct FsFile {
      pub origin: FsOrigin,
      pub kind: FileKind,            // from the extension
      /// Absolute pieces in file order, sum = min(size, what the clusters hold and the source holds). The whole
      /// listing holds at most `MAX_PIECES` (1 << 20) pieces: a file that would pass it is not listed.
      pub pieces: Vec<Fragment>,
      pub trusted: bool,             // D3
  }
  #[derive(Clone, Debug, Default, PartialEq, Eq)]
  pub struct Listing {
      pub volumes: Vec<Volume>,
      /// Photo/video files only (extension known to `FileKind::from_extension`), sorted by (first piece start, volume, path).
      pub files: Vec<FsFile>,
      /// Merged absolute ranges of every live file and live directory of any kind (D7 cap, `keep_moovs`, orphans, D18);
      /// the fixed FAT12/16 root region is not in it.
      pub live: Vec<Fragment>,
      /// First piece of every live file that is not a photo/video (any other extension), sorted by start: the scan
      /// consumes the head there without a finding (Task 7). There is no pre-skip list: nothing is skipped before the
      /// search (R10).
      pub others: Vec<Fragment>,
  }
  /// Lists the files of every volume (cancel checked per directory). Never fails.
  pub fn read(source: &dyn Source, cancel: &CancelToken) -> Listing;
  // formats/mod.rs
  impl FileKind { /// jpg jpeg jpe → Jpeg; mp4 m4v → Mp4; mov → Mov; cr2; cr3; nef nrw → Nef; arw; dng; orf ori → Orf; rw2;
                  /// raf; heic heif hif → Heic; tif tiff → Tiff; case-insensitive; anything else None.
      pub fn from_extension(ext: &str) -> Option<FileKind>; }
  // alloc.rs (crate-private, shared with Tasks 5 and 6)
  pub(crate) enum Entry { Free, Next(u32), End, Bad, Invalid }
  pub(crate) trait ClusterMap { fn entry(&mut self, c: u32) -> Entry; fn allocated(&mut self, c: u32) -> Option<bool>; }
  pub(crate) struct Claims { .. }   // bitmap of clusters claimed by live chains + contested set (≤ 65,536)
  pub(crate) struct Chain { pub runs: Vec<(u32, u32)>, pub complete: bool, pub conflict: bool }
  /// Follows the chain from `first` for `need` clusters, claiming each; stops at End, an invalid/free/bad entry, a
  /// cluster out of range or already claimed (conflict), or 4096 runs.
  pub(crate) fn follow(map: &mut dyn ClusterMap, claims: &mut Claims, first: u32, need: u64) -> Chain;
  ```
FAT walk rules: root = FAT12/16 fixed region, FAT32 `RootClus` chain; entries of 32 B; `name[0] == 0x00` ends the directory;
`attr == 0x0F` is an LFN part (13 UCS-2 chars at 1, 14, 28; ord & 0x3F, 0x40 = last; byte 13 = SFN checksum); `.`/`..`
skipped; volume label skipped; SFN = 8 + 3 trimmed, joined by `.` when the extension is not empty, NT lower-case bits
(byte 12: 0x08 base, 0x10 extension) applied, `0x05` first byte → `0xE5`, bytes outside 0x20..=0x7E → `_`. A live LFN is
used only if its ordinals run down to 1 right before the SFN and every checksum matches; text via
`String::from_utf16_lossy` up to the first 0x0000. First cluster: FAT32 `hi << 16 | lo`, else `lo`. Times: created =
date@16, time@14, tenths@13 (÷100 s added, floored), modified = date@24, time@22; invalid fields → None. FAT entries:
FAT12 packed 12 bits, FAT16 16, FAT32 low 28; EOC ≥ 0xFF8 / 0xFFF8 / 0x0FFF_FFF8, bad 0xFF7 / 0xFFF7 / 0x0FFF_FFF7; an
unreadable FAT sector is read from the next FAT copy. Directories claim their clusters; a directory whose first cluster
is already claimed is not entered (loop). Every live file (any extension) claims its chain; afterwards each file whose
pieces hold a contested cluster gets `trusted = false`. A file is `trusted` iff its chain is complete (exactly
`ceil(size / cluster)` clusters, the last one `End`), no conflict, ≤ 4096 runs. Size-0 files and pieces past the source
end are dropped. A directory is trusted when its chain ends in `End`, has no cycle or conflict and is within the cap
(exFAT `NoFatChain`: its whole `DataLength` claimed and marked in the bitmap); an untrusted directory is still walked.
When the contested set (≤ 65,536) fills, the volume is marked and every live file of it is untrusted. In a path
component read from the disk, `/` and NUL become `_`.

- [ ] **Step 1: Failing tests** (`fs_fat.rs`; `card()` = FAT32 48 MiB / 512 in `Mbr{8192}` + 8 MiB):
  ```rust
  #[test] fn lists_live_files_with_paths_names_and_times() {
      // DCIM/100MSDCF/DSC01234.ARW (testkit tiff_raw Arw, raw_len 200_000), DCIM/100MSDCF/DSC01235.JPG (jpeg 640×480),
      // PRIVATE/M4ROOT/CLIP/C0001.MP4 (testkit mp4, mdat 300_000), MISC/notes.txt, "DCIM/100MSDCF/Long name photo.jpg";
      // stamp(ARW, created 2025-12-14 07:12:04, modified 2025-12-14 07:12:06)
      // read(&src, &cancel): files.len() == 4 (no notes.txt), sorted by first piece start; the ARW: origin.path ==
      // "DCIM/100MSDCF/DSC01234.ARW", name() == "DSC01234.ARW", folder() == "DCIM/100MSDCF", state Live, placement Chain,
      // size == bytes.len(), kind Arw, pieces == [Fragment { start: 8192*512 + b.cluster_offset(first), len: size }], trusted;
      // created == Some(FsTime { local: "2025-12-14T07:12:04".into(), utc_offset_minutes: None }), modified "…07:12:06";
      // the long name survives; live ⊇ every file's pieces (notes.txt's too); others == [notes.txt's first run]
  }
  #[test] fn fat12_and_fat16_roots_are_read() { /* the same 3 photo paths on Fat12 2 MiB/1024 and Fat16 16 MiB/2048 */ }
  #[test] fn fragmented_live_file_follows_its_chain() {
      // a 40_000-byte jpeg via add_at(clusters 600..640 ++ 300..330 ++ 900..920) → pieces == those 3 runs in file order as
      // bytes, the last cut so the sum == 40_000; trusted
  }
  #[test] fn hostile_directory_loop_and_chain_cycle_are_bounded() {
      // raw_entry in DCIM/100: "LOOP" attr 0x10 → root cluster, "SELF" attr 0x10 → DCIM/100's own cluster;
      // file A (5 clusters) with set_fat(a[2], a[1]) (cycle); B and C with set_fat(c[last − 1], b[last]) (cross-link);
      // DCIM/BIG whose chain holds 70,000 entries' worth of clusters → read() < 5 s; no path contains "LOOP/" or "SELF/";
      // every DCIM/100 file once; A, B, C trusted == false; DCIM/BIG parsed up to 65,536 entries
  }
  #[test] fn fat_mirror_covers_an_unreadable_fat_sector() { /* source failing the first FAT's sector that holds the ARW's chain → same pieces, trusted */ }
  #[test] fn invalid_dates_are_none() { /* raw entry with month 13 and hour 25 → created None, modified None */ }
  ```
  More tests (`fs_fat.rs`): `contested_overflow_makes_the_volume_untrusted` (contested cap lowered by `cfg(test)` twin;
  overflow ⇒ every live file of the volume `trusted == false`), `piece_cap_bounds_the_listing` (`MAX_PIECES` lowered by
  `cfg(test)`: files past it are not listed, no panic), `hostile_names_with_slash_and_nul_are_flattened` (an LFN with `/`
  and NUL in a component ⇒ `name()`/`folder()` do not split it), `directories_are_trusted_only_with_a_complete_chain`.
  `alloc.rs` unit: `follow_stops_on_cycles_and_conflicts` (a map with 2→3→2: complete false; a second chain claiming 3:
  conflict true; 5000 one-cluster runs: stops at 4096 runs).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README ("File systems: FAT": what is listed, trust, bounds). Commit:
  `feat(core): list live FAT files with names, folders, times and cluster chains`.

---

### Task 5: FAT — arquivos e pastas apagados

**Files:**
- Modify: `crates/revenant-core/src/filesystem/{mod.rs,fat.rs,alloc.rs}`
- Test: `crates/revenant-core/tests/fs_fat.rs`, unit tests in `alloc.rs`; opt-in `tests/os_filesystems.rs`
- Docs: core README ("File systems: FAT", deleted files)

**Interfaces:**
- Produces (`alloc.rs`, used by Task 6):
  ```rust
  pub(crate) struct Guess { pub runs: Vec<(u32, u32)>, pub skipped: bool, pub short: bool }
  /// Per-volume budget of cluster examinations (`MAX_GUESS_STEPS = 1 << 26`, `cfg(test)` twin), shared by `guess`, the old
  /// exFAT chain and the bitmap check; exhausted ⇒ the caller lists no more deleted files of that volume (walk order).
  pub(crate) struct Budget { .. }
  /// D4. None when `first` is out of range or allocated (overwritten), or the budget is exhausted.
  pub(crate) fn guess(map: &mut dyn ClusterMap, budget: &mut Budget, first: u32, need: u64, last: u32) -> Option<Guess>;
  // filesystem/mod.rs: `pub fn read(source, cancel)` (tests) calls
  /// `meta` reads metadata (the scan passes its `CachedSource`); `probe` reads 512 B from the RAW source and refuses,
  /// without reading, any range the cache's bad-sector map already knows. A read error in a probe = "does not recognize".
  pub(crate) fn read_with(meta: &dyn Source, probe: &dyn Probe, cancel: &CancelToken) -> Listing;
  pub(crate) trait Probe { fn read512(&self, at: u64) -> Option<[u8; 512]>; }   // `read` uses a Probe over `source` itself
  ```
Heuristic (D4), exactly:
```
need = ceil(size / cluster); c = first; taken = 0; examined = 0; runs = []; skipped = false
if allocated(first) != Some(false) → None
while taken < need && c ≤ last && examined < 2 × need + 1024:
    examined += 1; budget.spend(1) (exhausted ⇒ return None)
    if allocated(c) == Some(false): extend the last run if it ends at c, else push (c, 1); taken += 1
    else if !runs.is_empty(): skipped = true
    c += 1
short = taken < need; runs capped at 4096 (cap ⇒ short)
```
Deleted entries (`name[0] == 0xE5`, SFN or LFN): placement Guessed; `trusted = runs.len() == 1 && !short`. First-letter
restore (D9): if the preceding deleted LFN entries (attr 0x0F, same checksum byte, taken nearest first, ≤ 20) give a
name whose first char upper-cased (ASCII) makes the SFN's checksum match, use it and the LFN text; else the most common
first byte among SFNs with equal bytes 1..4 — live or already restored — in the same directory, then in the whole
volume (ties: lowest byte); else `_`. Deleted directories are walked like live ones through `guess` (their entries are
Deleted whatever their own byte 0 says); they never claim. FAT32 high word (D10): a deleted SFN whose high word is 0 has the candidates `{lo}` (when a format recognizes
the head there with the extension's kind and the cluster is free) ∪ `{h ≥ 1 : (h << 16) | lo ≤ last}` (same condition on
`(h << 16) | lo`), at most `last >> 16` per entry. Every head check is a `Probe` read of 512 B (the 16 B check of `lo`
included) and counts against one cap of 65,536 per `read`; when the cap is spent no more candidates are tried. Exactly
one candidate → used; none or several → not listed. A cluster's range refused by the probe (known bad sector) is "does
not recognize". Among
several entries (live or deleted) with the same first cluster, the listing keeps them all; Task 7 picks one.

- [ ] **Step 1: Failing tests** (`fs_fat.rs`):
  ```rust
  #[test] fn deleted_file_is_listed_contiguous() {
      // DSC01234.JPG, DSC01235.JPG, DSC01236.JPG (jpeg 640×480 seeds 1–3) back to back in DCIM/100MSDCF; delete
      // DSC01235.JPG (Camera) → listed: path "DCIM/100MSDCF/DSC01235.JPG", state Deleted, placement Guessed, one piece at
      // its first cluster, len == size, trusted
  }
  #[test] fn first_character_comes_from_the_lfn_or_siblings() {
      // "Long name photo.jpg" deleted → name "Long name photo.jpg"; DSC01235.JPG deleted next to live DSC01234/DSC01236 →
      // "DSC01235.JPG"; Q1234567.JPG deleted alone in MISC/ with no SFN sharing "123" → "_1234567.JPG"
  }
  #[test] fn deleted_file_never_takes_live_clusters() {
      // A = 5 clusters c..=c+4, deleted; live L written by add_at into c+2 → A: pieces == clusters {c, c+1} and
      // {c+3, c+4, c+5} (skipping c+2), trusted false; no piece of A overlaps L's cluster;
      // a deleted B whose first cluster is then given to a live file → B not listed
  }
  #[test] fn windows_deleted_file_finds_its_high_word() {
      // FAT32 48 MiB/512 (≈ 96k clusters): jpeg at cluster 70_000 (add_at), delete(WindowsFat32) → listed at cluster
      // 70_000; a second one at 70_100 (deleted, WindowsFat32) whose cluster 70_100 − 65_536 = 4_564 holds a FREE testkit JPEG
      // head (raw bytes, no entry), so both `lo`-candidates recognize ⇒ not listed because of the ambiguity (not D4)
  }
  #[test] fn high_word_probes_are_capped() {
      // probe cap lowered to 4 by its cfg(test) twin: 5 deleted WindowsFat32 files → the 5th is not listed, no panic;
      // MultiFailingSource counts ≤ 4 × 512 B read outside the cache windows, and none in a range already known bad
  }
  #[test] fn hostile_deleted_entries_are_bounded() {
      // 10,000 deleted entries of size 4 GiB all pointing at the same free cluster → read() < 5 s, budget exhausted,
      // no panic, listed files all valid
  }
  #[test] fn deleted_directory_contents_are_listed() {
      // DCIM/100TEST with DSC00001.JPG, DSC00002.JPG; live DCIM/101TEST/DSC00010.JPG; delete DCIM/100TEST (Camera) →
      // "DCIM/_00TEST/DSC00001.JPG" and "DCIM/_00TEST/DSC00002.JPG", both Deleted
  }
  // os_filesystems.rs (opt-in)
  #[test] #[ignore] fn os_made_fat_lists_live_and_deleted() {
      // OsImage::create(Fat32, Mbr, 64) and (Fat16, None, 32); mounted: DCIM/100TEST with 6 testkit photos (one "Long
      // name photo.jpg"); delete 2 of them; detach → read(): every live file with its path and pieces whose bytes equal
      // the file read through the mount (read before detaching); each deleted one listed, Deleted, its name equal up to
      // the first character (LFN names exactly)
  }
  ```
  `alloc.rs` unit: `guess_follows_the_rules` (free map, one allocated cluster in the middle, first allocated → None,
  `examined` bound with all allocated after `first` → short).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; opt-in test.
- [ ] **Step 5:** core README (deleted files, D4, D9, D10 and its up to 65,536 reads of 512 B (32 MiB) outside the cache). Commit:
  `feat(core): list deleted FAT files and folders with their likely clusters`.

---

### Task 6: exFAT

**Files:**
- Create: `crates/revenant-core/src/filesystem/exfat.rs`; Modify: `filesystem/mod.rs`
- Test: `crates/revenant-core/tests/fs_exfat.rs` (new); opt-in `tests/os_filesystems.rs`
- Docs: core README ("File systems: exFAT")

**Interfaces:**
- Consumes: `ClusterMap`, `Claims`, `follow`, `guess`, `Budget` (Tasks 4–5); `FsFile`, `Listing` (Task 4).
- Produces: `read` lists exFAT volumes too (no new public item).

Rules: the root (FAT chain from `FirstClusterOfRootDirectory`) holds `0x81` (bitmap: FirstCluster@20, DataLength@24 ≥
`ceil(count / 8)`), `0x82`, `0x83`. A file entry set = `0x85`/`0x05` with SecondaryCount (byte 1) in 2..=18, then a
`0xC0`/`0x40` stream entry, then `ceil(NameLength / 15)` `0xC1`/`0x41` name entries (other secondary types are skipped
inside the count). SetChecksum (bytes 2..4 of the primary) over all `SecondaryCount + 1` entries, skipping bytes 2–3,
with every type byte's InUse bit (0x80) **set** first (a deleted set keeps the checksum of its live form); mismatch → the
set is skipped. A set is Deleted when its primary's InUse bit is clear, or an ancestor is deleted. Stream: flags@1
(bit 0 AllocationPossible, bit 1 NoFatChain), NameLength@3 (1..=255), ValidDataLength@8, FirstCluster@20, DataLength@24.
Times: DOS u32 at 8 (created) and 12 (modified), 10 ms bytes at 20/21, UTC offsets at 22/23 (bit 7 valid, bits 0–6
signed 15-minute units) → `utc_offset_minutes`. Directories: attr 0x10, DataLength = directory size (≤ 256 MiB). Live:
NoFatChain → one run of `ceil(DataLength / cluster)` clusters (claimed; any cluster the bitmap shows free ⇒ untrusted);
else `follow`. Deleted: NoFatChain → Recorded, the run as recorded, `trusted` iff every cluster is free now; first
cluster allocated → not listed; FAT-chained → the old chain if it has exactly `need` clusters, ends at End and every
cluster is free (placement Guessed, trusted iff one run), else `guess`. `ValidDataLength < DataLength` ⇒ untrusted.
No readable bitmap (missing, unreadable, too short) ⇒ `allocated()` answers from the live claims only and every deleted
file is untrusted. `0x00` ends a directory. The old FAT chain, the `guess` and the all-clusters-free bitmap check of a deleted file spend the
volume's `Budget` (`MAX_GUESS_STEPS`); exhausted ⇒ the following deleted files are not listed. Names are decoded with
`String::from_utf16_lossy` and `/` and NUL in a component become `_`. A directory is trusted by Task 4's rule.

- [ ] **Step 1: Failing tests** (`fs_exfat.rs`; `card()` = Exfat 16 MiB / 32 KiB in `Mbr{8192}` + 8 MiB):
  ```rust
  #[test] fn lists_exfat_files_with_utc_offsets() {
      // DCIM/100MSDCF/DSC01234.ARW (tiff_raw Arw raw_len 200_000), DCIM/100MSDCF/C0001.MP4 (mp4), "Fotos da corrida/IMG
      // 0001.JPG"; stamp(ARW, created 2025-12-14 07:12:04 −180 min, modified 07:12:06 −180)
      // → those 3 paths; the ARW Live, Chain, one piece, trusted, modified == Some(FsTime { local: "2025-12-14T07:12:06",
      // utc_offset_minutes: Some(-180) }) and its Display == "2025-12-14T07:12:06-03:00"
  }
  #[test] fn fat_chained_exfat_file_follows_the_fat() { /* add_at 2 non-consecutive runs → pieces == the 2 runs, trusted */ }
  #[test] fn deleted_exfat_files() {
      // contiguous file deleted → Deleted, Recorded, one piece, trusted; 2-run file deleted (FAT kept) → Guessed, the
      // 2 runs, trusted false; a deleted file whose first cluster was given to a live file → not listed
  }
  #[test] fn reused_clusters_make_a_deleted_file_partial() {
      // deleted NoFatChain A (4 clusters); live L add_at into A's 3rd cluster → A listed with its 4 recorded clusters,
      // trusted false
  }
  #[test] fn broken_entry_sets_are_skipped() {
      // raw sets with a wrong SetChecksum, SecondaryCount 1 and 19, NameLength 0, NameLength 40 with 1 name entry, a
      // 0x85 followed by a 0xC1 → none listed; the 2 good files around them are
  }
  #[test] fn hostile_exfat_is_bounded() {
      // each < 5 s, no panic: a directory whose FAT chain cycles; a subdirectory entry pointing at the root; root
      // DataLength 2^40 with a 3-cluster chain; bitmap DataLength 2^40 (→ no bitmap: deleted files untrusted);
      // ClusterCount 2^30 with FatLength, ClusterHeapOffset and VolumeLength patched coherently (VolumeLength past the
      // source's end) and the boot checksum redone (→ volume listed, no files); 10,000 deleted NoFatChain sets of 4 GiB
      // on one free cluster (bitmap check) < 5 s
  }
  #[test] fn valid_data_length_short_of_data_length_is_not_trusted() { /* raw set with VDL = DL − 4096 → trusted false */ }
  // os_filesystems.rs (opt-in)
  #[test] #[ignore] fn os_made_exfat_lists_live_and_deleted() { /* as the FAT one, with (Exfat, Mbr, 64) and (Exfat, Gpt, 64) */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; opt-in test.
- [ ] **Step 5:** core README ("File systems: exFAT"), CHANGELOG ("Added": FAT12/16/32 and exFAT file listing with
  names, folders and dates, deleted files included). Commit: `feat(core): list live and deleted exFAT files`.

---

### Task 7: A busca com os sistemas de arquivos — origem, mesma foto uma vez

**Files:**
- Create: `crates/revenant-core/src/scan/fs.rs` (the merge rules, pure functions); Modify: `src/scan/{mod.rs,pipeline.rs}`,
  `src/lib.rs` (re-exports), `src/pairing.rs` (literals), `src/inspect.rs` (literal), `src/filesystem/mod.rs` (the scan calls
  `read_with` with a raw `Probe`), `crates/revenant-dev/src/main.rs` (match arm, literals only)
- Test: `crates/revenant-core/tests/scan_fs.rs` (new), unit tests in `scan/fs.rs`; every `Finding {..}` literal in
  `crates/` (`grep -rn "Finding {" crates`, including `tests/{scan,pairing,inspect,extract}.rs`) gets `origin: None`
- Docs: core README ("Scan": file-system findings, "Deduplication"), CHANGELOG

**Interfaces:**
- Consumes: `filesystem::{read, Listing, FsFile, FsOrigin, Volume}` (Tasks 3–6), `Fragmented` (1.3).
- Produces:
  ```rust
  // scan: Finding gains
  /// Where the file system says this file is, when it does (name, folder, state, dates). None for a file found only by
  /// its signature.
  pub origin: Option<FsOrigin>,
  // Event gains, emitted before the first Progress, one per volume in index order:
  Volume(Volume),
  // ScanOptions gains `pub filesystems: bool` (default true; false = the 1.3 scan, no Volume events, no origins).
  // Docs on ScanOptions: the memory sentence gains "the file-system listing: up to 262,144 files and 2^20 pieces (≈ 60 MiB)";
  // `sector > 512` silently drops file-system files whose first piece is not sector-aligned.
  // lib.rs: pub use filesystem::{FsOrigin, FsState, FsTime, Placement, Volume};
  // scan/fs.rs (crate-private)
  pub(crate) struct ViewMeasure { pub m: Option<Measurement>, /** D6 */ pub tail_ok: bool }
  /// Detector over Fragmented(f.pieces) at 0, then the D6 tail check.
  pub(crate) fn measure_view(formats: &[Box<dyn Format>], source: &dyn Source, f: &FsFile) -> ViewMeasure;
  pub(crate) struct Merged { pub len: u64, pub kind: FileKind, pub health: Health, pub fragments: Vec<Fragment>, pub origin: FsOrigin }
  pub(crate) fn merge(f: &FsFile, view: &ViewMeasure, carved: Option<&Measurement>) -> Merged;
  /// Health order: Intact 2, Partial 1, Damaged 0; "better" = strictly greater. A missing `modified` loses to a present one.
  pub(crate) fn rank(h: Health) -> u8;
  /// The one job of a kept file at sector `at` (view + `carved`), called by the worker AND by every commit path that has
  /// no worker result (`NotMeasured`, deferred block, `threads 1`).
  pub(crate) fn fs_job(..., at: u64) -> FsJobResult;
  ```
  `Shared` keeps the kept starts (`Vec<u64>` sorted, plus an index to the `FsFile`) and `listing.others` starts.
Setup (scan thread, before the first block; D1): `listing = filesystem::read_with(&cache, &raw_probe, cancel)` (the probe over the raw source refuses ranges the cache
knows are bad); emit `Volume(v)` for each; **nothing is added to `skips`** (R10); per distinct first-piece start keep
**one** file: live before deleted, then the latest `modified` (absent loses), then listing order (the others are not
reported); a file whose first piece is not sector-aligned is dropped. `listing.live` is kept for the overlap cap and
`listing.others` for the head consumption.

Pipeline: a sector start holding a kept file's first piece always gets a job (recognized or not, inside a skip range or
not, bad or not): `fill` creates a `Head` for every kept start in the block, and the worker never drops that job because
of `skipped`. The job computes `view = measure_view(..)` and, for a Deleted file only, `carved = measure(at)` when a
format recognizes the head. **Reuse:** for a Deleted file with one piece, if `carved` is contiguous and `carved.len ≤
size`, `view.m = carved` (same bytes: the walk ended inside the file) and the tail check runs from there; a
`cfg(test)` switch turns reuse off. Every commit path with no worker result (`NotMeasured`, deferred block, `threads 1`) calls the same `fs_job(at)`.

Commit, at sector `at`, **before** the bad-sector and skip checks: if a kept file starts at `at`, the merged finding is
added (pending as usual) and the carving head at `at` is consumed; `carved` is passed to `merge` only if `at` is not bad
and not in a committed skip range at that moment. A start of `listing.others` (live non-photo file): only the head
exactly at its first cluster is consumed, without a finding; if that measurement is `Intact`, its range is added to the
skips (R10); other heads inside the file are examined and, by D7, come out at most `Partial`. Then the per-sector loop
goes on unchanged.

`merge` (D3, D5, D6), exactly:
```
A.kind = view.m.kind, else f.kind;  A.health = view.m.health, else Damaged
cap A.health at Partial if !f.trusted || !view.tail_ok || view.m has fragments (a join inside the file's own pieces)
A.fragments = f.pieces (empty when one piece), A.len = their sum, origin = f.origin
if f.origin.state == Deleted && carved is Some(c) && c.kind == A.kind && c.health better than A.health && c.len == f.origin.size:
    → (c.len, c.kind, c.health, c.fragments, origin with placement Carved)
else → A
```
`tail_ok` (D6): structural length `L` over the view of length `S`. `L == S` → ok. `L < S` → loop at most 8
times: bytes [L, S) all `00`/`FF` → ok; else a format recognizes the head at view offset `L` and measures `m2` not
`Damaged` with `L + m2.len ≤ S` → `L += m2.len`, again; else not ok. 

`add` changes: a finding whose origin is not Live and whose ranges overlap `listing.live` is capped at `Partial` (D7);
skip ranges: only a merged finding that measures `Intact` adds its ranges (all pieces), as today (R10); a Live `Partial`
finding skips nothing (its own previews are hidden by D12, Task 9). `keep_moovs` also drops candidates inside `listing.live`.

- [ ] **Step 1: Failing tests** (`scan_fs.rs`; `card()` = FAT32 48 MiB / 512 in `Mbr{8192}`, unless said):
  ```rust
  #[test] fn volume_event_comes_first() { /* first event == Volume(v) with v.fs == Fat32, v.offset == 8192*512; then Progress… */ }
  #[test] fn live_files_come_out_once_with_names() {
      // DCIM/100MSDCF: DSC01234.ARW (tiff_raw, preview sector-aligned), DSC01235.JPG, C0001.MP4 (avc_video, small_nals:
      // false, add_at in 2 runs 4 MiB apart), DSC01236.JPG = jpeg_smooth(1024, 768, 1) add_at 2 runs with DSC01237.JPG
      // (jpeg_smooth seed 2, whole) written in the gap (Q7b)
      // → exactly one Found per file, each with origin path, health Intact; the MP4 and DSC01236 with fragments == their
      // FS pieces; no Found for the ARW's preview; no Found without origin
  }
  #[test] fn live_chain_beats_the_gap_search_limits() { /* JPEG with an 8 KiB first piece at cluster 3000 and its second piece at cluster 100 (before it) → Intact, 2 pieces (Q8) */ }
  #[test] fn deleted_files_keep_their_name_and_health() { /* deleted DSC01235.JPG (contiguous) → Found, origin Deleted/Guessed, Intact, len == size */ }
  #[test] fn photo_written_over_a_raw_preview_is_found() {
      // deleted DSC01234.ARW whose preview's cluster now holds live DSC09999.JPG (add_at) → Found(DSC09999.JPG, Live,
      // Intact) and the ARW's Found health != Intact
  }
  #[test] fn carved_file_over_live_clusters_is_not_intact() {
      // raw jpeg bytes (no entry) in free clusters; live MISC/notes.txt is written by add_at onto the jpeg's LAST cluster
      // with exactly the jpeg's own bytes of that cluster (so only the D7 cap can lower the health) → that carved Found:
      // origin None, health Partial
  }
  #[test] fn forged_live_file_does_not_hide_a_deleted_photo() {
      // live MISC/DATA.BIN whose chain covers a deleted-without-entry JPEG's clusters → the JPEG is still a Found
      // (nothing is pre-skipped), health Partial by D7
  }
  #[test] fn live_non_photo_file_consumes_only_its_head() {
      // live DCIM/100MSDCF/INFO.THM (a JPEG head) → no Found for it; a jpeg written inside it, not at its first cluster,
      // is a Found with health Partial (D7)
  }
  #[test] fn dedup_keeps_distinct_files() {
      // deleted D.JPG (6 clusters, guessed one run); then raw bytes of another jpeg X (no entry) over D's clusters 3..6
      // → Found(D, origin Deleted, health != Intact) and Found(X at its start, origin None, Intact)
  }
  #[test] fn fs_size_mismatch_is_not_intact() {
      // entry size = 2 × jpeg.len(): tail noise → Partial; tail zeros → Intact; tail = a second testkit jpeg + zeros
      // (MPF-like) → Intact; tail = testkit mp4 (motion photo) → Intact
  }
  #[test] fn corrupt_count_in_an_fs_file_is_bounded_by_its_size() {
      // tiff_raw Arw with its raw StripByteCounts patched to 0x3000_0000, live in the FS with its real size → Found len ==
      // size, health Partial (1.2 pending item)
  }
  #[test] fn carving_join_of_another_size_is_not_used() {
      // the burst-sibling case reduced to what decides it: a = jpeg_smooth(1024, 768, 1), c = first 512-multiple ≥
      // a.len() / 2; a deleted entry of size a.len() + 4096 whose guess is one run from a[..c]'s cluster (noise after
      // it); a[c..] 2 MiB later (carving joins it Intact, len a.len()) → Found(origin Deleted, placement Guessed),
      // health != Intact, fragments empty (the join's length is not the recorded size, D5)
  }
  #[test] fn deleted_guess_prefers_a_better_carving_of_the_same_size() {
      // deleted jpeg_smooth(1024, 768, 3) written around live DSC00001.JPG (2 runs: guess skips it → untrusted) →
      // Found Intact, fragments == carving's 2 pieces, origin placement Carved
  }
  #[test] fn fs_events_identical_across_threads_and_caches() {
      // the live_files card + deleted files + 2 bad sectors (inside DSC01235.JPG, inside the FAT) →
      // events(threads 1) == events(2) == events(8) == events(16) == events(8, cache_bytes 1 MiB) ==
      // events(1, cache_bytes 1 MiB); 3 runs at 8 equal
  }
  #[test] fn fs_job_is_independent_of_who_measures() {
      // unit/integration: force the worker result to `Dropped`/`NotMeasured` (cfg(test) switch) → the commit path calls
      // `fs_job(at)` and the Merged finding equals the one from the worker; also with a deferred block and threads 1
  }
  #[test] fn cancel_inside_the_first_volume_callback() {
      // cancelling inside the callback of the first `Event::Volume` → no event after it and Err(Cancelled)
  }
  #[test] fn filesystems_off_is_the_old_scan() { /* the card with filesystems: false → no Volume, every origin None, Found starts == carving's */ }
  #[test] fn exfat_card_works_the_same() { /* the live_files layout on Exfat 32 MiB / 32 KiB → same paths and health */ }
  // scan/fs.rs unit
  #[test] fn merge_rules() { /* table: Live untrusted Intact view → Partial; Deleted trusted + carved better with len == size → carved;
                                same with len != size → view; Live + carved better → view; view None → kind from extension, Damaged;
                                `rank`: Intact > Partial > Damaged; modified absent loses to present in the choice */ }
  #[test] fn reuse_matches_view_measurement() { /* each testkit kind as a deleted single-run file: finding with reuse == without */ }
  ```
  All existing tests must pass unchanged (noise images have no volume).
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; `bench --images noise,dense-jpeg`
  (release, best of 3) on this commit and on `main` (`b5ec340`) built side by side, twice interleaved: within 5%, else
  stop and report.
- [ ] **Step 5:** core README ("Scan": file-system phase, `Volume` events, origin, the merge, D3/D5/D6/D7, read-once
  exceptions; the `ScanOptions` memory sentence and the `sector > 512` note; new "Deduplication" section), CHANGELOG ("Added": photos and videos keep their name, folder and dates when
  the card's file system lists them, deleted ones included; a file found both ways is listed once). Commit:
  `feat(core): file-system findings with names, one finding per file`.

---

### Task 8: CLI, bench e o cartão real de ponta a ponta

**Files:**
- Modify: `crates/revenant-dev/src/{main.rs,bench.rs}`, `crates/revenant-testkit/src/lib.rs` (`BenchImage::ExfatCard`)
- Test: `crates/revenant-dev/tests/cli.rs`, `crates/revenant-core/tests/os_filesystems.rs` (opt-in)
- Docs: dev README ("Output": `volume` line and origin keys; "Benchmark": `exfat-card`), CHANGELOG

**Interfaces:**
- Produces (CLI, all additive; lines without origin are byte-identical to 1.3's):
  - `{"event":"volume","index":0,"fs":"fat32","scheme":"mbr","offset":4194304,"len":…,"sector_size":512,"cluster_size":512,"heap_offset":…,"cluster_count":…,"label":"REVENANT","serial":305419896,"from_backup":false}` (`label` omitted when None), before any other line.
  - `found`/`updated` lines with an origin gain `"name"`, `"path"`, `"state"`, `"placement"`, `"volume"`, `"fs_size"`,
    and `"created"`/`"modified"` (FsTime Display) when present.
  - `BenchImage::ExfatCard` (`"exfat-card"`): an exFAT volume in `Mbr{8192}` filling the image, holding the DenseJpeg
    files as live files in `DCIM/100TEST` (every 4th one deleted); `placed` counts them all. It is added to
    `default_values` of `--images` in `main.rs` and to the help text, so a plain `bench` prints its line. It is built with
    `FsBuilder::build()`, so peak memory = the image size (1 GiB at the default size): the dev README "Benchmark" says so.

- [ ] **Step 1: Failing tests:**
  ```rust
  // cli.rs
  #[test] fn volume_and_origin_keys() {
      // FAT32 card (Mbr{8192}) with live DCIM/100MSDCF/DSC01234.JPG (modified 2025-12-14 07:12:06) and a raw jpeg with no
      // entry in free space → first line event "volume" with fs "fat32", offset 4194304; the named found line has
      // "name":"DSC01234.JPG","path":"DCIM/100MSDCF/DSC01234.JPG","state":"live","placement":"chain","volume":0,
      // "modified":"2025-12-14T07:12:06"; the other found line has none of those keys; summary still last
  }
  #[test] fn bench_knows_the_exfat_card() { /* bench --size-mib 16 --runs 1 --images exfat-card → one line, found == placed;
                                              bench --size-mib 16 --runs 1 with no --images → includes an exfat-card line */ }
  // os_filesystems.rs (opt-in)
  #[test] #[ignore] fn os_made_cards_scan_end_to_end() {
      // (Fat32, Mbr, 64), (Exfat, Mbr, 64), (Exfat, Gpt, 64), (Fat16, None, 32): mounted: DCIM/100TEST with 4 testkit
      // JPEGs (one "Long name photo.jpg"), an Arw, an mp4; write X (3 MiB), Y, delete X, write Z (5 MiB, likely
      // fragmented); delete 2 photos; detach → scan: every live photo/video once, with its path, Intact, extract bytes ==
      // what was written (Z whatever its pieces); every deleted one whose clusters were not reused: state Deleted, Intact,
      // name equal up to the first character
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; opt-in tests; `bench --images
  dense-jpeg,exfat-card` (release): paste the lines; if `exfat-card` < 0.8 × `dense-jpeg`, stop and report (the K1 cost: live non-photo files are scanned by
  heads; re-evaluate only for non-photo types).
- [ ] **Step 5:** dev README, CHANGELOG ("Added": `revenant-dev` prints the volumes and each file's name, folder, state
  and dates). Commit: `feat(dev): volumes and file-system origins in the scan output`.

---

### Task 9: Pendências — JPEGs próprios do RAW `Partial` (P17) e a grade de clusters no pareamento

**Files:**
- Modify: `crates/revenant-core/src/formats/{mod.rs,tiffraw.rs,raf.rs,isobmff.rs}`, `src/scan/{mod.rs,pipeline.rs}`,
  `src/pairing.rs`
- Test: `crates/revenant-core/tests/{scan.rs,pairing.rs}`
- Docs: core README ("Scan", "Video with a separated index"), CHANGELOG

**Interfaces:**
- Produces:
  ```rust
  // Format gains (default: no own files)
  /// Embedded files the file at `start` holds in its own data (JPEG previews, thumbnails, JPEG-compressed strips or
  /// tiles, a CR3's PRVW and full-size JPEG), absolute, at most 64. Called only for a measurement that is not Intact.
  fn own_files(&self, source: &dyn Source, start: u64) -> Vec<Fragment> { Vec::new() }
  // pairing: pair_videos gains `grids: &[(u64, u64, u64)]`  // (heap_offset, cluster_size, heap end) of each volume
  ```
Rules: the job that measures a `Partial` file with `TiffRaw`, `Raf` or `IsoBmff` (kind Cr3) also returns `own_files`;
`own_files` comes out of the same measurement function on every path (worker, `NotMeasured`, deferred block, `threads 1`).
A hidden finding (D12) that measures `Intact` still adds its skip range (R10), so a thumbnail inside an RW2 preview does not
become a finding. A
carved finding (no origin) at `at` is not reported (no id) when an earlier finding X is `Partial` at that moment, one of
X's own files starts at `at`, and the new finding lies wholly inside X's ranges (D12). Own starts are kept ≤ 65,536 and
pruned below the scan position. `pair_videos` (a grid is `(heap_offset, cluster_size, heap_offset + cluster_count × cluster_size)`): videos with a Live origin are not candidates; for a video whose start `s`
lies in a grid with `cluster ≥ 4096` and `(s − heap) % cluster == 0`, break points are first only the `L1` with
`(s + L1 − heap) % cluster == 0` (ambiguity counted among them) and the second piece must start on the grid; if none
passes, the 1.3 search over 512-byte steps runs as before (D13). An `Updated` video keeps its origin (placement Carved).

- [ ] **Step 1: Failing tests:**
  ```rust
  // scan.rs
  #[test] fn partial_raw_hides_its_own_jpegs_only() {
      // panasonic_rw2 with an unchecked packing (a `raw_format`/`bits` combination the detector does not check; Partial, JpgFromRaw at a sector start) → found == [(rw2, Rw2, Partial)];
      // plus a foreign testkit jpeg at another sector start inside the RW2's raw data → that jpeg is still found (Intact)
  }
  #[test] fn own_files_are_the_same_on_every_path() { /* partial RW2 with threads 1, a deferred block and a worker → identical events */ }
  #[test] fn damaged_raw_keeps_its_preview_as_a_finding() { /* Arw with an IFD offset past TIFF_MAX_LEN (`1 << 30` literal: the constant is crate-private) (Damaged) and a sector-aligned preview → the preview is still a Found */ }
  #[test] fn partial_cr3_hides_its_own_jpegs() { /* testkit cr3 with its PRVW broken (Partial) and the full-size JPEG at a sector start → one Found */ }
  // pairing.rs
  #[test] fn cluster_grid_makes_the_break_point_unique() {
      // the ambiguous_break_point_is_partial layout (small_nals: false) placed in an exFAT volume (32 KiB clusters) so
      // both pieces start on clusters and l1 is a multiple of 32 KiB, with no entry for the video → Updated health Intact;
      // ambiguous_break_point_is_partial (no volume) still Partial
  }
  #[test] fn live_fs_videos_are_not_paired() { /* a live FS MP4 whose chain lacks the moov (Damaged) + an orphan moov elsewhere → no Updated */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README, CHANGELOG ("Changed": a partial RAW no longer lists its own preview as a separate photo;
  videos on a FAT/exFAT card are put back together on cluster boundaries, so more come out intact). Commit:
  `fix(core): hide a partial RAW's own JPEGs; pair videos on the cluster grid`.

---

### Task 10: Cartão formatado rápido — os nomes das pastas antigas

**Files:**
- Create: `crates/revenant-core/src/filesystem/orphans.rs`; Modify: `filesystem/{mod.rs,fat.rs,exfat.rs}` (entry parsing
  shared; `Listing.walked`), `src/scan/{mod.rs,pipeline.rs}`
- Test: `crates/revenant-core/tests/scan_fs.rs`; opt-in `tests/os_filesystems.rs`
- Docs: core README ("File systems: after a quick format"), CHANGELOG

**Interfaces:**
- Produces (crate-private):
  ```rust
  // filesystem/mod.rs: Listing gains `pub(crate) walked: Vec<Fragment>` — merged ranges of EVERY directory the FS phase
  // walked, live or deleted, FAT and exFAT (filled in fat.rs and exfat.rs); kept by the scan with `volumes` and `live`.
  /// Old directory clusters met in the blocks the scan reads: absolute offset and the volume index.
  pub(crate) struct OrphanFinder { .. }  // fed like MoovFinder; ≤ 65,536 candidates (16 B each)
  pub(crate) struct OrphanEntry { pub path: String, pub start: u64, pub size: u64, pub kind: FileKind,
      pub created: Option<FsTime>, pub modified: Option<FsTime>, pub volume: u32 }
  pub(crate) fn orphan_entries(source: &dyn Source, volumes: &[Volume], dirs: &[(u64, u32)], walked: &[Fragment]) -> Vec<OrphanEntry>;
  ```
Detection (read stage, at each cluster start `p` of a volume inside a read block, not inside `listing.live` nor in a
directory the FS phase walked): FAT: bytes 0..11 == `".          "`, byte 11 & 0x10, bytes 32..43 == `"..         "`, byte
43 & 0x10. exFAT: byte 0 ∈ {0x85, 0x05}, SecondaryCount 2..=18, SetChecksum valid (InUse forced) within the cluster.
Candidates in committed skip ranges are dropped (as `moov` ones); the `OrphanFinder` copies the `keep_moovs` rules: the
per-block cap counts only in blocks not covered by skip ranges, the global cap applies in commit order, and a candidate
inside a skip range is dropped. After pairing: each candidate directory is parsed with
the Task 4–6 rules (its own cluster, then following clusters while they are not live and still parse, ≤ 2 MiB); paths:
FAT `..` cluster → the candidate whose entries point at this directory gives its name, up to 32 levels; exFAT the same
by first-cluster lookup; unknown → `"?"`. Each file entry with a photo/video extension: if a finding without origin
starts at the entry's cluster offset, with the same kind (Mp4/Mov count as one), it gets `origin { state: Orphaned,
placement: Carved, .. }`; several entries for one finding → latest `modified` (absent loses), then lowest candidate offset; health capped
at `Partial` when `finding.len != size` and the D6 tail check over `[finding end, + size − len)` (contiguous) fails, or
`len > size`. `Updated` events: one per id, merged with a pairing `Updated` of the same id, in id order (D11).

- [ ] **Step 1: Failing tests** (`scan_fs.rs`):
  ```rust
  #[test] fn quick_formatted_fat32_card_keeps_names() {
      // DCIM/100MSDCF: DSC01234.JPG, DSC01235.ARW, C0001.MP4 (contiguous); build_quick_formatted() in Mbr{8192}
      // → 3 Found without origin, then 3 Updated with origin { state: Orphaned, placement: Carved, path:
      //   "?/100MSDCF/DSC01234.JPG" .. }, health unchanged; summary.updated == 3
  }
  #[test] fn quick_formatted_exfat_card_keeps_names() { /* same on exFAT → same paths */ }
  #[test] fn orphan_events_identical_across_threads_and_caches() {
      // quick-formatted card with a video pair (moov apart): events(threads 1) == 2 == 8 == 16 == events(8, cache_bytes
      // 1 MiB), including the `Updated` of orphans
  }
  #[test] fn orphan_entry_size_mismatch_is_partial() { /* entry size 1.5 × the jpeg with noise after it → Updated health Partial */ }
  #[test] fn orphan_finder_ignores_live_and_noise() {
      // the live (not formatted) card → no Updated; 16 MiB noise with fake "." ".." entries every 512 B and no volume → no Updated
  }
  #[test] fn orphan_name_and_pairing_merge_into_one_update() {
      // quick-formatted card whose C0001.MP4 was written in 2 runs (moov apart) → exactly one Updated for its id, with
      // 2 fragments and origin
  }
  // os_filesystems.rs (opt-in)
  #[test] #[ignore] fn os_made_card_after_a_quick_format() {
      // OsImage (Fat32, Mbr, 64) with DCIM/100TEST and 4 photos; detach; the test reads the partition's offset from
      // the MBR and runs testkit quick_format_in_place on those bytes of its own image file (never newfs on a device)
      // → scan: each photo Updated with path "?/100TEST/<name>"
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt; `bench` (release, `dense-jpeg`, `noise`):
  within 5% of Task 7's lines.
- [ ] **Step 5:** core README (the "read once" note mentions the old folders read after the search), CHANGELOG ("Added": after a quick format, photos and videos get their old names and
  folders back when the old folders survive). Commit: `feat(core): names from old folders after a quick format`.

---

### Task 11: Nomes dos arquivos recuperados e extração

**Files:**
- Create: `crates/revenant-core/src/naming.rs`; Modify: `src/extract.rs`, `src/lib.rs`, `crates/revenant-dev/src/main.rs`
- Test: unit tests in `naming.rs`; `crates/revenant-core/tests/extract.rs`; `crates/revenant-dev/tests/cli.rs`
- Docs: core README ("Extract", new "Names"), dev README (`--extract`, `--previews`), CHANGELOG

**Interfaces:**
- Produces:
  ```rust
  // naming.rs, re-exported as revenant_core::naming
  /// D14: the sanitized original name; else "YYYY-MM-DD_HH-MM-SS[_<camera>].<EXT>" from `m.captured_at` (or the
  /// origin's modified time), EXT = kind.extension() upper-cased; else "revenant_<start as 12 hex>.<ext>" (1.3's name).
  pub fn file_name(f: &Finding, m: Option<&Metadata>) -> String;
  /// A safe single file name, or None. Replaces each of / \ : * ? " < > | , control chars (< 0x20, 0x7F), format controls (U+200B–200F,
  /// U+202A–202E, U+2066–2069, U+FEFF) and U+FFFD by '_'; trims leading and trailing '.' and ' '; None if empty; prefixes '_' to Windows reserved stems (CON, PRN, AUX,
  /// NUL, COM0–COM9, LPT0–LPT9 and their superscript forms COM¹²³/LPT¹²³, CONIN$, CONOUT$; any case, with or without
  /// extension; the stem is what precedes the FIRST '.', with trailing spaces trimmed, so `CON.tar.gz` and `CON .jpg` are
  /// reserved); at most 200 bytes, cut on a char boundary, keeping the extension (an extension longer than 16 bytes is
  /// not kept: the whole name is cut as a stem).
  pub fn sanitize(name: &str) -> Option<String>;
  /// Camera part: the model, minus the make's first word when the model starts with it and a space; only ASCII
  /// letters, digits and '-'; at most 24 chars; "" when unknown.
  pub fn camera_tag(make: Option<&str>, model: Option<&str>) -> String;
  // extract.rs
  /// Like `extract`, named `sanitize(name)` (fallback "revenant_<start hex>.<ext>"), then "<stem>_2.<ext>", … (10,000 tries).
  pub fn extract_as(source: &dyn Source, finding: &Finding, dest_dir: &Path, name: &str) -> Result<PathBuf, EngineError>;
  // `extract` becomes extract_as(.., &file_name(finding, None)) (unnamed findings keep 1.3's names).
  ```
  CLI: `--extract` inspects first, then `extract_as(.., &file_name(&f, Some(&details.metadata)))`; `--previews` writes
  `<stem of that name>.preview.jpg` (same `_N` rule).

- [ ] **Step 1: Failing tests:**
  ```rust
  // naming.rs (unit)
  #[test] fn readable_names() {
      // origin name "DSC01234.ARW" → "DSC01234.ARW"; no origin, captured_at "2025-12-14T07:12:05", make "SONY", model
      // "ILCE-7M4", Arw → "2025-12-14_07-12-05_ILCE-7M4.ARW"; make "Canon", model "Canon EOS R5", Cr3 →
      // "2025-12-14_07-12-05_EOSR5.CR3"; make "NIKON CORPORATION", model "NIKON Z 6_2" → "…_Z62.NEF"; no model, Jpeg →
      // "2025-12-14_07-12-05.JPG"; no time, start 0xA000, Jpeg → "revenant_00000000a000.jpg"; Deleted origin named
      // "_SC01235.JPG" → that name
  }
  #[test] fn sanitize_rules() {
      // "../../x.jpg" → Some("_.._x.jpg"); "/abs/x.jpg" → Some("_abs_x.jpg"); "C:\\x.jpg" → Some("C__x.jpg");
      // "a\u{0}b.jpg" → Some("a_b.jpg"); ".." → None; "  " → None; "CON.JPG" → Some("_CON.JPG"); "nul" → Some("_nul");
      // "x\u{FFFD}.jpg" → Some("x_.jpg"); "a".repeat(300) + ".jpg" → 200 bytes ending in ".jpg"; "DSC01234.ARW" unchanged;
      // "COM0" → Some("_COM0"); "lpt\u{00B9}" → Some("_lpt\u{00B9}"); "CONIN$" → Some("_CONIN$"); "CON.tar.gz" →
      // Some("_CON.tar.gz"); "CON .jpg" → Some("_CON .jpg"); "x.jpg." → Some("x.jpg"); "x.jpg " → Some("x.jpg");
      // "a." + "b".repeat(300) → 200 bytes; "foto\u{202E}gpj.exe" → Some("foto_gpj.exe")
  }
  // tests/extract.rs
  #[test] fn hostile_names_stay_inside_the_destination() {
      // findings with origins named each hostile name above plus one from add_utf16 with a lone surrogate (through a real
      // FAT32 scan and through a real exFAT scan): extract each → every returned path's parent == dest; dest holds exactly that many files; dest's
      // parent dir listing unchanged
  }
  #[test] fn case_insensitive_names_do_not_overwrite() {
      // DSC00001.JPG and dsc00001.jpg (different bytes) → two distinct files with the right bytes each (on a case-insensitive
      // volume the second becomes DSC00001_2…); the same for NFC/NFD forms of one name
  }
  #[test] fn original_names_collide_with_suffixes() { /* two findings named DSC00001.JPG → "DSC00001.JPG", "DSC00001_2.JPG" */ }
  // cli.rs
  #[test] fn extract_uses_original_and_readable_names() {
      // card with live DSC01234.JPG and a carved jpeg_with_exif (date 2025:12:14 07:12:05, model "ILCE-7M4") → --extract
      // "file" values end with "/DSC01234.JPG" and "/2025-12-14_07-12-05_ILCE-7M4.JPG"; --previews of a named RAW "DSC01235.ARW" →
      // "DSC01235.preview.jpg"
  }
  ```
  Existing extract/CLI tests on unnamed findings keep their `revenant_…` names unchanged.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** docs (README "Names" records that a total path over 260 chars on Windows is a write error, never an escape), CHANGELOG ("Added": recovered files keep their original name, or get a readable one from the
  date and camera). Commit: `feat(core): recovered files keep their original or a readable name`.

---

### Task 12: Agrupamento — sessão, dia, câmera e pasta

**Files:**
- Create: `crates/revenant-core/src/group.rs`; Modify: `src/lib.rs`, `src/inspect.rs` (a crate-private
  `pub(crate) fn seconds(local: &str) -> Option<i64>` next to the date helpers), `crates/revenant-dev/src/main.rs`
- Test: unit tests in `group.rs`; `crates/revenant-dev/tests/cli.rs`
- Docs: core README (new "Grouping"), dev README (`--group`), CHANGELOG

**Interfaces:**
- Produces (`revenant_core::group`):
  ```rust
  pub const SESSION_GAP_SECS: i64 = 2 * 60 * 60;   // D15: a longer pause starts a new session
  #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
  pub struct Camera { pub make: Option<String>, pub model: Option<String>, pub serial: Option<String> }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct GroupInput { pub id: u64, pub time: Option<String>, pub camera: Option<Camera>, pub folder: Option<String> }
  impl GroupInput {
      /// time (D16): photos → m.captured_at, else origin modified, else created; MP4/MOV → origin modified, else
      /// created, else m.captured_at. camera: None when make and model are both None. folder: Some(origin.folder()) for
      /// any origin, `Live`, `Deleted` or `Orphaned` (C16: the old folders recovered after a quick format count as a
      /// surviving structure, so "Por pasta" stays on); the root is Some("") = `Folder("")`, not `NoFolder`.
      pub fn new(f: &Finding, m: Option<&Metadata>) -> GroupInput;
  }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum GroupLabel {
      Session { camera: Option<Camera>, first: String, last: String }, UndatedSession { camera: Option<Camera> },
      Day(String), Undated, Camera(Camera), UnknownCamera, Folder(String), NoFolder,
  }
  #[derive(Clone, Debug, PartialEq, Eq)] pub struct Group { pub label: GroupLabel, pub ids: Vec<u64> }
  pub fn by_session(items: &[GroupInput]) -> Vec<Group>;
  pub fn by_day(items: &[GroupInput]) -> Vec<Group>;       // "YYYY-MM-DD", ascending, Undated last
  pub fn by_camera(items: &[GroupInput]) -> Vec<Group>;    // by camera key, UnknownCamera last
  /// None when no item has a folder (spec §4: "Por pasta" only when the structure survived); NoFolder last.
  pub fn by_folder(items: &[GroupInput]) -> Option<Vec<Group>>;
  ```
Session algorithm: camera key = (make, model) trimmed and upper-cased + serial; an item without serial takes the serial
when its (make, model) has exactly one distinct serial among the items; the label shows the strings of the item with the
smallest id in the key. Items whose time does not parse go to the camera's `UndatedSession`. Per key: sort by (time, id);
a gap `> SESSION_GAP_SECS` between consecutive items starts a session. Output: dated sessions by (first time, camera key,
first id), then undated sessions by camera key (`None` last); ids inside a group by (time, id). Pure: `BTreeMap`/sorts
only. CLI: `scan --group session|day|camera|folder` prints, after the last `found`/`updated` and before the summary,
`{"event":"group","by":"session","label":{…},"ids":[…]}` per group (label serialized as its variant name and fields),
or one `{"event":"group","by":"folder","available":false}`; it keeps each id's latest finding and metadata.

- [ ] **Step 1: Failing tests:**
  ```rust
  // group.rs (unit) — A = (SONY, ILCE-7M4, 111), B = (Canon, EOS R5, 222)
  #[test] fn sessions_split_on_gaps_and_cameras() {
      // 1 A 2025-12-14T08:00:00, 2 A 08:30, 3 A 10:31 (2 h 01 after 08:30), 4 B 09:00, 5 A no time, 6 (SONY, ILCE-7M4,
      // no serial) 08:15 → [Session{A, "…08:00:00", "…08:30:00"}: [1, 6, 2], Session{B, 09:00, 09:00}: [4],
      // Session{A, 10:31, 10:31}: [3], UndatedSession{A}: [5]] (ids by (time, id))
  }
  #[test] fn exactly_two_hours_is_the_same_session() { /* 08:00 and 10:00 → one session */ }
  #[test] fn two_bodies_of_one_model_stay_apart() { /* (SONY, ILCE-7M4, 111) and (…, 333) at 08:00/08:10, a serial-less one at 08:05 → 3 groups */ }
  #[test] fn videos_use_the_fs_time_when_present() {
      // Mp4 finding with origin modified "2025-12-14T08:20:00" and captured_at (UTC) "2025-12-14T11:20:00" → time 08:20,
      // joins A's 08:00 session; without origin → 11:20, its own session
  }
  #[test] fn day_camera_and_folder() {
      // by_day: ["2025-12-14": …, "2025-12-15": …, Undated]; by_camera ordered by key: [B, A, UnknownCamera]; by_folder
      // with no origins → None; with "DCIM/100MSDCF" and "DCIM/101MSDCF" → those two then NoFolder; a root file →
      // Folder(""); only Orphaned origins ("?/100MSDCF") → Some([Folder("?/100MSDCF"), ..])
  }
  #[test] fn grouping_is_order_independent() { /* 500 seeded items (3 cameras, 4 days, 5 folders, some undated), 20 seeded shuffles → each by_* identical */ }
  // cli.rs
  #[test] fn group_lines_come_before_the_summary() { /* --group session on a card with 2 photos 3 h apart → 2 group lines, then the summary; --group folder on noise → one line "available":false */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** docs, CHANGELOG ("Added": grouping by shooting session, day, camera and folder for the results screen).
  Commit: `feat(core): group recovered files by session, day, camera and folder`.

---

### Task 13: Documentação e fechamento da parte

**Files:**
- Modify: `README.md`, `crates/revenant-core/README.md` ("Current state", "Performance"), `crates/revenant-dev/README.md`,
  `CHANGELOG.md`, `CLAUDE.md`, `docs/superpowers/roadmap.md`
- Create: `docs/superpowers/history/2026-10-20-m1-p4-pendencias.md` (date from `date +%F` on the day of this task)

- [ ] **Step 1:** `cargo test --workspace`, `cargo test --release -p revenant-core`, `cargo clippy --all-targets -- -D
  warnings`, `cargo fmt --check`, `cargo deny check licenses` (if installed; else say so), `REVENANT_SAMPLES=samples/cache
  cargo test -p revenant-core --test real_samples -- --ignored` if `samples/cache` exists, and the opt-in suites with
  `REVENANT_OS_FS=1` (`-p revenant-testkit --test os_builders`, `-p revenant-core --test os_filesystems`, both
  `-- --ignored`); paste the summaries.
- [ ] **Step 2:** `cargo run --release -p revenant-dev -- bench` (all images, `exfat-card` included); it relies on `exfat-card` being in the `--images` default (Task 8); add the
  `exfat-card` line to the core README "Performance" table and say whether the other lines moved.
- [ ] **Step 3: Docs:** root README status (file systems, names, grouping); `CLAUDE.md` "Onde o desenvolvimento está"
  (pt-BR, today's date: 1.3 mergeada (PR #4); 1.4 concluída na branch `feat/m1-p4-filesystems`, aguardando PR/merge;
  decisões em `docs/superpowers/history/<date>-m1-p4-pendencias.md`; próxima: 1.5; o teste opcional `os_filesystems`
  como segunda exceção à regra de dados reais, explicando por que não toca disco físico); roadmap 1.4 marked done with
  what it covers (FAT12/16/32 e exFAT, vivos e apagados, formatação rápida pelos nomes, deduplicação, agrupamento, nomes)
  and what it does not (apagado fragmentado sai `Partial`; NTFS/APFS/HFS+).
- [ ] **Step 4:** history file (pt-BR, same shape as the 1.3 one): D1–D18 and the pre-flight rulings C1–C20, K1–K4 with their costs, decisions taken during
  execution, the bench lines, the 1.4 pending items closed (table above) and every item left open with its owner:
  **sem dono** — foto gravada no início de prévia de RAW sem metadados de sistema de arquivos; COUNT corrompido em
  achado só esculpido; **1.6** — apagado fragmentado `Partial` (D3) e cauda quase igual sem metadados; **Módulo 2** —
  a extração plana sem subpastas (D14), nomes comerciais de câmera, vídeo esculpido fora da sessão pelo fuso (D16), o
  motivo exibido quando "Por pasta" está desligado; **sem dono** — quadros de MJPEG de vídeo `Partial` (D12),
  conferência do Windows contra um driver real (D17); o D14 diverge do exemplo da spec §3 (`A7IV`); a leitura do C16 ("Por pasta"
  fica ligado com pastas antigas recuperadas) para o Módulo 2 confirmar.
- [ ] **Step 5:** Commit: `docs: part 1.4 status, numbers and pending items`.
