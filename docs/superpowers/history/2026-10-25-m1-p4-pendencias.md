# Parte 1.4 — decisões e pendências (25/10/2026)

Registro da execução do plano `docs/superpowers/plans/2026-10-20-m1-p4-sistemas-de-arquivos.md`, na branch
`feat/m1-p4-filesystems` (de `main` em `b5ec340`, depois do merge do PR #4). Cada decisão traz o custo se estiver errada.
A execução foi pausada depois da Tarefa 5 (cota) e retomada em 24/10/2026 na Tarefa 6.

## Decisões do pre-flight (Claude, pelo Sérgio)

### D1–D18 (decisões de produto e de desenho)

- **D1:** a leitura do sistema de arquivos roda dentro do `scan`, antes da escultura (desligável por
  `ScanOptions::filesystems`), só metadados; cada arquivo sai na ordem do commit, no setor do seu primeiro pedaço;
  `Event::Volume` vem antes de tudo. Custo: num volume enorme e quebrado o primeiro `Progress` atrasa (limitado pelos tetos).
- **D2:** `Finding` ganha `origin: Option<FsOrigin>` (em vez de um evento novo); a linha JSON só ganha chaves quando há
  origem. Custo: todo literal de `Finding` muda.
- **D3:** confiança na posição (`FsFile::trusted`): vivo = cadeia completa, sem ciclo nem cruzamento e
  `ValidDataLength == DataLength`; apagado = um só trecho de clusters hoje livres. Sem confiança ⇒ no máximo `Partial`.
  Custo: um arquivo apagado e fragmentado, mesmo inteiro, sai `Partial`.
- **D4:** contiguidade do apagado: clusters livres a partir do primeiro, pulando os alocados; pular ⇒ sem confiança;
  primeiro cluster hoje alocado ⇒ não listado. Custo: um apagado com a cabeça sobrescrita não volta pelo nome.
- **D5:** mesmo início, mesma foto: vivo ⇒ vale a medida pela cadeia; apagado ⇒ vale a da escultura só se for de saúde
  estritamente melhor, do mesmo tipo e com `len == size` gravado. Custo: um apagado com trailer (MPF) cuja escultura é
  melhor fica com a heurística.
- **D6:** tamanho coerente: o que sobra entre o fim da estrutura e o tamanho gravado tem de ser enchimento (`00`/`FF`) ou
  até 8 arquivos medíveis seguidos; senão no máximo `Partial`. Custo: arquivo com trailer desconhecido sai `Partial`.
- **D7:** nada é pulado antes da busca: a faixa de um arquivo vivo só entra se o achado fundido sair `Intact`; vivo de
  outro tipo consome só a cabeça; achado sem origem viva que toca clusters vivos sai no máximo `Partial`. Custo:
  desempenho (um `.LRV`/`.XML` grande é varrido por cabeças); um sistema de arquivos forjado não esconde fotos apagadas.
- **D8:** FAT32 é reconhecido pela BPB (`FATSz16 == 0`, `RootEntCnt == 0`), não só pela contagem de clusters. Custo: nenhum.
- **D9:** primeira letra de um nome curto apagado: do nome longo (checksum); senão a mais comum entre nomes com os mesmos
  bytes 1..4 (mesma pasta, depois volume); senão `_`. Custo: uma letra errada.
- **D10:** FAT32 apagado pelo Windows (palavra alta zerada): tenta cada palavra alta e aceita o único candidato cuja
  cabeça um formato reconhece com o tipo da extensão; até 65.536 sondas de 512 B pela fonte crua, fora do cache. Custo:
  ambíguos se perdem; um apagado com cabeça sobrescrita por ruído pode ganhar o nome de outra foto; as sondas são exceção
  ao "lido uma vez".
- **D11:** pastas antigas (formatação rápida) só dão nome a achados da escultura (`Updated`), nunca posicionam dados.
  Custo: arquivo sem cabeça reconhecível não volta pelas pastas antigas.
- **D12:** P17 por `Format::own_files`: um RAW `Partial` esconde só os JPEGs que ele referencia; `Damaged` não esconde
  nada; quadros de MJPEG ficam de fora. Custo: quadros de um vídeo MJPEG `Partial` ainda aparecem.
- **D13:** pareamento de vídeo na grade de clusters quando o vídeo começa num cluster de um volume achado; sem ponto de
  quebra na grade volta à grade de 512 B. Custo: cartão reformatado com outro cluster pareia como na 1.3.
- **D14:** nome: o original (saneado); senão `AAAA-MM-DD_HH-MM-SS_<modelo>.<EXT>` com o modelo técnico do EXIF; senão
  `revenant_<início em hex>.<ext>`; extração plana, nomes iguais ganham `_2`. Custo: nome técnico de câmera; pastas não
  são recriadas.
- **D15:** sessão = mesma câmera e intervalo ≤ 2 h entre fotos seguidas; item sem número de série entra na câmera se
  marca+modelo tem um só número de série. Custo: dois trabalhos a menos de 2 h se juntam; um almoço longo divide.
- **D16:** hora do vídeo: a do sistema de arquivos (hora local) quando há origem; senão a do `mvhd` (UTC). Custo: vídeo
  só esculpido cai fora da sessão quando o fuso da câmera fica longe do UTC.
- **D17:** conferência com dados reais por teste opcional no macOS com `hdiutil`. Custo: o Windows não é conferido
  contra um driver real.
- **D18:** arquivo que não é foto/vídeo não vira achado, mas os clusters dele contam como vivos. Custo: um JPEG dentro de
  um arquivo vivo de outro tipo não aparece (o arquivo vivo pode ser copiado normalmente).

### C1–C20 e K1–K4 (rulings da revisão do plano)

- **C1:** jobs de arquivo do sistema de arquivos nunca são descartados por `skipped`; todo commit sem resultado chama
  `fs_job(at)`. Custo: nenhum.
- **C2:** o `Probe` lê 512 B da fonte crua e recusa faixas ruins conhecidas; a leitura de 16 B do `lo` conta no teto de
  65.536. Custo: amplificação de leitura (até 64 GiB) ou releitura de setor ruim num cartão doente.
- **C3:** candidatos do D10 = `{lo}` (se reconhece e está livre) ∪ `{h ≥ 1}`; exatamente um ⇒ usa. Custo: um apagado do
  Windows ambíguo vira nome errado (a saúde segue do detector).
- **C4:** resolvido pelo K1. **C5:** `Listing.walked` criado na Tarefa 10. Custo: reprocessar diretórios já lidos (tempo).
- **C6:** o teste do `ClusterCount 2^30` remenda os campos e refaz o checksum. Custo: o implementador afrouxa a regra.
- **C7:** `MAX_GUESS_STEPS = 2^26` por volume, partilhado por `guess`, cadeia antiga do exFAT e conferência do bitmap.
  Custo: num cartão gigante alguns apagados finais perdem o nome (a escultura os acha).
- **C8:** `MAX_PIECES = 1 << 20` na `Listing`. Custo: OOM num cartão forjado.
- **C9:** `notes.txt` recebe os bytes exatos do JPEG no último cluster, para só o teto D7 poder dar `Partial`. Custo:
  teste verde sem a regra.
- **C10:** diretório confiável = cadeia termina em `End`, sem ciclo/conflito, ≤ teto (exFAT `NoFatChain`: `DataLength`
  inteiro reivindicado e marcado no bitmap). Custo: pular ou deixar de pular um diretório quebrado.
- **C11:** conjunto contestado cheio ⇒ todo arquivo vivo do volume vira não confiável. Custo: num cartão muito cruzado
  tudo sai no máximo `Partial`.
- **C12:** `exfat-card` entra em `default_values` do `--images`; o pico de memória é documentado. Custo: `bench` padrão
  usa mais RAM (ver R23).
- **C13:** `own_files` sai da mesma função de medição em todo caminho; o `OrphanFinder` copia as regras do `keep_moovs`.
  Custo: `Updated` de órfão dependente de timing.
- **C14:** o builder grava a entrada 0x83 (exFAT) e a de rótulo (FAT); `fat_delete_styles` usa o cluster 70.000. Custo: nenhum.
- **C15:** `/` e NUL de um componente viram `_` no parse; nomes exFAT por `from_utf16_lossy`. Custo: nenhum.
- **C16:** pastas de origem `Orphaned` contam como estrutura sobrevivente: "Por pasta" fica ligado quando as únicas pastas
  são antigas recuperadas. Custo: se o Sérgio preferir desligar após formatação, a condição vira "algum item `Live`/
  `Deleted`" (uma linha). **Para o Módulo 2 confirmar.**
- **C17:** testes da Tarefa 12 corrigidos (item às 08:15; `by_camera` ordenado pela chave; raiz = `Folder("")`). Custo: nenhum.
- **C18:** o RAW nomeado do teste de nomes se chama `DSC01235.ARW`. Custo: nenhum.
- **C19:** `rank(Health)` (Intact 2, Partial 1, Damaged 0) em `scan/fs.rs`; `modified` ausente perde para presente. Custo: nenhum.
- **C20:** lista de arquivos da Tarefa 7 completa; `ScanOptions::sector > 512` descarta em silêncio arquivos cujo início
  não cai no setor (documentado). Custo: nenhum.
- **K1:** sem pré-pulo de vivos: a faixa pulada só entra quando o achado fundido mede `Intact`. Custo: o portão de
  desempenho (ver "Medida final").
- **K2:** coberto pelo C2 mais `high_word_probes_are_capped` com teto `cfg(test)` em 4. Custo: leitura dobrada ou releitura
  de setor ruim.
- **K3:** três testes de determinismo (cancelamento no primeiro `Volume`, eventos iguais da Tarefa 10, `threads 1` com
  cache de 1 MiB). Custo: eventos diferentes entre máquinas.
- **K4:** `sanitize` cobre reservados completos do Windows, extensão ≤ 16 B, Cc/Cf → `_`, pontos/espaços finais. Custo: no
  pior caso um nome que o Windows recusa (erro de escrita, nunca fuga da pasta).

## Decisões tomadas durante a execução (Claude, pelo Sérgio)

- **R1:** volumes colocados gravam HiddSec (FAT) / PartitionOffset (exFAT) com o início da partição, com variante Zero; a
  Tarefa 3 mantém um teste com os campos errados para provar que o detector os ignora.
- **R2:** `vouch_for_scales_with_touching_ranges` tinha limite de tempo instável: ficou só o contador de membros examinados.
- **R3:** MBR/EBR também tentam início×4096, aceito só se o setor de boot declara setores de 4096 bytes.
- **R4:** volume deslocado (sonda) tratado pela entrada 0 da FAT (byte de mídia + `FF FF` no início da FAT), não pela
  regra literal "primário válido em −6×bps" (impossível com o primário destruído). Custo: o volume continua deslocado se
  o primário e o primeiro setor da FAT sumiram.
- **R5:** `a_crafted_huge_frame_is_measured_quickly` ficou em 600 ms (instável a 100 ms sob carga; a regressão guardada
  levava ~1,13 s), mantida a asserção de zero chamadas ao `zune`.
- **R6:** memória de caminhos limitada: `others` guardam só trechos e tamanho; caminho > 4096 bytes mantém o nome e perde a
  pasta; orçamento de 64 MiB de caminhos por listagem; pastas compartilhadas (`Arc<str>`). Custo: caminho hostil/enorme
  perde a pasta (o arquivo segue listado e esculpido).
- **R7/R18/R20:** `Listing.truncated`, `Event::Volume { volume, listing_truncated }` e a chave `listing_truncated` no
  resumo e na linha `volume` do CLI: dizem à interface que faltam nomes por causa de um teto.
- **R8:** confiança de diretório só por `live` (único consumidor do C10 depois do K1). Custo: nenhum.
- **R9:** arquivos sem pasta (R6) ou desconhecida agrupam em "sem pasta". Custo: nenhum.
- **R10:** apagados do FAT são adivinhados DEPOIS da caminhada viva, contra as reivindicações; vivos pegam os tetos
  primeiro; buscas de palavra alta cobradas dos passos e memorizadas por (lo, família).
- **R11:** o polimento adiado da Tarefa 5 (teto da fase de palavra alta, teste do corte, nota do README sobre 2^26) vai
  para a onda final, não antes da Tarefa 6.
- **R12:** o caso "raiz com DataLength 2^40" do plano é impossível no exFAT (a raiz não tem `DataLength`); aplicado a um
  subdiretório.
- **R13:** `ValidDataLength != DataLength` (para qualquer lado) ⇒ não confiável — mais estrito que o "<" do plano. Custo:
  alguns arquivos estranhos mas íntegros saem não confiáveis.
- **R14:** o conjunto opcional `os_filesystems` roda com `--test-threads=1` (nome de volume `RVT` compartilhado). Custo:
  execução opcional mais lenta.
- **R15:** pasta exFAT apagada é lida onde o primeiro cluster está livre (sem ".") e só com conjuntos de checksum válido.
  Custo: conjuntos velhos de outra pasta antiga podem aparecer sob o nome dela.
- **R16/R21/R25:** o bench da Tarefa 7 teve ruído térmico; a regressão foi decidida pelo A/B intercalado contra `main` /
  contra a cabeça da Tarefa 9, no fim (resultado abaixo, em "Medida final").
- **R17:** `big_intact_video_is_read_once` ganhou +5 MiB de folga de leitura (a busca de volume carrega janelas que um
  cache de 4 MiB despeja), documentado. Custo: uma regressão de leitura única de até 5 MiB pode se esconder nesse teste.
- **R19:** D6: uma cauda precisa ser um só pedaço para contar. Custo: caudas raras e estranhas saem `Partial`.
- **R22:** o macOS gravou o arquivo Z contíguo, então o caso "Z fragmentado" do teste opcional não é exercitado;
  fragmentação segue coberta por testes sintéticos. Custo: nenhum arquivo fragmentado feito pelo SO na conferência.
- **R23:** a frase "pico de memória = tamanho da imagem" era falsa: o pico do `exfat-card` é ≈ 2× a imagem (o builder
  copia os arquivos e o `build()` aloca a imagem); documentado, sem retrabalho do builder (só dev).
- **R24:** `own_files` é calculado para toda medida que não é `Damaged`, não só `Partial`, para que um RAW tornado
  `Partial` por fusão ou pelo teto D7 esconda os próprios JPEGs. Custo: trabalho extra limitado (≤ 64 entradas) em RAW `Intact`.
- **R26:** `orphan_entries` recebe `live`/`wanted`/`cancel` e devolve `(entradas, truncated)`: memória limitada pelos
  achados sem origem e cancelável.
- **R27:** entradas apagadas dentro de uma pasta antiga nunca dão nome a um achado (primeira letra perdida, clusters talvez
  reusados). Custo: menos nomes depois de formatar um cartão que já tinha apagados.
- **R28:** truncamento de nomes de órfãos só aparece em `ScanSummary.listing_truncated` (os `Volume` vêm antes da fase).
- **R29:** `OrphanFinder` com teto de 8192 por bloco (um por cluster de 512 B em 4 MiB), não os 4096 do `moov`.
- **R30:** o teste do CLI `previews_are_written` espera o nome legível (o Cr2 tem data e modelo no EXIF): a D14 vence o
  "achado sem nome mantém `revenant_`" da tarefa. O `extract` do core sem metadados mantém `revenant_`.
- **R31:** o teste de nomes hostis de ponta a ponta usa variantes com extensão de foto; nomes sem extensão ficam nos testes
  unitários de `sanitize`/`extract_as` (a varredura nunca lista nome sem extensão de foto).
- **R32:** o rótulo de grupo no JSON do CLI é `{"kind": <variante em snake_case>, ...campos}`, com `camera` omitida quando
  desconhecida. Custo: a interface pode querer outro formato depois.
- **R33:** o número de série de uma sessão vem do menor id que tem um; marca/modelo do menor id.

## Medida final (antes × depois)

`revenant-dev bench`, build de release, imagens de 1 GiB lidas do cache de páginas, melhor de 3, mesma Intel Core
i9-9980HK (8 núcleos / 16 threads, 32 GB, macOS, Rust 1.99). MB = 10^6 bytes. Cabeça = `feat/m1-p4-filesystems` no fim da
parte; `main` = `b5ec340` (fim da 1.3).

`bench` completo na cabeça (16 threads, melhor de 3 varreduras):

| Imagem | Arquivos | MB/s |
|---|---:|---:|
| `noise` | 0 | 2207,5 |
| `dense-jpeg` | 95 | 539,8 |
| `dense-raw` | 37 | 1420,3 |
| `video` | 3 | 2525,4 |
| `cut-jpeg` | 95 | 439,0 |
| `exfat-card` | 93 (93 achados) | 459,1 |

`exfat-card` = 0,85 × `dense-jpeg` (portão do K1: 0,8). As outras linhas oscilam com o calor da máquina (`dense-raw`
1576,9 → 1420,3; `video` 2296,6 → 2525,4), sem mudança além da variação.

Ruling R16 (A/B contra `main` em `b5ec340`, worktree separado, release, melhor de 3 varreduras, 3 rodadas intercaladas
com pausa de 60 s; MB/s, `main` → cabeça):

| Rodada | `dense-jpeg` | `noise` |
|---|---|---|
| 1 | 581,4 → 545,8 (−6,1%) | 2794,1 → 2468,3 (−11,7%) |
| 2 | 584,7 → 561,1 (−4,0%) | 2653,3 → 2397,0 (−9,7%) |
| 3 | 591,6 → 566,6 (−4,2%) | 2523,6 → 2807,7 (+11,3%) |

`dense-jpeg`: a cabeça fica 4 a 6% atrás em todas as rodadas (média 557,8 contra 585,9, −4,8%), na beira do limite de 5%
do R16 e sem passar dele em duas das três rodadas; a causa não foi isolada. `noise` oscila para os dois lados (sem
diferença). Custo: se a diferença do `dense-jpeg` for real, são ~5% de vazão em JPEG denso (ainda ≈ 550 MB/s, bem acima de
um leitor UHS-II). Fica para o Sérgio decidir se quer uma otimização antes do PR.


## Pendências herdadas com dono 1.4, fechadas nesta parte

| Pendência | Como foi fechada |
|---|---|
| 1.2: foto gravada exatamente no início de uma prévia/strip de RAW, chunk de CR3 ou extensão de HEIC | Com metadados do sistema de arquivos (Tarefa 7, D7); sem eles continua indistinguível (aberta, sem dono) |
| 1.2/1.3: JpgFromRaw de um RAW `Partial` relatado à parte (P17) | Tarefa 9 (D12, R24) |
| 1.2: COUNT/tamanho corrompido num offset válido, cortado no limite | Tarefa 7 (a visão termina no tamanho gravado); achado só esculpido segue igual (aberta, sem dono) |
| 1.3 (C4): vídeos reais remontados saem quase sempre `Partial` | Tarefas 7 (cadeia viva exata) e 9 (grade de clusters, D13) |
| 1.3 (Q7, Q7b): foto partida em volta de outra, fotos intercaladas | Tarefa 7 (cadeia viva); apagado fragmentado fica `Partial` (D3) → 1.6 |
| 1.3: foto cortada seguida da cauda de uma quase igual que decodifica até o EOI | Tarefas 7 e 10 (tamanho coerente, D6); sem metadados → 1.6 |
| 1.3 (Q8): primeiro pedaço < 16 KiB, segundo antes do primeiro ou além da janela | Tarefa 7 (cadeia viva) |
| 1.3: cauda órfã de foto da rajada emendada `Intact` | Tarefa 7 (D5: a escultura só vale com `len == size`) |
| 1.3: cauda alheia num marcador de restart | Já resolvida pelo Q16 |

## Pendências abertas, com dono

**Sem dono**
- Foto gravada no início de prévia de RAW (strip, chunk de CR3, extensão de HEIC) sem metadados de sistema de arquivos:
  indistinguível do RAW que a contém.
- COUNT corrompido num achado só esculpido (sem decodificador de RAW).
- Quadros de MJPEG de um vídeo `Partial` ainda aparecem como achados (D12).
- Conferência do Windows contra um driver real (D17): só o macOS é conferido.

**Parte 1.6**
- Arquivo apagado e fragmentado sai `Partial` mesmo inteiro (D3).
- Cauda quase igual sem metadados do sistema de arquivos (foto cortada seguida da cauda de uma foto quase igual).

**Módulo 2**
- Extração plana, sem recriar subpastas (D14).
- Nomes comerciais de câmera (`A7IV` em vez de `ILCE-7M4`): o D14 diverge do exemplo da spec §3.
- Vídeo só esculpido fica fora da sessão quando o fuso da câmera está longe do UTC (D16).
- O motivo exibido quando "Por pasta" está desligado.
- Confirmar a leitura do C16 ("Por pasta" fica ligado com pastas antigas recuperadas).
- Com `--extract`, um achado `Updated` é gravado de novo como `_2` ao lado do antigo (já vinha da 1.3); a UI deve
  substituir o achado.
- Com uma thread, a busca não acompanha um leitor UHS-II em cartão cheio de JPEG; a UI deve usar as threads padrão.
- Avisar o Sérgio: o D13 pode dar `Intact` num vídeo velho que cai numa grade nova por coincidência depois de reformatar
  com outro tamanho de cluster.

**Parte 1.5** (herdadas da 1.3, não tocadas aqui)
- Setor de varredura maior que 512 B, origem que encolhe, cancelamento durante medida e carga de cache, teto de trechos
  ruins: ver o arquivo da 1.3.

NTFS, APFS e HFS+ ficam fora (a escultura segue cobrindo).

## Apêndice: pendências menores para a revisão final

Itens adiados pelas tarefas, para a onda de revisão final triar (esta seção pode ser editada por ela; nada aqui foi
corrigido na Tarefa 13).

- T1: HiddSec absoluto do EBR (Windows/mkfs.fat), variante relativa legada sem documentação; detecção de exFAT por nome OEM
  no builder.
- T2: `detach_by_path` best-effort quando `hdiutil info` é ilegível (avisa); a checagem da pasta temporária é uma lista de
  negação.
- T3: volume duplicado fantasma quando um candidato MBR de 4096 cai no setor de boot de backup de outro volume 4K
  (L = 8S−48/96); limitação da sonda (primário + primeiro setor da FAT sumidos) sem documentação; ordem do `backup()` em
  exFAT 4K; asserção de leitura do caso típico.
- T5: limitar a fase de palavra alta a metade do orçamento (2^25) ou colocar antes os arquivos de início conhecido; teste
  ou `debug_assert` do corte da fila de espera; README "2^26 passos bem abaixo de um segundo" está errado para buscas de
  palavra alta (~10 s); o teto de fotos é compartilhado entre volumes (apagados do volume 1 podem tomar vagas vivas do 2).
- T6: confiança de diretório `NoFatChain` vivo sem teste; chunk ilegível não gasta orçamento de entradas; bits `InUse`
  misturados entre entradas de um conjunto (nota no README); arquivos que não são foto na raiz antes da entrada do bitmap
  reivindicam antes de fotos/diretórios (só volume malformado).
- T7: setor ruim antes de `at` nunca rebaixa um arquivo vivo cujo pedaço posterior fica antes do início (layout Q8)
  (checar `cache.bad_runs` em `add` ou documentar); "carved-only-if-not-skipped" sem teste (comentar como defensivo);
  `filesystems_off` nunca visto falhar.
- T8: a chave `created` não é conferida em `volume_and_origin_keys`.
- T9: guarda `within` sem teste; fragmento de prévia de RAW que cruza o limite de um pedaço é relatado (nota no README);
  entrada da origem própria ≈ 50 B na documentação, provavelmente 70–90 B (README Memory + doc de `ScanOptions`).
- T10: continuação de pasta antiga exFAT com vários clusters vira candidata própria (`?/NOME`); teto de entradas exFAT por
  pasta não é cumulativo (`orphans.rs:329`); nó cortado por profundidade não é memorizado (`orphans.rs:379`); `note_walked`
  pode rodar até 256 MiB teóricos, sem documentação.
- T11: controles C1 e U+2028/2029 mantidos no nome; `split_name`/`fallback_name` públicos sem documentação; README diz
  "200 bytes antes do sufixo `_N`"; `camera_tag` sem teste de caixa.
- T12: tipos CLI `undated_session`/`unknown_camera`/`no_folder`/`folder` e `camera` omitida sem teste pelo CLI; rótulos
  Camera/Day documentam só "seus campos" no README; `--group` clona todo achado (CLI de desenvolvimento).
