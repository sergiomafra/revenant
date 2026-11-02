# Parte 1.6 — decisões e pendências (02/11/2026)

Registro da execução do plano `docs/superpowers/plans/2026-10-26-m1-p6-fragmentacao-avancada.md`, na branch
`feat/m1-p6-fragments` (de `main` em `c9e590a`, o merge do PR #6 da 1.5; desenho `65f5d49`, plano `40e28cd`, plano
emendado com o Q1–Q19 em `649f35f`). O desenho é `docs/superpowers/specs/2026-10-26-m1-p6-fragmentacao-avancada-design.md`.
Cada decisão traz o custo se estiver errada. O roteiro para o Sérgio conferir com a A6700 está em
`docs/superpowers/history/2026-11-02-m1-p6-conferencia-manual.md`; os resultados dele vêm para a seção "Conferência
manual", no fim deste arquivo.

O Sérgio pediu para pular as perguntas do desenho e dispensou a revisão do plano; quase tudo abaixo é escolha do Claude
dentro do que o desenho §7 deixa ao construtor.

## Decisões do pre-flight (Claude, pelo Sérgio)

### D1–D22 (desenho do plano)

- **D1:** a remontagem é uma etapa **depois** da busca e do pareamento, antes do ponto `Checkpoint::Paired`, sequencial,
  em ordem de id, sobre candidatos guardados durante a busca; cada achado melhorado sai como `Event::Updated` (mesmo id),
  junto com o pareamento e as pastas antigas. Nada muda durante a busca. Custo: um pedaço de um arquivo que a busca acha
  como arquivo próprio (a prévia de um RAW) continua saindo também à parte (D10).
- **D2:** candidatos: JPEG não `Intact` com primeiro pedaço ≥ 16 KiB, MP4/MOV não `Intact`, RAW TIFF `Partial` e todo
  apagado desses tipos que saiu `Partial`; nunca um vivo; no máximo `MAX_CANDIDATES = 4096`, os primeiros por id; passado
  o teto, nenhuma remontagem sai `Intact`. Custo: num cartão com mais de 4096 arquivos partidos, nenhum sai `Intact`.
- **D3:** espaço na grade do volume quando o achado começa num início de cluster de um volume com cluster ≥ 4096; livre =
  no heap, fora de `Listing.live` e dos pedaços de todo `Intact`; sem grade, setores de 512 B, só para frente, janela de
  64 MiB, até 4 pedaços. Custo: um arquivo que começa fora da grade só é remontado em poucos pedaços.
- **D4:** ordem dos candidatos de cada emenda: "o próximo livre" até 256 MiB depois do pedaço, depois "o primeiro
  buraco" do início do heap; até `SEAM_CANDIDATES = 4096` sobreviventes do pré-filtro por emenda. Custo: uma continuação
  a mais de 256 MiB, ou depois de 4096 candidatos, não é achada (o arquivo sai como antes).
- **D5:** pontos de quebra entre o último ponto conferido e o sinal de quebra, do mais próximo para trás, no máximo
  `BREAK_POINTS = 64`. Custo: vídeo com unidades NAL enormes em cluster pequeno pode passar de 64 e sair `Partial`.
- **D6:** `Intact` exige uma única solução completa (cegas contam, Q2), nada esgotado, listagem não truncada (Q5),
  `Proven`, nenhum setor ilegível, tamanho conhecido batendo, a conferência própria do formato e o Q16; senão `Partial`
  com a primeira solução; `Updated` só quando melhora. Custo: uma remontagem parcial que não passa do achado antigo não
  aparece.
- **D7:** orçamentos por arquivo `FILE_TRIES = 2^16` e `FILE_READ = 512 MiB`, por busca `SCAN_TRIES = 2^22` e
  `SCAN_READ = 16 GiB`; `MAX_PIECES = 16` na grade, 4 fora. Custo: num cartão hostil a remontagem pode levar o tempo de
  reler 16 GiB.
- **D8:** releitura pelo `CachedSource` da busca, contada, nunca de setor ruim conhecido; nenhuma impressão por cluster.
  Custo: releituras dentro do orçamento; nenhuma memória nova durante a busca.
- **D9:** um início de achado dentro de um trecho livre termina o trecho ali, salvo um arquivo próprio do formato (D10).
  Custo: uma foto cujo meio parece um cabeçalho confirmado é cortada ali.
- **D10:** a prévia JPEG de um RAW achada como `Intact` não bloqueia o RAW quando está na posição que a estrutura aponta;
  o achado da prévia continua emitido. Custo: a prévia aparece duas vezes (dono: Módulo 2).
- **D11:** JPEG: o `Cursor` da 1.3, um vetor de DC por busca, prova final `seams_ok` com várias emendas e um pré-filtro de
  marcador no primeiro setor. Custo: uma foto com menos de 4 linhas de blocos nunca é provada.
- **D12:** vídeo em deslocamento do arquivo; `isobmff::check_sample` compartilhada; para `Intact`, toda amostra conferível
  conferida (fecha o C7, também no pareamento de 2 pedaços); `moov` órfão é âncora; `moov` antes, `mdat` 0 e `moof`
  tratados. Custo: um vídeo só com faixas não conferíveis nunca é provado.
- **D13:** RAW: estrutura TIFF sobre a vista, prévia estrita através das emendas, blocos `Compression 7` decodificados até
  o fim com a contagem exata; emenda nos dados do sensor sem compressão ou com perdas é `Unproven`. Custo: um ARW sem
  compressão espalhado nunca é `Intact` sem a cadeia. (Apertado depois pelo Q44/Q45: nem a emenda dentro de bloco sem
  perdas prova.)
- **D14:** exFAT: a cadeia FAT que sobrevive num apagado, com mais de um trecho, vira `Placement::OldChain` quando
  nenhuma outra entrada a reivindica e as conferências de conteúdo passam; senão é a primeira hipótese. Custo: `Placement`
  ganha uma variante (API pública, pré-1.0).
- **D15:** um apagado remontado mantém a origem com `placement: Carved`; a soma dos pedaços tem de ser o tamanho da
  entrada. Custo: nenhum.
- **D16:** o `Commit` ganha `frags`, `frags_capped`, `claimed`, `claimed_capped`, todos no `ScanState`; `Checkpoint::Paired`
  passa a ser "depois do pareamento e da remontagem". Custo: o estado cresce até 4096 achados + 2^18 trechos.
- **D17:** dois vídeos com primeiros pedaços idênticos são tratados como um; o de menor id é remontado. Custo: as cópias
  seguem `Damaged`.
- **D18:** `MAX_CLAIMED_RUNS = 2^18`; passado, nenhuma remontagem sai `Intact`. Custo: num cartão com mais de 262.144
  arquivos inteiros não contíguos entre si, só `Partial`.
- **D19:** testkit: `Alloc::NextFree | Alloc::FirstFit`, `DeleteStyle::ZeroChain`, `jpeg_burst`, `camera_video`,
  `lossless_jpeg` e `arw`. Como o firmware da Sony aloca é suposição. Custo: os cartões sintéticos podem não cobrir o
  padrão real (o roteiro manual confere).
- **D20:** nada novo no caminho por setor da busca, só a contabilidade por achado emitido. Custo: nenhum mensurável (a
  medição da Tarefa 11 confirma: as imagens sem candidato ficam dentro de 5% do `main`).
- **D21:** o DRI das prévias e dos JPEGs da Sony medido com amostras reais; resultado abaixo ("DRI da Sony"). Custo:
  nenhum no código.
- **D22:** os menores herdados da 1.4 entram onde são baratos (Tarefa 12, e o T9 na Tarefa 8). Custo: nenhum (todos
  fechados, tabela abaixo).

### Q1–Q19 (rodada do controlador antes de despachar)

- **Q1:** o pré-filtro roda durante a enumeração; o teto de 4096 conta sobreviventes; suas leituras entram no orçamento
  do arquivo; vídeo e RAW ganham pré-filtro. Custo: nenhum.
- **Q2:** a busca desce também nos candidatos cegos, e suas soluções contam para a unicidade. Custo: mais trabalho de
  busca, limitado pelo orçamento; fecha um caminho de falso `Intact`.
- **Q3:** `search(.., known: Option<u64>)`, preenchido com o tamanho da entrada; tem de bater. Custo: nenhum.
- **Q4 (risco aceito):** a unicidade só vale dentro da janela de 256 MiB (64 MiB sem grade). Custo: **um sósia fora da
  janela pode deixar uma emenda falsamente única**; a prova de conteúdo é a única guarda. Documentado no README do motor.
- **Q5:** `Intact` por remontagem exige a listagem não truncada. Custo: cartões enormes perdem `Intact` nos remontados.
- **Q6:** prova da cadeia antiga: bitmap presente, `valid == size`, "reivindica" definido, nenhum início de outro achado
  dentro da cadeia. Custo: menos `Intact` por `OldChain`.
- **Q7:** o arquivo próprio (prévia do RAW) casa por cobertura, não por igualdade de `Runs`. Custo: nenhum.
- **Q8:** testes com relógio viram determinísticos (contadores, fonte que cancela depois do fim). Custo: nenhum. (Dois
  testes antigos com relógio seguem; ver "Em aberto".)
- **Q9:** `reassembly_budget_measure` é teste de unidade `#[ignore]`; `real_previews_split_in_three` usa um cartão
  `FsBuilder` e o `scan` público. Custo: nenhum.
- **Q10:** `session::store::VERSION` passa a 2. Custo: sessões antigas recusadas (aceitável antes da 1.0).
- **Q11:** o cartão F mostra que remonta antes de o teste de retomada depender dele. Custo: nenhum.
- **Q12:** visibilidade `pub(crate)` ampliada onde preciso. Custo: nenhum.
- **Q13:** o RED da Tarefa 10 aparece num commit anterior ou sob mutação. Custo: nenhum.
- **Q14:** testes de emenda cega usam intervalos tomados entre os trechos. Custo: nenhum.
- **Q15:** bloco sem perdas só passa com o marcador de fim no fim declarado e a contagem exata. Custo: nenhum. (A Sony
  enche o fim de cada bloco com `FF`; ver Q59.)
- **Q16:** um RAW e a sua própria prévia que compartilham bytes são o mesmo arquivo (aninhado), registrado, não dois
  `Intact`; os oráculos isentam só esse caso. Custo: nenhum.
- **Q17:** a Tarefa 10 define a saúde esperada por tipo (refeita pelo Q51 depois do Q37). Custo: nenhum.
- **Q18:** as conferências de conteúdo dentro da busca (Tarefa 9) medidas dentro dos 5%. Custo: nenhum.
- **Q19:** os 11 cosméticos aplicados (C1–C11). Custo: nenhum. Do **C11**: nenhuma tarefa procurou amostras CC0 de
  **XAVC**; o desenho diz "se o construtor achar". Achou-se uma amostra CC0 de ARW da A6700 (Tarefa 11), nenhum vídeo da
  Sony. O vídeo real da câmera fica para o roteiro manual.

## Decisões tomadas durante a execução (Claude, pelo Sérgio)

Rulings Q20 a Q59 do controlador (do registro de execução). Cada uma com o custo se estiver errada.

- **Q20 (Tarefa 1):** o `add_at` do testkit também avança o ponteiro do próximo livre. Custo: nenhum.
- **Q21 (Tarefa 2):** a lista "primeiro buraco" se limita aos primeiros 256 MiB do heap. Custo: uma continuação mais funda
  não é achada (sai `Partial`).
- **Q22:** uma leitura do pré-filtro custa um setor de 512 B; repetida (início, emenda) não custa. Custo: nenhum.
- **Q23:** testes de teto com janelas de 64/1000/40 unidades (os valores do brief se contradiziam). Custo: nenhum.
- **Q24:** uma quebra exatamente na última fronteira conferida conta como ponto de quebra; a unidade logo depois do pedaço
  nunca é candidata. Custo: nenhum.
- **Q25:** a janela de 256 MiB limita a **distância** até um candidato, não o comprimento do pedaço. Custo: caminhadas
  mais longas, dentro do orçamento de leitura.
- **Q26:** depois de uma caminhada completa, pontos de quebra anteriores do último pedaço também são tentados; outra
  solução ⇒ não única. Custo: mais busca, limitada.
- **Q27:** pontos além dos 64 não tiram o "esgotado"; o formato dá uma ordem de suspeita. Custo: uma cauda quase igual
  cuja troca fica fora das 64 tentadas pode passar (risco ao lado do Q4).
- **Q28:** a reconferência de unicidade vira fase própria, em todo pedaço da solução, com orçamento próprio (2^14), nas
  quebras que o formato marca como suspeitas. Custo: um sósia encadeado fora das quebras tentadas ainda pode passar.
- **Q29:** a reconferência só conta alternativas com evidência de conteúdo depois da nova emenda (não desce em cegas).
  Custo: um sósia cego logo depois do ponto de troca escapa (parte do risco documentado).
- **Q30 (Tarefa 3):** `Seams::walk` recebe as emendas da vista; a reconferência corta na fronteira de unidade antes de
  cada posição sugerida. Custo: nenhum.
- **Q31:** a caminhada completa de um JPEG passa o teste de DC com várias emendas; a poda antecipada usa linhas abaixo das
  emendas. Custo: o risco da 1.3 (emenda verdadeira numa faixa áspera + sósia que passa), documentado.
- **Q32:** fora da grade, um JPEG em 3+ pedaços nunca é esgotado (no máximo `Partial`). Custo: cartões só esculpidos não
  dão `Intact` a JPEGs em 3+ pedaços.
- **Q33:** a reconferência nunca considera unidades da própria solução; com dica, os candidatos da quebra mais suspeita
  são andados até o fim, senão não esgotado. Custo: mais `Partial` quando o orçamento morde.
- **Q34:** a exclusão do Q33 estreitada à parte do pedaço depois do corte; um corte que exigiria um pedaço além do teto
  conta como feito. Custo: fotos certas ao lado de uma vizinha inteira de rajada ficam `Partial`.
- **Q35:** bater no teto de pedaços na quebra mais suspeita não conta como corte curto. Custo: nenhum.
- **Q36:** "irmã de rajada" (mesmo quadro e tabelas, DC da primeira linha quase igual) limita a `Partial`; exclusão de
  volta à da rodada 1. Custo: numa rajada apagada, JPEGs só por conteúdo ficam `Partial`. **Substituída pelo Q37.**
- **Q37 (a principal da parte):** o antiobjetivo "nunca colar duas fotos e chamar de `Intact`" vence o G1: um JPEG
  remontado pelo motor da 1.6 em **2 ou mais pedaços só pelo conteúdo** sai no máximo `Partial`, sempre; `Intact` só
  quando o sistema de arquivos prova o lugar (cadeia viva, `OldChain`). A junção de 2 pedaços da 1.3 fica como estava.
  Custo: **os JPEGs fragmentados da câmera do Sérgio voltam montados, mas `Partial`, mesmo perfeitos.**
- **Q38 (Tarefa 5):** `video_split_in_three_is_not_paired` pode mudar: o que ele dizia (3 pedaços não voltam) é o que a
  Tarefa 5 acrescenta. Custo: nenhum (a recusa do próprio pareamento segue coberta).
- **Q39:** a guarda de amostras de tamanho igual fica local (nenhuma emenda provada quando as amostras de cada lado são
  todas do mesmo tamanho). Custo: alguns vídeos a mais `Partial` com quadros uniformes numa emenda.
- **Q40 (Tarefa 6):** um doador com todos os bytes estruturais iguais aos de A (tamanhos e cabeçalhos NAL) é
  indistinguível por construção; risco aceito, com reprodução `#[ignore]`. Custo: um clipe com o mesmo arranjo NAL do
  pedaço perdido poderia ser colado (na prática, só uma cópia sem recodificar, cujos bytes então batem).
- **Q41:** vídeos com o índice antes passam a ter toda amostra conferida na busca (correção de falso `Intact` com dados
  alheios). Custo: uma leitura a mais desses vídeos (medida: imperceptível).
- **Q42:** sequência, tempo e posição não separam dois clipes da mesma câmera: uma emenda de MP4 fragmentado só é provada
  com os dois lados dentro de **um** fragmento; emenda na fronteira entre fragmentos nunca prova; `moof` sem a faixa de
  vídeo é aceito. Custo: MP4 fragmentado só por conteúdo cortado nas fronteiras fica `Partial`.
- **Q43:** a regra `replaces` só vale quando os pedaços do achado antigo são refutados pelo conteúdo, e nunca contra uma
  colocação do sistema de arquivos não refutada. Custo: alguns vídeos `Partial` mantêm uma colocação antiga pior.
- **Q44 (Tarefa 7):** o `Eoi` de um bloco sem perdas é evidência, não prova (os códigos de diferença se ressincronizam:
  um pedaço alheio do mesmo quadro passa 1–4% das vezes); emenda dentro de bloco sem perdas nunca é provada; só na
  fronteira exata de bloco com o próximo bloco inteiro. Custo: RAW só por conteúdo com emenda dentro de bloco fica
  `Partial`.
- **Q45 (Tarefa 8):** `RawSeams::prove` = `Proven` só quando toda emenda cai exatamente entre dois blocos conferidos com
  âncora de posição; nunca dentro de bloco, prévia, sensor ou dado sem lista. Custo: **quase todo RAW remontado só por
  conteúdo sai `Partial`** (bytes no lugar).
- **Q46:** um bloco com enchimento antes do `EOI` não ancora a emenda (de nenhum lado). Custo se errado: RAWs com blocos
  enchidos saem `Partial`.
- **Q47:** corrigir I-2, I-3, I-4, M-1, M-2, M-3, M-5 da revisão da Tarefa 8; M-4 para a Tarefa 11; a "preocupação 3"
  para as Tarefas 9/10. Custo: nenhum.
- **Q48 (Tarefa 9):** um JPEG apagado colocado só pelo conteúdo (cadeia zerada) sai `Partial` com os pedaços exatos; o
  tamanho da entrada é comprimento, não prova de lugar. Custo: nenhum além do Q37.
- **Q49:** as reivindicações vêm de **toda** entrada apagada de qualquer extensão, e qualquer achado (qualquer saúde) que
  cruze os pedaços de A quebra a prova `OldChain`. Custo se errado: mais `OldChain` caem para o conteúdo (`Partial` em
  fotos).
- **Q50:** N-2 aceito como resíduo do D14; N-1 é o limite da 1.4 (palpite de uma corrida), só o texto corrigido; M-7
  vira teste. Custo: uma rodada curta.
- **Q51 (Tarefa 10):** o oráculo `no_intact_finding_is_glued` fica estrito e nunca relaxado (só o aninhado do Q16);
  `recovered` exige bytes exatos e a saúde por tipo e prova. Custo: possível rodada extra.
- **Q52:** carimbo de tempo igual (apagado antigo vence o empate) é bug do motor: empate ⇒ nenhum dos nomes; o `moov`
  próprio de um vídeo apagado passa a ser buscado; testkit ganha reuso de entradas. Custo: num empate verdadeiro os dois
  nomes se perdem (a foto volta sem nome). As partes (3) a (5): (3) a volta do NextFree ao início do heap já funcionava
  (a ordem dos candidatos dá a volta: depois da janela, o "primeiro buraco" desde o início do heap); o clipe que dava a
  volta só falhava pelos inícios de entradas velhas (corrigido na mesma rodada) e sai `Intact` nos cartões exFAT
  NextFree, guardado pelo teste `deleted_video_wrapping_past_the_heap_end_is_intact`; sem resíduo; (4) a junção de dois pedaços da 1.3
  com cauda quase igual segue `Intact` pelo Q37 (as regras da 1.3 não mudam; o oráculo só aceita `Intact` com bytes
  exatos), limite documentado; (5) o testkit ganha `reuse_slots` (a entrada nova toma a primeira vaga livre ou apagada,
  como as câmeras) e um cartão de apagar na câmera por tamanho de cluster o usa, para exercitar o `OldChain` e o N-1.
- **Q53:** para uma entrada apagada, estrutura que termina antes do tamanho com só enchimento depois não prova
  identidade: o achado é a escultura, sem o nome. Custo: apagados com enchimento perdem o nome (voltam pelo conteúdo).
- **Q54:** (b) o enchimento do próprio formato (RAW TIFF arredondado a 4 KiB ou 64 KiB) não dispara o Q53; (a) substituído
  pelo Q55. Custo: um ARW B sobre um ARW A cujo tamanho coincide com o arredondamento mantém o nome de A (raro).
- **Q55:** `OldChain` só prova quando o conteúdo termina exatamente no tamanho; um achado sem nome (Q53) fica `Partial`
  até o fim da busca. Custo: nenhum além do Q53.
- **Q56:** o "fim do conteúdo" é a medida inteira do formato (JPEG com imagens anexadas, `mdat` 0 = fim da última
  amostra, RAW pela regra de grão). M-7 aceito (ver resíduos). Custo se errado: M-7 erra o nome, nunca mistura bytes.
- **Q57:** o fim do Q56 só vale para apagados (vivos mantêm a medida da 1.4); resultado de outro comprimento que a entrada
  não leva o nome (sem nome, no máximo `Partial`); I-5 vai para a revisão final; w1 e M-11 aceitos. Custo: uma rodada
  focada.
- **Q58 (Tarefa 11):** `real_previews_split_in_three` exige bytes exatos e **nunca** `Intact`; poda só com teste de que não
  muda resultado. Custo: tarefa de medição longa.
- **Q59:** (1) o ARW sem perdas inteiro da A6700 voltar `Partial` é regressão a corrigir já (causa real: a Sony enche cada
  bloco com menos de 16 `FF` depois do `EOI`, dentro do `TileByteCounts`; aceito, e esse bloco não é âncora); (2) DRI 0 em
  3+ pedaços que não volta é limite conhecido (o "nunca a corrida da vizinha" vale para `Intact`); (3) o clipe de 761 MiB
  esbarra na janela de 256 MiB; (4) `cut-jpeg` fora do portão de 5% (fragmentado de propósito); (5) o pré-filtro do RAW
  deixa de depender da caminhada anterior. Custo: uma rodada antes da revisão.
- **Tarefa 12 (sem ruling novo):** `Limits::high_word_steps = 2^25` (metade do orçamento de palpites) e um corte marca a
  listagem truncada, o que tira `Intact` e `OldChain` do cartão inteiro (a mesma troca do teto de passos que já havia);
  `Folders::get` memoriza os nós intermediários pelo corte de profundidade da primeira consulta (determinístico, fechado
  como não defeito, m4). Custo: num FAT32 hostil, a listagem truncada.
- **Q60 (Tarefa 13):** os 7 menores de documentação da revisão da Tarefa 13 vão para o único despacho de correção final
  (economiza uma rodada; só texto). Custo: nenhum.
- **Revisão final (`c9e590a..8f46be1`):** "com correções"; nenhum crítico. Importantes: I-1 (= I-5 do Q57, com a correção
  guardada já verificada por sonda) e I-2 (os dois testes de relógio). Menores: M-1 (outros limites de relógio de uma
  rodada só), M-2 (nenhuma checagem de MSRV na CI), M-3 (os menores do Q60), M-4 (a linha "cedo, depois outros bytes" da
  tabela mantinha o nome de A sobre B + sobra de A, `Partial`), M-5 (o histórico sem o desfecho do I-1). Todo menor
  adiado das tarefas foi julgado numa tabela: o guarda `uniform` da Tarefa 5 já é local desde a Tarefa 6 (Q39, resíduo Q40
  com reprodução `#[ignore]`); o menor 4 da Tarefa 1 fecha (nunca descrito, nada visível no código); os da Tarefa 3, 4
  (CHANGELOG, README), 5 (README), 6 (teste por trilha) e 11 já estavam corrigidos; os demais (Tarefas 4, 5, 6, 7, 8, 9,
  10) aceitos com motivo (afirmação redundante, piso de retomada igual ao D24, Q38, amostras dentro do trecho medido,
  leituras limitadas por passos e `MAX_BOXES`, texto cosmético, não é caminho de prova, documentação do testkit, resíduos
  da classe D14, conservador, defesa em profundidade); o comentário da Tarefa 4 (~1906) era opcional.
- **Q61 (revisão final):** um só despacho de correção (opus novo). I-1 com os **quatro** guardas (nenhum RAW TIFF cego
  depois do primeiro pedaço; manter `!view.padded`; excluir a escultura de `mdat` de tamanho 0; um A verdadeiro cuja
  vista é `Intact` no tamanho fica com os seus pedaços) e testes vistos falhando; o ARW B sem compressão ou com perdas
  segue perdido, resíduo documentado. I-2 pelos contadores de leitura (fica uma trava de relógio folgada). M-1 não
  falhou ainda: vai para "Depois", sem converter agora. M-2: `-W clippy::incompatible_msrv` no passo de clippy da CI.
  M-3 aplicado. M-4: a linha fica sem nome (princípio do Q57: nenhum nome que o tamanho ou a estrutura desmentem),
  linha de unidade + teste. M-5: este arquivo. Custo: uma mudança tardia na tabela de decisão; mitigado pela revisão
  restrita em opus e pelas suítes inteiras.
- **Desfecho do I-1 e do M-4 (correção final):** em `scan::fs::decide`, quando a escultura no início de uma entrada
  apagada é um arquivo inteiro numa corrida, `Intact`, mais longo que o primeiro pedaço da entrada e de outro tamanho, ela
  sai sozinha, sem nome (`Padded::Carving`), salvo se um dos quatro guardas vale: a escultura é RAW TIFF (**todos** os RAW
  TIFF, também o sem perdas: o refinamento por blocos ficou de fora); a vista tem enchimento (decide a regra do Q53); a
  escultura é um `mdat` de tamanho 0 (`ViewMeasure::open_carving`, calculado no `fs_job` por `size_zero_mdat_end`); ou a
  vista é `Intact` no tamanho. Cada guarda tem a sua célula vista falhando sem ele: sem o 1, `f_arw_unc_three_same_oldslack`
  sai `Intact` falso; sem o 3, um clipe de `mdat` 0 contíguo sobre a primeira corrida sai `Intact` com megabytes depois do
  fim; o 2 é a segunda barreira das células com enchimento (ARW com enchimento de 512 B, ARW sem compressão sobre a cadeia
  de A, clipe de `mdat` 0 mais curto): elas falham só com o 2 tirado junto com o 1 ou o 3; o 4 só se vê na tabela de
  unidade (nenhuma célula de integração o alcança). A JPEG e o MP4 contíguos (`q2_jpeg_three_runs_b_contig`, `r5_m8c`)
  saem `Intact`, exatos, sem nome. M-4: só nos pedaços do próprio registro da entrada (corrida gravada ou cadeia antiga),
  uma estrutura `Intact` numa corrida que termina antes do tamanho com outros bytes depois perde o nome como no
  enchimento; num palpite, o fim cedo desmente o palpite, não o nome, e o nome fica (escolha do construtor, para não tirar
  o nome de um JPEG apagado em pedaços cujo palpite de uma corrida mede cedo). `size_mismatch_is_never_intact` passou a
  afirmar a foto sem nome. Custo: um JPEG apagado com dados próprios não reconhecidos depois do `EOI` (um trailer de
  fabricante) perde o nome quando colocado pela cadeia ou corrida gravada; o arquivo continua recuperado.

### Notas carregadas para este arquivo

- **Desvios aceitos das tarefas:** cartão `scattered-card` com apagado final no computador (sem ele, 0 candidatos); fotos
  do cartão F em 320×240 (custo do debug); FAT32 de 260 MiB e 4 KiB (o menor FAT32 numa grade); carimbos de tempo por
  arquivo nos cartões; motor da Tarefa 8 além do brief (candidato cego no fim, unidade do próprio arquivo, parada antecipada
  do RAW, Q43 estendido ao RAW); três regras extras da Tarefa 9 (escultura de uma corrida nunca substitui colocação
  multicorrida do sistema de arquivos; os pedaços do sistema de arquivos competem como solução salvo refutados;
  `old_chain` limitado a `Partial` quando divide bytes com outro `Intact`).
- **Poda da Tarefa 11 (sem mudar resultado):** candidatos andados um a um; caminhada classificatória reaproveitada
  (cobrada ao menos como andar de novo); JPEG em pedaços para na primeira solução (é `Partial` de qualquer jeito, Q37).
  Sem orçamento estourado, o resultado é idêntico; com orçamento estourado pode ir mais longe, nunca a um `Intact` não
  ganho. `Limits` sem mudança.

## Resíduos aceitos (o que ainda pode sair errado, documentado)

- **Bloco alheio sem enchimento do mesmo tamanho (Q44/Q45, Tarefa 8):** um bloco sem perdas de outro arquivo com
  exatamente o tamanho comprimido do de A, numa emenda de fronteira de bloco, dá `Intact` com bytes alheios (~1e-3 por par
  em blocos reais). Custo: raro; nenhum teste recusa um bloco alheio posto **antes** de uma emenda.
- **D14, ruído não registrado (Q49):** ruído escrito sobre dados do sensor sem compressão ou com perdas, sem reivindicação,
  sem início de arquivo e sem achado que o cruze, segue `Intact` por `OldChain`. Custo: um RAW apagado com dados do sensor
  sobrescritos por algo que não deixou rastro.
- **N-1 (Q50):** quando outro arquivo reescreve parte da cadeia FAT de A, a cadeia não lê inteira e A cai no palpite de
  uma corrida (limite da 1.4), que num RAW sem compressão pode sair `Intact` com dados errados. Custo: o limite conhecido
  dos RAWs apagados; os cartões da Tarefa 10 exercitam o caminho com foto (refutada pela decodificação estrita).
- **N-2 (Q50):** um clipe com o tamanho do `mdat` não gravado mede 28–36 B e não chega à corrida de A, então A segue
  `Intact` por `OldChain` (classe do D14). Custo: igual ao do D14.
- **M-7 (Q56):** um arquivo sobrescrito por outro de **exatamente o mesmo tamanho** (ARWs sem compressão da mesma câmera,
  ou ARWs da Sony enchidos ao mesmo bloco) não se distingue pelo tamanho: os bytes são de um original só, exatos, mas o
  nome e as datas são os da entrada mais antiga. Custo: nome errado, nunca bytes misturados. Para o roteiro: conferir a
  identidade pelo EXIF (`DateTimeOriginal` contra as datas da entrada), item no `roadmap.md`.
- **w1 (Q57):** um achado sem nome `Intact` igual à imagem principal inteira de B, sem a prévia anexada desalinhada, é o
  comportamento antigo da escultura (sem bytes alheios, sem nome). Custo: a imagem anexada não volta junto.
- **M-11 (Q57):** um clipe apagado enchido com `mdat` de tamanho 0 sai sem nome e `Partial` (custo do Q53).
- **DRI 0 em 3+ pedaços (Q59):** um JPEG sem marcadores de reinício em 3 ou mais pedaços, colocado só pelo conteúdo,
  muitas vezes não volta: fica o palpite de uma corrida, `Partial`, com os bytes do que vem depois do primeiro pedaço (6 das
  9 prévias reais, a da A6700 entre elas). Custo: é a principal lacuna real da remontagem de JPEG; roadmap.
- **Janela de 256 MiB (Q59):** um clipe de 761 MiB em 3 pedaços fora de ordem não volta porque o pedaço 2 começa 270 MiB
  depois do fim do pedaço 1 (com 697 MiB, 248 MiB: volta `Intact`). Custo: clipes longos muito espalhados ficam como
  antes.
- **`cut-jpeg` fora do portão de 5% (Q59):** 25 s contra 2,4 s no `main`; cada foto cortada gasta seus 512 MiB e o orçamento
  de 16 GiB da busca acaba depois de ~32 dos 95 candidatos. Custo: num cartão cheio de fotos cortadas sem o resto, a
  remontagem custa até o tempo de reler 16 GiB e os candidatos do fim não são buscados.
- **RAW sobre o primeiro pedaço de um apagado (Q61):** um RAW B (ARW, CR2, NEF, DNG, ORF, RW2) que a câmera grava
  contíguo sobre a primeira corrida de uma entrada apagada A e passa dela não volta inteiro: os dados do sensor depois do
  primeiro pedaço não são conferidos por nada (sem compressão ou com perdas, cegos; o sem perdas poderia ser conferido
  bloco a bloco, mas o guarda exclui todo RAW TIFF). O início fica com os pedaços de A, `Partial` (sem nome quando a
  estrutura de B termina antes do tamanho de A com outros bytes depois, M-4). Custo: um RAW inteiro não oferecido nesse
  arranjo; nunca um falso `Intact`.
- **Herdado da 1.4 ("preocupação 3"):** um RAW sem compressão ou com perdas cujo primeiro pedaço é seguido por outros
  dados sem início de arquivo mede `Intact` pelos bytes contíguos e nunca vira candidato; a 1.6 estreitou isso para o
  apagado exFAT com cadeia inteira. Custo: segue para RAW esculpido sem sistema de arquivos e para o palpite de uma corrida.

## DRI da Sony (D21)

Medido pelo `sony_restart_intervals` (Tarefa 11) nas amostras CC0: ILCE-7S, prévia 1616×1080 e miniatura 160×120 com
**DRI 0**; ILCE-6700 sem perdas (raw.pixls.us id 6736, CC0, SHA-256 `dc3e2ed2…dbca34200a`, agora no `samples.toml`): JPEGs
embutidos 6192×4128, 1616×1080 e 160×120, **todos DRI 0**; 117 blocos de 512×512, `TileByteCounts` de 199.024 a 402.736 (116
distintos: o receio do Q15 não aparece). Não há amostra CC0 de JPEG avulso da A6700; o roteiro manual mede os do Sérgio.

O que isso quer dizer, sem rodeio: o teto do Q16 (JPEG com reinício em 2+ pedaços ⇒ `Partial`) **não** é o que pesa na
A6700. Pelo D21, os JPEGs fragmentados da câmera poderiam ser postos de volta pelo conteúdo, e são: mas **todo JPEG só por
conteúdo em 2 ou mais pedaços sai sempre `Partial` (Q37)**, e o **JPEG DRI 0 em 3 ou mais pedaços muitas vezes nem volta**
(fica o palpite de uma corrida, Q59). Esse é o principal custo da 1.6 para a câmera do Sérgio. Uma foto apagada na câmera
num cartão exFAT, cuja cadeia sobreviveu e prova o lugar (`OldChain`), continua podendo sair `Intact`. Os ARW sem perdas
saem `Intact` só com emendas exatamente entre blocos que terminam onde a estrutura diz (e a Sony enche os blocos: um bloco
enchido não é âncora, Q59), ou pela cadeia.

## Orçamentos e números

`reassemble::Limits::DEFAULT`, sem mudança depois da medição: `pieces` 16 (4 sem grade), `seam_candidates` 4096,
`seam_window` 256 MiB (64 MiB sem grade), `break_points` 64, `file_tries` 2^16, `scan_tries` 2^22, `recheck_tries` 2^14,
`file_read` 512 MiB, `scan_read` 16 GiB, `candidates` 4096, `claimed_runs` 2^18, `own_files` 64. Da Tarefa 12, no FAT:
`high_word_steps` 2^25.

### Tarefa 10: cartões realistas

25 cartões (exFAT 128 MiB com cluster de 32 e 128 KiB, `FirstFit` e `NextFree`; FAT32 260 MiB com 4 KiB): apagar na câmera
(sem perdas e sem compressão), com reuso de entradas e relógio não acertado, formatar na câmera, apagar no computador
(exFAT `ZeroChain` e FAT32 do Windows) e rajada de parecidos. Os dois oráculos (`no_intact_finding_is_glued`,
`recovered`) verdes em todos: **nenhum falso `Intact`**. Cada cartão tem ao menos 3 arquivos inteiros em pedaços que o
sistema de arquivos não coloca; com a remontagem desligada, todo teste de cartão falha menos o oráculo de colagem.

Contagem final (release; arquivos inteiros no cartão `Intact` / `Partial` / não recuperados), conferida de novo na
Tarefa 13 (`9fbd048`):

| Cartão | 32 KiB FirstFit | 32 KiB NextFree | 128 KiB FirstFit | 128 KiB NextFree |
|---|---|---|---|---|
| apagar na câmera, sem perdas | 34/3/0 | 36/1/0 | 35/2/0 | 37/1/0 |
| apagar na câmera, sem compressão | 35/2/0 | 33/4/0 | 35/2/0 | 35/3/0 |
| formatar na câmera | 36/2/0 | 36/3/0 | 34/4/0 | 38/2/0 |
| apagar no computador, exFAT `ZeroChain` | 33/4/0 | 34/3/0 | 33/4/0 | 35/3/0 |

Os outros: reuso de entradas 32 KiB FirstFit 37/2/0, 128 KiB NextFree 37/3/0; relógio não acertado 34/3/0; FAT32 4 KiB
FirstFit 31/6/0, NextFree 32/5/0; rajada 32 KiB 82/6/0, 128 KiB 39/4/0 (×3). Os `Partial` são, quase todos, JPEGs só por
conteúdo em pedaços (Q37), ARWs com emenda dentro de bloco ou do sensor (Q45) e vídeos com NAL grandes em 4 KiB.
Tempo: o arquivo de cartões leva ~4 min em debug e 16 a 27 s em release (84 s antes da poda da Tarefa 11).

### Tarefa 11: medição

- **Hostil** (imagem da Tarefa 4 a 1 GiB): 1 candidato, 12.287 tentativas, 144 MiB, esgotado, sem orçamento estourado,
  0,63 s (limite afirmado: 10 s por GiB).
- **`scattered-card` 1 GiB:** 435 candidatos, 313 buscas, 26,1 s (22,1 s depois da rodada 1), 9.412 tentativas, 769 MiB
  lidos, nenhum orçamento de arquivo estourado; 161 ARW 22,8 s (159 melhorados, todos `Partial`, Q45), 240 JPEG 3,3 s (37
  melhorados `Partial`), 34 MP4 0,02 s (1 `Intact`). Sem a poda: 143 s, os mesmos eventos.
- **`cut-jpeg` 1 GiB:** os 32 candidatos buscados estouram os 512 MiB cada; o orçamento de 16 GiB da busca acaba depois de 32
  dos 95. 25 s (81 s antes da poda; 2,4 s no `main`).
- **Clipes grandes** (exFAT 128 KiB, 3 pedaços fora de ordem, apagados no computador): 253, 381, 508, 634 e 697 MiB
  `Intact` em 2,6–3,1 s lendo 230–469 MiB; 761 MiB não volta (janela de 256 MiB). A caminhada do vídeo não lê todo quadro,
  então os 512 MiB por arquivo não são o limite.
- **Q41** (vídeo com índice antes, conferido na busca): sem custo mensurável (0,35 s numa imagem de 0,56 GiB).
- **Clusters pequenos:** 4 KiB e 8 KiB seguem presos ao orçamento de tentativas (195 mil tentativas num cartão de 12 MiB
  com 8 KiB): o custo dos cartões FAT32 de 4 KiB continua.
- **Amostras reais:** `real_samples` 168 → 99 s em release com a poda; nenhum candidato de remontagem nas amostras sozinhas
  depois da correção da A6700.
- **Portão de velocidade** (contra o `main` `c9e590a`, compilado à parte, intercalado, melhor rodada de 3): `noise` 0,42 →
  0,43 s, `dense-jpeg` 2,02 → 1,92, `dense-raw` 0,66 → 0,63, `video` 0,42 → 0,44 (0,02 s, dentro da variação do próprio
  `main`), `exfat-card` 1,98 → 1,88: as cinco imagens sem fragmentação dentro de 5%. Remedido depois das rodadas de
  correção: `noise` 0,45/0,46, `dense-jpeg` 2,01/1,98, `video` 0,45/0,45, `exfat-card` 2,31/2,34, `dense-raw` 0,65/0,65.

## Pendências herdadas com dono 1.6: resultado

| Pendência | Resultado |
|---|---|
| 1.3: JPEG em 3+ pedaços | Fechada: remontado (Tarefas 2–4), mas só por conteúdo sai `Partial` (Q37); `Intact` com cadeia viva ou `OldChain`. DRI 0 em 3+ pedaços muitas vezes não volta (Q59) |
| 1.3: vídeo em 3+ pedaços, peça do meio < 1/32 (C7) | Fechada: `tiny_middle_piece_is_found`, `pairing_checks_every_sample` (Tarefa 5) |
| 1.3: fragmentação de RAW | Fechada com limite: remontado com a estrutura, a prévia e os blocos sem perdas conferidos; `Intact` só com emendas em fronteira de bloco não enchido ou pela cadeia (Q44–Q46) |
| 1.3: `moov` antes, `mdat` 0, `moof`, primeiros pedaços idênticos | Fechada (Tarefa 6; Q41–Q43; D17) |
| 1.3 Q16: JPEG com reinício no máximo `Partial`; medir a Sony | Fechada: régua mantida; a Sony mede DRI 0 (ver "DRI da Sony") |
| 1.4 D3: apagado e fragmentado sai `Partial` mesmo inteiro | Fechada: vídeo `Intact` quando as amostras provam as emendas; `OldChain` no exFAT (Tarefa 9, Q48–Q50); foto só por conteúdo segue `Partial` (Q37) |
| 1.4: foto cortada seguida da cauda de uma quase igual | Fechada: `same_frame_neighbour_tail_is_not_intact`, `no_intact_finding_is_glued` |
| 1.4 T5: teto da fase de palavra alta; teste do corte da fila | Fechada: `high_word_steps` 2^25, `high_word_phase_stops_at_its_share_of_the_budget`, `waiting_queue_cut_marks_the_listing_truncated` (Tarefa 12) |
| 1.4 T6: `NoFatChain`; chunk ilegível sem custo; não-fotos antes do bitmap | Fechada: guardas `live_nofatchain_directory_is_trusted`, `unreadable_chunk_costs_no_entry_budget`, `non_photo_files_before_the_bitmap_claim_after_it`, vistas falhando por mutação (Tarefa 12) |
| 1.4 T9: prévia de RAW que cruza um pedaço | Fechada: `raw_preview_crossing_a_piece_is_not_reported_twice` (Tarefa 8) |
| 1.4 T10: continuação de pasta antiga exFAT; nó cortado por profundidade | Fechada: `multi_cluster_old_folder_continuation_is_one_folder`, `depth_cut_folder_is_memoized` (Tarefa 12) |
| 1.5 C13: leitor de plist duplicado | Decidida: a 1.6 não mexe em plists; fica com o **Módulo 2** |

## Verificação final (02/11/2026)

Mac Intel (Core i9-9980HK, Rust 1.99), na cabeça `9fbd048`, máquina carregada por outros projetos durante parte da
rodada:

- `cargo test --workspace`: 936 passaram, 0 falharam, 23 ignorados (os dois testes de relógio passaram nesta rodada).
- `cargo test --release -p revenant-core`: 847 passaram, 0 falharam, 14 ignorados.
- `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check`: limpos.
- `grep -rn "os::unix" crates`: todos os usos sob `#[cfg(unix)]`, `#[cfg(target_os = "macos")]` ou em arquivo só macOS
  (`os_builders.rs`); `source/unix.rs` entra por `#[cfg(unix)] mod unix`.
- `cargo deny check licenses`: **não rodado**, `cargo-deny` não está instalado neste Mac e o Homebrew não funciona; a CI
  roda o job `deny`. Esta parte não acrescentou nenhuma dependência.
- `real_samples` (release, `REVENANT_SAMPLES=samples/cache`, `--ignored`): 4 passaram (`real_camera_samples`,
  `real_previews_split_in_two`, `real_previews_split_in_three`, `sony_restart_intervals`), 119 s.
- Conjuntos opcionais, só em imagens que os próprios testes criam (nenhum `sudo`, `hdiutil info` vazio antes e depois):
  `os_filesystems` (`REVENANT_OS_FS=1`, `--test-threads=1`) 5 passaram; `os_builders` 3 passaram; `os_devices`
  (`REVENANT_OS_DEV=1`) 1 passou, aparelho 135/123 MB/s contra 165/160 MB/s no arquivo. O do Windows só roda na CI.
- `reassembly_cards` (release, `--nocapture`): os 25 cartões com as mesmas contagens da tabela da Tarefa 10 acima, 16 s.
- `revenant-dev bench` (todas as imagens, uma rodada, melhor de 3): `noise` 0,76 s (1419,1 MB/s), `dense-jpeg` 4,55 s
  (236,0), `dense-raw` 1,23 s (870,6), `video` 0,65 s (1645,7), `cut-jpeg` 43,71 s (24,6), `exfat-card` 3,55 s (302,8),
  `scattered-card` 35,37 s (30,4); todos os arquivos achados (`found` = `placed`; no `scattered-card` 2277, como na Tarefa 11). Tudo
  1,3 a 2,4 vezes mais lento que a tabela da Tarefa 11, **inclusive o `noise`**, que a 1.6 não toca: a carga subiu de 11
  para 48 durante a rodada (outro processo a 100% de CPU e o Spotlight). Não confere com a tabela por causa da máquina,
  não do código (desde a Tarefa 11 só a Tarefa 12 mexeu no motor, na listagem FAT); a tabela do README não foi
  reescrita.

Depois da correção final (Q61, cabeça `91df1dc` mais este texto), carga 6 a 58 durante a rodada: `cargo test --workspace`
943 passaram, 0 falharam, 23 ignorados (52 min); `cargo test --release -p revenant-core` 854 passaram, 14 ignorados;
`real_samples` 4 passaram (125 s); clippy com `-W clippy::incompatible_msrv` e fmt limpos; nenhum `os::unix` novo;
`reassembly_cards` (release) com as 25 contagens idênticas à tabela acima; a matriz de sondas da revisão final igual (69
passam, os mesmos 4 W1/W2 aceitos). `bench` intercalado contra `8f46be1` (melhor de 3 rodadas, carga 7 a 13):
`exfat-card` 2,17 s contra 2,57 s, `noise` 0,46 contra 0,50, `dense-jpeg` 2,41 contra 2,60; nenhuma perda (a vantagem é
ruído da carga).

## Em aberto, com dono

**Resolvido na correção final (Q61):** o I-5 (JPEG e MP4 voltam; RAW é resíduo, acima); os dois testes de relógio
(`fs_fat::hostile_high_word_entries_are_bounded` conta as leituras: nenhuma em cluster ≥ 0x10000, onde mora todo
candidato de palavra alta, e no máximo região de boot + FATs + pastas + 64 KiB; `vanish::cancel_is_prompt_during_a_long_measurement`
cancela na 24ª leitura dentro do `mdat`, por `SimSource::cancel_after_reads_in`, e limita as leituras dele depois do
cancelamento ao número de threads; cada um com uma trava de relógio de 30 s só contra travamento); o menor 4 da Tarefa 1
(fechado pela revisão final).

- **Menores adiados** (todos julgados na tabela da revisão final: aceitos com motivo ou já corrigidos):
  - Tarefa 3: a documentação do ajudante `intact()` sugere que o teto protege corridas únicas (quem protege é a
    reconferência).
  - Tarefa 4: a afirmação de saúde de `reassembly_never_rereads_known_bad_sectors` ficou vazia com o Q37 (afirmar que os
    pedaços de R contêm o setor ruim, ou tirar com comentário); `resume_reads_nothing_below_the_checkpoint` aceita
    leituras em [início do primeiro candidato, pos) (restringir às leituras depois da do fim da origem); comentário sobre o
    `Partial` de Q em `tests/reassembly.rs` (~1906, "um cluster por pedaço") impreciso. (O texto do CHANGELOG "nunca sobre
    outro arquivo" e o README que não nomeava os tipos já estão corrigidos.)
  - Tarefa 5: afirmação de unidade `pair_with` sobre o arranjo em três pedaços (`tests/pairing.rs:279` não afirma mais o
    resultado do pareamento).
  - Tarefa 6: o ramo `moof` da medida confere amostras dentro de `len`, não dentro do `mdat` seguinte (a remontagem confere:
    alinhar); a releitura de refutação e o `seams.clean` leem a origem sem contar no orçamento (limitados pelos passos e
    pelo `MAX_BOXES`: cobrar ou comentar); `MoofSeams` duplica a lógica de `clean` nos `Refutes` (ajudante comum).
  - Tarefa 7: texto da taxa de falsa passagem ("about 1%" no README contra "under 1%" no código; o teste limita a < 5%);
    linhas longas no README; tabelas da classe AC guardadas como DC sem teste; `baseline_jpeg_is_not_lossless` não guarda
    nada sozinho.
  - Tarefa 8: m-3, o dimensionamento do testkit usa SSSS 15 em blocos de 14 bits (doc diz "reduzido" e o código aumenta).
  - Tarefa 9: M-2 um cabeçalho alheio dentro de uma caixa do vídeo fora do `mdat` não é examinado; M-3 o teto de reinício
    do Q16 não se aplica a JPEG `OldChain`; M-5 qualquer teto da listagem desliga todo `OldChain` do volume.
  - Tarefa 10: M-8 o teto do pareamento sobre achados já sem nome não tem teste (só defesa); o lado da remontagem do I-3
    não tem caso que falhe (regra compartilhada).

**Módulo 2:**
- mostrar um achado remontado no lugar do antigo (o `Updated` substitui o `found`; a tela decide como);
- a prévia de um RAW relatada duas vezes (D10);
- o texto da colocação `old_chain` para quem usa;
- unir os leitores de plist do motor e do testkit, se valer (C13 da 1.5).

**Sérgio:**
- rodar o roteiro `docs/superpowers/history/2026-11-02-m1-p6-conferencia-manual.md` com a A6700 e registrar abaixo (o
  padrão de alocação real da Sony, D19, e o DRI dos JPEGs dele);
- decidir o merge do PR da 1.6.

**Depois:**
- os outros limites de relógio de uma rodada só (M-1 da revisão final; ainda não falharam): `fs_fat.rs:374` e `:1100`,
  `vanish.rs:215` (`cancel_is_prompt_while_a_window_fails_sector_by_sector`), `fs_exfat.rs:597`, `isobmff.rs:909`,
  `scan.rs:1482`, `preflight.rs:460` e `:593`, `jpeg.rs:392`: converter quando tocados, com o mesmo padrão de contador de
  leituras;
- impressões por cluster durante a busca (desenho §5.1), se cartões reais baterem no orçamento de leitura;
- JPEG DRI 0 em 3 ou mais pedaços: um jeito de achar o corte verdadeiro quando os dados fora de ordem decodificam sem erro;
- conferir a identidade de um apagado pelo EXIF (M-7);
- amostras CC0 de XAVC (C11), se aparecerem.

## Conferência manual

(vazio: o Sérgio preenche, seguindo o passo 8 do roteiro)
