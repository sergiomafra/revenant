# Parte 1.5 — decisões e pendências (25/10/2026)

Registro da execução do plano `docs/superpowers/plans/2026-10-25-m1-p5-disco-de-verdade.md`, na branch
`feat/m1-p5-real-disks` (de `main` em `08aae30`, depois do merge do PR #5). Cada decisão traz o custo se estiver errada.
O roteiro para o Sérgio conferir com hardware de verdade está em
`docs/superpowers/history/2026-10-25-m1-p5-conferencia-manual.md`.

## Decisões do pre-flight (Claude, pelo Sérgio)

### D1–D28 (decisões de produto e de desenho)

- **D1:** aparelho que some = `EngineError::DeviceGone { offset }` (`device_gone`), devolvido pela origem ou pelo cache ao
  receber leitura curta antes do `len()` anunciado (origem que encolhe). Nunca vira setor ruim; o cache guarda o primeiro
  `offset` e todo `emit` seguinte devolve o erro. Custo: imagem truncada no meio da busca para com `device_gone` em vez de
  terminar como "fim".
- **D2:** classificação dos erros de leitura: Mac `ENXIO`/`ENODEV`/`ENOENT` e Windows 1167, 433, 55, 21, 2 ⇒ sumiu; outro
  erro ⇒ setor ruim depois de conferir presença (nó `/dev/diskN` ou `IOCTL_STORAGE_CHECK_VERIFY2`). Custo: um leitor que
  devolve `EIO` para tudo ao ser puxado, mantendo o nó, marca alguns setores ruins antes de sumir.
- **D3:** o cache checa o cancelamento antes de cada carga e a cada setor da resolução setor a setor; cancelado ⇒ não
  guarda a janela nem marca setores. Custo: nenhum.
- **D4:** `DeviceSource` lê em unidades do setor lógico (alinhado vai direto ao buffer; o resto por buffer alinhado a 4096).
  No aparelho, a busca e a raspagem da cópia usam o setor **físico** (`scan_sector()`), então um setor ruim de 4096 é lido
  uma vez. Custo: sem granularidade de 512 B em discos 4Kn/512e; um memcpy a mais nas leituras desalinhadas.
- **D5:** geometria no Mac por `diskutil info -plist` (+ `ioreg` para o setor físico). Custo: aparelho 512e sem a chave sai
  com físico 512 (informativo).
- **D6:** número de série no Mac pelo `ioreg`, casado com o `BSD Name`. Custo: identidade mais fraca (o D7 compensa).
- **D7:** identidade do aparelho = tamanho + modelo + resumo FNV-1a 64 dos setores de identidade (setor 0, GPT, boot do 1º
  volume com bytes voláteis zerados), sempre; o série só confere a mais. Imagem = tamanho + resumo de três pontos; imagem
  com mapa (`of_mapped`) = o mesmo lendo o não resgatado como zeros + a identidade da origem que o mapa registra. Custo:
  reformatar o cartão entre a interrupção e a retomada faz a sessão ser "de outra origem" (correto); dois cartões clonados
  idênticos parecem o mesmo (aceito).
- **D8:** listagem no Mac: discos inteiros de `diskutil list`, sem contêineres APFS sintetizados; disco do sistema = o que
  guarda `/`; imagens do `hdiutil` com `virtual_disk: true`. Custo: arranjo exótico (RAID da Apple) pode sair com campos vazios.
- **D9:** listagem no Windows: `PhysicalDrive0..=63` abertos com acesso 0, `IOCTL_STORAGE_QUERY_PROPERTY`,
  `IOCTL_DISK_GET_LENGTH_INFO`, volumes por `FindFirstVolumeW` + extents. Custo: disco dinâmico em vários discos marca todos.
- **D10:** desmontar: Mac `diskutil unmountDisk` sem `force`; Windows `FSCTL_LOCK_VOLUME` + `FSCTL_DISMOUNT_VOLUME` com alça
  só leitura e, só se recusada, a exceção do Sérgio; as alças ficam no `UnmountGuard`. Nada remonta sozinho. Custo: no
  Windows, soltar o guarda deixa o sistema remontar no próximo acesso.
- **D11:** reconectar (P17): a cada 500 ms lista discos com mesmo tamanho e modelo; abre só leitura apenas para ler a
  identidade; se confere, desmonta antes de qualquer outra leitura e abre de novo. Custo: janela em que o sistema pode
  gravar (`.fseventsd`, marca de sujo) antes da nossa desmontagem; fechá-la é do ajudante do 2.1.
- **D12:** destino no Mac por `df -P -k` + `diskutil info`; `msdos` ⇒ arquivo máximo 4 GiB − 1; Windows por volume/GUID/extents.
  Origem aparelho + destino não identificável ⇒ `destination_unknown`. Custo: destino de rede no Mac é recusado para
  busca em aparelho.
- **D13:** `Destination` = pasta conferida uma vez contra a origem; `extract_to`, a cópia e a sessão a recebem. Custo:
  `extract_as` chama `diskutil` a cada arquivo com aparelho (o `revenant-dev` usa `Destination`).
- **D14:** conferência prévia: lista → acha → abre → desmonta → lê primeiro/meio/último setor (cada um numa thread com
  `SAMPLE_TIMEOUT` 10 s) → destino → sessão; devolve todas as pendências numa ordem fixa. Custo: uma leitura travada deixa
  uma thread presa.
- **D15:** cópia: bloco de 1 MiB; fase rápida com pulo (64 KiB dobrando até `min(1 GiB, tamanho/64)`); depois "pulados",
  "bordas", "raspagem", "repetições" (só com `retries > 0`). Custo: com erros espalhados a fase rápida deixa muito para as lentas.
- **D16:** mapa próprio em texto, `<imagem>.rvmap`, por temporário + `sync_all` + `rename`, sempre depois do `sync_data` da
  imagem; a cada 30 s ou 256 MiB, troca de fase e parada. Teto de 2^20 trechos. Custo: com milhões de erros perde-se setor
  bom vizinho de ruim.
- **D17:** imagem esparsa e nunca dividida; destino FAT32 maior que 4 GiB recusado. Custo: quem só tem pendrive FAT32 grande
  precisa formatar em exFAT.
- **D18:** `MappedSource`: leitura que toca byte não resgatado falha com `Read { offset }`; buracos viram `Unreadable`.
  Custo: nenhum.
- **D19:** cópia e busca são etapas separadas (Sérgio). Custo: disco morrendo é lido duas vezes no total (aparelho uma só).
- **D20:** `Event::DiskFailing` uma vez, na primeira fronteira de bloco com ≥ 16 trechos ou ≥ 4 MiB ilegíveis. Custo: cartão
  com 16 setores ruins espalhados mas legível no resto recebe o aviso.
- **D21:** `Event::Rate` só com `ScanOptions::rates`; fora do contrato de determinismo, nunca gravado na sessão. Custo:
  quem compara eventos com `rates: true` filtra `Rate`.
- **D22:** `ScanState` capturado só em fronteira de bloco, com o mapa de ruins inteiro; a captura desestrutura o `Commit`
  sem `..`. Custo: estado cresce com os achados pendentes.
- **D23:** a listagem do sistema de arquivos é gravada uma vez na sessão e nunca relida. Custo: o arquivo pode chegar a
  centenas de MiB no pior caso dos tetos.
- **D24:** o que a retomada relê é documentado (≤ cadência + `lookahead × block`, pedaços anteriores de arquivos do sistema
  de arquivos, o que o pareamento carrega); nunca um setor do mapa de ruins. Custo: até ≈ 320 MiB relidos com os padrões.
- **D25:** sessão = pasta vazia com `session.rvs`, `listing.rvs`, `events.rvs` (registros com CRC, só acréscimo) e dois
  pontos alternados `checkpoint-a/b.rvs`; cadência 256 MiB ou 30 s; cancelar ou sumir numa fronteira grava um ponto na
  hora. Custo: até 256 MiB refeitos depois de uma queda.
- **D26:** codificação binária própria, sem `serde` no motor; toda falha ⇒ `session_corrupt`/`map_corrupt`. Custo:
  ≈ 500 linhas de codificação, com testes de ida e volta.
- **D27:** HEIC só sem `moov` e com marca HEIC **ou** caixa `meta` no topo; um HEIC cujos extents terminam dentro do andado
  é completo. Custo: MP4 cortado antes do `moov` com `meta` solto e marca `mif1` segue HEIC.
- **D28:** teto de fotos da listagem: vivos de todos os volumes entram antes de qualquer apagado. Custo: num cartão no teto,
  apagados do volume 1 saem sem nome (a escultura os acha).

### P1–P18 (rodada do controlador antes de despachar)

- **P1:** limites `cfg(test)` não existem para `tests/*.rs`: testes de teto viram testes de unidade ou ganham parâmetros
  (`PreflightRequest.sample_timeout`, `RescueMap::with_cap`). Custo: nenhum.
- **P2:** o `revenant-dev` confere `--extract`, `--previews` e `--session` com `Destination::check` uma vez; `--allow-same-volume`
  recusado para aparelho; fecha o único caminho de escrita no cartão. Custo: nenhum.
- **P3:** listagem lida durante cancelamento ou `DeviceGone` nunca vira ponto de retomada; o `SourceProbe` repassa
  `DeviceGone`. Custo: nenhum.
- **P4:** `ScanState` conta todo evento menos `Rate`, em ordem fixa. Custo: nenhum.
- **P5:** a sessão guarda o mapa de ruins inteiro; teste confere que nenhum ruim conhecido é relido. Custo: mapa limitado
  pelo teto de 2^20 trechos (≈ 16 MiB).
- **P6:** `SimSource` e os cartões A–E em `tests/support`, incluídos por `#[path]`. Custo: nenhum.
- **P7:** identidade sempre com o resumo dos setores de identidade; imagem com mapa usa o mapa. Custo: clones idênticos
  parecem o mesmo.
- **P8:** no aparelho, busca e raspagem usam o setor físico. Custo: sem granularidade de 512 B em 4Kn/512e.
- **P9:** o teste de cancelamento precisa falhar no commit pai; senão, o relatório mostra por quê. Custo: nenhum.
- **P10:** disco virtual que declara 8 GiB sem alocá-los; `copy_to` é sempre o caminho do arquivo de imagem. Custo: nenhum.
- **P11:** imagem de zeros do teste vem de um JPEG do testkit com APP1 de enchimento parcialmente zerado. Custo: nenhum.
- **P12:** `copy` e `revenant-dev copy` conferem espaço, limite de arquivo e disco de origem antes de gravar. Custo: origem
  de imagem não consulta espaço.
- **P13:** `disk_failing` afirma ≥ 16 trechos; a retomada afirma ≥ 1 linha repetida. Custo: nenhum.
- **P14:** `source_has_no_write_path` varre também `src/device/`, `identity.rs` e `preflight.rs`. Custo: nenhum.
- **P15:** a exceção do `os_devices` entra no `CLAUDE.md` no commit da Tarefa 12. Custo: nenhum.
- **P16:** montagem de `DiskInfo`/`DestInfo`/desmontagem do Windows em `device/win_parse.rs`, puro e testado em toda
  plataforma; só a camada fina é `cfg(windows)`. Custo: nenhum.
- **P17:** na reconexão, abrir só leitura para ler a identidade antes de desmontar é permitido. Custo: janela curta em que
  o sistema ainda pode gravar num volume montado.
- **P18:** os 23 cosméticos aplicados no texto das tarefas. Custo: nenhum.

## Decisões tomadas durante a execução (Claude, pelo Sérgio)

Rulings P1 a P55 do controlador durante a execução (a numeração reinicia a do pre-flight: aqui `Ruling P…` do ledger).
Cada uma com o custo se estiver errada.

- **P1 (exec):** os overrides `cfg(test)` de limites são invisíveis para `tests/*.rs`: esses testes viram testes de
  unidade ou ganham parâmetros explícitos (`PreflightRequest.sample_timeout`, `RescueMap::with_cap`). Custo: nenhum.
- **P2:** o `revenant-dev` roda `Destination::check` uma vez para `--extract`, `--previews` e `--session`;
  `--allow-same-volume` é recusado para aparelho; extract/previews passam a usar `Destination` — fecha o único caminho de
  escrita no cartão. Custo: nenhum.
- **P3:** uma `Listing` lida durante cancelamento ou `DeviceGone` nunca é gravada como ponto de retomada; o `SourceProbe`
  precisa repassar `DeviceGone`. Custo: nenhum.
- **P4:** `ScanState` conta todo evento menos `Rate`, em ordem fixa, para o ponto depois de `DiskFailing` ser coerente.
  Custo: nenhum.
- **P5:** a sessão guarda o mapa inteiro de ruins conhecidos (não só os ≥ ponto) e um teste afirma que nenhum setor ruim
  conhecido é relido na retomada. Custo: mapa limitado pelo teto de trechos ruins já existente.
- **P6:** `SimSource` compartilhado com testes de unidade por `#[path]`. Custo: nenhum.
- **P7:** identidade do aparelho sempre inclui o resumo dos setores de identidade (bytes voláteis do boot zerados); o
  série só confere a mais; a identidade de uma imagem mapeada usa o mapa (não resgatado conta como zero) mais a identidade
  da origem que o mapa registra. Custo: dois cartões clonados idênticos parecem o mesmo (aceito).
- **P8:** no aparelho, o setor da busca e a granularidade de raspagem da cópia são o setor FÍSICO, então um setor ruim de
  4K é lido uma vez. Custo: perde-se a granularidade de 512 B em discos 4Kn (já conservador).
- **P9:** o teste de cancelamento precisa falhar no commit pai (fonte lenta com contador de leituras); se não der, o
  relatório mostra por quê e o teste fica como guarda. Custo: nenhum.
- **P10:** `SimSource`/disco virtual sem alocar 8 GiB na memória; `copy_to` tem um só significado: o caminho do arquivo de
  imagem (mapa ao lado). Custo: nenhum.
- **P11:** a fixture de JPEG com sequência de zeros zera parte de um segmento APP1 de enchimento de um JPEG do testkit.
  Custo: nenhum.
- **P12:** `copy` (motor) e `revenant-dev copy` rodam as conferências de destino (espaço livre ≥ tamanho do aparelho,
  limite de arquivo como FAT32, não estar no disco de origem) antes de gravar qualquer coisa — desenho §5.3. Custo: nenhum.
- **P13:** o teste de `disk_failing` afirma ≥ 16 trechos no evento, não exatamente 16; o de replay da sessão afirma
  contagem de linhas repetidas diferente de zero. Custo: nenhum.
- **P14:** `source_has_no_write_path` varre `src/device/`, `identity.rs` e `preflight.rs` (todo módulo que toca uma origem).
  Custo: nenhum.
- **P15:** a exceção do `os_devices` (anexar em leitura e escrita as NOSSAS imagens temporárias; VHD do `diskpart` no CI)
  entra no `CLAUDE.md` no commit da Tarefa 12. Custo: nenhum.
- **P16:** a montagem de discos do Windows fica num analisador puro, independente de plataforma e testado em toda parte;
  só a camada fina de consulta ao sistema é `cfg(windows)`. Custo: nenhum.
- **P17:** na reconexão, abrir o aparelho SÓ LEITURA para ler os setores de identidade antes de desmontar é permitido
  (abrir só leitura nunca grava); nenhuma outra leitura antes da desmontagem. Custo: janela curta em que o sistema ainda
  pode gravar num volume montado (documentada junto à janela de reconexão).
- **P18:** os 23 cosméticos aplicados ao texto do plano como o `preflight.md` propõe. Custo: nenhum.
- **P19:** copiar DE uma origem de imagem (caso de desenvolvimento) pula a conferência de espaço/limite do destino (sem
  `DestInfo`). Custo: um desenvolvedor que copia uma imagem para um disco pequeno demais falha no meio (só ferramenta de dev).
- **P20:** o resumo de identidade inclui o setor de boot do primeiro volume (bytes voláteis zerados), como o `preflight.md`
  pede. Custo: nenhum.
- **P21:** valores de teste da Tarefa 1 mudaram para os testes poderem falhar (cancelamento com cache de 8 MiB / 1 s;
  `shrink_after(8)`; cartão FAT32 de 48 MiB): os valores do brief não podiam falhar. Custo: nenhum.
- **P22:** setores ruins achados numa carga de janela cancelada ou interrompida por `DeviceGone` são descartados (D3) e
  relidos depois. Custo: uma leitura extra desses setores depois da retomada.
- **P23:** o cartão FAT32 de teste usa clusters de 512 B com fotos em fronteiras de 4096 B (48 MiB com cluster de 4096 é
  impossível em FAT32). Custo: nenhum.
- **P24:** o tamanho físico de bloco é lido de `Device Characteristics` e, depois, das propriedades do próprio aparelho (NVMe
  guarda ali) — além do D5. Custo: nenhum.
- **P25:** "uma leitura do setor ruim" = uma leitura do tamanho do setor depois da janela de 1 MiB que falhou (2 no total,
  nunca 8). Custo: nenhum.
- **P26:** `win_parse::disks` recebe o GUID do volume do sistema (não um número de disco), para um volume do sistema que
  atravessa dois discos marcar os dois (D9). Custo: nenhum.
- **P27:** chamadas a ferramentas do sistema expiram em 30 s (`system_tool_failed`); o aparelho aberto no Mac é conferido de
  novo contra o `diskutil info` (`device_not_found` se trocado); parecidos do mesmo modelo ficam para o resumo de
  identidade. Custo: uma ferramenta travada bloqueia até 30 s.
- **P28:** os caminhos `\\?\` do `canonicalize` no Windows falham FECHADO se uma chamada Win32 os rejeita
  (`destination_unknown`); o teste de CI da Tarefa 12 (pasta simples + link simbólico + junção) decide, e se pasta simples
  falhasse, a Tarefa 12 tiraria o `\\?\` de caminhos com letra de unidade. Custo: dispositivo no Windows bloqueado até a
  Tarefa 12, se estivesse errado (**confirmado na CI: link simbólico e junção recusados, pasta simples aceita, sem mudança no motor**).
- **P29:** o motor recusa desmontar o disco do sistema com um código novo, `system_disk` (defesa em profundidade; importa
  quando o ajudante do 2.1 expuser `DeviceOps`). Custo: nenhum.
- **P30:** um volume local sem mídia (`ERROR_NOT_READY`, ex.: leitor de cartão vazio) conta como "sem extents" (não pode
  estar no cartão que lemos), não como "consulta falhou"; senão qualquer PC com leitor vazio bloquearia toda desmontagem
  no Windows. Custo: nenhum.
- **P31:** a fixture de JPEG usa a maior sequência de zeros que cabe num segmento APP de enchimento (127 setores, 65.024 B)
  em vez de ≥ 64 KiB. Custo: nenhum.
- **P32:** arquivo de mapa ilegível ⇒ `open_failed`; conteúdo malformado ⇒ `map_corrupt`; `set` desalinhado devolve false
  como a recusa por teto. Custo: nenhum.
- **P33:** o `fsync` da pasta depois do `rename` do mapa é de melhor esforço (falha ignorada, documentada); o `sync` do
  próprio arquivo continua fatal. O teste opcional da Tarefa 12 copia para um destino exFAT E um FAT32 anexados. Custo: num
  sistema de arquivos que perde metadados da pasta na queda, o último mapa pode voltar um salvamento (a retomada refaz
  ≤ um intervalo).
- **P34:** retomar uma cópia com tamanho de setor diferente do do mapa é recusado (`map_mismatch`). Custo: nenhum.
- **P35:** com o mapa cheio (2^20 trechos), o resto não lido de um bloco falho é marcado `Bad` sem leitura (documentado).
  Custo: um pouco de setor bom perto de ruins em falhas extremamente fragmentadas.
- **P36:** o teste de reconexão foi adaptado ao D11: cartão com série diferente nunca é candidato (nunca é aberto); parecido
  sem série é aberto só leitura, nunca desmontado. Custo: nenhum.
- **P37:** depois que uma leitura de amostra expira, as amostras restantes são puladas (espera total ≤ um timeout); a
  thread da leitura presa fica bloqueada (documentado). Custo: uma thread bloqueada vazada por conferência com timeout.
- **P38:** a conferência prévia recusa explicitamente um disco do sistema como origem (`Issue::SystemDisk`, código
  `system_disk`) e para aí: sem desmontar, sem ler amostras. Custo: nenhum.
- **P39:** falha de abertura que não é permissão, ou listagem falha, num disco LISTADO é `Issue::OpenFailed`
  (`open_failed`), não `DeviceNotFound`. Custo: mais uma variante de `Issue`.
- **P40:** as sondas de reconexão são totalmente assíncronas, ≤ 16 em voo, sem parâmetro de timeout de identidade. Custo:
  ≥ 16 parecidos travados ao mesmo tempo deixariam o cartão de verdade sem vez (irreal para leitores).
- **P41:** um trecho ruim que termina no fim da origem (depois do último `Progress`) nunca dispara `DiskFailing`; um trecho
  aberto na fronteira de bloco conta quando o próximo setor legível é alcançado (determinístico, documentado). Custo: o
  sinal pode vir um bloco depois.
- **P42:** a diferença no bench foi tratada como ruído porque um build de controle do mesmo código espalhou igual
  (método do R36 da 1.4); medido de novo na verificação final (abaixo). Custo: uma lentidão real de ~5% descoberta tarde.
- **P43:** a cobertura do cache é podada abaixo da posição da caminhada (não da leitura à frente), então uma retomada nunca
  relê blocos dentro de um arquivo íntegro (D24) — só memória, eventos iguais, documentado. Custo: nenhum.
- **P44:** o `ScanState` carrega setor, bloco, sistemas de arquivos e total da origem, e `apply` recusa divergência (defesa
  em profundidade além da conferência da sessão). Custo: 25 bytes.
- **P45/P46:** o portão de 5% de velocidade vale para o custo PROPORCIONAL de uma sessão; um custo fixo por busca ≤ 0,5 s é
  aceito e documentado. A 4 GiB a sessão soma 0,08–0,2 s (≤ os 0,3–0,4 s de 1 GiB: fixo, não proporcional); a diferença no
  `dense-jpeg` é fatia/calor (ponto de retomada desligado não muda nada). Custo: nenhum.
- **P47:** `copy --allow-same-volume` existe só para origens de arquivo de IMAGEM (testes copiam uma imagem ao lado dela);
  recusado para disco. Custo: nenhum.
- **P48:** num disco, as pastas de saída são conferidas e o disco é aberto antes de desmontar (como na conferência): uma
  pasta recusada ou falta de `sudo` deixa o cartão montado e intocado. Custo: nenhum.
- **P49:** uma sessão de aparelho retomada precisa ser aberta com `Session::open_checked(&Destination)`; `Session::open`
  simples + retomada de aparelho ⇒ `destination_unknown`. Custo: quebra de API para quem chama `open` (ninguém fora do repo).
- **P50:** depois de uma queda, os arquivos achados entre o último ponto e a queda são extraídos de novo na retomada com
  nomes `_2` (CLI de dev; documentado). Custo: arquivos duplicados na ferramenta de dev; o Módulo 2 cuida de deduplicar.
- **P51:** o `hdiutil attach` do testkit da 1.4 escolhia a primeira entrada (uma partição em imagens MBR) como disco
  inteiro; corrigido para pegar o único disco inteiro, conferido com `hdiutil info` (correção de segurança no código da
  1.4, dentro do escopo). Custo: nenhum.
- **P52:** se o lock/dismount do Windows usou a alça só leitura ou a de escrita continua inobservável por teste; aceito
  (os dois caminhos são seguros por construção, a exceção é estreita). Custo: a exceção do `CLAUDE.md` pode ser mais larga
  que o necessário.
- **P53:** o macOS grava no volume ao remontar, então o resumo pós-`remount` é impresso, não afirmado; todos os resumos
  anteriores ao `remount` são afirmados iguais. Custo: nenhum (o `remount` é só a pedido).
- **P54:** o teste do volume fantasma usa uma segunda entrada de partição apontando para o setor de boot de backup do
  volume (uma entrada só não consegue cair no próprio backup). Custo: nenhum.
- **P55:** a troca D28 de um apagado por um vivo numa listagem cheia não devolve os pedaços do apagado ao orçamento de
  pedaços (limitado, documentado). Custo: truncamento um pouco mais cedo em listagens hostis enormes.

### Notas carregadas para este arquivo

- **C13 (Tarefa 3):** o leitor de plist do motor (`device/plist.rs`) é uma reimplementação independente do leitor do
  testkit; os dois existem de propósito (o testkit não pode depender do motor que testa), mas são código duplicado. Custo:
  uma correção num leitor precisa ser feita nos dois. Dono: quem mexer em plists (Módulo 2 ou 1.6) decide se vale unir.
- **C22 (Tarefa 4):** `DeviceOps::destination` não precisa de privilégio (só `df` e `diskutil info`), por isso a conferência
  do destino roda antes de qualquer abertura do aparelho e uma pasta recusada nunca deixa o cartão desmontado. Custo: nenhum;
  o ajudante do 2.1 não precisa expor esse método ao lado privilegiado.

## Pendências herdadas com dono 1.5: resultado

| Pendência | Resultado |
|---|---|
| 1.1: `FileSource` com aparelho | Fechada: `DeviceSource` (Tarefa 2, D4), leituras alinhadas |
| 1.1: `volume()` vazio com aparelho | Fechada: `Source::disk`, `Destination` (Tarefa 3, D12/D13) |
| 1.1/1.3: cópia própria no lugar do `dd`/`ddrescue` | Fechada: cópia com mapa (Tarefas 5, 6, 11); README "Making a card image" |
| 1.2: HEIC nunca "completo" | Fechada: D27 (Tarefa 13) |
| 1.2: MP4 `heic` cortado antes do `moov` | Fechada: D27 (Tarefa 13); custo documentado (MP4 com `meta` solto e `mif1` segue HEIC) |
| 1.3: setor de varredura > 512 B (4Kn) | Fechada: `four_kn_device_scans_like_its_image` (Tarefa 2) |
| 1.3: origem que encolhe | Fechada: D1 (Tarefa 1) |
| 1.3: cancelamento atrasado | Fechada: D3 (Tarefa 1), teste visto falhando no pai (P9) |
| 1.3: teto de 2^20 trechos ruins | Parcial: medido com origem sintética (Tarefa 13, `bad_run_cap_measure`: 0,36 s e 1024 trechos, **nunca chega a 2^20**; o caminho de estouro é coberto pelo teste de unidade `with_bad_cap(100)`); o cartão morrendo de verdade fica com o **Sérgio** (roteiro, passo 4) |
| 1.4 T1: HiddSec do EBR legado; exFAT por nome OEM | Fechada: documentada (Tarefa 13) |
| 1.4 T2: `detach_by_path`; pasta temporária por lista de negação | Fechada: lista de permissão e falha alta (Tarefa 12); P51 corrigiu a escolha do disco inteiro |
| 1.4 T3: volume fantasma 4K, sonda, ordem do `backup()`, leituras do caso típico | Fechada (Tarefa 13; P54); caso típico fixado em 8 leituras |
| 1.4 T5: teto de fotos entre volumes | Fechada: D28 (P55 documenta o que não é devolvido) |
| 1.4 T6: bits `InUse` misturados | Fechada: teste e nota (Tarefa 13) |
| 1.4 T7: "carving só se não pulado" sem teste; `filesystems_off` nunca visto falhar | Fechada: `carving_at_a_skipped_start_is_not_merged` e `filesystems_off_is_the_old_scan`, ambos vistos falhando por mutação (Tarefa 13) |

## Verificação final (25/10/2026)

Mac Intel (Core i9-9980HK, Rust 1.99), na cabeça `0cbe4ce`:

- `cargo test --workspace`: 710 passaram, 0 falharam, 13 ignorados.
- `cargo test --release -p revenant-core`: 632 passaram, 0 falharam, 10 ignorados.
- `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check`: limpos.
- `grep -rn "os::unix" crates`: todos os usos sob `#[cfg(unix)]` (ou em arquivo só macOS: `os_builders.rs`).
- `cargo deny check licenses`: **não rodado**, `cargo-deny` não está instalado neste Mac (`~/.cargo/bin` nem existe) e o
  Homebrew não funciona; a CI roda o job `deny` (verde nas execuções abaixo).
- `real_samples` (`REVENANT_SAMPLES=samples/cache`, `--ignored`): 2 passaram.
- Conjuntos opcionais, só nas imagens que os próprios testes criam (nenhum `sudo`, nenhum disco real; `hdiutil info` vazio
  antes e depois): `os_devices` (`REVENANT_OS_DEV=1`, `--test-threads=1`) 1 passou; `os_filesystems` (`REVENANT_OS_FS=1`)
  5 passaram; `os_builders` 3 passaram.

### CI do Windows

A execução 40237635001 (`35ec10c`) falhou no lint de código morto (`partition_ids` sem uso no Windows), corrigido em
`a4f3068` (execução 40239519027, tudo verde). Para a Tarefa 12, a execução 40243332458 passou **todos os 8 jobs**, incluindo
"real devices (Windows VHD)": o VHD criado pelo `diskpart` foi anexado como `PhysicalDrive2` (volume `E:`), o resumo antes
e depois do `remount` ficou igual, a leitura do aparelho deu 113 MB/s contra 373 MB/s do arquivo VHD. A execução 40242834333
tinha falhado por caminho curto 8.3 do `%TEMP%` no `diskpart` e por um `print_literal` do clippy, ambos corrigidos.

### Velocidade (`revenant-dev bench`, release, 1 GiB, melhor de 3, 16 threads)

Máquina carregada e esquentando: todas as linhas saíram bem abaixo das do fim da 1.4, inclusive as que a 1.5 não toca.

| Imagem | Fim da 1.4 (MB/s) | 1.5, rodada 1 | 1.5, rodada 2 |
|---|---:|---:|---:|
| `noise` | 2207,5 | 1403,6 | 1360,0 |
| `dense-jpeg` | 539,8 | 305,7 | 275,4 |
| `dense-raw` | 1420,3 | 938,8 | 841,5 |
| `video` | 2525,4 | 1529,2 | 1454,1 |
| `cut-jpeg` | 439,0 | 244,1 | 248,8 |
| `exfat-card` | 459,1 | 288,3 | 296,8 |

Como um build sozinho não enxerga ~6% neste Mac (R36 da 1.4), a cabeça da 1.4 (`main`, `08aae30`) foi compilada à parte e
rodada intercalada com a da 1.5, três rodadas com pausa de 30 s (MB/s, `main` → 1.5): `noise` 1322,0 / 1724,8 / 1587,6 →
1439,8 / 1285,7 / 1313,1; `dense-jpeg` 270,8 / 367,7 / 302,1 → 305,3 / 284,7 / 287,1; `exfat-card` 278,9 / 310,8 / 282,6 →
304,5 / 290,6 / 303,1. A variação do `main` contra ele mesmo (271 a 368 no `dense-jpeg`) é maior que a diferença entre os
builds e a ordem troca de rodada para rodada. Conclusão honesta: sem lentidão mensurável da 1.5, mas uma de poucos por
cento não está descartada. Os números absolutos da 1.4 não se reproduziram hoje.

Sessão (`bench --images noise,dense-jpeg --session`): `noise` 0,71 s plain contra 1,06 s com sessão, `dense-jpeg` 3,61 s
contra 3,97 s: +0,35 s e +0,36 s, o custo fixo dos `sync` (P46; a 4 GiB eram 0,08–0,2 s).

Aparelho contra imagem (impresso pelo `os_devices`, imagens anexadas pelo `hdiutil`, não um leitor de cartão): 76–79 MB/s
no aparelho cru contra 80–84 MB/s no arquivo (esta rodada); 113–127 contra 148–154 na primeira rodada da Tarefa 12. No
Windows (CI, VHD): 113 contra 373. Ler o aparelho cru custa um pouco, não um múltiplo. Com um leitor de verdade falta medir
(roteiro, passo 5).

## Em aberto, com dono

**1.6:** nada novo desta parte (arquivos em 3 ou mais pedaços e fragmentação de RAW seguem o roteiro).

**2.1 (ajudante privilegiado):**
- fechar a janela de remontagem: entre o sistema montar sozinho e a nossa desmontagem (D11/P17) ele pode gravar
  `.fseventsd` ou marca de sujo;
- ler o aparelho sem root/administrador pelo ajudante; o ajudante é quem expõe `DeviceOps` (P29 já recusa o disco do
  sistema no motor);
- a exceção do Windows (alça de volume com escrita só para lock/dismount) precisa ser reavaliada com o ajudante (P52).

**Módulo 2:**
- impedir o computador de dormir durante a leitura;
- texto de cada código da conferência prévia (`permission_denied`, `device_not_found`, `system_disk`, `open_failed`,
  `device_not_responding`, `sample_unreadable`, `unmount_failed`, `destination_*`, `session_*`) e do `disk_failing`;
- o aviso de seguir montado (quando o desmonte falha e a busca continua com volumes montados);
- deduplicar arquivos extraídos de novo depois de uma queda (P50);
- unir os leitores de plist do motor e do testkit, se valer (C13).

**Sérgio:**
- rodar o roteiro `docs/superpowers/history/2026-10-25-m1-p5-conferencia-manual.md` (cartão SD no Mac e no Windows, HD com
  defeito lógico, cartão que falha) e registrar lá;
- o teto de 2^20 trechos num cartão morrendo de verdade (passo 4 do roteiro);
- a velocidade do aparelho real contra a da imagem dele, com leitor de cartão (passo 5);
- decidir a próxima parte (1.6 ou Módulo 2) depois do merge.

**Depois:** copiar e buscar ao mesmo tempo (D19).

## Apêndice: pendências menores para a revisão final

Itens que a onda final de revisão trata (não corrigidos na Tarefa 14). Os `must-fix` primeiro.

**Must-fix da onda final:**
- Imagem retomada aceita por um LINK DURO a um arquivo do mesmo tamanho (uma sonda do revisor sobrescreveu a vítima): exigir
  `nlink() == 1` no Unix antes de gravar; no Windows, `GetFileInformationByHandle` com `nNumberOfLinks == 1` (Tarefa 6).
- Verificação de `nlink` no Windows também para `events.rvs` e para a imagem da cópia retomada; tetos de 1 GiB dos arquivos
  da sessão contra o texto do README "nunca uma alocação enorme" (apertar os tetos ou o texto) (Tarefa 10).
- `FileSource` aberto por caminho depois de checar arquivo regular (troca por nó de dispositivo entre a checagem e a
  abertura): conferir `fstat` `is_file` depois de abrir. A Tarefa 11 (rodada 2) já fechou isso com `FileSource::open`
  conferindo a alça; a onda final só confirma.
- `CLAUDE.md` "Duas exceções opcionais" contra "terceira exceção": **corrigido nesta tarefa** (agora "Três exceções opcionais").
- O job "real devices (Windows VHD)" verde precisa provar que o teste rodou (falhar na CI sem `REVENANT_OS_DEV`, ou
  procurar "attached as PhysicalDrive" na saída) (Tarefa 12). A saída da execução 40243332458 mostra a linha, mas o job não
  falha se faltar.

**Menores adiados:**
- Tarefa 1: cancelamento entre trechos de um recarregamento dividido por trecho ruim (`cache.rs`); comentário de doc do
  teste de listagem; `filesystem::read` público engole `DeviceGone`; o offset do sumiço é o início da leitura que falhou (doc).
- Tarefa 3: extents `MORE_DATA` devem falhar fechado; reconferir o `VolumeId` do `Destination` na hora de gravar; limite
  de DMG/VHD dentro do cartão (documentar); teto do tamanho de setor da identidade e `first_volume` em 4Kn; matar só o
  filho direto e ceifá-lo (sem teste); ordem de `FakeDevices::list`; listagem por IOCTL do Windows sem teto num disco
  morrendo; "movido para outro volume" é reportado como `destination_on_source` (significado errado); texto de erro de
  `system_windows_directory`; listagem em disco RAM do WinPE falha fechada.
- Tarefa 4: falha parcial no Windows solta os volumes travados (texto do README); comentário da ambiguidade de
  `ERROR_ACCESS_DENIED`; `remount` de volume RAW no Windows dá `remount_failed` espúrio.
- Tarefa 6: palavra "sparse" no Windows/HFS+/FAT; mapa terminado ainda confere espaço; nova tentativa sobre regiões
  `Bad` grossas; imagem sem mapa fica presa (README) e ramo "primeiro salvamento falha" sem teste; imagem não relacionada
  do mesmo tamanho retomada (inerente).
- Tarefa 7: comentário de `remount` de tentativa única (Windows); lacuna de teste nos offsets de leitura dos parecidos;
  releitura de identidade depois de reabrir sem teste (precisa de gancho de troca no `FakeDevices`); corrida de ms entre
  desmontar e reconfirmar; palavra "closed" para a thread do temporizador; ≥ 16 parecidos travados deixam o cartão de
  verdade sem vez; mesmo `DiskId` reaproveitado enquanto uma sonda travada ainda está em voo; teto de 16 sem teste.
- Tarefa 9: ordem do estado decodificado não totalmente validada; `starts`/`pending` sem teto fixo (texto do README);
  o teste I3 confere o auxiliar e não os pontos de chamada (overcommit do macOS); a morte de `run` depende de uma única
  escolha sorteada; documentação "cerca de 10 minutos" contra o relatório.
- Tarefa 10: o replay entrega um prefixo parcial antes de `session_corrupt` (documentar ou pré-passada de CRC); ramos sem
  teste (`log_len` de reserva, ponto de parada depois de `DiskFailing`, registro corrompido); corrida de checar-e-abrir em
  `open_regular` (só leitura, protegido por CRC).
- Tarefa 11: o re-desmonte depois da reconexão ignora `--no-unmount` (documentar); a conferência da cópia desmonta antes
  de reportar problemas de destino (a busca não); falta a dica de `remount` nos erros pós-desmontagem; pastas de
  extract/previews não reconferidas após a reconexão sob outro nome; sem tratador de Ctrl-C; `\\?\C:` (sem barra) é
  `open_failed` e não `not_whole_disk`; `//./PhysicalDrive2` é recusado (seguro).
- Tarefa 12: o cabeçalho e o README do `os_fs` exageram onde o `diskutil` roda (dizer: só verbos que mudam algo);
  `detach_by_path` silencioso quando não há disco inteiro único (chamar `loud()`); o segundo resumo do passo 8 não afirma
  nada (resumir `card.path`); o `Drop` do `Vhd` só avisa se o detach falha.
- Tarefa 13: sequência HEIF (`msf1`/`heis`) com `moov` depois de um HEIC completo é descartada (documentada na rodada de
  correção; permitir seria outra decisão); `moovless_layout` contra o tipo da caminhada em disposição artificial; a lista
  solta do CHANGELOG (corrigida na rodada de correção); o orçamento de pedaços não é devolvido na troca (P55).
