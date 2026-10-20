# Parte 1.3 — decisões e pendências (20/10/2026)

Registro da execução do plano `docs/superpowers/plans/2026-10-20-m1-p3-fragmentos-e-velocidade.md`, na branch
`feat/m1-p3-fragments-speed` (de `main` em `ef5e345`). Cada decisão traz o custo se estiver errada. Detalhes do pre-flight
em `.superpowers/sdd/2026-10-20-m1-p3-fragmentos-e-velocidade/preflight.md`.

## Decisões do pre-flight (Claude, pelo Sérgio)

- **Q1:** um JPEG de dois pedaços só é `Intact` com exatamente um candidato aprovado em toda a janela de 64 MiB, e todos
  tentados (teto de 4096 tentativas, `MAX_JOIN_TRIES`, e orçamento de leitura sem estouro); com dois ou mais aprovados
  fica o mais próximo, no máximo `Partial`. Custo: algumas junções reais saem `Partial`.
- **C1:** busca exaustiva com teto de 4096 tentativas; bater no teto ou no orçamento ⇒ `unique = false`; a tentativa
  aborta na linha de MCUs logo depois da emenda se o teste de DC já falha. Custo: mais decodificação por JPEG cortado
  (limitada pelo teto e pelo orçamento).
- **C2:** resolvido pelo teto de C1 (o número de tentativas de cada teste ficou no relatório da Tarefa 8). Custo: nenhum
  além do de C1.
- **C3:** o limite `x` do primeiro pedaço é o menor entre o erro de decodificação, o corte e o primeiro setor de um byte
  repetido (setor uniforme). Custo: uma foto com 512 bytes uniformes nos dados de entropia não é juntada (fica `Partial`).
- **C4:** mantida a regra "mais de um `L1` aprovado → o menor, `Partial`"; os testes `Intact` usam NAL pequenas. Vídeos
  reais H.264/HEVC com fatias grandes (e quase todo vídeo com som) saem `Partial` até a 1.4, quando o tamanho de cluster
  estreita o `L1`. Custo: a maioria dos vídeos reais remontados aparece `Partial` embora toque (no máximo um quadro
  estragado).
- **C5:** `pair_videos` recebe o cache e usa o mapa de setores ruins nos dois pedaços. Custo: pequeno acoplamento ao tipo
  do cache.
- **C6:** read-ahead = `min(16, max(1, cache_bytes / (2 × bloco)))` blocos, para caber no cache. Custo: menos
  sobreposição com cache minúsculo.
- **C7:** vídeo em três pedaços: o teste corta em 2/5 e 3/5; uma peça do meio com menos de 1/32 das amostras não é
  detectada. Custo: um vídeo raro de 3 pedaços com meio minúsculo é pareado (como se fossem dois).
- **C8:** filtro `(q − P) ≡ 0 (mod 512)` antes de carregar o `moov` e orçamento de 1 GiB de `moov` carregado por
  varredura. Custo: numa enxurrada de `moov` órfãos, alguns pares reais se perdem.
- **C9:** recarregar uma janela lê em volta dos trechos ruins conhecidos; o mapa de setores ruins guarda até 1.048.576
  trechos. Custo: um cartão com milhões de setores ruins espalhados tem os excedentes relidos (mais lento, mesmos eventos).
- **C10:** "cada setor lido uma vez" vale para a varredura e os detectores; a busca de lacuna de um JPEG pode reler a sua
  janela de 64 MiB. O `cut-jpeg` não tem meta, só é medido (final: 419,3 MB/s). Custo: nenhum (a documentação fica
  exata).
- **M1–M13** (ajustes de uma linha no plano, sem decisão de produto): arquivos de formato que faltavam na Tarefa 3 (M1),
  passo velho dos literais de `ScanOptions` (M2), `forget_coverage_below` no cache (M3), fonte de teste com vários
  trechos ruins definida na Tarefa 4 (M4), `lib.rs` na Tarefa 6 (M5), `index_health(.., false)` (M6),
  `MAX_DECODE_PIXELS` visível e `interval = 0` sem DRI (M7), quadros de teste do `seam_ok` (M8), contador de alocação por
  thread (M9), offset do erro de leitura no teste (M10), candidatos a `moov` em achado `Intact` no commit descartados
  sempre (M11), dono para 3+ pedaços e RAW (M12, abaixo), testes de corrupção que não podiam virar `Intact` (M13).
  Custo: nenhum.
- **M14:** o CHANGELOG fica em inglês, pela regra do dono posterior à spec (CLAUDE.md); a spec §8 ainda diz pt-BR e está
  desatualizada nesse ponto. Custo: nenhum (falta só corrigir a spec quando ela for revista).

## Decisões tomadas durante a execução (Claude, pelo Sérgio)

- **Q2:** o limite do `walk` do TIFF segue `min(disponível, TIFF_MAX_LEN)`, com `starts_past_cap` ⇒ `Damaged`; a saúde não
  depende da posição no teto e acima dele; abaixo do teto, uma faixa que começa depois do fim da origem é corte. Custo: um
  RAW perto do fim da origem com um offset ruim abaixo do teto sai `Partial` em vez de `Intact`/maior.
- **Q3:** o `CachedSource` não guarda lista de buffers sobrando; despeja a janela pronta mais velha antes de carregar e
  reaproveita o buffer dela. Custo: nenhum medido (sonda com 8 threads e cache de uma janela: dados certos, setor ruim
  lido no máximo 2 vezes).
- **Q4:** medida que quebra o contrato de pedaços é ignorada no build de release e dispara `debug_assert` no de debug.
  Custo: um bug de detector perde uma foto em silêncio no release (os testes de debug pegam).
- **Q5:** os dados de entropia contados de um JPEG acabam no primeiro setor de 512 bytes uniforme ou ilegível (ajudante
  `data_end`, reaproveitado pela busca de lacuna). Custo: uma foto real com uma área chapada codificada como um só byte
  num setor inteiro (raro, mas possível com tabelas de Huffman otimizadas, tudo `00`) é cortada ali.
- **Q6:** para JPEG que o decodificador próprio cobre (SOF0/SOF1, 8 bits, uma varredura), a prova de `Intact` é ele chegar
  ao EOI com exatamente o número de MCUs esperado (e os RST em sequência); o `zune-jpeg` rigoroso não roda para esses.
  Custo: um JPEG sequencial com coeficientes válidos na entropia mas semanticamente quebrados sai `Intact`.
- **Q7:** `unique` é falso quando um grupo de setores de um EOI foi truncado (setores não tentados podiam ser a
  continuação), a menos que todo setor não tentado se explique por uma cabeça de arquivo cuja medida contígua é `Intact`
  até aquele EOI; "duas fotos gravadas intercaladas" fica como não coberto. Custo: algumas remontagens reais saem
  `Partial`.
- **Q7b:** a cabeça que explica precisa ter outro quadro (geometria, tabelas, intervalo de restart). Custo: uma foto
  partida em volta de uma foto inteira da mesma câmera fica `Partial`.
- **Q8:** limites contra cabeças hostis: busca só com primeiro pedaço ≥ 16 KiB; orçamento de 64 MiB lidos por busca;
  tentativas que passam pelo mesmo estado de um fracasso param (até 16.384 estados); cancelamento olhado a cada linha de
  MCUs. Custo: uma foto fragmentada cujo primeiro pedaço tem menos de 16 KiB não é remontada.
- **Q9:** JPEG coberto cujo próprio caminho decodifica menos de uma linha de MCUs antes de falhar ⇒ `Damaged`, sem a
  decodificação tolerante. Custo: nenhum relevante (menos de uma linha não mostra nada).
- **Q10:** para JPEG coberto o `zune-jpeg` tolerante saiu de vez: ≥ 1 linha e depois falha ⇒ `Partial`; < 1 linha ⇒
  `Damaged`; EOI exato ⇒ `Intact`; o tolerante só fica para quadros não cobertos. Custo: um JPEG coberto que decodifica
  ≥ 1 linha mas que nenhum programa mostraria sai `Partial` em vez de `Damaged`.
- **Q11:** o pareamento aplica o filtro `c_min`/`c_end` antes de listar amostras, e um orçamento de 16.000.000 amostras
  listadas por varredura, cobrado pelo `stsz` antes de alocar (estouro ⇒ no máximo `Partial`). Custo: um cartão com mais
  de 16 milhões de amostras de vídeo listadas em `moov` órfãos tem pareamentos `Partial`.
- **Q13 (revisão final):** o pareamento de vídeo exige `L1` ≤ o comprimento do achado `Damaged` (a varredura já o cortou
  onde outro arquivo começa) e nenhum outro achado dentro do primeiro pedaço (senão não pareia); um achado que começa
  dentro do segundo pedaço deixa o pareamento no máximo `Partial`, como uma cabeça corta um vídeo contíguo. A varredura
  guarda o início de cada achado (8 B cada) para isso. Os quadros do próprio vídeo Motion-JPEG, que a varredura acha
  como JPEG, não contam (como no vídeo MJPEG contíguo, regra P2). Sonda da revisão refeita (vídeo de 4000 amostras
  partido em 2/5, foto de 23,5 KB gravada por cima): foto no segundo pedaço, vídeo `Intact` em 7 de 20 posições antes →
  0 (7 `Partial`, 13 sem pareamento); no primeiro pedaço, 1 de 12 antes (`L1` = 836.096 com o achado cortado em
  591.872) → 0 (nenhum pareado); a foto sai `Intact` em todas. Custo: um vídeo pareado com um arquivo alheio dentro
  dos pedaços (legítimo ou não) sai `Partial`.
- **Q14 (revisão final):** o aborto antecipado da emenda do JPEG virou rejeição certa. Em vez de 3 × a mediana das
  fronteiras acima da emenda + 4 (acima da emenda de uma paisagem fica o céu: base minúscula, 7 de 56 junções reais
  perdidas), compara com um limite superior da base final: a mediana das `n` fronteiras fora da emenda é no máximo a
  `⌊n/2⌋`-ésima menor (a partir de 0) de quaisquer `k > ⌊n/2⌋` delas, e as de cima são algumas delas. Com a emenda na
  metade de cima da imagem não há limite e a tentativa segue até o fim. O "3 × o máximo das de cima + 4" sugerido
  também rejeitaria junções boas (teste sintético céu sobre textura). Auditoria nas 112 divisões reais (56 com a própria
  cauda, 56 com cauda alheia): das 61 tentativas que passaram no teste final, a regra antiga derrubava 7 e a nova 0;
  das 1178 tentativas que chegaram à linha de checagem, a antiga abortava 1093 e a nova 524. Custo: mais tentativas
  completas em entrada hostil, ainda limitadas pelo Q8 (sondas da Tarefa 8 refeitas, abaixo).
- **Quebra logo depois de um `FF` (achado nesta rodada):** o decodificador aponta o erro de um marcador no `FF`, um byte
  antes do byte que não é da foto; quando esse `FF` fecha um setor, a fronteira logo depois dele (onde a foto quebra de
  verdade) nunca era candidata. Corrigido (o limite `x` é o byte depois do erro); recuperou 2 das 56 divisões reais
  (R6 em 7/10 e K-r em 4/10). Custo: nenhum (a janela de 4 fronteiras só anda um setor quando o `FF` fecha um setor).
- **Recalibração da EOS Kiss F (Q14) não feita:** as divisões em 3/10 e 5/10 caem numa faixa de textura forte (fronteiras
  46–83 da prévia, 3 a 9 vezes a mediana da imagem), e o teste final as rejeita. Comparar também com a mediana das 8
  fronteiras logo acima da emenda as juntaria (56/56), mas colou uma cauda alheia de mesmo quadro a mais (4 em vez de 3
  de 168, sondagem abaixo). Fica registrado: essas 2 divisões saem `Partial` contíguo, e o teste `real_samples` as lista
  como exceção conhecida. Custo: uma foto partida numa faixa muito mais áspera que o resto da imagem não é remontada.
- **Q15 (depois da rodada final, substituída pelo Q16):** emenda exatamente num marcador de restart ⇒ no máximo
  `Partial`. Não pegava as 3 colagens `Intact` de mesmo quadro (emenda no meio do intervalo: a cauda alheia decodifica
  válida por 108, 52 e 60 MCUs até o próximo marcador, onde os preditores zeram). Ficou redundante com o Q16 e saiu do
  código.
- **Q16:** toda junção de dois pedaços de um JPEG com marcadores de restart (DRI > 0) sai no máximo `Partial`; sem DRI,
  as regras de antes. Por quê: com restart, outra foto do mesmo quadro continua válida depois do próximo marcador, e só o
  teste de DC as separa; um falso `Intact` (duas fotos coladas) é o que não pode acontecer, e `Partial` ainda entrega o
  arquivo remontado. Resultado: colagens de mesmo quadro 3 de 168 `Intact` → 0 de 168. Custo: 21 das 56 divisões reais
  (todas as da Fujifilm X-S10, OM TG-7 e Pentax K-r, que usam DRI 4) saem `Partial` embora o arquivo venha inteiro;
  33 de 56 seguem `Intact` (as câmeras sem restart), mais as 2 da Kiss F `Partial`.
- **Desvios aceitos da Tarefa 9:** a faixa de pontos de quebra começa no início da amostra `k − 1` (não no fim), e as
  amostras `k − 1` e `k` são conferidas através do `L1`; os candidatos levam o tamanho declarado lido na varredura e o
  orçamento de 1 GiB é cobrado pelas janelas de 1 MiB que um `moov` ocupa; teto extra de 2^24 passos de conferência de
  amostra por varredura. Custo: ~1024 `moov` falsos de 4 KiB congruentes esgotam o orçamento (C8 já aceita perder pares
  numa enxurrada).
- **Busca de `moov` na thread da varredura (Tarefa 9):** custa ~10% da vazão do cache de páginas em `noise` e `video`;
  aceito, porque fica muito acima do que um leitor de cartão entrega. Custo: se um dia a varredura de cache importar,
  mover a busca para as threads de trabalho (mantendo "lido uma vez").
- **`--threads 1` (Tarefa 6):** mede na própria thread da varredura, sem threads de trabalho (o read-ahead segue). Custo:
  nenhum.
- **CI (Tarefa 10):** um runner (`ubuntu-latest`) roda `cargo test --release -p revenant-core`, porque alguns testes só
  existem no release (medida que quebra o contrato de pedaços é ignorada); o limite de
  `cancel_during_the_first_read_ahead_stops_the_scan` subiu de 300 ms para 1 s (sem cancelar levaria ~2 s), para
  runners carregados. Custo: um atraso de cancelamento entre 300 ms e 1 s não é pego por esse teste.
- **`ddrescue` (Tarefa 10):** o README do `revenant-dev` recomenda o GNU ddrescue com arquivo de mapa no lugar de
  `dd bs=4m conv=noerror,sync`. No Mac o comando é `sudo ddrescue /dev/rdiskN card.img card.map`, sem `-d`: `/dev/rdiskN`
  já é o aparelho bruto sem cache, e o `-d` (O_DIRECT) não existe no macOS; no Linux, `ddrescue -d /dev/sdX`. Não foi
  testado aqui (o ddrescue não está instalado e o Homebrew não funciona nesta máquina). Custo: se o ddrescue do Mac
  aceitar `-d`, só sobra uma diferença de texto.

## Medida final (antes × depois)

`revenant-dev bench`, build de release, imagens de 1 GiB lidas do cache de páginas, melhor de 3, numa Intel Core
i9-9980HK (8 núcleos / 16 threads, 32 GB, macOS, Rust 1.99). Antes = Tarefa 1 (`e575cd4`, uma thread, cada detector
lendo por conta); depois = fim da parte (`718f1f1`). MB = 10^6 bytes.

| Imagem | Arquivos | Antes (MB/s) | Depois, 16 threads (MB/s) | Depois, 1 thread (MB/s) |
|---|---:|---:|---:|---:|
| `noise` | 0 | 765,6 | 2381,4 | 2265,4 |
| `dense-jpeg` | 95 | 21,8 | 503,9 | 102,8 |
| `dense-raw` | 37 | 158,1 | 1576,9 | 589,5 |
| `video` | 3 | 797,2 | 2296,6 | 2216,3 |
| `cut-jpeg` | 95 | 26,8 | 419,3 | 85,6 |

Todos os arquivos colocados foram achados. A meta de ≥ 300 MB/s em JPEG e RAW densos foi cumprida com as threads padrão
(503,9 e 1576,9). Com uma thread, não: `dense-jpeg` dá 102,8 MB/s e `cut-jpeg` 85,6 MB/s, abaixo de 300 (conferir JPEG é
trabalho de CPU). Um leitor de cartão SD entrega no máximo ~100 MB/s (UHS-I) a ~300 MB/s (UHS-II). Os JPEGs do bench
são feitos de ruído (~11 MB por foto 3000x2000, mais entropia que uma foto real); as 8 amostras reais de câmera seguem
saindo cada uma como um achado `Intact` (8/8).

Rodada final de correções (Q13, Q14): A/B intercalado com `eda94e9`, 16 threads, 1 GiB, melhor de 3, duas rodadas
(antes → depois, MB/s): `dense-jpeg` 512,7 / 472,2 → 507,5 / 497,4; `cut-jpeg` 414,5 / 403,4 → 429,6 / 418,5. Sem
diferença além da variação entre rodadas. Sondas hostis da Tarefa 8 refeitas (release, melhor de 3, mesmas imagens nos
dois binários, duas rodadas): P1 64–67 ms → 64–67 ms; P2 391–405 ms → 425 ms; 16 cabeças com 33 KiB 214–238 ms →
235–278 ms; 64 cabeças com 2 KiB 51–53 ms → 68–71 ms. Os limites do Q8 seguem valendo. O teste `real_samples` com as
divisões: 54 de 56 juntadas `Intact` (eram 45 de 56 com estas imagens antes da rodada; 47 depois da correção do `FF`),
as 2 da EOS Kiss F `Partial`, e 0 de 56 caudas alheias `Intact`; com o Q16, 33 `Intact` e as 21 das câmeras com
restart juntadas `Partial`.

## Pendências da 1.1 e da 1.2 fechadas nesta parte

- 1.1: setores ruins relidos a cada `measure` → mapa de setores ruins compartilhado no cache (Tarefa 4).
- 1.1: ~62 MB/s e `mdat` lido duas vezes → medido e resolvido (Tarefas 1, 4, 5 e 6; tabela acima).
- 1.1: `dd bs=4m conv=noerror,sync` zerando 4 MiB por setor ruim → README recomenda `ddrescue` com mapa (Tarefa 10); a
  cópia própria fica para a 1.5.
- 1.1 (revisão): JPEG cortado logo depois do cabeçalho saía Parcial sem nenhum pixel → `Damaged` (Tarefa 7).
- 1.2: `moov` carregado várias vezes por medida, `all_formats()` refeito a cada medida, custo do P13 e releituras do
  MJPEG → resolvidos (Tarefa 5).
- 1.2: saúde dependente da posição perto do fim da origem → resolvida no teto e acima dele (Tarefa 2, Q2); abaixo do teto
  o fim da origem ainda conta, e isso está escrito no README.
- 1.2: `confirms` do ISO BMFF aceitava o que `measure` recusa → agora confirma exatamente o que é mensurável (Tarefa 2).
- 1.3 (revisão da Tarefa 9): faltava um teste de inspeção de um vídeo pareado com o `moov` no segundo pedaço → feito na
  rodada final.
- 1.2: comentários de `claim()`/`TiffInfo.data` → dizem que o corte vale para toda faixa (Tarefa 2).

## Pendências herdadas (adiadas por esta parte)

| Pendência | Dono |
|---|---|
| 1.1: `FileSource` com aparelho (tamanho 0, `dev()`, leituras alinhadas) e `volume()` None com aparelho | 1.5 |
| 1.2: JPEGs próprios de um achado Parcial relatados (JpgFromRaw de RW2, P17) | 1.4 (deduplicação) |
| 1.2: COUNT/tamanho corrompido num offset válido é cortado no limite | 1.4 (tamanhos do sistema de arquivos) |
| 1.2: ARW da DSLR-A100, empacotamentos de RW2 não conferidos, MakerNote `OLYMP\0`, HEIC real | sem dono (pedem amostras reais com licença) |

As outras pendências da 1.1 e da 1.2 com dono 1.4, 1.5, Módulo 2 ou sem dono seguem abertas como estão em
`2026-10-19-m1-p1-pendencias.md` e `2026-10-19-m1-p2-pendencias.md`; esta parte não as tocou.

## Pendências novas (dono: parte indicada)

**Parte 1.4 (sistemas de arquivos e deduplicação)**
- Vídeos reais remontados (fatias grandes, ou com som intercalado) saem quase sempre `Partial` (C4): o tamanho de cluster
  deve estreitar o `L1` aos limites de cluster.
- Foto partida em volta de uma foto inteira da mesma câmera fica `Partial` (Q7b), e duas fotos da mesma câmera gravadas
  intercaladas ficam no máximo `Partial` (Q7): a cadeia de clusters do sistema de arquivos resolve.
- Uma foto cortada seguida, sem lacuna, pela cauda de uma foto quase igual do mesmo quadro (mesma câmera e ajustes) cujos
  bytes decodificam exatos até o EOI é um JPEG válido e sai `Intact`: o teste de emenda de DC só roda nas junções que a
  busca faz. Os metadados do sistema de arquivos são o único jeito de ver isso.
- Primeiro pedaço com menos de 16 KiB não é remontado (Q8); segundo pedaço antes do primeiro, ou terminando depois da
  janela de 64 MiB, não é achado. Com o sistema de arquivos, os clusters dizem onde está o resto.
- Quando a cauda da própria foto sumiu (sobrescrita) e a janela tem a cauda órfã de uma foto quase igual do mesmo quadro
  (rajada), essa cauda pode ser a única candidata aprovada e a junção sai `Intact`: o Q1 não enxerga uma cauda que não
  existe. Os clusters do sistema de arquivos resolvem.
- Cauda alheia de mesmo quadro emendada num marcador de restart: sondagem nesta rodada com as 8 prévias reais
  regravadas com restart a cada linha (`jpegtran -restart 1`) e caudas da mesma prévia espelhada ou girada
  (`-flip vertical`, `-flip horizontal`, `-rotate 180`) começando na mesma linha de MCUs da emenda: 3 de 168 colagens
  saem `Intact` (OM TG-7 girada, LX7 espelhada, K-r girada), com o código desta rodada (o teste final de emenda não
  mudou nela). O teste de DC é a única defesa nesse caso (o README já diz que é ele que rejeita a cauda colada num
  restart). Resolvido pelo Q16 (junção de JPEG com restart sai no máximo `Partial`): 0 de 168 `Intact`. Sem restart,
  a rajada acima continua valendo.

**Parte 1.5 (disco de verdade)**
- Setor de varredura maior que 512 bytes: os detectores veem como ilegível o setor inteiro quando o aparelho falhou só
  em parte (só deixa a saúde mais conservadora). Conferir com aparelhos de setor 4Kn.
- Origem que encolhe no meio da busca: a janela curta fica no cache sem cobertura e a busca a relê.
- Cancelamento: uma medida já rodando termina antes de a busca parar (só a busca de lacuna do JPEG desiste na hora).
  Num aparelho lento isso pode atrasar o cancelamento; medir com aparelho de verdade.
- Passado o teto de 1.048.576 trechos ruins, setores ruins novos são relidos a cada recarga de janela (C9): medir num
  cartão que está morrendo.
- As cargas do cache ignoram o cancelamento: uma janela que falha é relida setor a setor (até 2048 leituras) antes de a
  varredura ver o cancelamento. Num aparelho lento isso atrasa o cancelamento; tratar junto com o disco de verdade.

**Parte 1.6 (fragmentação avançada; dono escolhido pelo M12)**
- Arquivos em 3 ou mais pedaços, JPEG e vídeo (a spec §5 pede "2+ pedaços"; esta parte cobre 2). Inclui a peça do meio de
  um vídeo com menos de 1/32 das amostras, que hoje passa despercebida (C7).
- Fragmentação de RAW: sem decodificador de RAW não há como validar a emenda dos dados brutos; um RAW cortado segue
  `Partial`. Reavaliar com o tamanho de cluster da 1.4 e com a prévia JPEG.
- Vídeo com `moov` antes do `mdat`, `mdat` de tamanho 0, MP4 fragmentado (`moof`) e dois vídeos com primeiros pedaços
  idênticos não são pareados.
- A 1.6 vem depois da 1.5 e não segura o marco A; se o Sérgio preferir, pode subir para antes da 1.5.

**Módulo 2 (app)**
- Com `--extract`, um vídeo remontado (`Updated`) é gravado de novo como `_2` ao lado do arquivo `Damaged`; o app deve
  substituir o achado antigo em vez de mostrar os dois.
- Com uma thread, a busca não acompanha um leitor UHS-II em cartão cheio de JPEG (102,8 MB/s): o app deve usar as threads
  padrão.

**Por desenho (Q5), registrado**
- Um setor ruim no segundo pedaço de uma foto impede a junção (os dados contados acabam no primeiro setor ilegível):
  a foto fica `Partial` contígua (meia foto).

**Menores, sem dono**
- Os JPEGs do bench são feitos de ruído (~11 MB cada) e todos os RAWs do bench são little-endian; as afirmações de
  velocidade são conferidas também com as amostras reais.
- RW2 com empacotamento desconhecido: um offset além do limite não marca a flag (diferente do caminho de `claim`); uma
  linha longa no README do core.
- Cabeças num bloco lido adiantado são medidas antes de se saber se um achado `Intact` anterior as cobre; o resultado é
  jogado fora (trabalho especulativo limitado ao read-ahead; o caso comum são as prévias de RAW).
- A busca de `moov` roda na thread da varredura (~10% no cache de páginas); mover para as threads de trabalho se importar.
- `search_cut_short_by_the_cap_is_partial` usa o teto real (teste de integração não vê `cfg(test)`); o teste unitário
  `hitting_the_cap_is_not_unique` cobre o botão.
- O plano (linha ~606) ainda diz que dados de entropia reais nunca enchem 512 bytes com um só valor; o código e o README
  foram suavizados (Q5).
