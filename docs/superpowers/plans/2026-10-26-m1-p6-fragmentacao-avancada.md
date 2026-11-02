# Módulo 1 · Parte 6 — Fragmentação avançada — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** O motor passa a remontar fotos e vídeos espalhados em 3 ou mais pedaços (JPEG, MP4/MOV H.264/H.265 incluindo
`moov` antes do `mdat`, `mdat` de tamanho 0 e `moof`, RAW TIFF com ARW primeiro), apagados ou não, usando a grade de
clusters e o mapa de clusters livres do volume, e só os chama `Intact` quando cada emenda é provada pelo conteúdo e é a
única possível dentro do orçamento; um arquivo apagado e fragmentado no FAT/exFAT pode enfim sair `Intact` (D3 da 1.4).

**Architecture:** Uma etapa nova depois do pareamento (`reassemble`), sequencial e determinística, sobre os achados
candidatos guardados durante a busca. Um motor genérico de busca de emendas (`reassemble::search`) anda em profundidade
pelos pedaços, com candidatos em início de cluster livre numa ordem fixa ("o próximo livre", depois "o primeiro buraco"),
e pergunta a um validador por formato (`Seams`: JPEG pelo decodificador da 1.3, vídeo pela tabela de amostras e pelas
unidades NAL, RAW pela estrutura TIFF, pela prévia e pelos blocos JPEG sem perdas) se a continuação passa. Toda leitura
passa pelo cache da busca (nunca relê setor ruim conhecido) e tem orçamento por arquivo e por busca. O resultado sai como
`Event::Updated`; o estado novo da busca entra no `ScanState`, e a retomada refaz o pareamento e a remontagem.

**Tech Stack:** Rust 2024 (MSRV 1.88), só `std` e as dependências atuais (nenhum crate novo). Testes com o testkit
(`FsBuilder`, geradores de JPEG, vídeo e RAW), opcionais com amostras reais CC0.

**Spec:** `docs/superpowers/specs/2026-10-26-m1-p6-fragmentacao-avancada-design.md` (o desenho; a autoridade, incluindo
§5.1). Acima dele, `docs/superpowers/specs/2026-10-19-revenant-design.md` (§4 estados, §5 "Escultura com fragmentação").
**Antes:** `docs/superpowers/history/2026-10-20-m1-p3-pendencias.md`, `2026-10-25-m1-p4-pendencias.md`,
`2026-10-25-m1-p5-pendencias.md`. **Roteiro:** `docs/superpowers/roadmap.md`. **Plano anterior (estilo):**
`docs/superpowers/plans/2026-10-25-m1-p5-disco-de-verdade.md`.

## Global Constraints

- **Language [Sérgio]:** everything in the repository in English (identifiers, comments, error codes, commits, READMEs,
  CHANGELOG); `CLAUDE.md` and `docs/superpowers/**` stay in pt-BR.
- **Never write to the source.** `Source` keeps no write method; reassembly only reads, and only through the scan's
  `CachedSource` (design §5.1: it may re-read within a budget but **never re-reads a sector the bad map holds**).
- **No `unsafe` outside `crates/revenant-core/src/source/windows.rs`** (nothing in this part needs it). Code using
  `std::os::unix` stays behind `cfg`; before each commit `grep -rn "os::unix" crates` and check every use is gated.
- **Licenses:** only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, CC0-1.0, BSL-1.0.
  **No new crate** anywhere. Nothing copied from PhotoRec/TestDisk or any GPL/LGPL/MPL project: the seam search, the
  lossless-JPEG decoder (ITU-T T.81 Annex H) and the ARW layout are written from the public specifications and our tests.
- **Errors by code**, no UI text in the engine. This part adds **no** error code: a budget hit, an ambiguity or a missing
  proof is a health (`Partial`), never an error.
- **Rust:** edition 2024, `rust-version = "1.88"`. Dev machine: Intel Mac, Rust from `/usr/local/bin` (no `rustup`);
  **never run `brew`**; check a binary exists (`which`, `~/.cargo/bin`, `target/release`) before building or installing
  any tool.
- **Health rule [design §4, §5]:** `Intact` only when every seam is validated by content AND is the only one that passes
  within the search (no cap or budget hit, every candidate of every seam tried, **blind candidates included**: Q2; the
  search window is 256 MiB / 64 MiB, a look-alike beyond it is an accepted, documented risk: Q4); otherwise `Partial`
  with the best pieces found. Through reassembly, `Intact` also needs the volume listing not truncated (Q5). Q16 stays: a JPEG with restart markers (DRI > 0) reassembled from 2+ pieces is at most `Partial`. A RAW seam inside
  uncompressed or lossy sensor data is never a proof (bytes placed, at most `Partial`), except a live chain (as today) or a
  proven exFAT old chain (D14).
- **Where a piece may lie [design §5]:** on a volume's cluster grid when the file starts on one (cluster ≥ 4096, as D13 of
  1.4), else on 512-byte sectors with a smaller budget; only clusters no live file claims (`Listing.live`) and that no
  `Intact` finding uses (a piece is never used by two `Intact` files; a RAW and its own preview sharing bytes are the
  same file, nested, recorded, not two `Intact` files: Q16).
- **Known size [design §5]:** a file-system entry's size, or the format's (`moov` sample table, TIFF extent): the pieces
  sum to it exactly.
- **Determinism:** for a source that answers the same every time, the events (minus `Event::Rate`) are identical for any
  `threads` and `cache_bytes`, and a session resumed from any checkpoint yields exactly the uninterrupted sequence. The
  reassembly is sequential, in id order, and depends only on the source's bytes and the saved state.
- **Speed:** no slowdown above 5% on images without fragmentation (control-build method of 1.4/1.5: this commit and its
  parent built side by side, interleaved, best of 3); reassembly time bounded by the budgets and measured.
- **Bounded everything** (a constant may have a `cfg(test)` twin seen only by unit tests inside the crate; integration
  tests lower a limit through an explicit public parameter, P1 of 1.5: nothing `cfg(test)` or crate-private is visible to
  `tests/*.rs`): every cap below is in `reassemble::Limits`.
- **Docs in the same commit [Sérgio]:** each task updates the READMEs it affects and `CHANGELOG.md` `[Unreleased]` for
  anything user-visible.
- **Commit trailer, always:** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Conventional Commits. Never
  `git add -A` (add files by name). Never push (the controller opens the PR).
- **Tests never touch real data or physical disks:** synthetic images in memory or `tempfile` only. Opt-in exception:
  `real_samples` (CC0 samples in `samples/cache/`, never committed). Every new test is seen failing before it passes
  (record RED in the report).

## Review Focus

1. **Two look-alike files interleaved (a burst of the same scene, the RAW and the JPEG of one shot, two clips of the same
   camera) whose pieces could be swapped** → never an `Intact` file made of another file's bytes:
   `same_frame_neighbour_tail_is_not_intact` (Task 3), `two_clips_of_one_camera_interleaved_are_not_swapped` (Task 5),
   `burst_arw_neighbours_are_not_glued_intact` (Task 8), `no_intact_finding_is_glued` (Task 10, over every card).
2. **A hostile or very fragmented card (thousands of free clusters, many look-alike candidates)** → the scan ends within
   the documented budget and the file comes out as it does today (no hang, no `Intact` on an unfinished search):
   `toy_budget_hit_is_not_exhaustive` (Task 2), `hostile_card_stays_within_budget` (Task 4),
   `reassembly_budget_measure` (Task 11).
3. **A session interrupted and resumed on a card with fragmented files** → exactly the same events, the reassembly redone
   identically: `resume_with_reassembly_equals_the_whole_scan` (Task 4), `state_round_trips` (extended, Task 4).
4. **A bad sector inside a candidate piece or a piece of the result** → never read again, and the file is at most
   `Partial`: `reassembly_never_rereads_known_bad_sectors` (Task 4).
5. **A live file's clusters, or the pieces of another `Intact` file, taken into a reassembly** → never:
   `pieces_never_overlap_live_files_or_another_intact` (Task 4), `old_chain_reused_by_a_later_file_is_not_proof` (Task 9).

## Decisões do pre-flight (D1–D22)

Cada linha: decisão — custo se estiver errada. Decididas pelo Claude dentro do que o desenho §7 deixa ao construtor
(algoritmos, tetos, orçamentos, H.264/H.265, divisão em tarefas).

- **D1** A remontagem é uma etapa **depois** da busca e do pareamento, antes do ponto `Checkpoint::Paired`, sequencial,
  em ordem de id, sobre candidatos guardados durante a busca; cada achado melhorado sai como `Event::Updated` (mesmo id),
  junto com o pareamento e os nomes de pastas antigas (um `Updated` por id). Nada muda durante a busca: as faixas puladas,
  os `Found` e a ordem continuam as de hoje — custo: um pedaço de uma foto que a busca acha como arquivo próprio (a prévia
  de um RAW) continua saindo também como achado à parte (D10).
- **D2** Candidatos (guardados em `Commit::emit_found`, até `MAX_CANDIDATES = 4096`, os primeiros por id): JPEG não
  `Intact` com primeiro pedaço ≥ 16 KiB (o `MIN_FIRST_PIECE` da 1.3), MP4/MOV não `Intact`, RAW TIFF (`Arw Cr2 Nef Dng Orf
  Rw2 Tiff`) `Partial`, e todo achado de origem `Deleted` desses tipos que saiu `Partial`; nunca um de origem `Live` (a
  cadeia diz onde ele está). Passado o teto, `frags_capped` e nenhuma remontagem sai `Intact` (um candidato descartado
  podia disputar os mesmos pedaços) — custo: num cartão com mais de 4096 arquivos partidos, nenhum sai `Intact`.
- **D3** Espaço (`Space`): grade do volume quando o achado começa num início de cluster de um volume com cluster ≥ 4096
  (mesma regra do D13 da 1.4); livre = dentro do heap, fora de `Listing.live`, fora dos pedaços de todo achado `Intact` já
  emitido (`Commit::claimed`) e de toda remontagem `Intact` anterior (em ordem de id). Sem grade: setores de 512 B, só
  para frente, janela de 64 MiB depois do pedaço atual (a da 1.3), no máximo 4 pedaços — custo: um arquivo que começa
  fora da grade (cartão reformatado com outro cluster) só é remontado em poucos pedaços.
- **D4** Ordem dos candidatos de cada emenda: os inícios de unidade livres em ordem crescente a partir do fim do pedaço
  atual, até 256 MiB depois dele ("o próximo livre"), depois os do início do heap até esse fim ("o primeiro buraco"); no
  máximo `SEAM_CANDIDATES = 4096` **sobreviventes do pré-filtro** por emenda (Q1; o pré-filtro roda durante a
  enumeração). Mais que isso ⇒ a emenda não foi esgotada (no máximo `Partial`). A unicidade vale só dentro da janela
  (Q4) — custo:
  uma continuação a mais de 256 MiB num cartão quase vazio à frente, ou depois de 4096 clusters livres, não é achada
  (o arquivo sai como hoje).
- **D5** Pontos de quebra de cada emenda: as fronteiras de unidade dentro do pedaço atual entre o último ponto que o
  validador conferiu (`Walked::checked`) e o primeiro sinal de quebra, da mais próxima do sinal para trás, no máximo
  `BREAK_POINTS = 64`; mais ⇒ não esgotada — custo: um vídeo com unidades NAL enormes num cluster pequeno pode passar de
  64 fronteiras sem cabeçalho no meio e sair `Partial`.
- **D6** `Intact` exige: exatamente uma solução completa na busca toda (candidatos cegos também contam: Q2), nada esgotado por
  teto ou orçamento, a listagem do volume não truncada (Q5), `prove` =
  `Proven` (cada emenda provada pelo conteúdo), nenhum setor ilegível nos pedaços (`bad_runs` do cache), o tamanho
  conhecido batendo, a conferência própria do formato sobre a vista (`decode_health` estrito no JPEG, `index_health` no
  vídeo, `preview_health` no RAW) e o Q16. Senão `Partial` com a primeira solução na ordem do D4 (ou, sem solução, o
  caminho que validou mais bytes). Um `Updated` só sai quando melhora o achado (`improves`, Task 4) — custo: uma
  remontagem parcial que não passa do achado antigo não aparece.
- **D7** Orçamentos [escolha do Claude, medidos na Task 11]: por arquivo `FILE_TRIES = 2^16` caminhadas de candidato e
  `FILE_READ = 512 MiB` pedidos à origem; por busca `SCAN_TRIES = 2^22` e `SCAN_READ = 16 GiB`; `MAX_PIECES = 16` pedaços
  na grade, 4 fora dela. Passado o orçamento do arquivo, ele sai com o que se juntou (no máximo `Partial`); passado o da
  busca, os candidatos restantes ficam como estão — custo: num cartão hostil a remontagem pode levar o tempo de reler 16 GiB.
- **D8** Releitura em vez de impressão por cluster (design §5.1, escolha do construtor): a remontagem lê pelo
  `CachedSource` da busca, contando os bytes pedidos numa fonte contadora (`Counted`), e nunca relê setor ruim conhecido
  (o cache já o recusa); nada é gravado por cluster durante a busca — custo: releituras dentro do orçamento; nenhuma
  memória nova durante a busca.
- **D9** Um início de achado (`Commit::starts`) dentro de um trecho livre termina o trecho ali (como o Q13 da 1.3): um
  pedaço nunca contém o início de outro achado, salvo um "arquivo próprio" do formato na posição exata que a estrutura
  aponta (D10) — custo: uma foto cujo meio por acaso parece um cabeçalho confirmado é cortada ali.
- **D10** Arquivos próprios (`Seams::own`): a prévia JPEG de um RAW que caiu num pedaço posterior foi achada pela busca
  como JPEG `Intact` e reivindicou seus bytes; ela não bloqueia o RAW quando está exatamente na posição que a estrutura
  TIFF aponta (mesmo deslocamento relativo e mesmo tamanho). O achado da prévia continua emitido — custo: a prévia aparece
  duas vezes (como no MJPEG da 1.4).
- **D11** JPEG: o estado é o `Cursor` da 1.3 e um vetor de DC por busca (truncado ao voltar); uma continuação passa a
  emenda quando decodifica pelo menos a linha de MCU abaixo dela e o teste antecipado (`early_base`) não a recusa; a
  prova final é `seams_ok` (a régua da 1.3 com várias emendas: a base é a mediana das fronteiras fora de **todas** as
  emendas). Um pré-filtro barato descarta o candidato cujo primeiro setor tem um marcador que não é de dados — custo: uma
  foto pequena (menos de 4 linhas de blocos) nunca é provada.
- **D12** Vídeo: o arquivo vive em "deslocamento do arquivo" (o índice dá cada amostra); a conferência de amostra do
  pareamento (tamanhos NAL e bytes de cabeçalho, prefixo do `avcC`/`hvcC`, ou SOI/EOI do MJPEG) vira
  `isobmff::check_sample`, compartilhada; para `Intact` **toda** amostra conferível é conferida na posição final (fecha o
  C7, também no pareamento de 2 pedaços). O `moov` órfão é âncora: o último pedaço tem de pô-lo onde ele está. `moov`
  antes do `mdat`: o primeiro pedaço traz o índice; `mdat` de tamanho 0: o tamanho conhecido é o fim da última amostra;
  `moof`: a cadeia de caixas (`moof`/`mdat`) com as amostras do `trun` — custo: um vídeo só com faixas não conferíveis
  (áudio, outro codec) nunca é provado.
- **D13** RAW: estrutura TIFF andada sobre a vista (nenhum erro estrutural, toda IFD dentro dela legível), a prévia JPEG
  decodifica estrita através das emendas, e os blocos `Compression 7` (JPEG sem perdas, SOF3) decodificam até o fim com o
  número exato de amostras; uma emenda dentro de um desses é provada; uma emenda dentro de dados do sensor sem compressão
  ou com perdas (Sony `Compression 32767`) é `Unproven`: o pedaço é posto no primeiro candidato da ordem do D4 com trecho
  livre suficiente e o RAW sai no máximo `Partial` — custo: um ARW sem compressão espalhado nunca é `Intact` sem a cadeia.
- **D14** exFAT, cadeia FAT que sobrevive num apagado (design §5.1): `old_chain` com mais de um trecho vira
  `Placement::OldChain` ("old_chain") quando nenhuma outra entrada apagada do volume reivindica nenhum desses clusters;
  `merge` deixa o achado `Intact` quando a medida da vista é `Intact`, a cauda confere, o tamanho bate **e**
  `reassemble::content_checks` passa (JPEG estrito; vídeo: toda amostra; RAW: estrutura, prévia, blocos sem perdas, sem
  exigir prova de emenda nos dados do sensor). Senão a cadeia é só a primeira hipótese (o motor tenta ela primeiro) —
  custo: `Placement` ganha uma variante (API pública, pré-1.0).
- **D15** Um achado de sistema de arquivos apagado remontado mantém a origem com `placement: Carved` (como o pareamento);
  a soma dos pedaços tem de ser o tamanho da entrada — custo: nenhum.
- **D16** Estado: o `Commit` ganha `frags`, `frags_capped`, `claimed`, `claimed_capped`; a captura (sem `..`) obriga a
  pô-los no `ScanState` e no codec; `Checkpoint::Paired` passa a ser "depois do pareamento **e da remontagem**"; a
  retomada a partir dele refaz as duas (determinísticas) — custo: o estado cresce até 4096 achados + 2^18 trechos.
- **D17** Dois vídeos com primeiros pedaços idênticos (bytes iguais, mesmo tamanho, comparados pelo cache dentro do
  orçamento): tratados como um só; o de menor id é remontado e os outros ficam como estão — custo: as cópias seguem
  `Damaged`.
- **D18** `MAX_CLAIMED_RUNS = 2^18` trechos de achados `Intact`; passado, `claimed_capped` e nenhuma remontagem sai
  `Intact` — custo: num cartão com mais de 262.144 arquivos inteiros não contíguos entre si, só `Partial`.
- **D19** Testkit: `FsBuilder::alloc(Alloc::NextFree | Alloc::FirstFit)` imita os dois jeitos de alocar; `DeleteStyle::
  ZeroChain` imita o computador que zera a cadeia no exFAT; `jpeg_burst` (fotos parecidas da mesma cena e quadro);
  `camera_video` (H.264/H.265, `moov` no fim, no começo, `mdat` 0, `moof`); `lossless_jpeg` e `arw` (sem compressão, sem
  perdas, com perdas). Como o firmware da Sony aloca é suposição: registrado como risco — custo: os cartões sintéticos
  podem não cobrir o padrão real (o roteiro manual da Task 13 confere).
- **D20** Nada novo no caminho por setor da busca: só a contabilidade por achado emitido (empurrar num vetor, somar
  trechos num `Runs`) — custo: nenhum mensurável (Task 4 mede).
- **D21** Q16: medido com amostras reais (`real_samples`): o DRI das prévias dos ARW da Sony e, se houver amostra CC0, de
  um JPEG da A6700; o resultado vai para o histórico; se usarem DRI, o limite fica documentado como o principal custo da
  1.6 para a câmera do Sérgio — custo: nenhum no código.
- **D22** Os menores herdados da 1.4 (T5, T6, T9, T10) entram onde são baratos (Task 12); o que não couber volta ao
  histórico com dono — custo: nenhum.

## Decisões do pre-flight da execução (Q1–Q19)

Rodadas do controlador antes de despachar as tarefas, sobre os achados de
`.superpowers/sdd/2026-10-26-m1-p6-fragmentacao-avancada/preflight.md`. Vinculantes: onde diferirem das D1–D22 acima, valem
as Q. As tarefas já carregam o texto de cada uma. Cada linha: decisão — custo.

- **Q1** (B1) O pré-filtro roda durante a enumeração dos candidatos; o teto de 4096 conta os sobreviventes; as leituras do
  pré-filtro entram no orçamento do arquivo; vídeo e RAW ganham pré-filtro (Tasks 2, 3, 5, 8, 9) — custo: nenhum.
- **Q2** (B2) A busca desce também nos candidatos CEGOS; suas soluções completas contam para a unicidade (uma continuação
  cega verdadeira ao lado de uma vizinha evidente ⇒ não única ⇒ `Partial`) (Tasks 2, 3, 4, 8) — custo: mais trabalho de
  busca, limitado pelo orçamento; fecha um caminho de falso `Intact`.
- **Q3** (B3) `search(.., known: Option<u64>)` (Task 2), preenchido com `origin.size` na Task 4 (Tasks 6, 9 só consomem); o
  tamanho tem de bater quando conhecido — custo: nenhum.
- **Q4** (S1) Unicidade só dentro da janela de 256 MiB (64 MiB sem grade) é um risco aceito e documentado (Task 2, README,
  Task 13) — custo: um sósia fora da janela pode deixar uma emenda falsamente única.
- **Q5** (S2) `Intact` por remontagem exige também a listagem não truncada (`ctx.listing_truncated`, Task 4) — custo:
  cartões enormes perdem `Intact` nos arquivos remontados.
- **Q6** (S3) Prova da cadeia antiga (Task 9): bitmap presente, `valid == size`, "reivindica" definido, nenhum início de
  outro achado dentro da cadeia — custo: menos `Intact` por `OldChain`.
- **Q7** (S4) Arquivo próprio (prévia do RAW) casa por cobertura, não por igualdade de `Runs` (Tasks 2, 8) — custo: nenhum.
- **Q8** (S5) Testes com relógio viram determinísticos: contadores e uma fonte que cancela na primeira leitura após o fim
  da busca (Task 4); o limite de tempo vai para a medição da Task 11 — custo: nenhum.
- **Q9** (S6) `reassembly_budget_measure` vira teste unitário `#[ignore]` no crate; `real_previews_split_in_three` usa um
  cartão `FsBuilder` e o `scan` público (Task 11) — custo: nenhum.
- **Q10** (S7) `session::store::VERSION` passa a 2 (Task 4) — custo: sessões antigas recusadas, aceitável antes da 1.0.
- **Q11** (S8) O cartão F tem de mostrar que remonta (`Updated`, `frags`, `claimed` não vazios) antes de o teste de
  retomada depender dele (Task 4) — custo: nenhum.
- **Q12** (S9) Visibilidade `pub(crate)` ampliada nas tarefas que usam, com os arquivos listados: `formats/jpeg.rs`
  e `jpeg_join` (Task 3), `pairing::Video` (Task 5), `preview_health` (Task 8) — custo: nenhum.
- **Q13** (S10) O RED da Task 10 aparece num commit anterior (antes da Task 3) ou sob mutação — custo: nenhum.
- **Q14** (S11) Testes de emenda cega usam intervalos tomados entre os trechos (Tasks 2, 8) — custo: nenhum.
- **Q15** (S12) Bloco sem perdas só passa com o marcador de fim exatamente no fim declarado do bloco e a contagem exata;
  a Task 11 registra se os blocos reais da Sony têm contagens iguais (Tasks 7, 8, 11) — custo: nenhum.
- **Q16** (S13) RAW e sua própria prévia compartilhando bytes são o mesmo arquivo (aninhado), registrado, não dois
  `Intact`; os oráculos das Tasks 4 e 10 isentam os aninhados — custo: nenhum.
- **Q17** (S14) A Task 10 define a saúde esperada por tipo (JPEG DRI 0 `Intact`, ARW sem perdas `Intact`, ARW sem
  compressão/com perdas dividido no sensor `Partial` com bytes exatos, vídeo conferível `Intact`) — custo: nenhum.
- **Q18** (S15) As conferências de conteúdo dentro da busca (Task 9) ganham medição de velocidade dentro dos 5% — custo:
  nenhum.
- **Q19** Os 11 achados cosméticos aplicados: C1 parâmetro público (Constraints), C2 "D12 da 1.4" (Task 8), C3 candidato
  da Task 6 vira nota sem mudança, C4 só o construtor de tabelas Huffman (Task 7), C5 `sony_restart_intervals` exige uma
  amostra Sony (Task 11), C6 CR2 em `sensor` documentado (Task 8), C7 `CheckedSample.size: u64` (Task 5), C8 ajudantes em
  `tests/support` (Tasks 4, 10), C9 passos por faixa e teto de caixas `moof` (Tasks 2, 6), C10 `capped` do pareamento
  documentado (Task 4), C11 sem amostras XAVC no histórico (Task 13) — custo: nenhum.

## Pendências herdadas com dono 1.6

| Pendência | Destino |
|---|---|
| 1.3: JPEG em 3+ pedaços | Tasks 2, 3, 4 (`jpeg_in_four_pieces_on_a_grid_is_unique`, cartões da Task 10) |
| 1.3: vídeo em 3+ pedaços, peça do meio com < 1/32 das amostras (C7) | Task 5 (`tiny_middle_piece_is_found`, `pairing_checks_every_sample`) |
| 1.3: fragmentação de RAW (reavaliar com cluster e prévia) | Tasks 7, 8 (D13) |
| 1.3: `moov` antes do `mdat`, `mdat` de tamanho 0, `moof`, dois vídeos com primeiros pedaços idênticos | Task 6 (D12, D17) |
| 1.3 Q16: JPEG com restart sai no máximo `Partial`; medir a Sony | Tasks 3 (régua mantida) e 11 (D21) |
| 1.4 D3: apagado e fragmentado sai `Partial` mesmo inteiro | Task 9 (G5, D14, D15) |
| 1.4: foto cortada seguida da cauda de uma quase igual, sem metadados | Tasks 3 e 10 (`same_frame_neighbour_tail_is_not_intact`, `no_intact_finding_is_glued`) |
| 1.4 T5: teto da fase de palavra alta; teste do corte da fila | Task 12 |
| 1.4 T6: confiança de diretório `NoFatChain`; chunk ilegível sem custo; não-fotos antes do bitmap | Task 12 |
| 1.4 T9: prévia de RAW que cruza um pedaço | Task 8 (`raw_preview_crossing_a_piece_is_not_reported_twice`) |
| 1.4 T10: continuação de pasta antiga exFAT; nó cortado por profundidade não memorizado | Task 12 |
| 1.5 C13: leitor de plist duplicado (dono "Módulo 2 ou 1.6") | Task 13 (decisão registrada: a 1.6 não mexe em plists; fica com o Módulo 2) |

Fora desta parte (design §3): decodificar RAW, formatos novos, NTFS/APFS/HFS+, copiar e buscar ao mesmo tempo, telas.

## Arquivos

- `crates/revenant-core/src/reassemble/` (novo, `pub(crate)`): `mod.rs` (`Limits`, `Taken`, `Space`, `Seams`, `Walked`,
  `WalkEnd`, `Proof`, `Search`, `Budget`, `search`, `reassemble`, `content_checks`, `improves`), `jpeg.rs` (`JpegSeams`),
  `video.rs` (`VideoSeams`, `MoofSeams`), `raw.rs` (`RawSeams`).
- `crates/revenant-core/src/formats/jpeg_lossless.rs` (novo): JPEG sem perdas (SOF3) para conferir blocos.
- `crates/revenant-core/src/formats/{jpeg_join.rs,jpeg_entropy.rs,isobmff.rs,mod.rs}`, `src/tiff.rs`, `src/pairing.rs`,
  `src/scan/{mod.rs,state.rs,state_tests.rs,fs.rs}`, `src/filesystem/{listing.rs,exfat.rs,fat_deleted.rs,orphans.rs}`,
  `src/lib.rs`.
- `crates/revenant-testkit/src/{lib.rs,fs.rs}`; `crates/revenant-dev/src/bench.rs` (imagem `scattered-card`).
- Testes novos: `crates/revenant-core/tests/{reassembly.rs,reassembly_cards.rs}`; ampliados: `tests/{jpeg.rs,pairing.rs,
  isobmff.rs,tiffraw.rs,scan_fs.rs,fs_exfat.rs,fs_fat.rs,real_samples.rs}`, `tests/support/mod.rs` (cartão `f`),
  `crates/revenant-testkit/tests/fs_builder.rs`.

---

### Task 1: Testkit — alocação "próximo livre", apagar no computador, rajada e vídeos de câmera

**Files:**
- Modify: `crates/revenant-testkit/src/fs.rs` (`Alloc`, `FsBuilder::alloc`, `DeleteStyle::ZeroChain`),
  `crates/revenant-testkit/src/lib.rs` (`jpeg_burst`, `VideoCodec`, `VideoLayout`, `camera_video`; `avc_video` delegates)
- Test: `crates/revenant-testkit/tests/fs_builder.rs`, new `crates/revenant-testkit/tests/media.rs` (byte-level checks
  only: the testkit never depends on the engine it tests), and the parse checks in `crates/revenant-core/tests/{jpeg.rs,
  isobmff.rs}`
- Docs: testkit doc comments only (the testkit has no README); CHANGELOG: nothing (not user-visible)

**Interfaces:**
- Produces:
  ```rust
  // fs.rs
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
  pub enum Alloc {
      /// The lowest free cluster (today's behavior): fills the first gap.
      #[default] FirstFit,
      /// The first free cluster at or after the one after the last cluster allocated, wrapping to cluster 2 at the end
      /// (a rotating pointer, as many cameras and FAT drivers do).
      NextFree,
  }
  impl FsBuilder { pub fn alloc(&mut self, a: Alloc) -> &mut Self; }  // affects every later `add`/`mkdir`
  pub enum DeleteStyle { Camera, WindowsFat32, /// exFAT: as Camera and every FAT entry of the chain set to 0 (a computer
      /// that clears the chain); FAT: same as Camera.
      ZeroChain }
  // lib.rs
  /// `n` photos of one scene and one frame (same size, tables and sampling): photo k is `jpeg_smooth`'s scene shifted by
  /// `k` pixels right, so neighbouring photos have near-equal DC grids (the "burst" a seam test must still tell apart).
  pub fn jpeg_burst(width: u32, height: u32, seed: u64, n: usize) -> Vec<Vec<u8>>;
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum VideoCodec { Avc, Hevc }
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum VideoLayout { MoovAtEnd, MoovFirst, /// `ftyp, moov, mdat` with the mdat size field 0 (to the end of the file)
      MoovFirstMdatSizeZero, /// `ftyp, moov (with mvex/trex, empty sample tables), then (moof, mdat)` per fragment
      Fragmented { samples_per_fragment: usize } }
  /// A camera video. Hevc: `hvc1` sample entry with an `hvcC` (lengthSizeMinusOne = 3), NAL headers of 2 bytes (type 19
  /// for the first sample of a chunk, else 1; `nuh_temporal_id_plus1 = 1`). `AvcVideo::moov` is the `moov` box wherever
  /// it is; `mdat_payload` the first `mdat`'s payload.
  pub fn camera_video(opts: &AvcOptions, codec: VideoCodec, layout: VideoLayout) -> AvcVideo;
  // avc_video(o) == camera_video(o, VideoCodec::Avc, VideoLayout::MoovAtEnd), byte for byte
  ```

- [ ] **Step 1: Failing tests**
  ```rust
  // fs_builder.rs
  #[test] fn next_free_allocation_rotates_past_freed_gaps() {
      // Exfat 32 MiB / 32 KiB, alloc(NextFree): add A (4 clusters), B (4), delete A (Camera), add C (6 clusters):
      // C's clusters == the 6 after B's last (not A's freed ones); with FirstFit (default) C starts at A's first cluster
      // and holds A's 4 then 2 after B (clusters(path) asserted exactly)
  }
  #[test] fn zero_chain_delete_clears_the_exfat_chain() {
      // Exfat, a FAT-chained file (add_at non-consecutive clusters [10, 11, 20, 21]) deleted with ZeroChain: the FAT
      // entries of 10, 11, 20, 21 read 0 in the built image; with Camera they keep 11, End/20... (read with fat_entry helper)
  }
  // media.rs
  #[test] fn burst_photos_share_a_frame_and_differ() {
      // jpeg_burst(512, 384, 7, 3): 3 photos, each decodes with the testkit's `image` crate, the bytes from SOI to the
      // end of SOS are equal in all three, the photos are pairwise different
  }
  #[test] fn avc_video_is_camera_video_moov_at_end() { /* for seeds 1..4: avc_video(o).bytes == camera_video(o, Avc, MoovAtEnd).bytes */ }
  // crates/revenant-core/tests/jpeg.rs
  #[test] fn burst_photos_are_intact_alone() { /* each of jpeg_burst(512, 384, 7, 3) measured at 0 → Intact, len == bytes.len() */ }
  // crates/revenant-core/tests/isobmff.rs
  #[test] fn camera_videos_parse_in_every_layout() {
      // for codec in [Avc, Hevc], layout in [MoovAtEnd, MoovFirst, MoovFirstMdatSizeZero, Fragmented{8}]: IsoBmff.measure
      // at 0 in a Vec source → Some, health Intact, len == bytes.len()
  }
  ```
- [ ] **Step 2:** `cargo test -p revenant-testkit` and `cargo test -p revenant-core --test jpeg --test isobmff` → FAIL (no
  `Alloc`, `ZeroChain`, `jpeg_burst`, `camera_video`).
- [ ] **Step 3:** implement as in Interfaces (the builder's allocator keeps a `next` pointer next to `first_free`).
  `camera_video` builds HEVC's `hvcC` with the 23-byte fixed header (configurationVersion 1, `lengthSizeMinusOne` in byte
  21 low bits) and no arrays; the fragmented layout writes `mvex/trex` and per fragment `moof(mfhd, traf(tfhd
  default-base-is-moof, trun with sample sizes and data offset))` then `mdat`.
- [ ] **Step 4:** `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` → green.
- [ ] **Step 5:** Commit: `test(testkit): next-free allocation, zeroed chains, bursts and camera video layouts`.

---

### Task 2: O motor de busca de emendas e o espaço livre

**Files:**
- Create: `crates/revenant-core/src/reassemble/mod.rs`; Modify: `src/lib.rs` (`pub(crate) mod reassemble;`),
  `src/pairing.rs` (`Grid` re-exported, `grid_of` → `pub(crate)`)
- Test: unit tests in `reassemble/mod.rs` (a toy format)
- Docs: core README new section "Reassembly" (the search, the space, the order of candidates, the rule for `Intact`, the
  budgets with their values); CHANGELOG: nothing yet

**Interfaces:**
- Consumes: `Fragment`, `Reader`, `SECTOR` (`formats`), `Runs` (`cache`), `Fragmented` (`source`), `CancelToken`,
  `pairing::Grid = (u64, u64, u64)` (heap, cluster, heap end).
- Produces (crate-private):
  ```rust
  #[derive(Clone, Debug)]
  pub(crate) struct Limits {
      pub pieces: usize,          // MAX_PIECES = 16 on a grid
      pub pieces_no_grid: usize,  // 4
      pub seam_candidates: usize, // SEAM_CANDIDATES = 4096
      pub seam_window: u64,       // SEAM_WINDOW = 256 MiB, on a grid
      pub no_grid_window: u64,    // NO_GRID_WINDOW = 64 MiB (the 1.3 JPEG_GAP_MAX), without a grid
      pub break_points: usize,    // BREAK_POINTS = 64
      pub file_tries: u64, pub scan_tries: u64,   // 2^16, 2^22
      pub file_read: u64, pub scan_read: u64,     // 512 MiB, 16 GiB
      pub candidates: usize,      // MAX_CANDIDATES = 4096 findings per scan
      pub claimed_runs: usize,    // MAX_CLAIMED_RUNS = 2^18
  }
  impl Limits { pub(crate) const DEFAULT: Limits; }
  /// Bytes no piece may use: `Listing.live` and the pieces of every `Intact` finding (and `Intact` reassembly so far).
  pub(crate) struct Taken { .. }
  impl Taken {
      pub(crate) fn new(live: &[Fragment], claimed: &Runs) -> Taken;
      pub(crate) fn claim(&mut self, pieces: &[Fragment]);
      /// The claimed `Intact` ranges overlapping `[start, end)`, ascending. `Runs` merges touching ranges, so these are
      /// merged ranges, not one finding's piece: the caller tests coverage by `own` (Q7), never equality.
      pub(crate) fn overlapping(&self, start: u64, end: u64) -> Vec<Fragment>;
  }
  pub(crate) struct Space<'a> { .. }
  impl<'a> Space<'a> {
      /// For a file starting at `start`: the grid of `grids` holding it at a cluster start (cluster ≥ 4096), else none.
      pub(crate) fn new(grids: &[Grid], start: u64, source_len: u64, taken: &'a Taken, starts: &'a [u64],
                        limits: &Limits) -> Space<'a>;
      pub(crate) fn unit(&self) -> u64;        // cluster size, or SECTOR
      pub(crate) fn on_grid(&self) -> bool;
      pub(crate) fn max_pieces(&self) -> usize;
      /// Bytes free in a row from `g` (whole units, at most `max`): stops at a taken range (unless every claimed byte of
      /// it is covered by `own` ranges at their exact absolute positions: a coverage test, not equality, because `Runs`
      /// merges touching ranges: Q7), at a range of `used`, at a finding start other than `g` itself or an `own` start
      /// (D9, D10), at the heap end or the source end. Steps over taken ranges by range, never per cluster (a 2 TB heap).
      pub(crate) fn free_run(&self, g: u64, used: &[Fragment], own: &[Fragment], max: u64) -> u64;
      /// Candidate starts after a piece ending at `end` (D4): free unit starts ascending from `end` up to `end + window`,
      /// then (grid only) from the heap start up to `end`; never inside `used`. `keep` (the seam's prefilter, which
      /// `search` wraps so that its reads are charged to the file's read budget and memoized per file by (absolute start,
      /// logical seam)) is applied WHILE enumerating, and `cap` counts the survivors only (Q1). The enumeration is
      /// bounded by window/unit and steps over taken ranges by range. The `bool` is `true` when more survivors existed
      /// than `cap` ("cap hit"); a window cut is NOT a cap hit (Q4).
      pub(crate) fn candidates(&self, end: u64, used: &[Fragment], cap: usize,
                               keep: &mut dyn FnMut(u64) -> bool) -> (Vec<u64>, bool);
  }
  pub(crate) trait Seams {
      type State: Clone;
      /// The state before the file's first byte (logical 0).
      fn begin(&mut self) -> Self::State;
      /// Walks the file's logical bytes `view` (the pieces so far, then a candidate run) from `from` up to `limit`.
      fn walk(&mut self, view: &dyn Source, from: &Self::State, limit: u64) -> Walked<Self::State>;
      /// Final proof over the whole file, `seams` = logical positions where one piece ends and the next begins.
      fn prove(&mut self, view: &dyn Source, seams: &[u64]) -> Proof;
      /// The file's length when the format says it (moov sample table, TIFF extent).
      fn known_len(&self) -> Option<u64> { None }
      /// Logical offset `l` that must lie at absolute `p` (a video's orphan `moov`).
      fn anchor(&self) -> Option<(u64, u64)> { None }
      /// Logical `[offset, len)` ranges of the file's own embedded files (D10).
      fn own(&self) -> Vec<Fragment> { Vec::new() }
      /// A cheap reject of candidate `g` for the seam at logical `seam` (default: keep). Called while the candidates are
      /// listed (Q1); `r` reads through the counted source, so its bytes are charged to the file's read budget.
      fn prefilter(&mut self, r: &mut Reader, g: u64, seam: u64) -> bool { let _ = (r, g, seam); true }
  }
  pub(crate) struct Walked<S> {
      pub end: WalkEnd,
      /// Logical end of the last unit the walk checked by content (its evidence).
      pub checked: u64,
      /// States where the walk may continue in another piece, as (logical position, state), ascending; the last one at
      /// or before `checked`.
      pub resume: Vec<(u64, S)>,
  }
  pub(crate) enum WalkEnd { Complete { len: u64 }, Broken { at: u64 }, Stopped }
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) enum Proof { Proven, Unproven, Failed }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub(crate) struct Search {
      pub pieces: Vec<Fragment>,  // absolute, file order; one piece = contiguous
      pub len: u64,
      pub complete: bool,         // the walk reached the file's end (and the known size)
      pub unique: bool,           // exactly one complete solution
      pub exhaustive: bool,       // no cap or budget hit anywhere
      pub proof: Proof,
  }
  pub(crate) struct Budget { .. }  // tries and bytes left, per scan; `for_file()` derives the per-file one
  impl Budget { pub(crate) fn new(limits: &Limits) -> Budget; pub(crate) fn spent(&self) -> bool; }
  /// `known`: the file-system entry's size for a Deleted-origin file (Q3), else `None`. The effective known length is the
  /// format's (`Seams::known_len`) and `known`; when both are `Some` they must agree, else no solution is `complete`.
  /// When any is known, the pieces must sum to it exactly.
  pub(crate) fn search<S: Seams>(seams: &mut S, source: &dyn Source, space: &Space, start: u64, known: Option<u64>,
                                 limits: &Limits, budget: &mut Budget, cancel: &CancelToken) -> Option<Search>;
  ```
  The search (an algorithm the signatures do not fix):
  ```text
  dfs(pieces, state, piece_start g, logical L):
    run  = space.free_run(g, used = pieces, own = seams.own() mapped, max = known − L or seam_window)  // known = effective
    view = Fragmented(source counted, pieces ++ [g, run]);   w = seams.walk(view, state, L + run)
    charge 1 try + bytes read; on budget/cancel: exhaustive = false, stop
    Complete{len} (len == known when known, anchor satisfied)  → record solution (stop at the 2nd: ambiguous)
    Broken{at}:
      if pieces.len()+1 == max_pieces → record best partial, exhaustive = false, return
      floor  = the last resume position ≤ w.checked (or L)
      breaks = logical b in (floor, at] with g + (b − L) a unit boundary, nearest to `at` first, ≤ break_points
      for b in breaks: (pos, st) = last resume with pos ≤ b
        cands = anchor-implied start first (if free), then space.candidates(abs(b), pieces ++ [g, b−L], cap, prefilter)
                (the prefilter runs inside; the cap counts survivors; its reads are charged to the file's budget)
        for g2 in cands: w2 = walk from st over pieces ++ [g, b−L] ++ [g2, run2]
            passes if Complete, or w2.checked > b (evidence past the seam)          → evidence list
            blind  if neither broken nor evidence before the end of run2          → blind list
            broken                                                                  → dropped
        // Q2: BLIND candidates are descended into too, always, never only when no candidate had evidence. A true
        // continuation that is blind (a 1-cluster free run shorter than one unit of evidence) must be able to make an
        // evident look-alike neighbour NON-unique.
        for g2 in evidence list (D4 order), then blind list (D4 order): dfs(..., g2, b)   // a path whose own seam stayed
                                                                                          // blind is Unproven
        every complete solution through either list counts for `unique`; a blind descent that the budget or a cap cuts
        short sets exhaustive = false
    Stopped → exhaustive = false
  result: first solution in candidate order (evidence candidates before blind ones at each seam; else best partial = most
  logical bytes checked); unique = exactly one solution over evidence AND blind paths; proof = prove(view of that
  solution, seams) min Unproven if any seam was blind
  ```

- [ ] **Step 1: Failing tests** (unit, `reassemble/mod.rs`; toy format `Toy`: a file of `n` units of 4096 bytes, unit i =
  `[id: u32 LE][i: u32 LE][n: u32 LE][seeded bytes]`; `Toy::walk` checks units in order (id, index, n), `Complete` after
  unit `n − 1`, `Broken` at the first wrong unit; `blind: Range<u32>` units are not checked (no evidence); `prove` gives
  `Unproven` if a seam falls in a blind unit, else `Proven`; grid = heap 1 MiB, cluster 4096, in a 16 MiB `Mem`):
  ```rust
  #[test] fn space_lists_free_cluster_starts_in_prior_order() {
      // taken: live [2 MiB, 3 MiB); claimed [5 MiB, 5 MiB + 8 KiB); candidates(end = 4 MiB, used [], cap 8, keep all) ==
      // [4 MiB, 4 MiB + 4K, ..] (8 of them) and true; candidates(end = 15 MiB + 1 MiB − 8 KiB, ..) == the 2 forward
      // ones then 1 MiB, 1 MiB + 4K, ... (first gap), skipping [2 MiB, 3 MiB)
  }
  #[test] fn space_without_grid_is_forward_only_on_sectors() { /* no grids: unit 512, candidates only in (end, end + 64 MiB) */ }
  #[test] fn candidate_cap_counts_survivors_not_enumerated_units() {
      // keep accepts 1 start in 8: with cap 8 over a window of 1000 free units the 8 survivors come back (the 64th unit
      // is the 8th survivor) and the bool is false; with 125+ survivors available and cap 8 the bool is true; a window
      // that cuts the enumeration with fewer than cap survivors gives false (a window cut is not a cap hit)
  }
  #[test] fn prefilter_reads_are_charged_and_memoized() {
      // a Toy prefilter reading 16 bytes per candidate: the budget's bytes grow by 16 per distinct (start, seam), and a
      // second seam query of the same pair costs nothing
  }
  #[test] fn free_run_stops_at_taken_used_and_finding_starts() {
      // run from 4 MiB stops at a claimed range, at a used piece, at a finding start at 4 MiB + 12 KiB; does not stop at
      // a claimed range wholly covered by `own` at its exact positions (also when two claimed ranges touch and `Runs`
      // merged them), and does stop at one 512 B off or only partly covered
  }
  #[test] fn toy_file_in_five_pieces_is_unique() {
      // a 40-unit toy file at 1 MiB in 5 runs of 8 units, the runs placed out of order with toy noise (other id) in the
      // gaps, gaps on free clusters → Search { pieces == the 5 runs, complete, unique, exhaustive, proof: Proven }
  }
  #[test] fn toy_twin_continuations_are_ambiguous() {
      // the 3rd run copied twice (both free): unique false, exhaustive true, pieces use the copy first in D4 order
  }
  #[test] fn toy_budget_hit_is_not_exhaustive() { /* Limits { file_tries: 3, .. }: exhaustive false, complete false */ }
  #[test] fn piece_cap_is_respected() { /* 6 runs, Limits { pieces: 4 }: complete false, pieces.len() == 4 */ }
  #[test] fn known_len_must_match() { /* known_len = 41 units: the 40-unit solution is not complete */ }
  #[test] fn entry_size_must_match_when_known() {
      // search(.., known: Some(41 units)) with a format that gives no known_len: the 40-unit solution is not complete;
      // known Some(40) completes; known Some(40) with known_len 41 (they disagree): not complete
  }
  #[test] fn anchor_fixes_the_last_piece() {
      // the last run copied at two places, anchor (logical 32 × 4096, the first copy's start): pieces end with that copy,
      // unique true
  }
  #[test] fn blind_seam_is_placed_in_prior_order_and_unproven() {
      // units 16..40 blind, a seam at unit 24. With blind data the walk never breaks, so a piece ends only where
      // `free_run` stops (Q14): the gaps between the runs are TAKEN (claimed `Intact` ranges or finding starts), nothing
      // else. The run is placed at the first D4 candidate (the true one, next free), complete true, proof Unproven
  }
  #[test] fn blind_true_continuation_next_to_an_evident_look_alike_is_not_unique() {
      // Q2. Toy file of 40 units, unit 8 blind (`blind: 8..9`). Run A = units 0..8 at 1 MiB. The TRUE continuation is
      // T = unit 8 alone, on a single free cluster with taken ranges on both sides (a 1-unit run, blind: no evidence),
      // followed by the rest R = units 9..40 elsewhere. Next to it a free LOOK-ALIKE L: a run carrying units 8..40 with
      // correct headers (id, index, n; other noise bytes), so it has evidence past the seam and completes on its own.
      // Both reach 40 units: search → complete true, unique FALSE, exhaustive true; and the result is never
      // (unique && proof == Proven). On the old rule (blind descended into only when no candidate had evidence) only L
      // is explored and the result is unique: this test is RED there. Without L the same card gives unique true with
      // proof Unproven (the seam at unit 8 stayed blind)
  }
  #[test] fn budget_cut_inside_a_blind_descent_is_not_exhaustive() {
      // the same card with Limits { file_tries } just enough for the evidence path but not for the blind one:
      // exhaustive false
  }
  #[test] fn cancel_stops_the_search() { /* cancelled token → None, no try counted */ }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core reassemble` → FAIL (no module).
- [ ] **Step 3:** implement as in Interfaces. `Counted` (private) wraps the source like `jpeg_join::Overlay` and counts
  bytes read; the budget charges them per try. Cancel is looked at before each try and through `formats::cancelled()` (the
  search runs inside a `CancelScope`).
- [ ] **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README "Reassembly" (new section after "Findings in pieces": what a seam is, the space and D3/D4,
  break points D5, the `Intact` rule D6, budgets D7 with their values, the reading rule D8, and, stated plainly,
  that uniqueness holds only inside the 256 MiB window (64 MiB without a grid): a look-alike beyond it could leave a
  false unique seam, the content proof being the guard: Q4; blind candidates count toward uniqueness: Q2). Commit:
  `feat(core): seam search engine over the free clusters of a volume`.

---

### Task 3: JPEG em 3 ou mais pedaços

**Files:**
- Create: `crates/revenant-core/src/reassemble/jpeg.rs`
- Modify: `src/formats/jpeg_join.rs` (`seams_ok`, `early_base`, `boundary_means`, `within`, `Dc`, `seam_mean` →
  `pub(crate)`: `boundary_means` takes the private `Dc`), `src/formats/jpeg.rs` (`decode_health`, `walk_header`,
  `preview_health` → `pub(crate)`; Tasks 4, 8, 9 use them: Q12), `src/formats/jpeg_entropy.rs` (nothing new public
  unless needed: `Cursor`, `decode_rows`, `Frame` are already `pub(crate)`)
- Test: unit tests in `reassemble/jpeg.rs` and `jpeg_join.rs`
- Docs: core README "Fragmented JPEG" (3+ pieces, the multi-seam test), CHANGELOG nothing yet (wired in Task 4)

**Interfaces:**
- Consumes: Task 2 (`Seams`, `Walked`, `WalkEnd`, `Proof`, `search`, `Space`, `Limits`, `Budget`); 1.3 `jpeg_entropy::
  {frame, decode_rows, Cursor, Frame, End}`, `jpeg::walk_header`, `jpeg_join::{early_base, boundary_means, within}`.
- Produces:
  ```rust
  // jpeg_join.rs
  /// The DC seam test with several seams: the base is the median of the block-row boundary means off every seam; each
  /// seam's mean must be ≤ 3 × base + 4; fewer than 4 block rows → false. `seam_ok(f, dc, s) == seams_ok(f, dc, &[s])`.
  pub(crate) fn seams_ok(frame: &Frame, dc: &[i32], seams: &[u64]) -> bool;
  // reassemble/jpeg.rs
  pub(crate) struct JpegSeams { .. }   // frame, scan_start, one DC buffer per search (truncated on backtrack, D11)
  impl JpegSeams {
      /// `None` when the JPEG at `start` has no frame the 1.3 decoder covers.
      pub(crate) fn new(source: &dyn Source, start: u64) -> Option<JpegSeams>;
      pub(crate) fn restart_interval(&self) -> u16;
  }
  impl Seams for JpegSeams { type State = JpegState; /* Cursor + MCU index; `checked` = the position after the last
      complete MCU row; a candidate passes with ≥ one MCU row decoded below the seam and the early test (`early_base`)
      not rejecting it; `prove` = Proven iff `seams_ok` on the whole DC grid with the seams' MCU indexes; prefilter = the
      1.3 window rule on the candidate's first sector (a non-data marker, a uniform or unreadable sector → reject), run
      while the candidates are listed, its reads charged to the file budget (Q1) */ }
  ```

- [ ] **Step 1: Failing tests:**
  ```rust
  // jpeg_join.rs
  #[test] fn seams_ok_with_one_seam_is_seam_ok() { /* for the grids of `seam_check_rejects_a_glued_dc_grid`: same answers */ }
  #[test] fn seams_ok_rejects_a_second_glued_seam() {
      // rows 0..16 gradient A, 16..32 A (seam at 16 true), 32..48 gradient B: seams [16×64, 32×64] → false; seams [16×64]
      // over the all-A grid → true
  }
  // reassemble/jpeg.rs (helper `scatter(jpeg, cluster, order)`: the JPEG cut at cluster multiples into runs placed at the
  // given clusters of a Mem with seeded noise elsewhere; grid heap 1 MiB, cluster 32 KiB)
  #[test] fn jpeg_in_four_pieces_on_a_grid_is_unique() {
      // jpeg_smooth(2048, 1536, 3) cut in 4 runs placed at clusters [40, 90, 61, 200] (each ≥ 2 clusters) → Search pieces
      // == those runs in file order, complete, unique, exhaustive, Proven
  }
  #[test] fn same_frame_neighbour_tail_is_not_intact() {
      // jpeg_burst(1024, 768, 5, 2) = [a, b]: a's runs at [40, 61, 90], b's at [44, 70, 120]; both last runs free:
      // the search of a never returns b's run as a piece with unique && Proven (either a's own runs unique, or
      // unique false); same for b
  }
  #[test] fn blind_one_cluster_continuation_next_to_a_burst_tail_is_not_intact() {
      // Q2. A wide photo (jpeg_burst(4096, 1024, 5, 2), asserted in the test: one MCU row is larger than one 32 KiB
      // cluster) whose middle piece is exactly 1 cluster with taken neighbours (blind: less than one MCU row), the rest
      // after it; the burst neighbour's tail is free and passes: the search of the photo is never (unique && Proven) —
      // either the true path is found and unique is false, or the result is not Intact
  }
  #[test] fn restart_interval_is_reported() { /* jpeg_flat_with_restarts(512, 384, 4): restart_interval() == 4 */ }
  #[test] fn jpeg_without_grid_in_three_pieces_within_the_window() {
      // no grids: jpeg_smooth(1024, 768, 1) cut at sector multiples in 3 runs, the 2nd 3 MiB ahead and the 3rd 7 MiB ahead
      // → pieces exact, unique. The prefilter runs during the enumeration and the 4096 cap counts its survivors (Q1):
      // the 64 MiB window of 512 B sectors is reached, not cut at 2 MiB
  }
  #[test] fn erased_gap_is_not_a_continuation() { /* runs separated by zeroed clusters: the zeroed clusters never appear in pieces */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (D11). `walk` decodes with `decode_rows` from the state over `view`, keeps
  DC, records a resume point at each MCU-row boundary (bounded by the 1.3 `RESUME_POINTS`), `Complete { len }` at `EOI`
  with every MCU (`len` = EOI + 2), `Broken { at }` at a decode error, a uniform sector (`data_end`) or the run end before
  EOI. Restart markers: unchanged decoding; the Q16 cap is applied in Task 4's health rule.
- [ ] **Step 4:** suite, clippy, fmt; `cargo test --release -p revenant-core jpeg` (timing tests of 1.3 still green).
- [ ] **Step 5:** core README "Fragmented JPEG" (new paragraph: 3+ pieces through the reassembly step, the multi-seam test,
  Q16 unchanged). Commit: `feat(core): JPEG seams for reassembly in three or more pieces`.

---

### Task 4: A etapa de remontagem na busca — candidatos, `Updated`, estado e retomada

**Files:**
- Modify: `crates/revenant-core/src/reassemble/mod.rs` (`reassemble`, `improves`, health rule), `src/scan/mod.rs`
  (`Commit` fields, `emit_found`, `reassemble` step, `update` merge, docs of `scan`), `src/scan/state.rs` (+ codec),
  `src/session/store.rs` (`VERSION` 1 → 2, Q10), `src/scan/state_tests.rs`, `tests/support/mod.rs` (card `f`, helpers
  `final_view`, `exfat_card`), `src/pairing.rs` (`Video` → `pub(crate)` only if the reassembly needs it: Q12)
- Test: new `crates/revenant-core/tests/reassembly.rs`; `scan/state_tests.rs`
- Docs: core README "Scan" (the step after pairing), "Resumable scans" (what the state holds now), "Sessions" (the
  format version is now 2: sessions saved by 1.5 are refused by the version check; Q10), "Reassembly" (which findings
  are candidates, D2; the pairing cap `capped` also blocks JPEG reassembly from `Intact`, conservative: C10; a truncated
  listing blocks `Intact` through reassembly: Q5), CHANGELOG ("Added": photos in three or more pieces are put back
  together; "Changed": sessions saved by an earlier version are no longer resumable)

**Interfaces:**
- Consumes: Tasks 2 and 3; `Commit::{starts, grids, videos, moovs, capped}`, `FsPlan::live`, `pairing::pair_videos`.
- Produces:
  ```rust
  // reassemble/mod.rs
  pub(crate) struct Context<'a> {
      pub grids: &'a [Grid], pub live: &'a [Fragment], pub claimed: &'a Runs, pub starts: &'a [u64],
      pub moovs: &'a [(u64, u64)], pub capped: bool, pub listing_truncated: bool, pub limits: &'a Limits,
  }
  /// Every candidate (id order) searched; returns the findings it improves (same id, origin kept with placement Carved
  /// for a file-system file, D15). For a candidate of origin `Deleted`, `search` gets `known: Some(origin.size)` (Q3):
  /// the pieces must sum to the entry's size from this task on. `paired`: the pairing's results (their Intact pieces are
  /// claimed first). `listing_truncated`: any volume listing dropped entries, so live clusters may be unknown (Q5).
  pub(crate) fn reassemble(source: &CachedSource, candidates: &[Finding], paired: &[Finding], ctx: &Context,
                           cancel: &CancelToken) -> Vec<Finding>;
  /// Whether `new` should replace `old` (D6): strictly better health, or both Partial and `new` is complete (its walk
  /// reached the end) and longer.
  pub(crate) fn improves(old: &Finding, new: &Finding, complete: bool) -> bool;
  // scan/mod.rs — new Commit fields (captured, D16)
  frags: Vec<Finding>,       // reassembly candidates (D2), id order, ≤ Limits::candidates
  frags_capped: bool,
  claimed: Runs,             // pieces of every Intact finding emitted (D3), ≤ Limits::claimed_runs
  claimed_capped: bool,
  fn reassemble(&mut self, paired: &[Finding]) -> Result<Vec<Finding>, EngineError>;  // Cancelled when cancelled
  fn update(&mut self, paired: Vec<Finding>, reassembled: Vec<Finding>) -> Result<(), EngineError>; // reassembled wins per id
  ```
  Health of a result (D6): `Intact` iff `unique && exhaustive && complete && proof == Proven && !ctx.capped &&
  !frags_capped && !claimed_capped && !ctx.listing_truncated && bad_runs(pieces) empty && known size matches && the
  format's own check on the view
  is Intact && !(Jpeg && restart_interval > 0)`; else `Partial`. A result with 1 piece is contiguous (`fragments` empty).
  `unique` counts complete solutions through blind candidates too (Q2). Order in `scan_with`: `pair()` →
  `reassemble(&paired)` → `Checkpoint::Paired` → `update`.

- [ ] **Step 1: Failing tests** (`tests/reassembly.rs`; helper `final_view(events) -> BTreeMap<u64, Finding>` = `Found`
  replaced by `Updated` with the same id; helper `exfat_card()` = testkit `FsBuilder::new(Exfat, 64 MiB, 32 KiB)` in
  `disk(Mbr{8192})`; both helpers live in `tests/support/mod.rs` so `reassembly_cards.rs` reuses them: C8). Write and
  see the first test below pass its GREEN preconditions before the resume test relies on card F (Q11):
  ```rust
  // scan/state_tests.rs, FIRST: card F (see below) must really exercise the new state (Q11)
  #[test] fn the_cards_exercise_what_a_state_carries() {
      // card F: the scan emits at least one `Updated` produced by reassembly (R in 3 pieces, 3 pieces in the final
      // finding); the state captured at `Checkpoint::Paired` holds non-empty `frags` and non-empty `claimed`
  }
  // reassemble/mod.rs unit (next to the health rule)
  #[test] fn health_needs_every_condition() {
      // a Search that satisfies the whole D6 rule → Intact; each of unique, exhaustive, complete, Proven, !capped,
      // !frags_capped, !claimed_capped, !listing_truncated (Q5), no bad run, known size, own check, Q16 false alone
      // → Partial
  }
  #[test] fn session_version_one_is_refused() {
      // a session file written with the 1.5 format version (1) is refused by the store's existing version check, with
      // the same error it gives for any unknown version (asserted); a version-2 file round-trips (Q10)
  }
  #[test] fn three_piece_jpeg_on_a_quick_formatted_card_is_intact() {
      // exfat_card: 6 photos P1..P6 (jpeg 1024×768 seeds 1–6) added one after another; R (jpeg_smooth 2048×1536 seed 11)
      // added with add_at in 3 runs of ≥ 2 clusters placed after P2, after P4 and after P6; Q (seed 9) in 2 runs after P3
      // and after R's last run; build_quick_formatted (no names left). Scan: R's final finding has 3 pieces == R's
      // clusters, Intact, bytes (extract to a temp dir) == R; Q's has 2 pieces, Intact
  }
  #[test] fn reassembly_events_identical_across_threads_and_caches() { /* the card above: threads [1, 2, 8, 16] × cache [default, 1 MiB] → equal events */ }
  #[test] fn pieces_never_overlap_live_files_or_another_intact() {
      // card with live files around 3 deleted fragmented JPEGs (exFAT, Camera delete on a FAT-chained layout via add_at):
      // no final finding's piece overlaps a live file's clusters; Intact findings' pieces pairwise disjoint, except a
      // RAW and its own nested preview finding, which are the same file (Q16): the oracle exempts nested own findings
  }
  #[test] fn reassembly_never_rereads_known_bad_sectors() {
      // support::SimSource over the first card with a bad sector inside R's 2nd run: the final R is Partial; in the read
      // log, no read touching that sector comes after the first one-sector read of it (the scan's resolution)
  }
  #[test] fn hostile_card_stays_within_budget() {
      // a 256 MiB image (no volume): a jpeg_smooth 2048×1536 first piece, then 4096 sectors each starting with the same
      // valid-looking continuation bytes and ending in FF D9: scan finishes, the JPEG final finding is not Intact
      // (Partial or unchanged). No wall-clock assertion (Q8): the time bound lives in Task 11's ignored measure, and the
      // work bound is the counted-tries assert of `toy_budget_hit_is_not_exhaustive`
  }
  #[test] fn cancel_during_reassembly_is_prompt() {
      // deterministic (Q8): the hostile image behind a source wrapper that trips the CancelToken at the FIRST read past
      // the scan's end (i.e. inside reassembly); the scan returns Err "cancelled". No timers
  }
  #[test] fn live_files_are_never_candidates() { /* a live fragmented JPEG (add_at, 3 runs): Found Intact with the chain, no Updated */ }
  // scan/state_tests.rs (card `support::cards::f()`: the card of `three_piece_jpeg_on_a_quick_formatted_card_is_intact`
  // plus one bad sector inside P5, as a `Card`, scanned with `block: 1 MiB`)
  #[test] fn resume_with_reassembly_equals_the_whole_scan() {
      // card F, threads [1, 8], resume at the first, the last, 4 seeded boundaries and the Paired point → E[..n] ++ resumed == E
  }
  // and `state_round_trips` / `resume_at_any_block_equals_the_whole_scan` gain card F
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. `emit_found` pushes candidates (D2) and claims the ranges of `Intact`
  findings (D3, D18); `capture` destructures the new fields (compile error until listed), the codec writes them (`Runs`
  as `(start, end)` like `own_moovs`), `apply` checks the caps (`SessionCorrupt` past them); `session::store::VERSION` becomes 2 (Q10). In this task only JPEG
  candidates are searched (videos and RAW keep today's results until Tasks 5–8 plug their seams into `reassemble`).
- [ ] **Step 4:** suite, clippy, fmt; `cargo test --release -p revenant-core`; `bench --images noise,dense-jpeg,exfat-card`
  (release, best of 3) on this commit and its parent, interleaved twice, control rebuild as in 1.4/1.5: within 5%, else
  stop and report (D20).
- [ ] **Step 5:** docs as listed; CHANGELOG "Added: photos split into three or more pieces on a card are put back
  together; they are reported intact only when every seam is proven and no other continuation fits". Commit:
  `feat(core): reassemble fragmented JPEGs after the scan`.

---

### Task 5: Vídeo em 3 ou mais pedaços — índice, NAL H.264/H.265 e a peça do meio (C7)

**Files:**
- Create: `crates/revenant-core/src/reassemble/video.rs`
- Modify: `src/formats/isobmff.rs` (`CheckedSample`, `check_sample`), `src/pairing.rs` (uses `check_sample`; `fits`,
  `load_moov` and the struct `Video<'f>` → `pub(crate)`, needed by `VideoSeams::with_orphan`: Q12; `Intact` needs every
  sample checked), `src/reassemble/mod.rs` (video candidates)
- Test: unit tests in `reassemble/video.rs`, `isobmff.rs`, `pairing.rs`; `tests/pairing.rs` (unchanged, must stay
  green); `tests/reassembly.rs`
- Docs: core README "Video with a separated index" (C7 closed: every sample), "Reassembly" (videos), CHANGELOG

**Interfaces:**
- Consumes: Task 2; `isobmff::{index_tracks, Check, moovless_layout, index_health}`, `pairing::{load_moov, fits}`.
- Produces:
  ```rust
  // isobmff.rs
  #[derive(Clone, Copy, Debug)] pub(crate) struct CheckedSample { pub offset: u64, pub size: u64, pub check: Check }
  // (`size: u64` as `isobmff::Sample.size`; a NAL walk converts with a checked conversion: C7)
  /// The 1.3 structural check of one sample (NAL lengths and header bytes with `length_size` from avcC/hvcC, or SOI/EOI
  /// for Motion-JPEG), reading through `read(file offset, buf) -> bool`; `steps` is charged per NAL unit, false when spent.
  pub(crate) fn check_sample(read: &mut dyn FnMut(u64, &mut [u8]) -> bool, s: &CheckedSample, steps: &mut u64) -> bool;
  // reassemble/video.rs
  pub(crate) struct VideoSeams { .. }
  impl VideoSeams {
      /// A moov-less video at `start` (its `MdatLayout`) with the orphan `moov` at `q`: known length P + moov size, anchor
      /// (P, q), samples from the checkable tracks (≤ the pairing's sample budget).
      pub(crate) fn with_orphan(source: &dyn Source, start: u64, q: u64, size: u64) -> Option<VideoSeams>;
  }
  impl Seams for VideoSeams { type State = usize /* next sample index */; /* walk: samples in file order, each wholly
      inside the view checked; `checked` = end of the last passing sample; Broken at the first failing one; Complete
      when the view reaches the known length with every sample passing and the moov bytes at the anchor; prove = Proven
      iff every seam lies inside or between checked samples and every checkable sample passed; prefilter (Q1) = the
      first sample that starts past the seam, at its mapped position, passes `check_sample` (a seam with no checkable
      sample after it is blind and, by Q2, still descended into) */ }
  ```

- [ ] **Step 1: Failing tests:**
  ```rust
  // isobmff.rs unit
  #[test] fn check_sample_matches_the_pairing_check() { /* testkit avc_video and camera_video(Hevc): every sample passes in place; one shifted by 512 fails */ }
  // pairing.rs unit (calls `pair_with` directly, no reassembly)
  #[test] fn pairing_checks_every_sample() {
      // C7: avc_video 256 samples small_nals, cut in 3: [0, 2/5), a middle piece holding 4 samples placed elsewhere, the
      // rest + moov → the 2-piece pairing result is never Intact (on the parent commit it is: record RED)
  }
  // tests/reassembly.rs (exFAT card 64 MiB / 32 KiB, videos added with add_at at chosen clusters, then deleted Camera)
  #[test] fn h264_video_in_five_pieces_with_moov_at_end_is_intact() { /* avc_video 300 samples small_nals in 5 runs → final finding 5 pieces == clusters, Intact, bytes equal */ }
  #[test] fn h265_video_in_four_pieces_is_intact() { /* camera_video(o, Hevc, MoovAtEnd) in 4 runs → Intact */ }
  #[test] fn tiny_middle_piece_is_found() { /* 256 samples, a middle run of 1 cluster holding < 1/32 of the samples → 3 pieces, Intact */ }
  #[test] fn two_clips_of_one_camera_interleaved_are_not_swapped() {
      // two avc_video (seeds 1, 2, same options) written alternating cluster runs (NextFree), both deleted: each final
      // finding's bytes == its own clip, or it is not Intact; never a clip's run inside the other's Intact pieces
  }
  #[test] fn large_nal_video_on_small_clusters_is_at_most_partial_when_ambiguous() {
      // small_nals false (2–24 KiB NAL units), cluster 4096, 4 runs: whatever comes out, an Intact one has bytes == the
      // clip (asserted), and the outcome (Intact/Partial) is recorded in the report
  }
  ```
- [ ] **Step 2:** FAIL (C7 pairing test included). **Step 3:** implement (D12). `reassemble` takes the videos the pairing
  did not make `Intact`: for each, every orphan `moov` that passes `fits` (loaded with `load_moov` under the pairing's
  budgets) is one search; two or more searches with a complete solution ⇒ `unique = false`. Pairing: replace `spaced`
  with every sample (`MAX_STEPS` unchanged; a spent budget already makes it `Partial`).
- [ ] **Step 4:** suite, clippy, fmt; `cargo test --release -p revenant-core --test pairing --test reassembly`.
- [ ] **Step 5:** docs; CHANGELOG ("Added": videos in three or more pieces, H.264 and H.265; "Fixed": a video in three
  pieces with a tiny middle one is no longer put back together as two). Commit:
  `feat(core): reassemble videos in three or more pieces from their index`.

---

### Task 6: Os vídeos que a 1.3 deixou — `moov` antes, `mdat` 0, `moof`, primeiros pedaços idênticos

**Files:**
- Modify: `crates/revenant-core/src/reassemble/{video.rs,mod.rs}`, `src/formats/isobmff.rs` (a `moof`/`trun` reader:
  `fragment_samples`). No change to the candidate rule in `src/scan/mod.rs`: D2 already pushes every non-`Intact`
  MP4/MOV, so a `moov`-in-the-first-piece video is a candidate already (C3)
- Test: unit tests in `reassemble/video.rs`, `isobmff.rs`; `tests/reassembly.rs`; `tests/isobmff.rs`
- Docs: core README "Formats: MP4/MOV" (what is put back now), "Reassembly"; CHANGELOG

**Interfaces:**
- Produces:
  ```rust
  impl VideoSeams {
      /// A video whose `moov` is in its first piece (`ftyp, moov, mdat`): known length = the end of `mdat` when its size
      /// is declared, else (size 0) the highest sample end; no anchor.
      pub(crate) fn with_own_moov(source: &dyn Source, start: u64) -> Option<VideoSeams>;
  }
  // isobmff.rs
  /// The samples a `moof` box describes (`traf/tfhd/trun`: base data offset, default-base-is-moof, sizes), absolute in
  /// the file; `None` when garbled or past `MAX_SAMPLES`.
  pub(crate) fn fragment_samples(moof_at: u64, moof: &[u8], check: Check) -> Option<Vec<CheckedSample>>;
  pub(crate) struct MoofSeams { .. }  // reassemble/video.rs
  impl MoofSeams { pub(crate) fn new(source: &dyn Source, start: u64) -> Option<MoofSeams>; }
  impl Seams for MoofSeams { type State = (u64 /* next box offset */, usize); /* walk: top-level boxes in order,
      each header checked (size, type in moof/mdat/sidx/styp/free/skip/mfra), each moof's samples checked in the next
      mdat; Complete when an `mfra` closes the chain or the effective known size is reached (the FS size comes in through
      `search`'s `known`, Task 2/Q3; `MoofSeams` itself has none); else the walk ends at the last valid box (at most
      Partial). The box walk has a box-count cap (`MAX_BOXES`, C9) so a hostile `moof` chain is bounded by count, not
      only by bytes */ }
  ```
  D17 lives in `reassemble`: before searching, videos that fit the same `moov` are compared first piece to first piece
  (same length, bytes equal through the counted cache); equal ones are one group, only the lowest id is searched.

- [ ] **Step 1: Failing tests:**
  ```rust
  // tests/reassembly.rs
  #[test] fn moov_first_video_cut_into_three_is_intact() { /* camera_video(Avc, MoovFirst) 3 runs, deleted → Intact, 3 pieces */ }
  #[test] fn mdat_size_zero_with_moov_first_is_reassembled() { /* camera_video(Avc, MoovFirstMdatSizeZero) 3 runs → len == bytes.len(), Intact */ }
  #[test] fn fragmented_mp4_in_three_pieces_is_reassembled() {
      // camera_video(Hevc, Fragmented{16}) 3 runs, the FS entry deleted (known size): Intact; the same on a quick-formatted
      // card (no size, no mfra): Partial, complete pieces equal the clip's runs
  }
  #[test] fn identical_first_pieces_reassemble_once() {
      // the same moov-less video first piece written twice (two deleted entries, identical bytes), one orphan moov and the
      // rest: the lower id comes out reassembled (Intact), the other stays Damaged (no Updated for it)
  }
  // isobmff.rs unit
  #[test] fn fragment_samples_lists_the_trun() { /* Fragmented{8} video: the samples of each moof == the testkit's layout */ }
  #[test] fn fragment_samples_rejects_garbled_moofs() { /* trun count past the box, data offset past u64 → None, no panic */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (D12, D17). **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** docs; CHANGELOG ("Added": videos whose index comes first, whose media box has no size, fragmented MP4,
  and two copies of a video start are now put back together). Commit:
  `feat(core): reassemble videos with the index first, open-ended or fragmented`.

---

### Task 7: JPEG sem perdas (SOF3) — conferir blocos de RAW

**Files:**
- Create: `crates/revenant-core/src/formats/jpeg_lossless.rs`; Modify: `src/formats/{mod.rs,jpeg_entropy.rs}` (share ONLY the Huffman table builder
  and, if needed, the bit reader as `pub(crate)`: the baseline decoder rejects SSSS > 11 and must keep doing so, while
  lossless codes go to SSSS 16: C4), `src/tiff.rs`
  (`TiffInfo::lossless`, `TiffInfo::ifds`), `crates/revenant-testkit/src/lib.rs` (`lossless_jpeg`)
- Test: unit tests in `jpeg_lossless.rs`, `tiff.rs`; testkit `tests/media.rs`
- Docs: core README "TIFF reader (internal)" (lossless ranges, IFD ranges), new "Lossless JPEG check" paragraph under
  "Formats: TIFF-based RAW"; CHANGELOG nothing (internal)

**Interfaces:**
- Produces:
  ```rust
  // testkit lib.rs
  /// A lossless JPEG (SOF3, ITU-T T.81 Annex H): `components` (1–4) of `precision` bits (2–16), predictor 1, point
  /// transform 0, one DC Huffman table covering SSSS 0–16 shared by every component, no restart, seeded smooth samples.
  pub fn lossless_jpeg(width: u32, height: u32, components: u8, precision: u8, seed: u64) -> Vec<u8>;
  // jpeg_lossless.rs
  pub(crate) struct LosslessFrame { pub width: u32, pub height: u32, pub components: u8, pub precision: u8, .. }
  /// From SOI to the end of SOS (DHT, SOF3, optional DRI, SOS); `None` unless SOF3 with Huffman tables for every component.
  pub(crate) fn lossless_frame(r: &mut Reader, start: u64, limit: u64) -> Option<(LosslessFrame, u64 /* scan start */)>;
  #[derive(Clone, Debug)] pub(crate) struct LlCursor { pub pos: u64, pub sample: u64, .. }
  pub(crate) struct LlRun { pub end: End /* the 1.3 End */, pub samples: u64, pub resume: Vec<LlCursor> }
  /// Decodes difference codes (Huffman SSSS + SSSS extra bits; 16 → 32768, no extra) from `from`, reading `[from.pos,
  /// limit)`, `limit` being the tile's declared end (TileOffsets + TileByteCounts); `Eoi` only with exactly width ×
  /// height × components samples AND the FF D9 marker ending exactly at `limit` (Q15: Huffman streams resynchronize, so
  /// a count alone can be reached by chance on a neighbour's tail); restart markers checked when DRI > 0; a resume
  /// point every row of samples (≤ the 1.3 RESUME_POINTS).
  pub(crate) fn decode_lossless(r: &mut Reader, f: &LosslessFrame, from: LlCursor, limit: u64) -> LlRun;
  // tiff.rs: TiffInfo gains
  pub lossless: Vec<(u64, u64)>,   // `data` ranges of Compression 7 strips/tiles whose first SOF is SOF3
  pub ifds: Vec<(u64, u64)>,       // each IFD's entry table (2 + 12 × count + 4 bytes), relative
  pub sensor: Vec<(u64, u64)>,     // `data` ranges neither JPEG-coded nor lossless: uncompressed or vendor/lossy (blind)
  ```

- [ ] **Step 1: Failing tests:**
  ```rust
  #[test] fn lossless_tile_decodes_whole() { /* lossless_jpeg(256, 256, 1, 14, 3): frame parsed, decode → Eoi at len − 2, samples 65 536 */ }
  #[test] fn lossless_count_must_be_exact() { /* height field patched to 257 → not Eoi (Error or OutOfData); to 255 → Error (data after the last sample) */ }
  #[test] fn lossless_eoi_must_sit_at_the_declared_tile_end() {
      // Q15. tile decoded with limit = len → Eoi; limit = len + 64 (the marker is early) → not Eoi; limit = len − 2 → not
      // Eoi; a tile whose sample count is exact but whose FF D9 is 8 bytes before the declared end → not Eoi
  }
  #[test] fn lossless_cut_tile_breaks_near_the_cut() { /* tile cut at 60% then noise: End::Error at ≥ the cut − 64 */ }
  #[test] fn lossless_tail_of_another_tile_fails() { /* tile a[..c] ++ tile b[c..] (same frame, other seed) → never Eoi with the exact count */ }
  #[test] fn lossless_resumes_across_a_view() { /* decode through Fragmented([a[..c], a[c..]] at two places) == contiguous decode (same End, samples) */ }
  #[test] fn baseline_jpeg_is_not_lossless() { /* testkit jpeg(64, 64, 1, 0) → lossless_frame None */ }
  // tiff.rs unit
  #[test] fn compression_7_lossless_tiles_are_listed() { /* a TIFF with one IFD, Compression 7, 4 lossless tiles → lossless == the 4 tile ranges; sensor empty; ifds holds IFD0 */ }
  #[test] fn uncompressed_and_sony_lossy_data_are_sensor() { /* Compression 1 and 32767 strips → in sensor, not in lossless */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement from T.81 Annex H (predictors 1–7 accepted; validation needs only the
  difference codes, not the reconstructed samples, so predictor arithmetic is not computed). **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** docs. Commit: `feat(core): lossless JPEG structure check for RAW tiles`.

---

### Task 8: RAW fragmentado — ARW primeiro, depois os outros TIFF

**Files:**
- Create: `crates/revenant-core/src/reassemble/raw.rs`; Modify: `src/reassemble/mod.rs` (RAW candidates, `own`), `src/
  formats/jpeg.rs` (`preview_health` → `pub(crate)` if Task 3 did not already: Q12),
  `crates/revenant-testkit/src/lib.rs` (`ArwOptions`, `ArwCompression`, `arw`; `TiffRaw` gains `tiles`, empty in
  `tiff_raw`, `panasonic_rw2` and `olympus_orf`)
- Test: unit tests in `reassemble/raw.rs`; `tests/reassembly.rs`; `tests/tiffraw.rs`
- Docs: core README "Formats: TIFF-based RAW" (fragmented RAW: what is proven, what is only placed), "Reassembly";
  CHANGELOG

**Interfaces:**
- Consumes: Task 7 (`lossless_frame`, `decode_lossless`, `TiffInfo::{lossless, ifds, sensor}`), 1.3 JPEG decoder for the
  preview (`JpegSeams` logic or `decode_health` on the preview range of the view).
- Produces:
  ```rust
  // testkit
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum ArwCompression { Uncompressed, Lossless, Lossy }
  pub struct ArwOptions { pub compression: ArwCompression, pub width: u32, pub height: u32, pub tile: u32,
                          pub preview: (u32, u32), pub seed: u64, pub exif: TestExif }
  /// A Sony-like ARW: IFD0 (Make "SONY", a JPEG preview via JPEGInterchangeFormat), a SubIFD with the sensor data:
  /// Compression 1 (one strip), 7 (lossless_jpeg tiles of `tile`×`tile`, TileOffsets/TileByteCounts) or 32767 (one strip
  /// of seeded bytes); the preview before the sensor data, as Sony lays it out.
  pub fn arw(opts: &ArwOptions) -> TiffRaw;   // TiffRaw { bytes, preview, raw, tiles: Vec<Range<usize>> (empty unless Lossless) }
  // reassemble/raw.rs
  pub(crate) struct RawSeams { .. }
  impl RawSeams { pub(crate) fn new(source: &dyn Source, start: u64) -> Option<RawSeams>; }
  impl Seams for RawSeams { type State = usize /* next unit */; /* units in file order from the TIFF walk of the first
      piece and, as the view grows, of the view: IFD tables (must parse: no structural error), the previews (strict decode
      when wholly inside the view; a seam inside one is checked by decoding across it), lossless tiles (decode_lossless
      with `limit` = the tile's declared end: Eoi exactly there with the exact count: Q15), sensor ranges (blind: no
      evidence, yet descended into like any blind candidate: Q2, so an evident neighbour can still make a seam
      non-unique); known_len = the TIFF extent; own = the previews and JPEG-coded ranges (D10; the free-run exemption
      is a coverage test, Q7); prefilter (Q1) = the first checked unit at the seam passes; prove = Proven iff every seam
      lies inside a checked unit that passed or exactly between two checked units, Unproven if one lies in a sensor
      range */ }
  ```

- [ ] **Step 1: Failing tests** (`tests/reassembly.rs`, exFAT card 64 MiB / 32 KiB, RAW written with `add_at`, deleted
  Camera, quick-formatted where said):
  ```rust
  #[test] fn lossless_arw_in_four_pieces_is_intact() { /* arw(Lossless, 1024×768, tile 256) 4 runs → Intact, bytes equal */ }
  #[test] fn uncompressed_arw_in_three_pieces_is_partial_with_bytes_in_place() {
      // arw(Uncompressed) in 3 runs; the gaps between the runs are TAKEN (claimed Intact files or finding starts), as
      // "nothing in between" means exactly that (Q14): blind sensor data never breaks, so a piece ends only where
      // `free_run` stops. Final Partial, extracted bytes == the RAW (placed by D4 order), never Intact
  }
  #[test] fn lossy_arw_is_never_intact_when_split_in_sensor_data() { /* arw(Lossy), seam inside the strip → Partial */ }
  #[test] fn preview_split_across_a_seam_is_validated() {
      // arw(Uncompressed) with the seam inside the preview and a burst neighbour's preview tail free nearby: the chosen
      // piece is the true one (bytes of the preview equal), the RAW Partial (sensor seam), its preview decodes
  }
  #[test] fn burst_arw_neighbours_are_not_glued_intact() {
      // two arw(Lossless) of the same options but seeds 1, 2, runs interleaved NextFree: each Intact final finding's bytes
      // == its own RAW
  }
  #[test] fn raw_preview_found_in_a_later_piece_is_its_own() {
      // the preview in run 2 is found as a JPEG Intact; the RAW still reassembles (D10) and the JPEG finding stays. The
      // preview and the RAW sharing those bytes are the same file, nested: recorded, not two Intact files (Q16); the
      // match is by coverage of the claimed ranges by `own` at their exact positions, also when the preview sits next
      // to an EXIF thumbnail claim that `Runs` merged with it (Q7)
  }
  // tests/tiffraw.rs (1.4 T9)
  #[test] fn raw_preview_crossing_a_piece_is_not_reported_twice() {
      // a LIVE ARW (add_at, 2 runs) whose preview crosses the run boundary: the preview is not a second finding (D12 of
      // part 1.4, own files; not this plan's D12);
      // if this is green on the parent commit, record it and keep it as a guard (the 1.4 note then becomes a README fact)
  }
  ```
- [ ] **Step 2:** FAIL (record each). **Step 3:** implement (D13, D10). The other TIFF RAWs (CR2, NEF, DNG, ORF, RW2)
  use the same `RawSeams` (their sensor data is in `sensor` unless lossless); one test with a testkit `tiff_raw(Nef)` in 3
  runs asserting `Partial` with bytes in place goes with `uncompressed_arw_…`.
- [ ] **Step 4:** suite, clippy, fmt; `cargo test --release -p revenant-core --test reassembly`.
- [ ] **Step 5:** docs (README: a RAW split inside uncompressed or lossy sensor data comes back with its bytes placed but
  `Partial`; lossless-compressed ARW tiles prove their seams; CR2 slices (Compression 6 with SOF3) land in `sensor`,
  blind, conservatively: C6); CHANGELOG ("Added": RAW photos in pieces, ARW first).
  Commit: `feat(core): reassemble fragmented RAW photos where the seams can be proven`.

---

### Task 9: Apagado e fragmentado no FAT/exFAT — tamanho da entrada e a cadeia que sobrevive

**Files:**
- Modify: `crates/revenant-core/src/filesystem/{listing.rs,exfat.rs}` (`Placement::OldChain`, the "no other deleted
  entry claims these clusters" sweep in `place`), `src/scan/fs.rs` (`ViewMeasure::checks`, `merge`, which receives `Commit::starts`), `src/scan/mod.rs` (hands the
  sorted `starts` to the FS jobs), `src/reassemble/mod.rs` (`content_checks`; first piece hypothesis = the listing's
  pieces; `Space::with_hint`; the FS size as `known` is already plumbed by Task 4/Q3), the `PLACEMENTS` table of the
  `Placement` codec in `listing.rs`, `src/formats/jpeg.rs` / `src/pairing.rs` items already widened in Tasks 3 and 5
- Test: `tests/scan_fs.rs`, `tests/fs_exfat.rs`, `tests/fs_fat.rs`; unit tests in `exfat.rs`, `scan/fs.rs`
- Docs: core README "File systems: exFAT" (old chain as proof, D14 with the conditions below), "Deleted files and
  folders" (FAT deleted fragmented files are reassembled), "Deduplication" (merge rule), CHANGELOG

**Interfaces:**
- Produces:
  ```rust
  pub enum Placement { Chain, Recorded, Guessed, /// exFAT deleted file placed by its surviving FAT chain (D14)
      OldChain, Carved }                                            // as_str "old_chain"
  // reassemble/mod.rs
  /// Every content check the format has over the view of a whole file (D14): JPEG decodes strictly; video: index Intact
  /// and every checkable sample passes; RAW: TIFF structure, previews strict, lossless tiles whole (sensor seams not
  /// required). Bounded by the per-file budget. It runs INSIDE the scan, in the FS jobs, only for `OldChain` deleted
  /// files and bounded per file (Q18, D20).
  pub(crate) fn content_checks(view: &dyn Source, kind: FileKind, len: u64, cancel: &CancelToken) -> bool;
  impl Space<'_> {
      /// The listing's pieces as the first hypothesis: at each seam, the start of the hint's next piece (when free) is put
      /// at the head of the D4 order.
      pub(crate) fn with_hint(self, pieces: &[Fragment]) -> Self;
  }
  // scan/fs.rs
  pub(crate) struct ViewMeasure { .., pub checks: bool }  // content_checks, only computed for OldChain deleted files
  // merge: a Deleted OldChain file keeps an Intact view measurement when tail_ok, checks and len == origin.size
  ```
  The old chain is proof (`Placement::OldChain`) only when ALL hold (design §5.1 four conditions plus Q6): (1) the chain
  has more than one run; (2) the allocation bitmap is present (`map.bits.is_some()`) and `valid == size`, as `place`
  already requires for trust; (3) no other deleted entry "claims" any of its clusters, where claims = that entry's
  NoFatChain recorded run, its own old chain, or its first cluster; (4) no other finding's header start
  (`Commit::starts`) lies inside the chain's clusters, except its own embedded files (this matters most for an
  uncompressed or lossy ARW, whose sensor bytes nothing checks); (5) `content_checks` passes, the tail conforms and the
  length equals the entry's size. Otherwise the chain is only the first hypothesis.

- [ ] **Step 1: Failing tests:**
  ```rust
  // tests/scan_fs.rs
  #[test] fn deleted_fragmented_fat32_jpeg_is_intact_when_reassembled() {
      // FAT32 64 MiB / 4 KiB: jpeg 2048×1536 in 4 runs (add_at), deleted Camera (chain zeroed): final finding 4 pieces,
      // Intact, origin Deleted with name, placement Carved, len == origin.size
  }
  #[test] fn fat32_windows_delete_fragmented_video_is_intact() { /* WindowsFat32 high word zeroed, avc_video 5 runs → Intact */ }
  #[test] fn exfat_computer_delete_zeroed_chain_is_reassembled() { /* ZeroChain, jpeg 3 runs → Intact, placement Carved */ }
  #[test] fn deleted_exfat_old_chain_is_proof_when_every_check_passes() {
      // exFAT Camera delete keeps the FAT: jpeg in 3 runs → Intact, placement OldChain, pieces == the chain
  }
  #[test] fn old_chain_of_an_uncompressed_arw_is_proof_with_structure_and_preview() { /* arw(Uncompressed) 3 runs, Camera → Intact, OldChain */ }
  #[test] fn old_chain_reused_by_a_later_file_is_not_proof() {
      // A (FAT-chained, 3 runs) deleted Camera; B written contiguous (NoFatChain) over A's 2nd run, then B deleted:
      // A's chain is only a hypothesis: A is not Intact with B's bytes (Partial or reassembled from its true clusters
      // if they survive elsewhere: here they do not → Partial); B comes out Intact by its own NoFatChain run
  }
  #[test] fn size_mismatch_is_never_intact() { /* entry size patched +4096 via raw_entry: not Intact */ }
  #[test] fn old_chain_without_a_bitmap_or_with_invalid_length_is_not_proof() {
      // Q6: the same jpeg in 3 runs, once with the allocation bitmap entry missing, once with ValidDataLength < size:
      // placement is not OldChain-trusted, the file is not Intact by the chain
  }
  #[test] fn old_chain_with_another_findings_start_inside_is_not_proof() {
      // Q6: an uncompressed ARW whose old chain's 2nd run was overwritten by a later clip whose entry slot was reused
      // (no visible claim): the clip's header start lies inside the chain → not Intact by the chain
  // exfat.rs unit
  #[test] fn old_chain_claimed_by_another_deleted_entry_is_guessed() {
      // two deleted entries whose chains share a cluster → both placement Guessed; the same when the other entry only
      // has a NoFatChain recorded run or its first cluster there (Q6 definition of "claims")
  }
  // scan/fs.rs unit
  #[test] fn merge_keeps_intact_for_a_checked_old_chain() { /* OldChain + checks + tail_ok + size → Intact; checks false → Partial */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (D14, D15; G5). A FS deleted candidate's search starts from the entry's
  first cluster with `Space::with_hint(listing pieces)` (guess or old chain tried first); its `known = origin.size` is
  already passed by Task 4 (Q3).
- [ ] **Step 4:** suite, clippy, fmt; the 1.4 opt-in suites still compile (`cargo test -p revenant-core --test
  os_filesystems --no-run`). `content_checks` now runs during the scan: run `bench --images exfat-card,noise,dense-jpeg`
  (release, best of 3) on this commit and its parent, interleaved twice, within 5%, else stop and report (Q18). The
  FAT32 64 MiB / 4 KiB tests above rely on the cap counting prefilter survivors (Q1).
- [ ] **Step 5:** docs; CHANGELOG ("Added": deleted photos and videos in pieces on FAT and exFAT cards can come back
  intact; "Changed": a new placement `old_chain`). Commit:
  `feat(core): deleted fragmented files on FAT and exFAT can come back intact`.

---

### Task 10: Cartões realistas — alocação, intercalados, parecidos, apagar, formatar

**Files:**
- Create: `crates/revenant-core/tests/reassembly_cards.rs`
- Modify: nothing in `src/` unless a test finds a bug (then fix in the owning module and say so in the report)
- Docs: core README "Reassembly" (the scenarios covered and the allocation assumption, D19 risk); CHANGELOG nothing

**Interfaces:**
- Consumes: everything above; testkit `Alloc`, `DeleteStyle`, `jpeg_burst`, `arw`, `camera_video`, `build_quick_formatted`.
- Produces: helper `shoot(b: &mut FsBuilder, seq: &[Shot]) -> Vec<Placed>` (a `Shot` = RAW+JPEG pair or a clip; written in
  that order, so RAW and JPEG interleave), and the oracle below.

- [ ] **Step 1: Failing tests** (each card: exFAT 128 MiB with 32 KiB clusters and 128 KiB clusters, both allocations;
  oracle `no_intact_finding_is_glued(events, originals)`: every `Intact` final finding's extracted bytes equal exactly one
  original file (a RAW and its nested own preview finding are the same file, exempt: Q16); and `recovered(events,
  originals)`: the originals whose bytes are all still on the card come out with their exact bytes, with the expected
  health set PER KIND (Q17): JPEG with DRI 0, whole on the card → `Intact`; lossless ARW → `Intact`; uncompressed or
  lossy ARW split in sensor data → `Partial` with exact bytes; video with every sample checkable → `Intact`; anything
  else → exact bytes or not `Intact`. The helpers `final_view`, `exfat_card` come from `tests/support` (C8)):
  ```rust
  #[test] fn delete_in_camera_then_shoot_again() { /* 12 shots (ARW lossless + JPEG), delete shots 3, 6, 9 (Camera), 8 more shots + 2 clips; per allocation */ }
  #[test] fn delete_in_camera_uncompressed_raw() { /* same with arw(Uncompressed): ARWs split in sensor data are Partial with exact bytes */ }
  #[test] fn format_in_camera_then_shoot_again() { /* 20 shots, build_quick_formatted, 6 more shots over the start: old files carved and reassembled where whole */ }
  #[test] fn delete_on_a_computer() { /* exFAT ZeroChain and FAT32 WindowsFat32 deletes of 5 fragmented shots */ }
  #[test] fn burst_of_look_alikes_interleaved() { /* jpeg_burst(2048, 1536, 4, 6) written NextFree after holes of 1–3 clusters: no_intact_finding_is_glued */ }
  #[test] fn no_intact_finding_is_glued() { /* runs the oracle over every card above (one place, so the Review Focus item has its name) */ }
  ```
- [ ] **Step 2:** show RED (Q13), not on the Task 9 head where they may already pass: run the file on the commit before
  Task 3 if it compiles there, else under mutation (a temporary, uncommitted change that makes `reassemble` return no
  findings) and record, per test, red or "green on first run (kept as a guard)". Then run on the Task 9 head: whatever
  fails there is a bug. **Step 3:** fix what the cards find; each fix gets its own focused test in the owning module.
- [ ] **Step 4:** suite (debug and `--release`), clippy, fmt; record the counts (Intact / Partial / not recovered per
  card) for the history.
- [ ] **Step 5:** README "Reassembly" (scenarios, allocation assumption). Commit:
  `test(core): realistic fragmented cards with look-alike neighbours`.

---

### Task 11: Medir — orçamentos, velocidade, amostras reais e o DRI da Sony

**Files:**
- Modify: `crates/revenant-testkit/src/lib.rs` (`BenchImage::ScatteredCard`, "scattered-card": exFAT 128 KiB clusters,
  the `delete_in_camera_then_shoot_again` pattern repeated to fill the image), `crates/revenant-dev/src/bench.rs`,
  `crates/revenant-core/src/reassemble/mod.rs` (the budget measure, an `#[ignore]` UNIT test: Q9),
  `crates/revenant-core/tests/real_samples.rs`, `samples/samples.toml` (A6700 / recent Sony samples if CC0),
  `src/reassemble/mod.rs` (limits adjusted to the measurements, if needed)
- Docs: core README "Performance" (scattered-card line, reassembly time), "Reassembly" (budgets with measured numbers),
  "Tested against real files" (Sony DRI), dev README ("Benchmark": the new image); CHANGELOG nothing unless limits change

**Interfaces:**
- Produces: `BenchImage::ScatteredCard` (name `scattered-card`, in `ALL`, `parse`); real-samples tests below.

- [ ] **Step 1: Tests:**
  ```rust
  // reassemble/mod.rs, a unit test inside the crate (the counters are crate-private, invisible to tests/*.rs: Q9;
  // release-only, #[ignore], prints numbers)
  #[test] #[ignore] fn reassembly_budget_measure() {
      // the hostile image of Task 4 (built here with the testkit) at 1 GiB and a scattered-card of 1 GiB: prints the
      // reassembly step's time, tries and bytes read per file and per scan, and whether a budget was hit; this is where
      // the time bound dropped from Task 4's tests (Q8) lives
  }
  // tests/real_samples.rs (ignored like the others)
  #[test] #[ignore] fn sony_restart_intervals() {
      // for every Sony sample (ARW previews; any Sony JPEG): prints file, preview size, DRI (restart_interval helper);
      // asserts nothing about the value (it is a measurement, D21), only that at least one Sony sample was found when
      // the samples folder exists (C5). Also prints whether the real Sony lossless tiles have equal TileByteCounts (a
      // seam at a tile boundary is then no proof: Q15)
  }
  #[test] #[ignore] fn real_previews_split_in_three() {
      // every sample's largest preview cut in 3 at 32 KiB multiples on an exFAT image built with the public testkit
      // `FsBuilder` and `add_at` (a card, not crate-private `search`/`Space`), another sample's preview runs as
      // neighbours, scanned through the public `scan` (Q9): each comes back with its exact bytes; Intact iff DRI == 0
      // (Q16), never a neighbour's run inside an Intact result
  }
  ```
- [ ] **Step 2:** run them (release): `cargo test --release -p revenant-core --lib reassembly_budget_measure -- --ignored
  --nocapture`;
  `scripts/fetch-samples.sh` then `REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --test real_samples --
  --ignored --nocapture` (if the network is not available, say so and leave the measurement to Sérgio in the history).
  For new samples: only raw.pixls.us entries marked CC0 (a Sony ILCE-6700 one if listed; else the newest Sony ARW with
  lossless compression), SHA-256 recorded.
- [ ] **Step 3:** if a budget is hit on `scattered-card` or the hostile image takes more than 10 s per GiB, adjust
  `Limits::DEFAULT` and re-run; every change of a constant goes in the README with the number that justified it.
- [ ] **Step 4:** `cargo run --release -p revenant-dev -- bench` (all images) on this commit and on `main` built in a
  separate target, interleaved twice, control rebuild as in 1.4/1.5: every pre-existing image within 5%; record all.
- [ ] **Step 5:** docs (Performance table "End of 1.6" column, scattered-card line, reassembly time; Sony DRI result).
  Commit: `perf(core): measure reassembly budgets, speed and Sony restart intervals`.

---

### Task 12: Os menores herdados da 1.4 (T5, T6, T10)

**Files:**
- Modify: `crates/revenant-core/src/filesystem/{fat_deleted.rs,fat.rs,exfat.rs,orphans.rs,mod.rs}` (`Limits`)
- Test: unit tests next to the code (they need the `cfg(test)` limits, P1 of 1.5)
- Docs: core README ("Deleted files and folders": the high-word phase cap; "File systems: exFAT"; "File systems: after a
  quick format"), CHANGELOG ("Fixed" for anything user-visible)

**Interfaces:**
- Produces: `Limits::high_word_steps` (default `MAX_GUESS_STEPS / 2 = 2^25`): the D10 high-word phase spends at most it,
  the rest of the guess budget stays for the placements.

- [ ] **Step 1: Failing tests:**
  ```rust
  // fat_deleted.rs (T5)
  #[test] fn high_word_phase_stops_at_its_share_of_the_budget() { /* guess_steps 64, high_word_steps 32: 100 open questions → the phase spends ≤ 32 and the placements still run (≥ 1 deleted file placed) */ }
  #[test] fn waiting_queue_cut_marks_the_listing_truncated() { /* limits.dirs 2 with 5 deleted folders waiting → truncated true, 2 walked */ }
  // exfat.rs (T6)
  #[test] fn live_nofatchain_directory_is_trusted() { /* a NoFatChain live folder whose clusters are all claimed and allocated → its range in `live` */ }
  #[test] fn unreadable_chunk_costs_no_entry_budget() { /* a directory chunk unreadable (BadSector source): the entries budget left is unchanged by it */ }
  #[test] fn non_photo_files_before_the_bitmap_claim_after_it() { /* a malformed root with a .TXT before the bitmap entry: the photo after is still listed and trusted */ }
  // orphans.rs (T10)
  #[test] fn depth_cut_folder_is_memoized() { /* a chain of 40 old folders: Folders::get called twice on the deepest → same Arc (ptr_eq), no second walk */ }
  #[test] fn multi_cluster_old_folder_continuation_is_one_folder() { /* an exFAT old folder over 2 clusters: one candidate, its files named "?/NAME" once (documented today; asserted now) */ }
  ```
- [ ] **Step 2:** FAIL (record each; a test green on the parent is recorded and kept as a guard). **Step 3:** implement
  the smallest fix for each red one. **If a fix would take more than about 100 lines or touch the listing's contract, do
  not do it: write the item back with owner and reason for the history (D22).** **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** docs. Commit: `fix(core): close the minor items owned by part 1.6`.

---

### Task 13: Documentação, roteiro manual da A6700 e fechamento da parte

**Files:**
- Modify: `README.md`, `crates/revenant-core/README.md` ("Current state"), `crates/revenant-dev/README.md`, `CHANGELOG.md`,
  `CLAUDE.md`, `docs/superpowers/roadmap.md`
- Create: `docs/superpowers/history/<date>-m1-p6-pendencias.md` and `docs/superpowers/history/<date>-m1-p6-conferencia-manual.md`
  (`<date>` from `date +%F` on the day of this task)

- [ ] **Step 1:** `cargo test --workspace`, `cargo test --release -p revenant-core`, `cargo clippy --all-targets -- -D
  warnings`, `cargo fmt --check`, `cargo deny check licenses` (if installed; else say so), `grep -rn "os::unix" crates`
  (all gated), `REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --test real_samples -- --ignored` if
  `samples/cache` exists, the opt-in suites `REVENANT_OS_FS=1` (1.4) and `REVENANT_OS_DEV=1 … os_devices` (1.5) when
  available; paste the summaries.
- [ ] **Step 2:** `cargo run --release -p revenant-dev -- bench` (all images, `scattered-card` included); check the
  Performance table matches Task 11 or update it with this run, saying which.
- [ ] **Step 3: Docs:** root README status (fragmented files: what comes back intact, what partial); `CLAUDE.md` "Onde o
  desenvolvimento está" (pt-BR, today's date: 1.5 mergeada (PR #6); 1.6 concluída na branch `feat/m1-p6-fragments`,
  aguardando PR/merge; decisões em `docs/superpowers/history/<date>-m1-p6-pendencias.md`; próxima: Módulo 2); roadmap 1.6
  marcada como feita, com o que cobre (JPEG, vídeo H.264/H.265 incluindo `moov` antes, `mdat` 0, `moof`, RAW onde a emenda
  é provada, apagados no FAT/exFAT, cadeia exFAT como prova) e o que não (RAW sem compressão espalhado nos dados do sensor
  sai `Partial`; continuação fora da janela de 256 MiB ou depois de 4096 candidatos; o padrão de alocação real da Sony a
  conferir).
- [ ] **Step 4:** history file (pt-BR, same shape as the 1.5 one): D1–D22 with their costs, the pre-flight rulings
  Q1–Q19 with their costs (including the accepted risk Q4: a look-alike beyond the window; the nesting of a RAW and its
  preview, Q16; and, from Q19/C11, that no task looked for XAVC CC0 samples: design says "se o construtor achar"),
  decisions taken during execution, the budgets and the numbers of Tasks 10 and 11, the Sony DRI result (D21: if the A6700 previews/JPEGs use
  restart markers, say plainly that their fragmented JPEGs come back `Partial` and that this is the main cost of 1.6 for
  Sérgio's camera), the inherited table above with each item's result (C13 included: stays with Módulo 2), and every
  item left open with its owner (**Módulo 2**: show a reassembled finding in place of the old one; the preview reported
  twice (D10); the `old_chain` placement's text; **Sérgio**: the manual checklist below; **depois**: fingerprints per
  cluster if real cards hit the read budget).
- [ ] **Step 5: Roteiro manual** (`<date>-m1-p6-conferencia-manual.md`, pt-BR, passos numerados com o comando exato e o
  que se espera ver; o Sérgio roda quando quiser): (1) preparar: um cartão SDXC formatado **na A6700**, RAW+JPEG, e
  anotar o modo de gravação do RAW (sem compressão, sem perdas, comprimido); (2) fotografar 30 fotos e gravar 3 clipes
  XAVC curtos; (3) apagar na câmera as fotos 5, 10, 15, 20 e o clipe do meio; (4) fotografar mais 15 e gravar um clipe
  longo (para encher os buracos); (5) `revenant-dev digest`, `copy` para uma imagem, `scan` da imagem com `--extract`;
  conferir que cada arquivo apagado e regravado volta com os bytes iguais aos de uma cópia feita antes (passo 2 copiou o
  cartão inteiro para o Mac); (6) anotar quantos vieram `Intact`, `Partial`, nenhum, por tipo, e o DRI das prévias
  (`revenant-dev inspect`); (7) repetir com "apagar no computador" (Mac Finder e, se houver, Windows) e com "formatar na
  câmera" + 5 fotos novas; (8) o que registrar e onde (o histórico da 1.6, seção "Conferência manual").
- [ ] **Step 6:** Commit: `docs: part 1.6 status, numbers, pending items and manual checklist`.
