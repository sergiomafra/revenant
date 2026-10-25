# Módulo 1 · Parte 5 — Disco de verdade — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** O motor passa a ler o aparelho inteiro (cartão ou HD) no Mac e no Windows, sempre só leitura: lista os discos,
desmonta sem ejetar, confere antes de começar o que já se sabe que vai falhar, faz uma cópia tolerante a setores ruins
(retomável, com mapa) e busca no aparelho, numa imagem com mapa ou numa sessão salva que retoma de onde parou com os
mesmos eventos de uma busca sem interrupção; um aparelho que some não vira setor ruim, e um disco que falha em série é
avisado cedo. O `revenant-dev` ganha tudo isso. Fecha o marco A e as pendências com dono 1.5.

**Architecture:** Leitura do aparelho atrás de duas interfaces que um canal entre processos (Módulo 2.1) poderá servir:
`RawDevice` (leituras alinhadas) e `DeviceOps` (listar, abrir, desmontar, remontar, informar o destino). `DeviceSource`
alinha por dentro e implementa `Source`, então busca, sistemas de arquivos, extração e pareamento não mudam. Um aparelho que
some é `EngineError::DeviceGone`, que o cache nunca grava como setor ruim e que para a busca antes de qualquer evento que
dependa dele. A cópia (`rescue`) escreve uma imagem esparsa e um mapa próprio; `MappedSource` lê a imagem e devolve erro de
leitura em tudo que não foi resgatado. A busca ganha um estado capturável em cada fronteira de bloco (`ScanState`); a
sessão grava a listagem uma vez, os eventos num registro só de acréscimo e pontos de retomada atômicos, e retoma
repetindo os eventos gravados e seguindo do ponto salvo.

**Tech Stack:** Rust 2024 (MSRV 1.88), só `std` e as dependências atuais; no Windows, mais *features* do `windows-sys`
0.61 já usado. Mac: `/usr/sbin/diskutil`, `/usr/sbin/ioreg` e `/bin/df` com saída estruturada (plist XML lido por um
leitor mínimo próprio). Testes opcionais: `hdiutil` (Mac) e `diskpart` (Windows, no CI).

**Spec:** `docs/superpowers/specs/2026-10-25-m1-p5-disco-de-verdade-design.md` (o desenho aprovado; a autoridade). Acima
dele, `docs/superpowers/specs/2026-10-19-revenant-design.md` (§3 passos 1 a 4, §5 "Somente leitura, sempre" e
"Desempenho", §6 "Acesso ao disco bruto").
**Antes:** `docs/superpowers/history/2026-10-19-m1-p1-pendencias.md`, `2026-10-19-m1-p2-pendencias.md`,
`2026-10-20-m1-p3-pendencias.md`, `2026-10-25-m1-p4-pendencias.md`. **Roteiro:** `docs/superpowers/roadmap.md`.
**Plano anterior (estilo):** `docs/superpowers/plans/2026-10-20-m1-p4-sistemas-de-arquivos.md`.

## Global Constraints

- **Language [Sérgio]:** everything in the repository in English (identifiers, comments, error codes, commits, READMEs,
  CHANGELOG); `CLAUDE.md` and `docs/superpowers/**` stay in pt-BR.
- **Never write to the source.** `Source` keeps no write method; devices are opened read-only (macOS `/dev/rdiskN` with
  `File::open`; Windows `\\.\PhysicalDriveN` with `GENERIC_READ` only, sharing `FILE_SHARE_READ | FILE_SHARE_WRITE`). The
  copy, the map and the session write only to the checked destination, never through anything in `src/source/`.
  **Single exception [Sérgio, design §6]:** on Windows a VOLUME handle may be opened with `GENERIC_READ | GENERIC_WRITE`
  solely to send `FSCTL_LOCK_VOLUME` / `FSCTL_DISMOUNT_VOLUME`, only after a read-only handle was refused, never used to
  read or write data, kept for the session; it lives in `source/windows.rs` and is recorded in `CLAUDE.md` in Task 4.
- **No `unsafe` outside `crates/revenant-core/src/source/windows.rs`** (every block with `// SAFETY:`). Every Windows FFI
  call (IOCTLs, `DeviceIoControl`, volume enumeration, free space) lives there behind safe `pub(crate)` functions. On
  macOS sizes and sector sizes come from `diskutil`/`ioreg` plists; **if a macOS query proves impossible without `unsafe`,
  the task stops and asks Sérgio** (it would be an exception to a fixed rule).
- **Licenses (spec §7):** only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, CC0-1.0,
  BSL-1.0. **No new crate** in any crate of the workspace; `windows-sys` gains only features (`Win32_System_Ioctl`,
  `Win32_System_IO`, `Win32_Security` as needed). No code copied from PhotoRec/TestDisk/ddrescue or any GPL/LGPL/MPL project;
  the copy's phases are written from the design (§5.4) only, and its map format is our own (not ddrescue's).
- **Errors by code** (`code() -> &'static str`, snake_case), no UI text in the engine. New codes (exact):
  `device_gone`, `permission_denied`, `device_not_found`, `not_whole_disk`, `system_tool_failed`, `unsupported_platform`,
  `unmount_failed`, `remount_failed`, `destination_unknown`, `map_corrupt`, `map_mismatch`, `map_other_source`,
  `session_corrupt`, `session_version`, `session_other_source`, `session_not_empty`. Pre-flight issue codes in Task 7.
- **Rust:** edition 2024, `rust-version = "1.88"`. Dev machine: Intel Mac, Rust from `/usr/local/bin` (no `rustup`); **never
  run `brew`**; check a binary exists (`which`, `/sbin`, `/usr/sbin`, `~/.cargo/bin`) before using it. macOS-only code is
  behind `#[cfg(target_os = "macos")]` (a macOS-only test file: `#![cfg(target_os = "macos")]` at the top); before each
  commit `grep -rn "os::unix" crates` and check every use is gated (CLAUDE.md "Armadilhas"). Linux (the CI release job)
  compiles the engine: there `SystemDevices` answers `unsupported_platform`.
- **System tools:** called by absolute path (`/usr/sbin/diskutil`, `/usr/sbin/ioreg`, `/bin/df`), output read up to
  16 MiB, plists parsed with depth ≤ 32; any failure is `system_tool_failed` (never a panic). Never `sudo`, never
  `diskutil` verbs other than `list`, `info`, `unmountDisk`, `mountDisk`.
- **Determinism:** for a source that returns the same bytes and errors on every read, the event sequence (minus
  `Event::Rate`, which is opt-in and timing-based) is identical for any `threads` and `cache_bytes`, and a session resumed
  from any checkpoint yields exactly the uninterrupted sequence. Nothing in the contract depends on time or map order.
- **Bounded everything** (constants with `cfg(test)` twins where a test lowers them): rescue map ≤ 2^20 regions
  (`MAX_MAP_REGIONS`); cache bad map ≤ 2^20 runs (as today; stored in sessions, ≈ 16 MiB); coverage runs ≤ 65,536;
  `PhysicalDrive0..=63`; system tool output ≤ 16 MiB; pre-flight sample timeout 10 s (`SAMPLE_TIMEOUT`).
- **Docs in the same commit [Sérgio]:** each task updates the READMEs it affects and `CHANGELOG.md` `[Unreleased]` for
  anything user-visible.
- **Commit trailer, always:** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Conventional Commits. Never
  `git add -A` (add files by name). Never push (the controller opens the PR).
- **Tests never touch real data or physical disks:** synthetic sources and images in `tempfile` only. Opt-in exceptions:
  `real_samples`, `os_filesystems`/`os_builders` (1.4) and, new, `os_devices` (macOS, `REVENANT_OS_DEV=1`, images
  `hdiutil` attaches from the test's own temp dir) and `os_devices_windows` (Windows, `REVENANT_OS_DEV=1`, a VHD
  `diskpart` creates in the test's temp dir). Listing the machine's disks also stays opt-in. Every new test is seen failing
  before it passes (record RED in the report).

## Review Focus

1. **A destination on the source disk reached by another path** (another partition of the card, an APFS container whose
   physical store is the source, the session folder, the copy's image) **and a session or copy resumed on the wrong
   card** → `destination_on_an_apfs_container_maps_to_its_physical_disk`, `destination_on_the_source_disk_is_refused`
   (Task 3), `resume_against_another_source_is_refused` (Task 6), `session_on_the_source_is_refused`,
   `resume_on_another_card_is_refused` (Task 10).
2. **A cable pulled in the middle of a scan, a copy or an extraction** (must stop and keep its place, never mark sectors
   bad, never write zeros as data) → `vanished_device_is_not_bad_sectors`, `extract_stops_on_a_vanished_device`
   (Task 1), `copy_stops_on_a_vanished_device_without_marking_bad` (Task 6), `reconnect_unmounts_before_reading` (Task 7),
   `session_resumes_after_device_gone` (Task 10).
3. **Power loss while the map or the session is being written** (must come back at the last good point, never a wrong
   state) → `map_survives_a_torn_write` (Task 5), `copy_resumes_from_a_crash_snapshot` (Task 6),
   `torn_checkpoint_falls_back_to_the_previous_one`, `garbage_after_the_last_checkpoint_is_dropped`,
   `resume_from_a_crash_copy_equals_the_whole_scan` (Task 10).
4. **Unrescued holes of the copy read as zeros and a file in them called `Intact`** →
   `unrescued_zeros_are_never_intact` (Task 5), `scan_with_a_map_marks_holes_unreadable` (Task 11).
5. **A dying disk hammered or hanging** (bad sectors read again, a read that never returns, a cancel that waits minutes)
   → `copy_never_rereads_bad_sectors_without_retries` (Task 6), `resume_never_rereads_known_bad_sectors` (Task 9),
   `not_responding_device_is_reported_within_the_timeout` (Task 7),
   `cancel_is_prompt_while_a_window_fails_sector_by_sector` (Task 1).

## Decisões do pre-flight (D1–D28)

Cada linha: decisão — custo se estiver errada. Decididas pelo Claude (o desenho §8 deixa ao construtor formato da sessão,
do mapa, tamanhos de bloco e pulo, limites do aviso, nomes, listagem do Windows e o CI do Windows).

- **D1** Aparelho que some = `EngineError::DeviceGone { offset }` (`device_gone`): quando a origem o devolve, ou quando o
  cache recebe uma leitura curta antes do `len()` que a origem anunciou (origem que encolhe, pendência 1.3). O cache nunca
  o grava como setor ruim, guarda o primeiro `offset`, e todo `emit` posterior devolve esse erro — custo: uma imagem
  truncada no meio da busca para com `device_gone` em vez de terminar como "fim".
- **D2** Classificação dos erros de leitura do aparelho: Mac `ENXIO`/`ENODEV`/`ENOENT` ⇒ sumiu; Windows
  `ERROR_DEVICE_NOT_CONNECTED` (1167), `ERROR_NO_SUCH_DEVICE` (433), `ERROR_DEV_NOT_EXIST` (55), `ERROR_NOT_READY` (21),
  `ERROR_FILE_NOT_FOUND` (2) ⇒ sumiu; qualquer outro erro ⇒ setor ruim **depois** de conferir presença (Mac: o nó
  `/dev/diskN` ainda existe; Windows: `IOCTL_STORAGE_CHECK_VERIFY2`); ausente ⇒ sumiu — custo: um leitor que devolve
  `EIO` para tudo ao ser puxado e mantém o nó vivo por um instante marca alguns setores ruins antes de sumir.
- **D3** O cache checa o cancelamento antes de cada carga e a cada setor da resolução setor a setor; cancelado ⇒ a carga
  devolve `Cancelled` sem guardar a janela nem marcar setores — custo: nenhum (a busca já ia parar).
- **D4** `DeviceSource` lê em unidades do setor lógico; um pedido alinhado vai direto para o buffer do chamador, os
  outros por um buffer intermediário alinhado a 4096 B; a busca segue com `ScanOptions::sector = 512` mesmo num aparelho
  4Kn (um setor físico ruim vira 8 setores de 512 ilegíveis, e a pendência 1.3 do "setor > 512" fecha assim) — custo: um
  memcpy a mais nas leituras desalinhadas (só cabeças e medidas pequenas).
- **D5** Geometria no Mac: `diskutil info -plist diskN` (`TotalSize` ou `Size`, `DeviceBlockSize`); setor físico de
  `ioreg -a -r -c IOBlockStorageDevice -l` (`Device Characteristics` → `Physical Block Size`) quando houver, senão igual ao
  lógico — custo: num aparelho 512e sem essa chave o setor físico sai 512 (só informativo).
- **D6** Número de série no Mac pelo mesmo `ioreg` (`Device Characteristics` → `Serial Number`, casado com o `BSD Name`
  do `IOMedia` filho); sem ele, `None` — custo: identidade mais fraca (D7 compensa).
- **D7** Identidade da origem: aparelho = tamanho + modelo + número de série; **sem série**, mais um resumo (FNV-1a 64)
  do setor 0 e, se o MBR for protetor (0xEE), dos setores 1..=33 GPT, com os bytes voláteis do setor de boot zerados
  (FAT12/16 byte 37, FAT32 byte 65, exFAT 106, 107 e 112). Imagem = tamanho + FNV-1a 64 do primeiro, do MiB do meio e do
  último MiB — custo: reformatar o cartão entre a interrupção e a retomada faz a sessão ser "de outra origem" (correto);
  dois cartões idênticos sem série e com o mesmo setor 0 seriam confundidos (o número de série do volume FAT/exFAT os
  separa na prática).
- **D8** Listagem no Mac: discos inteiros de `diskutil list -plist`, sem os contêineres APFS sintetizados (que aparecem
  só como "disco do sistema" pelos seus discos físicos); disco do sistema = o que guarda `/` (`diskutil info -plist /`:
  `ParentWholeDisk` e os discos de `APFSPhysicalStores`). Imagens anexadas pelo `hdiutil` aparecem com
  `virtual_disk: true` — custo: um disco com outro arranjo (RAID da Apple) pode sair com campos vazios.
- **D9** Listagem no Windows: `\\.\PhysicalDrive0..=63` abertos com acesso 0 (só consulta); `IOCTL_STORAGE_QUERY_PROPERTY`
  (fabricante, produto, série, removível, `StorageAccessAlignmentProperty`, `StorageDeviceSeekPenaltyProperty`),
  `IOCTL_DISK_GET_LENGTH_INFO`; volumes por `FindFirstVolumeW`/`FindNextVolumeW` + `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`,
  sistema de arquivos e rótulo por `GetVolumeInformationW`; disco do sistema = o do volume de `%SystemDrive%` — custo: um
  disco dinâmico que se estende por vários discos marca todos eles.
- **D10** Desmontar: Mac `diskutil unmountDisk` sem `force` (falha ⇒ `unmount_failed` com os volumes ainda montados);
  Windows: para cada volume do disco, `FSCTL_LOCK_VOLUME` + `FSCTL_DISMOUNT_VOLUME` com alça só leitura, e só se recusada
  a exceção do Sérgio; as alças ficam no `UnmountGuard` até o fim. Nada remonta sozinho; `remount` existe (Mac
  `diskutil mountDisk`; Windows solta as travas e consulta o volume) — custo: no Windows, soltar o guarda deixa o sistema
  remontar no próximo acesso.
- **D11** Reconectar: a cada 500 ms lista os discos até um com a mesma identidade (D7), **desmonta antes de abrir**, e só
  então abre. A janela entre o sistema montar sozinho e a nossa desmontagem fica documentada (fechá-la é do ajudante do
  2.1) — custo: o sistema pode gravar `.fseventsd`/marca de sujo nessa janela.
- **D12** Destino no Mac: `df -P -k <pasta>` (nó do volume e espaço livre) + `diskutil info -plist <nó>` (`ParentWholeDisk`,
  discos de `APFSPhysicalStores`, `FilesystemType`); `msdos` ⇒ arquivo máximo 4 GiB − 1. Windows: `GetVolumePathNameW` →
  GUID do volume → `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, `GetDiskFreeSpaceExW`, nome do sistema de arquivos (`FAT`/`FAT32`
  ⇒ 4 GiB − 1). Origem que é aparelho + destino que não se identifica ⇒ `destination_unknown` (pendência 1.1) — custo: um
  destino de rede no Mac (sem `ParentWholeDisk`) é recusado para busca em aparelho.
- **D13** `Destination` = pasta conferida uma vez contra a origem (volume, disco, aparelho); `extract_to`, a cópia e a
  sessão a recebem; `extract`/`extract_as` continuam e conferem a cada chamada (barato numa imagem) — custo: com aparelho,
  `extract_as` chama `diskutil` a cada arquivo (o `revenant-dev` usa `Destination`).
- **D14** Conferência prévia: lista → acha o disco → abre → desmonta (se pedido) → lê o primeiro, o do meio e o último setor,
  cada um numa thread com `SAMPLE_TIMEOUT = 10 s` → destino da cópia (espaço = tamanho inteiro numa cópia nova, ou o que
  falta pelo mapa numa retomada; FAT32 recusado se o tamanho passa de 4 GiB − 1) → sessão (Task 10). Devolve **todas** as
  pendências, numa ordem fixa — custo: uma leitura que trava deixa uma thread presa até o sistema desistir.
- **D15** Cópia: bloco de 1 MiB alinhado; fase rápida pula `skip` à frente após um erro (64 KiB, dobrando a cada erro
  seguido até `min(1 GiB, tamanho / 64)`, zerado num acerto); fase "pulados" relê o não tentado sem pular; "bordas" lê
  setor a setor de cada ponta de um trecho falho até o primeiro erro; "raspagem" setor a setor no miolo; "repetições"
  só com `retries > 0` (padrão 0) — custo: num cartão com erros espalhados a fase rápida deixa muito para as fases lentas.
- **D16** Mapa próprio em texto, `<imagem>.rvmap`, gravado por arquivo temporário + `sync_all` + `rename` (+ `fsync` da pasta
  no Unix), sempre **depois** de `sync_data` da imagem; a cada 30 s ou 256 MiB resgatados, a cada troca de fase e ao parar.
  Teto de 2^20 trechos: passado ele, um bloco falho vira `bad` inteiro (sem bordas nem raspagem) — custo: num disco
  com milhões de erros espalhados perde-se setor bom vizinho de setor ruim.
- **D17** A imagem é criada esparsa (`set_len` ao tamanho do aparelho) e nunca dividida; num destino FAT32 maior que 4 GiB a
  conferência prévia recusa — custo: quem só tem um pendrive FAT32 grande precisa formatá-lo em exFAT.
- **D18** `MappedSource`: uma leitura que toca qualquer byte não resgatado falha com `EngineError::Read { offset }` do
  primeiro byte não resgatado (o cache resolve setor a setor ⇒ os buracos viram `Unreadable` e nada neles é `Intact`) —
  custo: nenhum.
- **D19** Cópia e busca são etapas separadas (Sérgio); nada de busca durante a cópia — custo: um disco morrendo é lido duas
  vezes no total (cópia, depois a busca na imagem), nunca o aparelho duas vezes.
- **D20** `Event::DiskFailing` uma vez por busca, na primeira fronteira de bloco em que os trechos ilegíveis emitidos
  somam ≥ 16 (`FAILING_RUNS`) ou ≥ 4 MiB (`FAILING_BYTES`); determinístico, entra na sessão — custo: um cartão com 16
  setores ruins espalhados mas legível no resto recebe o aviso.
- **D21** `Event::Rate { bytes_per_sec, eta_secs, stalls }` só com `ScanOptions::rates` (padrão `false`), logo depois de
  cada `Progress`; média móvel exponencial; `stalls` = cargas de janela que levaram mais de 2 s (`STALL`). É o único evento
  fora do contrato de determinismo e nunca é gravado na sessão — custo: quem compara eventos com `rates: true` precisa
  filtrar `Rate`.
- **D22** Estado da busca (`ScanState`) capturado só em fronteira de bloco, depois do `Progress`: tudo o que o `Commit`, o
  `Shared` e os localizadores carregam (lista na Task 9) mais o mapa de setores ruins e a cobertura do cache a partir do
  ponto; a captura desestrutura o `Commit` **sem `..`**, então um campo novo quebra a compilação até entrar no estado —
  custo: o estado cresce com os achados pendentes e as listas de pareamento (tetos já existentes).
- **D23** A listagem de sistemas de arquivos é gravada uma vez na sessão e **nunca relida** na retomada (relê-la com o mapa
  de setores ruins já cheio mudaria o que a sonda recusa e, com isso, a listagem) — custo: o arquivo da listagem pode chegar
  a centenas de MiB no pior caso dos tetos da 1.4.
- **D24** O que a retomada relê, documentado: os blocos lidos à frente depois do último ponto (≤ cadência + `lookahead ×
  block`), os pedaços anteriores ao ponto de arquivos do sistema de arquivos que começam depois dele, e o que o pareamento
  carrega; nunca um setor do mapa de ruins, nunca o miolo de um achado `Intact` que atravessa o ponto (a cobertura volta
  com o estado) — custo: até ≈ 320 MiB relidos com os padrões.
- **D25** Sessão = pasta vazia escolhida por quem chama, com `session.rvs` (manifesto: versão, identidade, `sector`,
  `block`, `filesystems`), `listing.rvs`, `events.rvs` (registros `[len u32][crc32 u32][dados]`, só acréscimo) e dois
  pontos de retomada alternados `checkpoint-a.rvs`/`checkpoint-b.rvs` (geração, tamanho do registro, nº de eventos,
  `ScanState`, terminado + resumo), cada um escrito por temporário + `sync_all` + `rename`, depois de `sync_data` do
  registro. Abrir pega o ponto válido mais novo e corta o registro no tamanho dele. Cadência: 256 MiB ou 30 s; cancelar ou
  sumir **numa fronteira** grava um ponto na hora, senão vale o último — custo: até 256 MiB refeitos depois de uma queda.
- **D26** Codificação binária própria (sem `serde` no motor), enums gravados pelo seu `as_str`, todo comprimento
  conferido contra o que resta; qualquer falha ⇒ `session_corrupt`/`map_corrupt`, nunca pânico — custo: código de
  codificação para cada tipo (≈ 500 linhas, com testes de ida e volta).
- **D27** HEIC (pendências 1.2): é HEIC só sem `moov` e com a marca principal HEIC **ou** uma caixa `meta` no topo; um HEIC
  cujos extents do `iloc` terminam dentro do que já foi andado conta como completo (o que vem depois não o rebaixa nem
  muda o tipo) — custo: um MP4 cortado antes do `moov` que traz um `meta` solto e marca `mif1` segue HEIC.
- **D28** Teto de fotos da listagem (1.4 T5): vivos de todos os volumes entram antes de qualquer apagado — custo: num
  cartão no teto, apagados do volume 1 saem sem nome (a escultura os acha).

## Pendências herdadas com dono 1.5

| Pendência | Destino |
|---|---|
| 1.1: `FileSource` com aparelho (tamanho 0, leituras alinhadas) | Task 2 (`DeviceSource`, D4) |
| 1.1: `volume()` vazio com aparelho precisa recusar a extração | Task 3 (`Source::disk`, `Destination`, D12/D13) |
| 1.1/1.3: cópia própria no lugar do `dd`/`ddrescue` do README | Tasks 5, 6 e 11 (README "Making a card image") |
| 1.2: HEIC nunca "completo" (setor ilegível logo depois o deixa Parcial; `moov` solto muda o tipo) | Task 13 (D27) |
| 1.2: MP4 com marca `heic` cortado antes do `moov` vira HEIC Danificado | Task 13 (D27) |
| 1.3: setor de varredura > 512 B (aparelho 4Kn) | Task 2 (`four_kn_device_scans_like_its_image`, D4) |
| 1.3: origem que encolhe no meio da busca | Task 1 (D1) |
| 1.3: cancelamento atrasado durante medida e carga do cache num aparelho lento | Task 1 (D3) |
| 1.3: teto de 2^20 trechos ruins | Task 13 (medido com origem sintética) + roteiro manual (cartão morrendo, Sérgio) |
| 1.4 T1: HiddSec do EBR na variante relativa legada; detecção de exFAT por nome OEM no builder | Task 13 (documentado; o nome `EXFAT   ` é o da especificação) |
| 1.4 T2: `detach_by_path` best-effort; pasta temporária por lista de negação | Task 12 (lista de permissão, falha alta) |
| 1.4 T3: volume fantasma num boot de backup 4K; limitação da sonda; ordem do `backup()` em exFAT 4K; leituras do caso típico | Task 13 |
| 1.4 T5: teto de fotos compartilhado entre volumes | Task 13 (D28) |
| 1.4 T6: bits `InUse` misturados num conjunto | Task 13 (teste + nota) |
| 1.4 T7: "carving só se não pulado" sem teste; `filesystems_off` nunca visto falhar | Task 13 |

Fora desta parte: o ajudante privilegiado e o canal entre processos (2.1); impedir o computador de dormir e todo texto
de interface (Módulo 2); copiar e buscar ao mesmo tempo (ideia para depois); NTFS/APFS/HFS+; Linux; fragmentação avançada (1.6).

## Arquivos

- `crates/revenant-core/src/source/device.rs` (novo): `DiskId`, `Geometry`, `RawDevice`, `DeviceSource`.
- `crates/revenant-core/src/source/{mod.rs,unix.rs,windows.rs}`: `Source::disk`, abertura e classificação de erros do
  aparelho; todo FFI do Windows (listagem, travas, destino).
- `crates/revenant-core/src/device/` (novo): `mod.rs` (`DeviceOps`, `DiskInfo`, `VolumeInfo`, `DestInfo`, `UnmountGuard`,
  `SystemDevices`), `plist.rs` (leitor mínimo), `macos_parse.rs` (puro, compila em toda plataforma), `macos.rs` (chamadas,
  só macOS), `win.rs` (lógica segura sobre o FFI, só Windows).
- `crates/revenant-core/src/identity.rs`, `src/preflight.rs`, `src/rate.rs` (novos).
- `crates/revenant-core/src/rescue/{mod.rs,map.rs,mapped.rs,copy.rs}` (novo).
- `crates/revenant-core/src/codec.rs` (novo, `pub(crate)`), `src/scan/state.rs`, `src/scan/state_tests.rs` (novos).
- `crates/revenant-core/src/session/{mod.rs,store.rs}` (novo).
- `crates/revenant-core/tests/support/mod.rs` (novo): `SimSource`, `MemDevice`, `FakeDevices` (com `#![allow(dead_code)]`).
- Testes novos: `tests/{vanish,device,destination,rescue_map,rescue_copy,preflight,signals,session,os_devices,os_devices_windows}.rs`;
  fixtures em `tests/fixtures/diskutil/`.

---

### Task 1: Aparelho que some, cancelamento rápido e as origens de teste

**Files:**
- Modify: `crates/revenant-core/src/{error.rs,cache.rs,extract.rs}`, `src/scan/{mod.rs,pipeline.rs}`
- Create: `crates/revenant-core/tests/support/mod.rs`; Test: `crates/revenant-core/tests/vanish.rs` (new), unit tests in
  `cache.rs`
- Docs: core README ("Source": vanished devices; "Scan": cancellation), CHANGELOG

**Interfaces:**
- Produces:
  ```rust
  // error.rs
  #[error("device_gone")] DeviceGone { offset: u64 },          // code "device_gone"
  // cache.rs (crate-private)
  impl CachedSource<'_> {
      /// Cancellation checked before every window load and at each sector of a sector-by-sector resolution (D3).
      pub(crate) fn with_cancel(self, cancel: CancelToken) -> Self;
      /// First offset at which the source vanished (D1): a `DeviceGone` from the source, or a short read before `len()`.
      pub(crate) fn gone(&self) -> Option<u64>;
  }
  // tests/support/mod.rs (shared by the integration tests with `mod support;`)
  pub struct SimSource { .. }   // Source over a Vec<u8>, Send + Sync, interior mutability for the read log
  impl SimSource {
      pub fn new(data: Vec<u8>) -> SimSource;
      pub fn bad(self, start: u64, len: u64) -> Self;              // a read touching it fails: Read { offset: first bad byte }
      pub fn flaky(self, start: u64, len: u64, failures: u32) -> Self; // the first `failures` reads touching it fail, then ok
      pub fn slow(self, start: u64, len: u64, per_read: Duration) -> Self; // sleeps per read call touching it
      pub fn gone_at(self, offset: u64) -> Self;   // the first read reaching `offset` and every later read: DeviceGone
      pub fn gone_after(self, reads: u64) -> Self; // read call number `reads + 1` and every later one: DeviceGone
      pub fn shrink_after(self, reads: u64, new_len: u64) -> Self; // later reads return short past `new_len`; len() unchanged
      // Task 2 adds `pub fn disk(self, id: &str) -> Self` (Source::disk() == Some(DiskId)) once DiskId exists
      pub fn revive(&self);                        // a gone source reads again (reconnect tests)
      pub fn reads(&self) -> Vec<(u64, usize)>;    // every read_at call, in order
      pub fn clear_reads(&self);
  }
  ```
  `scan`: the cache is built `with_cancel(cancel.clone())`; `Commit::emit` returns `Err(DeviceGone { offset })` when
  `cache.gone()` is set (before calling `on_event` and after), like the cancel check; the loop top checks it too.
  `extract`: `read_lenient` returns `Result<(), EngineError>`; `DeviceGone` aborts the copy, removes the partial file and
  is returned as is (never zeros for a vanished device).

- [ ] **Step 1: Failing tests** (`vanish.rs`; `card()` = 32 MiB of testkit noise (seed 5) with 6 testkit JPEGs
  (`jpeg(640, 480, seed, 0)`, seeds 1–6) at sector starts every 4 MiB from 1 MiB):
  ```rust
  #[test] fn vanished_device_is_not_bad_sectors() {
      // full = events of scan(SimSource::new(card())) (threads 1); for threads in [1, 8]:
      // events of scan(SimSource::new(card()).gone_at(20 << 20)) → Err with code() == "device_gone";
      // the events emitted are a prefix of `full` (events == full[..events.len()]); none is Unreadable
  }
  #[test] fn shrinking_source_is_gone() {
      // SimSource::new(card()).shrink_after(40, 18 << 20) → Err code "device_gone"; emitted events a prefix of `full`
  }
  #[test] fn extract_stops_on_a_vanished_device() {
      // a Finding over [1 MiB, +8 MiB) of SimSource::new(card()).gone_after(2) → extract(..) Err code "device_gone";
      // the destination dir is empty afterwards
  }
  #[test] fn cancel_is_prompt_while_a_window_fails_sector_by_sector() {
      // SimSource 8 MiB, bad(0, 1 << 20), slow(0, 1 << 20, 5 ms): scan on a thread; cancel 100 ms after start →
      // returns Err code "cancelled" within 1 s of the cancel (the whole window would take ≈ 10 s)
  }
  #[test] fn cancel_is_prompt_during_a_long_measurement() {
      // testkit mp4 with a 60 MiB mdat at 0 in a 64 MiB source, slow over the mdat (50 ms per read call);
      // cancel 300 ms after start → Err code "cancelled" within 1 s of the cancel
  }
  // cache.rs unit
  #[test] fn gone_is_not_remembered_as_bad() {
      // inner source failing with DeviceGone at 3 MiB: read through the cache → Err(DeviceGone { offset: 3 MiB });
      // bad_runs(0, u64::MAX) is empty; gone() == Some(3 MiB); a later read of [0, 1 MiB) cached still succeeds
  }
  ```
- [ ] **Step 2:** run `cargo test -p revenant-core --test vanish` and the unit test → FAIL (no `DeviceGone`, slow cancel).
- [ ] **Step 3:** implement as in Interfaces (D1, D3). `EngineError::code` gains `"device_gone"`; `codes_are_stable` gains it.
- [ ] **Step 4:** `cargo test -p revenant-core`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` → green.
- [ ] **Step 5:** core README ("Source": a vanished source is `device_gone`, never a bad sector; a short read before the
  announced length counts as vanished. "Scan": cancellation is seen inside window loads, so a slow failing device stops
  within about one sector read), CHANGELOG ("Changed": a disconnected card stops the scan with `device_gone` instead of
  being reported as unreadable; cancelling is prompt on slow devices). Commit:
  `fix(core): a vanished device is not a bad sector; prompt cancel on slow reads`.

---

### Task 2: Origem de aparelho — leitura alinhada, geometria, 4Kn

**Files:**
- Create: `crates/revenant-core/src/source/device.rs`, `src/device/{mod.rs,plist.rs,macos_parse.rs,macos.rs}`
- Modify: `src/source/{mod.rs,unix.rs,windows.rs,fragmented.rs}`, `src/cache.rs` (forwards `disk()`), `src/lib.rs`,
  `crates/revenant-core/Cargo.toml` (`windows-sys` features), `tests/support/mod.rs` (`MemDevice`, `SimSource::disk`)
- Test: `crates/revenant-core/tests/device.rs` (new); unit tests in `device/plist.rs` and `device/macos_parse.rs`;
  fixtures `tests/fixtures/diskutil/info-*.plist`, `ioreg-*.plist`
- Docs: core README ("Source": devices), CHANGELOG

**Interfaces:**
- Produces (`revenant_core::source`):
  ```rust
  /// A whole disk: macOS "disk4" (device path "/dev/rdisk4"), Windows "PhysicalDrive2" (path r"\\.\PhysicalDrive2").
  #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)] pub struct DiskId(String);
  impl DiskId {
      /// Accepts "disk4", "/dev/disk4", "/dev/rdisk4", "PhysicalDrive2", r"\\.\PhysicalDrive2" on every platform (pure);
      /// a partition ("disk4s1") or anything else → Err(EngineError::NotWholeDisk), code "not_whole_disk".
      pub fn parse(s: &str) -> Result<DiskId, EngineError>;
      pub fn as_str(&self) -> &str;
      pub fn device_path(&self) -> std::path::PathBuf;
  }
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub struct Geometry { pub size: u64, pub logical_sector: u32, pub physical_sector: u32 }
  /// What a privileged helper serves (Module 2.1): aligned reads only. `offset` and `buf.len()` are multiples of
  /// `geometry().logical_sector`; fills except at the end; `DeviceGone` when the device vanished (D2), `Read` otherwise.
  pub trait RawDevice: Send + Sync {
      fn geometry(&self) -> Geometry;
      fn read_aligned(&self, offset: u64, buf: &mut [u8]) -> Result<usize, EngineError>;
  }
  pub struct DeviceSource { .. }
  impl DeviceSource {
      pub fn new(raw: Box<dyn RawDevice>, disk: DiskId) -> DeviceSource;
      /// `SystemDevices.open(disk)` wrapped (Task 3 moves the body there; this task opens directly).
      pub fn open(disk: &DiskId) -> Result<DeviceSource, EngineError>;
      pub fn geometry(&self) -> Geometry;
      pub fn disk_id(&self) -> &DiskId;
  }
  impl Source for DeviceSource { /* len = size; any offset/len (D4); volume() None; disk() Some */ }
  // Source gains (default None; Fragmented and CachedSource forward it):
  /// The whole disk the source reads, when it is a device (extraction and copies refuse every volume of it).
  fn disk(&self) -> Option<DiskId> { None }
  ```
  New `EngineError` variants and codes: `PermissionDenied` (`permission_denied`: open `EACCES`/`EPERM`,
  `ERROR_ACCESS_DENIED`), `DeviceNotFound` (`device_not_found`), `NotWholeDisk` (`not_whole_disk`),
  `SystemTool { tool: &'static str }` (`system_tool_failed`), `Unsupported` (`unsupported_platform`).
  Platform pieces: `unix.rs` `pub(crate) fn open_device(path) -> io::Result<File>` (= `File::open`) and
  `pub(crate) fn is_gone_errno(raw: i32) -> bool`; `windows.rs` `open_device` (`OpenOptions::read(true)` + `share_mode`),
  `device_geometry(&File) -> io::Result<Geometry>` (`IOCTL_DISK_GET_LENGTH_INFO`, `StorageAccessAlignmentProperty`, else
  `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`), `device_present(&File) -> bool` (`IOCTL_STORAGE_CHECK_VERIFY2`),
  `is_gone_code(raw: i32) -> bool` (D2 list). `device/plist.rs`: `pub(crate) enum Plist { Dict(Vec<(String, Plist)>),
  Array(Vec<Plist>), Str(String), Int(i64), Bool(bool), Data(Vec<u8>), Real(f64) }`, `pub(crate) fn parse(text: &str) ->
  Option<Plist>` (depth ≤ 32, entities unescaped, `None` on anything malformed). `device/macos_parse.rs`:
  `pub(crate) fn geometry(info: &Plist, ioreg: Option<&Plist>, bsd: &str) -> Option<Geometry>` (D5);
  `device/macos.rs`: `pub(crate) fn run(tool: &'static str, args: &[&str]) -> Result<String, EngineError>`.

- [ ] **Step 1: Failing tests** (`device.rs`; `MemDevice::new(data, logical: u32)` in `support` panics on any misaligned
  `read_aligned`, and has `.bad(start, len)`, `.gone_after(reads)`):
  ```rust
  #[test] fn unaligned_reads_are_aligned_inside() {
      // MemDevice 1 MiB of seeded bytes, logical 4096; for (offset, len) in [(0, 1), (1, 4095), (4095, 2), (4096, 4096),
      // (10_000, 70_000), (len − 3, 10), (len, 5)] → read_at returns min(len, size − offset) bytes equal to the data
      // (no panic: every inner call aligned)
  }
  #[test] fn four_kn_device_scans_like_its_image() {
      // testkit FsBuilder Fat32 48 MiB / 4096 cluster in disk(Mbr{2048}) with 4 photos and a deleted one: scan of
      // DeviceSource(MemDevice logical 4096) == scan of a FileSource of the same bytes (threads 1 and 8, events equal)
  }
  #[test] fn bad_physical_sector_fails_only_its_own_bytes() {
      // MemDevice logical 4096, bad(8192, 4096): read_at(8192 + 512, 512) Err code "read_failed"; read_at(4096, 4096)
      // Ok; a scan reports exactly Unreadable { start: 8192, len: 4096 }
  }
  #[test] fn gone_device_reports_device_gone() { /* MemDevice gone_after(3): the 4th read_at → Err code "device_gone" */ }
  #[test] fn disk_ids_parse() {
      // "disk4", "/dev/disk4", "/dev/rdisk4" → as_str "disk4"; "PhysicalDrive2", r"\\.\PhysicalDrive2" → "PhysicalDrive2";
      // "disk4s1", "", "/dev/sda", "disk", r"\\.\C:" → Err code "not_whole_disk"
  }
  // plist.rs unit
  #[test] fn plist_parses_diskutil_output() { /* fixture info-sdcard.plist → Dict with "DeviceBlockSize" Int(512) etc. */ }
  #[test] fn plist_rejects_deep_or_broken_input() { /* 40 nested <dict> → None; truncated document → None; no panic */ }
  // macos_parse.rs unit
  #[test] fn geometry_from_diskutil_and_ioreg() {
      // info-sdcard.plist + ioreg-sdcard.plist → Geometry { size: <fixture TotalSize>, logical_sector: 512,
      // physical_sector: 512 }; without ioreg → physical == logical; info without a size → None
  }
  ```
  Fixtures: captured on the dev Mac with `diskutil info -plist disk0`, `diskutil info -plist <an hdiutil-attached image
  disk>` and `ioreg -a -r -c IOBlockStorageDevice -l` (no `sudo`; serial numbers replaced by `TESTSERIAL`); the SD-card
  fixture is the attached image's capture with `Removable`/`Ejectable` set (say so in a comment).
- [ ] **Step 2:** FAIL. **Step 3:** implement (D4, D5). macOS `DeviceSource::open`: `diskutil info -plist` + `ioreg` →
  `Geometry`, then `open_device("/dev/rdiskN")`; open errors map to `permission_denied` / `device_not_found`; read errors
  by D2. **If any macOS query needs `unsafe`, stop and ask.** Windows compiles only in CI: keep FFI in `windows.rs`.
- [ ] **Step 4:** suite, clippy, fmt; `grep -rn "os::unix" crates` all gated.
- [ ] **Step 5:** core README ("Source": devices — whole disk only, raw node on macOS, aligned internally, 4Kn transparent
  with 512-byte scan sectors, root/Administrator needed, `device_gone` vs bad sector), CHANGELOG ("Added": the engine reads
  a whole card or disk directly, read-only). Commit: `feat(core): read whole devices read-only with aligned reads`.

---

### Task 3: Listar discos, conferir o destino, identificar a origem

**Files:**
- Modify: `crates/revenant-core/src/device/{mod.rs,macos_parse.rs,macos.rs}`, `src/source/windows.rs`, `src/extract.rs`,
  `src/lib.rs`, `tests/support/mod.rs` (`FakeDevices`)
- Create: `crates/revenant-core/src/device/win.rs`, `src/identity.rs`
- Test: `crates/revenant-core/tests/destination.rs` (new); unit tests in `macos_parse.rs`, `identity.rs`; fixtures
  `tests/fixtures/diskutil/{list.plist,info-root.plist,info-apfs-volume.plist,info-msdos-volume.plist,df-*.txt}`
- Docs: core README (new "Disks", "Destination"; "Extract"), CHANGELOG

**Interfaces:**
- Consumes: `DiskId`, `Geometry`, `RawDevice`, `DeviceSource` (Task 2).
- Produces:
  ```rust
  // revenant_core::device
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct VolumeInfo { pub id: String, pub fs: Option<String>, pub label: Option<String>,
                          pub mount_point: Option<std::path::PathBuf>, pub size: u64 }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct DiskInfo {
      pub id: DiskId, pub geometry: Geometry, pub vendor: Option<String>, pub model: Option<String>,
      pub serial: Option<String>, pub removable: bool, pub internal: bool, pub system: bool,
      pub virtual_disk: bool, pub solid_state: Option<bool>, pub volumes: Vec<VolumeInfo>,
  }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct DestInfo { pub disks: Vec<DiskId>, pub fs: Option<String>, pub free_bytes: u64, pub max_file_bytes: Option<u64> }
  /// Device operations a privileged helper could serve over a channel (Module 2.1). Errors are codes.
  pub trait DeviceOps: Send + Sync {
      fn list(&self) -> Result<Vec<DiskInfo>, EngineError>;           // sorted by id
      fn open(&self, disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError>;
      fn destination(&self, dir: &std::path::Path) -> Result<DestInfo, EngineError>;
  }
  /// The running OS (macOS, Windows); elsewhere every call is Err(Unsupported).
  pub struct SystemDevices;
  // identity.rs, re-exported as revenant_core::SourceIdentity
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum SourceIdentity {
      Device { size: u64, model: Option<String>, serial: Option<String>, table: Option<u64> },
      Image { size: u64, digest: u64 },
  }
  impl SourceIdentity {
      /// D7: `table` = Some(digest of sector 0 [+ GPT 1..=33], volatile boot bytes zeroed) only when `serial` is None.
      pub fn of_device(info: &DiskInfo, source: &dyn Source) -> Result<SourceIdentity, EngineError>;
      /// D7: size + FNV-1a 64 of the first, the middle (aligned down to 1 MiB) and the last MiB.
      pub fn of_image(source: &dyn Source) -> Result<SourceIdentity, EngineError>;
  }
  pub(crate) fn fnv1a64(state: u64, bytes: &[u8]) -> u64;   // offset basis 0xcbf2_9ce4_8422_2325, prime 0x100_0000_01b3
  // extract.rs
  /// A folder checked once against a source: refused (DestinationOnSource) when on the source's volume or on any disk in
  /// `source.disk()`'s set; a source with `disk()` whose destination disks cannot be determined → `destination_unknown`.
  /// An image source (`disk()` None) only gets the volume rule, without calling `ops`.
  pub struct Destination { .. }
  impl Destination {
      pub fn check(ops: &dyn DeviceOps, source: &dyn Source, dir: &Path) -> Result<Destination, EngineError>;
      pub fn dir(&self) -> &Path;
      pub fn info(&self) -> Option<&DestInfo>;
  }
  pub fn extract_to(source: &dyn Source, finding: &Finding, dest: &Destination, name: &str) -> Result<PathBuf, EngineError>;
  // extract_as(..) = Destination::check(&SystemDevices, ..) + extract_to(..); extract unchanged on top of it.
  ```
  New error: `DestinationUnknown` (`destination_unknown`). `DeviceSource::open` becomes `SystemDevices.open` +
  `DeviceSource::new`. Pure parsing in `macos_parse.rs` (compiled everywhere): `pub(crate) fn disks(list: &Plist, infos:
  &BTreeMap<String, Plist>, root: &Plist, ioreg: Option<&Plist>) -> Vec<DiskInfo>` (D8) and `pub(crate) fn destination(df:
  &str, info: &Plist) -> Option<DestInfo>` (D12). Windows (D9, D12): FFI in `windows.rs`, assembly in `win.rs`.
  `FakeDevices` (support): `new()`, `add(info: DiskInfo, data: Vec<u8>)`, `remove(id)`, `set_destination(dir, Result<DestInfo,
  EngineError-code &'static str>)`, `log() -> Vec<String>` ("list", "open disk4", …); later tasks add to it.

- [ ] **Step 1: Failing tests:**
  ```rust
  // macos_parse.rs unit (fixtures from the dev Mac, serials → TESTSERIAL)
  #[test] fn lists_whole_disks_and_flags_the_system_disk() {
      // list.plist + infos (disk0 internal SSD, disk1 synthesized APFS container on disk0s2, disk2 = attached image with an
      // exFAT volume) → ids [disk0, disk2]; disk0.system && disk0.internal; disk2.virtual_disk; disk2.volumes[0].fs ==
      // Some("exfat"), mount_point Some(..); no entry for disk1
  }
  #[test] fn destination_on_an_apfs_container_maps_to_its_physical_disk() {
      // df-root.txt + info-apfs-volume.plist → disks == [disk1, disk0] (the container and its physical store), fs
      // Some("apfs"), free_bytes == df available × 1024, max_file_bytes None
  }
  #[test] fn fat32_destination_caps_the_file_size() { /* df-msdos.txt + info-msdos-volume.plist → max_file_bytes Some(4_294_967_295) */ }
  // identity.rs unit
  #[test] fn device_identity_uses_the_table_only_without_serial() {
      // DiskInfo with serial Some → table None; without serial → Some; two exFAT boot sectors differing only in bytes
      // 106, 107, 112 → equal table digests; differing in the volume serial (byte 100) → different
  }
  #[test] fn image_identity_reads_three_mib() { /* SimSource 64 MiB: reads == [(0, 1 MiB), (32 MiB, 1 MiB), (63 MiB, 1 MiB)] in any order; one changed byte at 32 MiB + 5 → different digest */ }
  // destination.rs (FakeDevices)
  #[test] fn destination_on_the_source_disk_is_refused() {
      // SimSource.disk("disk4"); FakeDevices destination(dir) → disks [disk4] → Destination::check Err code
      // "destination_on_source"; disks [disk2] → Ok
  }
  #[test] fn unknown_destination_with_a_device_is_refused() { /* destination Err("system_tool_failed") + device source → Err code "destination_unknown" */ }
  #[test] fn image_sources_keep_the_volume_rule() {
      // FileSource: Destination::check never calls ops (FakeDevices log has no "destination") and extract_to writes the
      // same bytes extract wrote in 1.4
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (D7, D8, D9, D12, D13). `SystemDevices::list` on macOS: `diskutil list
  -plist`, then `diskutil info -plist` per whole disk and per partition, `diskutil info -plist /`, one `ioreg` call.
- [ ] **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README ("Disks": what is listed and how, system disk, solid state; "Destination": the rules, why a
  device source refuses an unknown destination; "Extract": `extract_to`), CHANGELOG ("Added": list connected disks;
  "Changed": extraction from a device refuses any volume of that device). Commit:
  `feat(core): list disks, check destinations and identify sources`.

---

### Task 4: Desmontar sem ejetar e remontar

**Files:**
- Modify: `crates/revenant-core/src/device/{mod.rs,macos_parse.rs,macos.rs,win.rs}`, `src/source/windows.rs`,
  `crates/revenant-core/tests/source.rs` (`source_has_no_write_path`), `tests/support/mod.rs`, `CLAUDE.md`
- Test: unit tests in `macos_parse.rs`; `tests/source.rs`
- Docs: core README ("Disks": unmounting), CHANGELOG, `CLAUDE.md` (the Windows exception, pt-BR)

**Interfaces:**
- Consumes: `DeviceOps`, `DiskInfo` (Task 3).
- Produces:
  ```rust
  /// Keeps what an unmount must hold: nothing on macOS; on Windows the locked volume handles (released on drop, after
  /// which Windows may mount them again on the next access). Never remounts by itself.
  pub struct UnmountGuard { .. }
  impl UnmountGuard { pub fn held(&self) -> usize; pub fn empty() -> UnmountGuard; }
  // DeviceOps gains
  /// Unmounts every volume of `disk` without ejecting (D10). Nothing mounted → Ok(empty guard).
  /// Failure → Err(EngineError::UnmountFailed { volumes }) with the volumes still mounted, code "unmount_failed".
  fn unmount(&self, disk: &DiskId) -> Result<UnmountGuard, EngineError>;
  /// macOS `diskutil mountDisk`; Windows: drops `guard` and queries each volume. Err code "remount_failed".
  fn remount(&self, disk: &DiskId, guard: Option<UnmountGuard>) -> Result<(), EngineError>;
  // macos_parse.rs
  pub(crate) fn unmount_result(ok: bool, after: &[DiskInfo], disk: &DiskId) -> Result<(), Vec<String>>;
  // windows.rs: the only write-access open in the crate, marked
  // `// WRITE-ACCESS EXCEPTION (CLAUDE.md): volume lock/dismount only, never reads or writes data.`
  pub(crate) fn lock_volume_for_dismount(volume: &str) -> io::Result<OwnedHandle>; // read-only handle first; on
  //   ERROR_ACCESS_DENIED from the FSCTL, reopen with GENERIC_READ | GENERIC_WRITE and retry; FSCTL_LOCK then DISMOUNT
  ```
  `FakeDevices` gains `fail_unmount(id, volumes: &[&str])`, `mounted(id) -> bool`, and logs "unmount disk4" /
  "remount disk4".

- [ ] **Step 1: Failing tests:**
  ```rust
  // macos_parse.rs unit
  #[test] fn unmount_failure_lists_busy_volumes() {
      // ok false, after = disk2 with volume disk2s1 still mounted → Err(vec!["disk2s1"]); ok true and no mount points → Ok
  }
  // tests/source.rs: source_has_no_write_path changes to
  #[test] fn source_has_no_write_path() {
      // as before for every file of src/source, except: windows.rs contains "GENERIC_WRITE" exactly once, on a line
      // inside `fn lock_volume_for_dismount` (between its `fn` line and the next top-level `fn`/`pub`), and the line
      // above the function holds "WRITE-ACCESS EXCEPTION (CLAUDE.md)"; device.rs, unix.rs, file.rs, fragmented.rs,
      // mod.rs contain none of the banned strings
  }
  ```
  The real unmount/remount is checked by the opt-in tests (Task 12).
- [ ] **Step 2:** FAIL (the new `source_has_no_write_path` fails until the marker exists; record it). **Step 3:** implement
  (D10). **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** `CLAUDE.md` "Diretrizes fixas", after "Nunca escrever na origem", add (pt-BR): "Exceção única
  (Sérgio, 1.5): no Windows, uma alça de **volume** pode ser aberta com escrita só para `FSCTL_LOCK_VOLUME`/
  `FSCTL_DISMOUNT_VOLUME`, quando a alça só leitura é recusada, nunca para ler ou gravar dados; fica em
  `source/windows.rs` (`lock_volume_for_dismount`), marcada, e o teste `source_has_no_write_path` confere." Core README
  ("Disks": unmount without eject, never remount automatically, the Windows lock held for the session and the exception),
  CHANGELOG ("Added": unmount a card without ejecting it, and remount on request). Commit:
  `feat(core): unmount without ejecting and remount on request`.

---

### Task 5: O mapa da cópia e a imagem lida pelo mapa

**Files:**
- Create: `crates/revenant-core/src/rescue/{mod.rs,map.rs,mapped.rs}`; Modify: `src/lib.rs`, `src/error.rs`
- Test: `crates/revenant-core/tests/rescue_map.rs` (new)
- Docs: core README (new "Rescue copy": the map), CHANGELOG

**Interfaces:**
- Consumes: `SourceIdentity` (Task 3).
- Produces (`revenant_core::rescue`):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum RegionState { Untried, Rescued, ToTrim, ToScrape, Bad }  // as_str "untried" "rescued" "to_trim" "to_scrape" "bad"
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum Pass { Fast, Skipped, Trim, Scrape, Retry(u32), Done }  // as_str "fast" "skipped" "trim" "scrape" "retry" "done"
  #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
  pub struct MapTotals { pub untried: u64, pub rescued: u64, pub to_trim: u64, pub to_scrape: u64, pub bad: u64 }
  pub const MAX_MAP_REGIONS: usize = 1 << 20;  // cfg(test) twin
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct RescueMap { .. }   // merged regions covering [0, size), plus the pass and its position
  impl RescueMap {
      pub fn new(size: u64, sector: u32, identity: SourceIdentity) -> RescueMap;  // all Untried, Pass::Fast at 0
      pub fn size(&self) -> u64; pub fn sector(&self) -> u32; pub fn identity(&self) -> &SourceIdentity;
      /// `start`/`len` multiples of the sector (the end clamps to size). false (nothing changed) when it would need a
      /// region past MAX_MAP_REGIONS.
      pub fn set(&mut self, start: u64, len: u64, state: RegionState) -> bool;
      pub fn state_at(&self, offset: u64) -> RegionState;
      pub fn regions(&self) -> Vec<(u64, u64, RegionState)>;   // merged, ascending, contiguous, summing to size
      pub fn totals(&self) -> MapTotals;
      pub fn pass(&self) -> (Pass, u64);
      pub fn set_pass(&mut self, pass: Pass, pos: u64);
      pub fn load(path: &Path) -> Result<RescueMap, EngineError>;   // "map_corrupt" on anything wrong
      pub fn save(&self, path: &Path) -> Result<(), EngineError>;   // D16: tmp + sync_all + rename (+ dir fsync on Unix)
  }
  pub fn map_path(image: &Path) -> PathBuf;   // "<image>.rvmap"
  /// The image read through its map (D18): a read touching any non-Rescued byte fails with Read { offset } of the first
  /// one. volume() = the image file's. Map size != image length → "map_mismatch".
  pub struct MappedSource { .. }
  impl MappedSource { pub fn open(image: &Path) -> Result<MappedSource, EngineError>; pub fn map(&self) -> &RescueMap; }
  impl Source for MappedSource { .. }
  ```
  Map text format (ours), UTF-8, decimal numbers:
  ```
  revenant-rescue-map 1
  size <u64> sector <u32>
  identity device <size> <hex model or -> <hex serial or -> <table hex or -> | identity image <size> <digest hex>
  pass <fast|skipped|trim|scrape|retry:N|done> <pos>
  <start> <len> <state>          (one line per region, ascending)
  end <crc32 of every byte before this line, 8 lowercase hex digits>
  ```
  New errors: `MapCorrupt` (`map_corrupt`), `MapMismatch` (`map_mismatch`), `MapOtherSource` (`map_other_source`, used
  in Task 6). The CRC is the 1.4 `filesystem::crc32` (moved to a crate-level helper if needed).

- [ ] **Step 1: Failing tests** (`rescue_map.rs`):
  ```rust
  #[test] fn map_round_trips() { /* 200 seeded random set() calls on a 64 MiB map; save; load == original; regions identical */ }
  #[test] fn map_merges_and_covers_the_whole_size() {
      // set(0, 4096, Rescued), set(4096, 4096, Rescued) → one region; set(2048, 1024, Bad) splits it in 3; regions are
      // contiguous, start at 0, sum to size; totals match
  }
  #[test] fn map_survives_a_torn_write() {
      // save v1; write half of v2's text into "<map>.tmp" (as a crash during save would leave it) → load == v1;
      // the map file truncated at each of 20 seeded byte positions → load Err code "map_corrupt" (never Ok with
      // other content); a flipped digit in a region line → "map_corrupt"
  }
  #[test] fn map_cap_refuses_new_regions() { /* cap twin 8: the set() that would make region 9 returns false, map unchanged */ }
  #[test] fn mapped_source_reads_rescued_bytes() {
      // 4 MiB image, map Rescued except [1 MiB, +64 KiB) Untried: read_at(0, 1 MiB) Ok; read_at(1 MiB − 512, 1024) Err
      // Read { offset: 1 MiB }; read_at(2 MiB, 4096) Ok
  }
  #[test] fn unrescued_zeros_are_never_intact() {
      // testkit jpeg_with_app_segments whose APP payload holds ≥ 64 KiB of zero bytes on a sector-aligned run, at 1 MiB
      // in an 8 MiB image; map Rescued except that zero run (Untried): scan(FileSource) → the jpeg Intact;
      // scan(MappedSource) → the jpeg's health != Intact and an Unreadable event exactly over the run
  }
  #[test] fn map_mismatch_is_refused() { /* map size 4 MiB, image 8 MiB → MappedSource::open Err code "map_mismatch" */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (D16, D18). **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README ("Rescue copy": region states, the map format, crash-safe saving, the cap and how it
  degrades, how a scan of the image treats unrescued regions), CHANGELOG ("Added": scanning a card image together with its
  rescue map treats the unrescued parts as unreadable, never as zeros). Commit:
  `feat(core): rescue map and scanning an image through its map`.

---

### Task 6: A cópia tolerante a setores ruins

**Files:**
- Create: `crates/revenant-core/src/rescue/copy.rs`, `src/rate.rs`; Modify: `src/rescue/mod.rs`, `src/lib.rs`
- Test: `crates/revenant-core/tests/rescue_copy.rs` (new); unit tests in `rate.rs`
- Docs: core README ("Rescue copy": phases, resume, progress), CHANGELOG

**Interfaces:**
- Consumes: `RescueMap`, `RegionState`, `Pass`, `MapTotals`, `map_path` (Task 5); `Destination` (Task 3);
  `SourceIdentity` (Task 3); `EngineError::DeviceGone` (Task 1).
- Produces:
  ```rust
  // rescue/copy.rs
  #[derive(Clone, Debug)]
  pub struct CopyOptions { pub sector: u64, pub block: u64, pub min_skip: u64, pub retries: u32,
                           pub save_every: Duration, pub save_bytes: u64 }
  // Default: sector 512, block 1 MiB, min_skip 64 KiB, retries 0, save_every 30 s, save_bytes 256 MiB.
  // max skip = min(1 GiB, size / 64) rounded down to the sector, at least min_skip (D15).
  #[derive(Clone, Debug, PartialEq)]
  pub enum CopyEvent {
      Progress { pass: Pass, pos: u64, totals: MapTotals, bytes_per_sec: u64, eta_secs: Option<u64> },
      Bad { start: u64, len: u64 },   // a region just marked Bad
      Saved,                          // the map was written
  }
  #[derive(Clone, Debug, PartialEq, Eq)] pub struct CopySummary { pub totals: MapTotals, pub complete: bool }
  /// Copies `source` into `dest.dir()/image` (D15–D17): created sparse at `source.len()` when absent; else resumed from
  /// its map, whose identity must equal `identity` ("map_other_source") and size the source's ("map_mismatch").
  /// `DeviceGone` or cancel → map saved, Err returned. `complete` = no Untried/ToTrim/ToScrape left.
  pub fn copy(source: &dyn Source, identity: &SourceIdentity, dest: &Destination, image: &str, opts: &CopyOptions,
              cancel: &CancelToken, on_event: &mut dyn FnMut(CopyEvent)) -> Result<CopySummary, EngineError>;
  // rate.rs, re-exported as revenant_core::rate
  pub struct RateMeter { .. }   // exponential moving average, half-life 10 s
  impl RateMeter { pub fn new() -> RateMeter; pub fn sample(&mut self, done: u64, at: Instant);
      pub fn bytes_per_sec(&self) -> u64; pub fn eta_secs(&self, remaining: u64) -> Option<u64>; }
  ```
  Passes (D15), exactly:
  ```
  Fast:    from the map's pass position, over Untried, in block-aligned chunks (≤ block):
           Ok → write at the same offset, Rescued, skip = min_skip
           Read error → chunk ToTrim; the next skip bytes stay Untried (jumped); skip = min(2 × skip, max_skip)
  Skipped: the remaining Untried, same chunks, no jumping (errors → ToTrim)
  Trim:    each ToTrim run: sector by sector from its start forward until a sector fails (→ Bad), then from its end
           backward until one fails (→ Bad); what is left between → ToScrape
  Scrape:  each ToScrape sector: Ok → Rescued, error → Bad
  Retry(n) for n in 1..=retries: each Bad sector read once more: Ok → Rescued
  Done
  A set() refused by the cap → the chunk becomes Bad whole (D16). DeviceGone anywhere → stop, nothing marked.
  Before every map save: image sync_data, then RescueMap::save.
  ```

- [ ] **Step 1: Failing tests** (`rescue_copy.rs`, `SimSource` from support, `Destination` over a temp dir with
  `FakeDevices`):
  ```rust
  #[test] fn clean_copy_is_byte_identical() { /* 32 MiB seeded → image bytes == source; map all Rescued; pass Done; complete */ }
  #[test] fn bad_sectors_are_isolated() {
      // bad runs (512 B at 3 MiB + 512), (4096 at 9 MiB), (1 MiB + 512 at 20 MiB − 256 KiB, crossing a block) →
      // map Bad regions == exactly those runs; every other byte Rescued and equal; the image is zero in the bad runs;
      // totals.bad == their sum
  }
  #[test] fn fast_pass_skips_ahead_and_comes_back() {
      // 64 MiB, bad(4 MiB, 8 MiB): in the read log, the first read after the first failing chunk starts ≥ its end +
      // 64 KiB; after the second consecutive failure ≥ end + 128 KiB; final Bad == [4 MiB, 12 MiB) exactly; reads
      // touching the bad region ≤ failing chunk reads + 8 MiB / 512 + 2 × (ToTrim runs)
  }
  #[test] fn copy_never_rereads_bad_sectors_without_retries() {
      // after a finished copy of bad_sectors_are_isolated's source: copy again → zero reads; with retries 2 → each
      // Bad sector read exactly 2 more times, nothing else read
  }
  #[test] fn retries_rescue_a_flaky_sector() { /* flaky(7 MiB, 512, 2): retries 0 → Bad; retries 3 → Rescued, bytes equal */ }
  #[test] fn copy_stops_on_a_vanished_device_without_marking_bad() {
      // gone_at(20 MiB) → Err code "device_gone"; saved map has no Bad and no ToTrim; revive(); copy again → complete,
      // image == source
  }
  #[test] fn copy_resumes_from_a_crash_snapshot() {
      // 5 seeds: source with 3 bad runs; save_bytes 1 MiB; at a seeded CopyEvent index copy the image and map files aside,
      // then cancel; restore them; resume → complete, Rescued bytes == source, map == an uninterrupted copy's map;
      // the resumed run reads nothing inside regions Rescued in the restored map
  }
  #[test] fn resume_against_another_source_is_refused() { /* map identity Image{digest A}, source identity B → Err code "map_other_source" */ }
  #[test] fn copy_progress_has_rate_and_eta() { /* every Progress: totals sum == size; last Progress pass Done; some bytes_per_sec > 0 */ }
  #[test] fn map_cap_degrades_without_failing() { /* cap twin 16, a bad sector every 2 MiB over 64 MiB → complete, regions ≤ 16, documented degrade */ }
  // rate.rs unit
  #[test] fn rate_meter_smooths_and_estimates() { /* 10 MB/s steady for 30 s of fake Instants → ≈ 10 MB/s ± 5%; eta(100 MB) ≈ 10 s; no samples → eta None */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. The image is opened `create_new` on the first run, `read + write` (no
  truncate) on resume, written with `FileExt::write_at` / `seek_write`; this code is in `rescue/`, never `src/source/`.
- [ ] **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README ("Rescue copy": the passes and numbers, no retries by default and why, resume, sparse
  image, never split, progress), CHANGELOG ("Added": a bad-sector-tolerant, resumable copy of a card or disk). Commit:
  `feat(core): bad-sector-tolerant, resumable rescue copy`.

---

### Task 7: Conferência prévia e reconexão

**Files:**
- Create: `crates/revenant-core/src/preflight.rs`; Modify: `src/device/mod.rs` (`reconnect`), `src/lib.rs`,
  `tests/support/mod.rs` (`FakeDevices`: `deny_open(id)`, `hang_reads(id, offset)`, `bad(id, offset, len)`)
- Test: `crates/revenant-core/tests/preflight.rs` (new)
- Docs: core README (new "Pre-flight check"; "Disks": reconnecting), CHANGELOG

**Interfaces:**
- Consumes: `DeviceOps`, `UnmountGuard`, `DestInfo`, `SourceIdentity` (Tasks 3–4); `RescueMap`, `map_path` (Task 5).
- Produces:
  ```rust
  pub const SAMPLE_TIMEOUT: Duration = Duration::from_secs(10);   // cfg(test) twin 200 ms
  pub struct PreflightRequest<'a> { pub disk: &'a DiskId, pub unmount: bool, pub copy_to: Option<&'a Path> }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum Issue {
      PermissionDenied, DeviceNotFound, NotResponding { offset: u64 }, SampleUnreadable { offset: u64 },
      UnmountFailed { volumes: Vec<String> }, DestinationTooSmall { need: u64, free: u64 }, DestinationOnSource,
      DestinationUnknown, DestinationFileTooLarge { limit: u64 }, SessionOtherSource, SessionCorrupt,
  }
  // codes: "permission_denied" "device_not_found" "device_not_responding" "sample_unreadable" "unmount_failed"
  //        "destination_too_small" "destination_on_source" "destination_unknown" "destination_file_too_large"
  //        "session_other_source" "session_corrupt" (the last two produced from Task 10 on)
  impl Issue { pub fn code(&self) -> &'static str; }
  pub struct Preflight { pub disk: Option<DiskInfo>, pub issues: Vec<Issue>, pub guard: Option<UnmountGuard> }
  /// D14, in this order: find → open → unmount → samples (first, middle, last logical sector, each SAMPLE_TIMEOUT on
  /// its own thread) → copy destination. Every issue found is reported; DeviceNotFound/PermissionDenied end it early.
  pub fn preflight(ops: &dyn DeviceOps, req: &PreflightRequest) -> Preflight;
  // device/mod.rs
  pub struct Reconnected { pub source: DeviceSource, pub unmount: Result<UnmountGuard, EngineError> }
  /// D11: lists every `poll` until a disk with this identity is back, unmounts it, THEN opens it.
  pub fn reconnect(ops: &dyn DeviceOps, identity: &SourceIdentity, cancel: &CancelToken, poll: Duration)
      -> Result<Reconnected, EngineError>;
  ```
  Copy-space rule: no map beside `copy_to` → need = disk size; a map of this disk's identity → need = size − its rescued
  bytes; a map of another identity counts as a fresh copy (need = size; the copy itself refuses it with
  `map_other_source`). `DestinationFileTooLarge` when `max_file_bytes < size`.

- [ ] **Step 1: Failing tests** (`preflight.rs`, `FakeDevices` with disk4 = 64 MiB, model "SDXC", serial "S1", one
  mounted volume disk4s1):
  ```rust
  #[test] fn healthy_card_has_no_issues() {
      // copy_to dir with free 1 GiB on disk2 → issues == []; guard Some; log order: "list", "open disk4",
      // "unmount disk4", then the 3 sample reads; disk == Some(info)
  }
  #[test] fn each_problem_has_its_code() {
      // deny_open → [PermissionDenied]; unknown id → [DeviceNotFound]; bad last sector → [SampleUnreadable { offset:
      // size − 512 }]; fail_unmount(["disk4s1"]) → [UnmountFailed { volumes: ["disk4s1"] }] and the samples still read;
      // free 10 MiB → [DestinationTooSmall { need: 64 MiB, free: 10 MiB }]; dest disks [disk4] → [DestinationOnSource];
      // dest Err → [DestinationUnknown]; msdos dest with an 8 GiB disk → [DestinationFileTooLarge { limit: 4_294_967_295 }];
      // an existing map with 48 MiB rescued and free 20 MiB → [] (need 16 MiB)
  }
  #[test] fn everything_is_reported_at_once() {
      // unmount fails + 10 MiB free + msdos on an 8 GiB disk → [UnmountFailed{..}, DestinationTooSmall{..},
      // DestinationFileTooLarge{..}] in that order
  }
  #[test] fn not_responding_device_is_reported_within_the_timeout() {
      // hang_reads(disk4, middle) → [NotResponding { offset: middle sector }] and preflight returns in < 1 s (twin 200 ms)
  }
  #[test] fn reconnect_unmounts_before_reading() {
      // disk4 removed; after 300 ms the same card comes back as disk5 with a mounted volume → reconnect(poll 50 ms)
      // returns source.disk_id() == disk5, unmount Ok; log: "unmount disk5" before "open disk5"; another card (serial
      // "S2") added meanwhile is never opened; cancel while waiting → Err code "cancelled"
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (D11, D14). **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** core README ("Pre-flight check": each code, order, the sample timeout and the stuck thread;
  "Disks": reconnecting and the remount window), CHANGELOG ("Added": a pre-flight check reports, before a long read, a
  missing permission, a silent device, a failed unmount and a copy destination that cannot hold the card). Commit:
  `feat(core): pre-flight check and reconnecting a device`.

---

### Task 8: Sinais cedo — disco falhando e velocidade da busca

**Files:**
- Modify: `crates/revenant-core/src/scan/{mod.rs,pipeline.rs}`, `src/cache.rs` (stall count),
  `crates/revenant-dev/src/main.rs` (match arms only: `DiskFailing` printed as a JSON line, `Rate` ignored)
- Test: `crates/revenant-core/tests/signals.rs` (new)
- Docs: core README ("Scan": early signals), dev README ("Output": `disk_failing`), CHANGELOG

**Interfaces:**
- Consumes: `SimSource` (Task 1), `RateMeter` (Task 6).
- Produces:
  ```rust
  // Event gains
  /// Once per scan (D20), right after the Progress of the first block boundary where the Unreadable events emitted so
  /// far reach FAILING_RUNS runs or FAILING_BYTES bytes. Deterministic.
  DiskFailing { unreadable_runs: u64, unreadable_bytes: u64, read: u64 },
  /// Only with ScanOptions::rates (D21), right after each Progress. Timing-based: outside the determinism contract and
  /// never stored in a session. `stalls`: window loads that took more than STALL so far.
  Rate { bytes_per_sec: u64, eta_secs: Option<u64>, stalls: u64 },
  pub const FAILING_RUNS: u64 = 16; pub const FAILING_BYTES: u64 = 4 << 20; pub const STALL: Duration = Duration::from_secs(2);
  // ScanOptions gains `pub rates: bool` (default false)
  // cache.rs: pub(crate) fn stalls(&self) -> u64
  ```
  dev line: `{"event":"disk_failing","unreadable_runs":16,"unreadable_bytes":8192,"read":…}` (serde rename).

- [ ] **Step 1: Failing tests** (`signals.rs`):
  ```rust
  #[test] fn failing_disk_is_flagged_early() {
      // 256 MiB SimSource (noise), a bad 512-B sector every 256 KiB from 1 MiB; block 1 MiB → exactly one DiskFailing
      // { unreadable_runs: 16, .. }, right after the Progress whose read is the first block boundary ≥ 1 MiB + 15 ×
      // 256 KiB + 512; events identical for threads 1, 8 and cache_bytes 1 MiB
  }
  #[test] fn few_bad_sectors_do_not_flag() { /* 15 isolated bad sectors → no DiskFailing */ }
  #[test] fn big_bad_region_flags_by_bytes() { /* one bad run of 4 MiB → DiskFailing { unreadable_runs: 1, unreadable_bytes: 4 MiB, .. } */ }
  #[test] fn rates_only_when_asked() {
      // default → no Rate; rates: true → a Rate right after every Progress, and removing the Rate events gives the default
      // sequence; with slow(10 MiB, 512, 2500 ms) → the last Rate has stalls ≥ 1
  }
  ```
  Existing tests that assert exact event lists and now meet the threshold get the `DiskFailing` event added explicitly
  (never loosened); list them in the report.
- [ ] **Step 2:** FAIL. **Step 3:** implement (D20, D21). **Step 4:** suite, clippy, fmt; `bench --images noise,dense-jpeg`
  (release, best of 3) on this commit and on its parent, built side by side and interleaved twice: within 5%, else stop and report.
- [ ] **Step 5:** core README ("Scan": `DiskFailing` thresholds and why, `Rate` and its exclusion from determinism),
  dev README, CHANGELOG ("Added": an early "disk failing, make a copy" signal). Commit:
  `feat(core): early disk-failing signal and scan rates`.

---

### Task 9: O estado da busca — retomar com os mesmos eventos

**Files:**
- Create: `crates/revenant-core/src/codec.rs`, `src/scan/state.rs`, `src/scan/state_tests.rs` (`#[cfg(test)] mod`)
- Modify: `src/scan/{mod.rs,pipeline.rs,fs.rs}`, `src/cache.rs` (restore bad map and coverage), `src/filesystem/listing.rs`
  (codec for `Listing`)
- Docs: core README (new "Resumable scans": the state, what a resume reads again, memory), CHANGELOG (nothing user-visible yet)

**Interfaces:**
- Consumes: everything the scan has (1.4) plus `DiskFailing` state (Task 8).
- Produces (crate-private):
  ```rust
  // codec.rs
  pub(crate) struct Enc(Vec<u8>);   // u8, u32, u64 little-endian; bool; str/bytes with u32 length; Vec with u32 count
  pub(crate) struct Dec<'a> { .. }  // every length checked against what remains → Err(EngineError::SessionCorrupt)
  // error.rs gains SessionCorrupt ("session_corrupt") here; Task 10 adds the other session errors
  pub(crate) trait Codec: Sized { fn enc(&self, e: &mut Enc); fn dec(d: &mut Dec) -> Result<Self, EngineError>; }
  // implemented for Fragment, FileKind and Health (by as_str), FsOrigin, FsTime, FsState, Placement, Volume, Finding,
  // Event (except Rate: encoding a Rate panics in debug), ScanSummary, FsFile, Listing, SourceIdentity
  // scan/state.rs
  pub(crate) struct ScanState { .. }
  impl ScanState {
      pub(crate) fn position(&self) -> u64;   // the next block's start
      pub(crate) fn events(&self) -> u64;     // events emitted up to and including this boundary's Progress
  }
  impl Codec for ScanState
  pub(crate) enum Checkpoint<'a> {
      /// Once, after the file-system listing (or with an empty listing when `filesystems` is off), before any event.
      Listing(&'a Listing),
      /// At every block boundary, after its Progress (and DiskFailing, Rate). `state()` builds the state on demand.
      Boundary { read: u64, events: u64, state: &'a dyn Fn() -> ScanState },
      /// After the pairing step, before the Updated events (pairing reads again; see D24).
      Paired { events: u64, state: &'a dyn Fn() -> ScanState },
  }
  pub(crate) struct Resume { pub listing: Listing, pub state: ScanState }
  /// `scan` = scan_with(.., None, None). With `resume`, nothing before `state.position()` is examined again and the
  /// listing is not read (D23); the events continue exactly after `state.events()`.
  pub(crate) fn scan_with(source: &dyn Source, formats: &[Box<dyn Format>], opts: &ScanOptions, cancel: &CancelToken,
      on_event: &mut dyn FnMut(Event), resume: Option<Resume>,
      hook: Option<&mut dyn FnMut(Checkpoint) -> Result<(), EngineError>>) -> Result<ScanSummary, EngineError>;
  ```
  `ScanState` holds (D22), exactly: position, events, and the `Commit` fields `summary`, `run`, `pending` (finding, ranges,
  end), `next_id`, `moovs`, `own_moovs`, `videos`, `capped`, `starts`, `next_kept`, `own` (each owner's ranges by value),
  `own_len`, `dirs`, `carved`, `orphans_capped`, the `DiskFailing` counters/flag; `Shared::skips`; the finders' tails **as
  of the position**: `Block` gains `tails: FinderTails` captured in `fill` before feeding the block, and the state takes
  `ahead.front()`'s tails, or the finders' current tails when nothing is read ahead; the cache's bad runs and its coverage
  runs at or above the position. Derived on resume, not stored: `grids`, `volumes`, `orphan_heads`, `Shared::fs`
  (`FsPlan::new(listing, ..)`). `capture` destructures `Commit` with every field named and no `..` (a new field fails to
  compile until it is in the state). On resume the cache is built with the stored bad map and coverage, `fill` starts at
  the position, and the stored events count is the starting point of the event index.

- [ ] **Step 1: Failing tests** (`scan/state_tests.rs`; cards: **A** noise 48 MiB + 8 testkit JPEGs + 3 bad runs + a testkit
  mp4 whose `moov` is 6 MiB after its `mdat` (pairing); **B** the 1.4 FAT32 card of `fs_events_identical_across_threads_and_caches`
  (live, deleted, fragmented, 2 bad sectors); **C** a quick-formatted FAT32 card (`build_quick_formatted`) with a video in 2
  runs (orphans + pairing); **D** a Partial RW2 with its JpgFromRaw at a sector start (D12); **E** a 40 MiB contiguous mp4
  crossing many blocks; all scanned with `block: 1 MiB`):
  ```rust
  #[test] fn state_round_trips() { /* every Boundary state of A–E: encode → decode → encode gives identical bytes */ }
  #[test] fn resume_at_any_block_equals_the_whole_scan() {
      // for each card, threads in [1, 2, 8, 16], cache_bytes in [default, 1 MiB]: E = uninterrupted events (with the hook
      // recording (events, encoded state) at every Boundary and the Paired point); for the first, the last and 6 seeded
      // boundaries: E[..n] ++ events(scan_with(resume = decoded state + the Listing the hook saw)) == E, and the
      // summaries are equal
  }
  #[test] fn resume_never_rereads_known_bad_sectors() { /* card A through SimSource: after resuming at each of 4 boundaries, no read overlaps a bad run present in that state */ }
  #[test] fn resume_reads_nothing_below_the_checkpoint() { /* card A without the mp4 pair, filesystems off: every resumed read starts ≥ position */ }
  #[test] fn resume_does_not_reread_an_intact_file_across_the_checkpoint() { /* card E, a boundary inside the mp4: no resumed read inside [position, mp4 end) */ }
  #[test] fn resumed_listing_is_not_read_again() { /* card B: resumed reads never touch [0, the first data cluster) except pieces of kept files starting ≥ position */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (D22–D24). `scan()` keeps its signature and behavior (it calls `scan_with`
  with `None, None`); the whole existing suite passes unchanged.
- [ ] **Step 4:** suite, clippy, fmt; `cargo test --release -p revenant-core`; `bench --images noise,dense-jpeg,exfat-card`
  (release, best of 3) on this commit and its parent, interleaved twice: within 5%, else stop and report.
- [ ] **Step 5:** core README ("Resumable scans": what the state holds, the determinism guarantee and its condition (a
  source that answers the same every time), what a resume reads again (D24), memory: the state is bounded by the existing
  caps, the bad map ≤ 2^20 runs ≈ 16 MiB). Commit: `feat(core): resumable scan state with identical events`.

---

### Task 10: A sessão salva — gravar, cair, retomar, reabrir

**Files:**
- Create: `crates/revenant-core/src/session/{mod.rs,store.rs}`; Modify: `src/lib.rs`, `src/error.rs`, `src/preflight.rs`
  (session check), `crates/revenant-dev/src/bench.rs` + `main.rs` (`bench --session`)
- Test: `crates/revenant-core/tests/session.rs` (new), `tests/preflight.rs`
- Docs: core README (new "Sessions"), dev README ("Benchmark": `--session`), CHANGELOG

**Interfaces:**
- Consumes: `scan_with`, `Checkpoint`, `Resume`, `ScanState`, `Codec` (Task 9); `Destination` (Task 3);
  `SourceIdentity` (Task 3); `Issue` (Task 7).
- Produces (`revenant_core::session`):
  ```rust
  #[derive(Clone, Debug)] pub struct SessionOptions { pub checkpoint_bytes: u64, pub checkpoint_every: Duration } // 256 MiB, 30 s
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum SessionState { Fresh, Interrupted { read: u64, total: u64 }, Finished(ScanSummary) }
  pub struct Session { .. }
  impl Session {
      /// In `dest.dir()`, which must be empty ("session_not_empty"). Stores the identity and opts.sector, opts.block,
      /// opts.filesystems (D25).
      pub fn create(dest: &Destination, identity: SourceIdentity, opts: &ScanOptions) -> Result<Session, EngineError>;
      /// Newest valid checkpoint; the event log cut to it. "session_corrupt" / "session_version" on a bad manifest.
      pub fn open(dir: &Path) -> Result<Session, EngineError>;
      pub fn dir(&self) -> &Path;
      pub fn identity(&self) -> &SourceIdentity;
      pub fn state(&self) -> &SessionState;
      /// Events stored up to the checkpoint: the first `replay_len()` events of `scan_session` are these, replayed.
      pub fn replay_len(&self) -> u64;
      /// The stored events, without any source (a finished session's results).
      pub fn events(&self) -> Result<Vec<Event>, EngineError>;
  }
  /// Replays the stored events, then scans on from the checkpoint (or from the start), checkpointing by `sopts`;
  /// `identity` must equal the session's ("session_other_source"). The session's sector/block/filesystems win over
  /// `opts`; threads, cache_bytes and rates come from `opts`. A finished session only replays (no read). Cancel or
  /// DeviceGone seen at a block boundary writes a checkpoint there; elsewhere the last one stands.
  pub fn scan_session(source: &dyn Source, identity: &SourceIdentity, formats: &[Box<dyn Format>], opts: &ScanOptions,
      session: &mut Session, sopts: &SessionOptions, cancel: &CancelToken, on_event: &mut dyn FnMut(Event))
      -> Result<ScanSummary, EngineError>;
  /// For the pre-flight: Ok when `dir` holds no session or a session of `identity`.
  pub fn check_session(dir: &Path, identity: &SourceIdentity) -> Result<(), EngineError>;
  // PreflightRequest gains `pub session: Option<&'a Path>` → Issue::SessionOtherSource / SessionCorrupt
  ```
  Errors: `SessionVersion`, `SessionOtherSource`, `SessionNotEmpty` (`SessionCorrupt` exists since Task 9; codes in Global
  Constraints). Files
  and writes exactly as D25 (`store.rs`): every file starts with `RVNTSESS`, a kind byte, version `1u16`, then its payload;
  checkpoints carry a generation `u64` and a CRC32 of their payload.

- [ ] **Step 1: Failing tests** (`session.rs`; cards A, B, C of Task 9 rebuilt from the testkit; `Destination` via
  `FakeDevices` over temp dirs; `SessionOptions { checkpoint_bytes: 1 MiB, .. }`, `block: 1 MiB`):
  ```rust
  #[test] fn uninterrupted_session_matches_scan() { /* scan_session events == scan() events; state Finished(summary); events() == them */ }
  #[test] fn finished_session_returns_results_without_reading() {
      // open the finished session; scan_session with a source whose read_at panics → the same events and summary
  }
  #[test] fn resume_from_a_crash_copy_equals_the_whole_scan() {
      // threads 1 and 8, 5 seeds per card: in on_event at a seeded index k, copy the session dir aside (what a crash
      // leaves), then cancel; Session::open(copy) + scan_session with a fresh source → all events == the reference
  }
  #[test] fn session_resumes_after_device_gone() {
      // SimSource gone_at(seeded offset) → Err code "device_gone"; revive; reopen + scan_session → events == reference;
      // no checkpoint's bad map holds a run at or past the gone offset
  }
  #[test] fn torn_checkpoint_falls_back_to_the_previous_one() {
      // after an interrupted run with ≥ 2 checkpoints, flip one byte in the newest checkpoint file → open picks the
      // older one (replay_len smaller); resume → events == reference; both flipped → open Err code "session_corrupt"
  }
  #[test] fn garbage_after_the_last_checkpoint_is_dropped() { /* append 1000 random bytes to events.rvs → open Ok, replay_len unchanged, resume == reference */ }
  #[test] fn resume_on_another_card_is_refused() {
      // device session (Device{size, "SDXC", "S1"}) resumed with Device{.., "S2"} → Err code "session_other_source";
      // with the card's image identity → the same; preflight with session: Some(dir) → [SessionOtherSource]
  }
  #[test] fn session_in_a_non_empty_dir_is_refused() { /* a file in the dir → Err code "session_not_empty" */ }
  #[test] fn session_on_the_source_is_refused() { /* Destination::check for a dir on disk4 with a disk4 source → Err "destination_on_source"; no file created */ }
  #[test] fn options_come_from_the_session() { /* created with block 1 MiB, resumed with opts.block 4 MiB → events == the 1 MiB reference */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement (D25). `bench --session` runs each image once more scanning through a session
  in the temp dir (default `SessionOptions`) and prints the line with `"session":true`.
- [ ] **Step 4:** suite, clippy, fmt; `bench --images noise,dense-jpeg --session` (release, best of 3): the session lines
  within 5% of the plain ones, else stop and report.
- [ ] **Step 5:** core README ("Sessions": layout, crash safety, cadence, replay, finished sessions read nothing, one
  session per source, the bench numbers), dev README, CHANGELOG ("Added": a scan can be saved and resumed after a
  disconnect, a crash or closing the program, with the same results as an uninterrupted scan). Commit:
  `feat(core): saved and resumable scan sessions`.

---

### Task 11: `revenant-dev` — discos, conferência, cópia, resumo e busca no aparelho

**Files:**
- Modify: `crates/revenant-dev/src/main.rs`, `crates/revenant-dev/README.md`, root `README.md` ("Trying the engine")
- Test: `crates/revenant-dev/tests/cli.rs`
- Docs: dev README (new "Reading a card directly", "Copying a card", "Saved scans"; "Making a card image" rewritten),
  CHANGELOG

**Interfaces:**
- Consumes: `SystemDevices`, `DeviceOps`, `DiskId`, `DeviceSource`, `Destination`, `SourceIdentity`, `preflight`,
  `copy`, `CopyEvent`, `MappedSource`, `map_path`, `Session`, `scan_session`, `reconnect` (Tasks 2–10).
- Produces (CLI; every output a JSON line on stdout, progress on stderr, errors `error: <code>` and exit 1):
  - `disks` → one `{"event":"disk","id":"disk4","size":…,"logical_sector":512,"physical_sector":512,"vendor":…,"model":…,
    "serial":…,"removable":true,"internal":false,"system":false,"virtual":false,"solid_state":null,"volumes":[{"id":"disk4s1",
    "fs":"exfat","label":"CARD","mount_point":"/Volumes/CARD","size":…}]}` per disk (absent options omitted).
  - `preflight <DISK> [--copy-to IMAGE] [--session DIR] [--no-unmount]` → `{"event":"preflight","disk":"disk4","issues":
    [{"code":"destination_too_small","need":…,"free":…}]}`; exit 2 when `issues` is not empty.
  - `unmount <DISK>` → `{"event":"unmount","disk":"disk4","held":0}`; `remount <DISK>` → `{"event":"remount","disk":"disk4"}`.
  - `copy <SOURCE> <IMAGE> [--retries N] [--no-unmount]` (SOURCE: a disk id or an image file) → `copy_progress` lines
    (`pass`, `pos`, the totals, `bytes_per_sec`, `eta_secs`), `copy_bad` lines, then `{"event":"copy_summary",…,"complete":…}`;
    resumes when `<IMAGE>.rvmap` exists; stderr ends with how to remount.
  - `digest <SOURCE>` → `{"event":"digest","bytes":…,"fnv64":"<16 hex>","unreadable_bytes":…}` (whole read, unreadable
    sectors hashed as zeros and counted).
  - `scan <SOURCE> [--session DIR] [--no-unmount] [existing options]`: SOURCE parsing as `DiskId` → device (unmount first,
    guard held to the end, `{"event":"unmount",…}` line or `{"event":"unmount","error":"unmount_failed","volumes":[…]}` and
    go on mounted); else a file: with `<SOURCE>.rvmap` beside it → `MappedSource` and a first line `{"event":"map",
    "rescued":…,"unrescued":…}`. `--session`: creates the dir if absent, resumes if it holds a session; replayed lines get
    `"replayed":true` and are not extracted again; `device_gone` on a device → `reconnect` (poll 500 ms, stderr says to
    plug the card back) and the scan goes on in the same session. `disk_failing` lines; `Rate` on the stderr progress line
    (`… MB/s, ETA …`). Hidden dev flag `--stop-after-events N` (cancels after N events; exit 3) for the tests.

- [ ] **Step 1: Failing tests** (`cli.rs`, image files only, no device):
  ```rust
  #[test] fn copy_of_an_image_file_is_identical() { /* copy img out.img → out.img == img; out.img.rvmap exists; last line copy_summary with complete true */ }
  #[test] fn scan_with_a_map_marks_holes_unreadable() {
      // unrescued_zeros_are_never_intact's image + a map with that hole → first line "map"; an "unreadable" line over the
      // hole; the jpeg's found line has health != "intact"; without the map file → "intact"
  }
  #[test] fn scan_session_resumes_and_replays() {
      // full = scan img; run 1: scan img --session d --stop-after-events 7 → exit 3; run 2: scan img --session d →
      // lines with "replayed" removed == full; replayed lines are a prefix and none was extracted (with --extract)
  }
  #[test] fn finished_session_scan_reads_nothing_new() { /* run 2 on a finished session: every line replayed, summary equal */ }
  #[test] fn preflight_needs_a_whole_disk() { /* preflight disk4s1 → exit 1, stderr "error: not_whole_disk" */ }
  #[test] fn digest_of_an_image() { /* digest img → "bytes" == len, "fnv64" == the FNV-1a 64 the test computes */ }
  #[test] fn disk_failing_line() {
      // an image file has no bad sectors, so a map supplies them: an 8 MiB image whose map leaves 20 isolated sectors
      // unrescued → exactly one "disk_failing" line with "unreadable_runs":16
  }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** suite, clippy, fmt.
- [ ] **Step 5:** dev README: "Reading a card directly" (macOS: `sudo revenant-dev scan disk4`; Windows: a terminal opened
  with "Run as administrator", `revenant-dev scan PhysicalDrive2`; `disks` first; **do not open the card in
  Finder/Explorer**, stop using it; the unmount, not ejecting, never remounting automatically, `remount` when done; the
  reconnect window), "Copying a card" (`preflight --copy-to`, `copy`, resuming, `--retries`, scanning the image with its
  map), "Saved scans" (`--session`, resume after a crash or a pulled cable, replayed lines), "Making a card image" now uses
  `revenant-dev copy` (ddrescue no longer recommended), every new JSON line in "Output"; root README "Trying the engine"
  mentions devices. CHANGELOG ("Added": `revenant-dev` lists disks, runs the pre-flight check, unmounts and remounts,
  copies a card with a map, prints a digest, and scans a device, an image with its map, or a saved session). Commit:
  `feat(dev): disks, preflight, copy, digest and resumable device scans`.

---

### Task 12: Testes opcionais com aparelhos de verdade (Mac e Windows)

**Files:**
- Modify: `crates/revenant-testkit/src/os_fs.rs` (T2 + `with_attached`), `.github/workflows/ci.yml`
- Create: `crates/revenant-core/tests/os_devices.rs` (`#![cfg(target_os = "macos")]`),
  `crates/revenant-core/tests/os_devices_windows.rs` (`#![cfg(windows)]`)
- Docs: dev README ("Checking against real devices (opt-in)"), CHANGELOG (nothing user-visible)

**Interfaces:**
- Consumes: everything from Tasks 2–11; `OsImage`, `OsFs`, `OsLayout` (1.4).
- Produces (`revenant_testkit::os_fs`):
  ```rust
  /// true only on macOS with REVENANT_OS_DEV=1 and hdiutil present (prints "skipped: <reason>" otherwise).
  pub fn dev_available() -> bool;
  pub struct AttachedDisk { pub disk: String /* "disk7" */, pub devices: Vec<String>, pub mount: Option<PathBuf> }
  /// Attaches READ-WRITE (so any write would show), mounted or not, runs `f`, always detaches (drop guard). Only images
  /// inside a temp dir this module created (allow-list, T2); detach only the device hdiutil printed; a failing `hdiutil
  /// info` makes the detach panic loudly instead of silently skipping (T2).
  pub fn with_attached<R>(image: &Path, mount: bool, f: impl FnOnce(&AttachedDisk) -> R) -> R;
  ```
  The engine is the only caller of `diskutil` in these tests, always on `AttachedDisk::disk` (asserted equal to the whole
  disk `hdiutil attach` printed). Never `sudo`: when `/dev/rdiskN` of the attached image cannot be opened
  (`permission_denied`), the test prints "skipped: needs read access to /dev/rdiskN" and returns (the manual checklist
  covers it).

- [ ] **Step 1: Failing tests:**
  ```rust
  // os_devices.rs (#[ignore]; run with REVENANT_OS_DEV=1 … -- --ignored --test-threads=1)
  #[test] #[ignore] fn os_device_round_trip() {
      // for (Exfat, Mbr, 64) and (Fat32, Mbr, 64): OsImage::create; with_mounted: DCIM/100TEST with 4 testkit JPEGs, an
      // Arw, an mp4; delete one JPEG; then with_attached(mount: true):
      //  1. SystemDevices.list() holds d = DiskId::parse(&att.disk) with virtual_disk, geometry.size == 64 MiB, a mounted volume
      //  2. unmount(d) Ok; list again: d still listed (not ejected), no mount point
      //  3. h1 = digest of DeviceSource::open(&d) (whole device)  [permission_denied → skipped]
      //  4. preflight(copy_to temp, session temp) → issues == []
      //  5. scan(device) events == scan(FileSource of a byte copy of the image taken after step 2)
      //  6. copy(device) → image bytes == that byte copy; complete
      //  7. scan_session with a cancel at the middle event, then resumed → events == step 5's
      //  8. h2 = digest of the device == h1 == digest of the byte copy (nothing written)
      //  9. remount(d) → list shows a mount point again
      // prints MB/s of the device reads vs the file reads (informational, for the history)
  }
  // os_devices_windows.rs (#[ignore]; REVENANT_OS_DEV=1)
  #[test] #[ignore] fn windows_vhd_round_trip() {
      // diskpart script in a temp dir: create vdisk file=<tmp>\card.vhd maximum=64 type=fixed; attach vdisk; convert
      // mbr; create partition primary; format fs=fat32 quick label=RVT; assign → write 4 testkit JPEGs through the drive
      // letter; then steps 1–8 above with PhysicalDriveN (from "detail vdisk"); detach in a drop guard.
      // On CI (CI=true) a failing diskpart step FAILS the test (no silent skip); elsewhere it prints skipped
  }
  ```
  `ci.yml` gains job `windows-devices` (`windows-latest`): `cargo test -p revenant-core --test os_devices_windows --
  --ignored --test-threads=1` with `REVENANT_OS_DEV: 1`.
- [ ] **Step 2:** without the env var both print "skipped" (record); on the dev Mac with `REVENANT_OS_DEV=1` the macOS test
  runs (record PASS, or the "skipped: needs read access" line — then say so). **Step 3:** implement. **Step 4:** suite,
  clippy, fmt; the opt-in macOS test; the 1.4 opt-in suites still pass (`REVENANT_OS_FS=1`, T2 changed `os_fs`).
- [ ] **Step 5:** dev README section (what each opt-in test does, why it never touches a physical disk, the RW attach so a
  write would show, `--test-threads=1`, the Windows job and its fallback). The Windows CI result is checked by the
  controller on the PR: if the runner cannot create or attach the VHD, a follow-up commit removes the job and the
  history records the fallback (compile + unit tests + the manual checklist). Commit:
  `test: opt-in checks against attached disk images and virtual disks`.

---

### Task 13: Pendências herdadas — HEIC, volumes 4K, teto de fotos, notas

**Files:**
- Modify: `crates/revenant-core/src/formats/isobmff.rs`, `src/filesystem/{partitions.rs,boot.rs,listing.rs,exfat.rs}`,
  `crates/revenant-testkit/src/fs.rs` (doc comment, T1)
- Test: `crates/revenant-core/tests/{isobmff.rs,fs_volumes.rs,fs_fat.rs,fs_exfat.rs,scan_fs.rs,scan.rs}`
- Docs: core README ("Formats: CR3 and HEIC", "File systems: volumes", "File systems: exFAT", "Scan"), CHANGELOG

**Interfaces:**
- Consumes: `SimSource` (Task 1), testkit `heic`, `mp4`, `FsBuilder`, `disk`, `mbr_disk`.
- Produces: no new public item. Rules: D27 (HEIC), D28 (live first across volumes).

- [ ] **Step 1: Failing tests:**
  ```rust
  // isobmff.rs
  #[test] fn unreadable_sector_after_a_complete_heic_keeps_it_intact() { /* testkit heic at 0, an unreadable sector right after its last box (BadSector source) → Intact, len == heic.len() */ }
  #[test] fn loose_moov_after_a_heic_does_not_change_its_kind() { /* heic followed by a 512-aligned orphan moov box → kind Heic, Intact, len == heic.len() */ }
  #[test] fn mp4_listing_heic_cut_before_moov_is_a_damaged_mp4() { /* ftyp major isom, compatible [isom, mif1, heic], mdat, cut before moov → kind Mp4, Damaged */ }
  // fs_volumes.rs
  #[test] fn no_ghost_volume_from_a_4k_backup_boot_sector() { /* exFAT and FAT32 volumes with 4096-byte sectors in Mbr{256} → exactly one Volume each */ }
  #[test] fn backup_boot_of_a_4k_exfat_is_found() { /* primary zeroed on a 4K exFAT → one Volume, from_backup true */ }
  #[test] fn typical_card_reads_few_sectors() { /* Counting source: volumes() of a Fat32 Mbr{8192} card reads ≤ the count measured now (pin it; say the number in the report) */ }
  // fs_fat.rs
  #[test] fn live_files_are_kept_before_deleted_ones_across_volumes() {
      // MAX_FILES twin 4: volume 0 with 4 deleted photos, volume 1 with 2 live → the listing holds the 2 live of volume 1
      // and 2 deleted of volume 0; truncated true
  }
  // fs_exfat.rs
  #[test] fn mixed_in_use_bits_follow_the_primary() { /* primary InUse set, its name entry's InUse clear → listed Live; primary clear, stream set → Deleted */ }
  // scan_fs.rs
  #[test] fn carving_at_a_skipped_start_is_not_merged() { /* a deleted FS file whose first cluster lies inside an Intact finding's skip range: merged without the carving (origin placement Guessed), never Carved */ }
  // scan.rs (release-only, #[ignore], prints numbers)
  #[test] #[ignore] fn bad_run_cap_measure() { /* SimSource of 2^30 + 2^20 virtual bytes (zeros generated) with an isolated bad sector every 1 KiB past 1 GiB: scan time and peak bad-map runs printed; the cap holds (≤ 2^20 runs) */ }
  ```
  T7 "never seen failing": temporarily make `scan` ignore `opts.filesystems` (local edit, not committed), see
  `filesystems_off_is_the_old_scan` fail, revert; record both outputs in the report.
- [ ] **Step 2:** FAIL (record each). **Step 3:** implement (D27; T3: a candidate whose boot sector is the backup of a volume
  already found — same serial, at its offset + 6 × S or 12 × S — is dropped; D28). T1: core README notes the legacy
  relative-HiddSec EBR variant is not supported; the builder's doc comment says the exFAT check by `EXFAT   ` is the
  spec's FileSystemName. T3 docs: the probe cannot find a volume whose primary boot sector and first FAT sector are both
  gone. T6: README note on mixed InUse bits.
- [ ] **Step 4:** suite, clippy, fmt; run `bad_run_cap_measure` in release and keep its numbers for the history.
- [ ] **Step 5:** core README sections above, CHANGELOG ("Fixed": a complete HEIC followed by an unreadable sector or a stray
  `moov` stays intact; an MP4 that lists a HEIC brand and lost its `moov` is reported as a damaged MP4; no duplicate volume
  on 4K-sector disks; live files are listed before deleted ones when the listing is full). Commit:
  `fix(core): close the pending items owned by part 1.5`.

---

### Task 14: Documentação, roteiro manual e fechamento da parte

**Files:**
- Modify: `README.md`, `crates/revenant-core/README.md` ("Current state", "Performance"), `crates/revenant-dev/README.md`,
  `CHANGELOG.md`, `CLAUDE.md`, `docs/superpowers/roadmap.md`
- Create: `docs/superpowers/history/<date>-m1-p5-pendencias.md` and `docs/superpowers/history/<date>-m1-p5-conferencia-manual.md`
  (`<date>` from `date +%F` on the day of this task)

- [ ] **Step 1:** `cargo test --workspace`, `cargo test --release -p revenant-core`, `cargo clippy --all-targets -- -D
  warnings`, `cargo fmt --check`, `cargo deny check licenses` (if installed; else say so), `grep -rn "os::unix" crates`
  (all gated), `REVENANT_SAMPLES=samples/cache cargo test -p revenant-core --test real_samples -- --ignored` if
  `samples/cache` exists, the 1.4 opt-in suites with `REVENANT_OS_FS=1` and `REVENANT_OS_DEV=1 cargo test -p revenant-core
  --test os_devices -- --ignored --test-threads=1`; paste the summaries.
- [ ] **Step 2:** `cargo run --release -p revenant-dev -- bench` and `-- bench --images noise,dense-jpeg --session`; update
  the core README "Performance" table (session lines) and say whether the other lines moved.
- [ ] **Step 3: Docs:** root README status (devices, copy, sessions); `CLAUDE.md` "Onde o desenvolvimento está" (pt-BR,
  today's date: 1.4 mergeada (PR #5); 1.5 concluída na branch `feat/m1-p5-real-disks`, aguardando PR/merge; decisões em
  `docs/superpowers/history/<date>-m1-p5-pendencias.md`; marco A fechado; próxima: 1.6 ou o Módulo 2, a decidir pelo
  Sérgio; o teste opcional `os_devices`/`os_devices_windows` como mais uma exceção à regra de dados reais, explicando por que
  não toca disco físico); roadmap 1.5 marcada como feita, com o que cobre (aparelho no Mac e no Windows, desmontar sem
  ejetar, conferência prévia, cópia com mapa, sessão retomável, sinais cedo) e o que não (ajudante privilegiado e janela de
  remontagem: 2.1; impedir o sono: Módulo 2; copiar e buscar juntos: ideia para depois).
- [ ] **Step 4:** history file (pt-BR, same shape as the 1.4 one): D1–D28 with their costs, decisions taken during
  execution, the bench lines, the device-vs-file speed printed by Task 12, the Windows CI outcome, the inherited table
  above with each item's result, and every item left open with its owner: **2.1** — fechar a janela de remontagem, ler sem
  root/administrador pelo ajudante; **Módulo 2** — impedir o sono, textos de cada código da conferência prévia e do
  `disk_failing`, o aviso de seguir montado; **Sérgio** — o roteiro manual (abaixo), o teto de 2^20 trechos num cartão
  morrendo, a velocidade do aparelho real contra a da imagem; **depois** — copiar e buscar ao mesmo tempo.
- [ ] **Step 5: Roteiro manual** (`<date>-m1-p5-conferencia-manual.md`, pt-BR, passos numerados com o comando exato e o que
  se espera ver; o Sérgio roda quando tiver o hardware): (1) cartão SD formatado no Mac: `revenant-dev disks`, `sudo
  revenant-dev digest disk4` (antes), `preflight`, `scan disk4 --session …` interrompido tirando o leitor e retomado, `copy`,
  `scan card.img`, `digest` (depois) igual ao de antes, `remount`; (2) o mesmo no Windows como administrador; (3) o HD
  velho com defeito lógico: busca direta, primeiros achados e tempo estimado nos primeiros minutos; (4) um cartão ou
  pendrive que falha: `disk_failing` cedo, `copy` com setores ruins, a busca na imagem; (5) velocidade: `scan` do aparelho
  contra `scan` da imagem dele (MB/s), anotar no histórico; (6) o que registrar e onde.
- [ ] **Step 6:** Commit: `docs: part 1.5 status, numbers, pending items and manual checklist`.
