# Parte 1.6: fragmentação avançada (desenho)

Data: 26/10/2026. Autoridade acima deste documento: a spec `2026-10-19-revenant-design.md` (§4 estados, §5 "Escultura com
fragmentação": "tentar remontar arquivos fragmentados em 2+ pedaços"). O que não está aqui segue a spec. Marcas:
**[Sérgio]** é decisão dele; **[escolha do Claude]** é decisão minha, que ele pode trocar. O Sérgio pediu para pular as
perguntas e ir direto à produção (26/10/2026), então quase tudo aqui é escolha minha.

## 1. Por que, e para quem

Hoje (1.3 e 1.4) o motor junta um JPEG em **2** pedaços, pareia o índice (`moov`) de um vídeo separado do resto e segue a
cadeia de clusters de um arquivo **vivo** no FAT/exFAT. Um arquivo **apagado** e espalhado em 3 ou mais pedaços sai
`Partial` (ou nem sai), mesmo quando todos os bytes estão no cartão.

O caso real do Sérgio **[Sérgio]**: fotografa sempre **RAW + JPEG** e filma em **LOG** numa **Sony A6700**. Ou seja, a
prioridade é:

- **ARW** (RAW da Sony) e o **JPEG** que a câmera grava junto;
- **vídeo XAVC** (MP4 com H.264 ou H.265, arquivos grandes) num cartão **exFAT** (todo SDXC de 64 GB ou mais).

Vídeo grande é o arquivo que mais se fragmenta num cartão usado: apagar clipes no meio deixa buracos, e o próximo clipe
os preenche.

## 2. Objetivos

- **G1. JPEG em 3 ou mais pedaços**, emendas validadas pelo decodificador próprio (o da 1.3), com a mesma régua de
  unicidade: só `Intact` quando nenhuma outra emenda serve.
- **G2. Vídeo MP4/MOV (XAVC) em 3 ou mais pedaços**, usando o índice do vídeo (`moov`: tamanhos e posições de cada amostra)
  para saber onde cada pedaço tem de estar e conferir cada amostra pelo formato das unidades H.264/H.265 (tamanhos
  prefixados que precisam fechar). Inclui a peça do meio com poucas amostras que hoje passa despercebida (C7 da 1.3).
- **G3. Os casos de vídeo deixados pela 1.3**: `moov` antes do `mdat`, `mdat` de tamanho 0, MP4 fragmentado (`moof`), e
  dois vídeos com os primeiros pedaços idênticos.
- **G4. RAW (ARW primeiro, depois os outros RAW já suportados) fragmentado, onde a emenda pode ser validada**: a estrutura
  TIFF (cada deslocamento tem de cair dentro do arquivo remontado) e a prévia JPEG embutida (que decodifica através das
  emendas) são validáveis. Os dados brutos do sensor não são (§4).
- **G5. Arquivo apagado e fragmentado no FAT/exFAT (D3 da 1.4)**: quando a remontagem (G1–G4) prova o arquivo inteiro, ele
  pode sair `Intact` mesmo sem a cadeia de clusters; o tamanho registrado na entrada do sistema de arquivos e o mapa de
  clusters livres limitam onde os pedaços podem estar.
- **G6. Pendências com dono 1.6** (§8).

## 3. Fora da 1.6 (não-objetivos)

- Decodificar RAW (seria preciso um decodificador por câmera; não é o caminho).
- Outros formatos além dos que o motor já reconhece.
- NTFS, APFS, HFS+.
- Copiar e buscar ao mesmo tempo (ideia guardada da 1.5).
- Telas: Módulo 2, que vem logo depois **[Sérgio]**.

## 4. Antiobjetivos

- **Colar pedaços de duas fotos ou vídeos diferentes e chamar de `Intact`.** É o pior erro possível: o usuário confia e
  perde a foto de verdade. A régua da 1.3 vale para tudo: na dúvida, `Partial`.
- **RAW `Intact` sem prova.** Uma emenda que cai nos dados brutos do sensor não tem como ser conferida. Nesse caso o RAW sai
  remontado (os bytes no lugar mais provável), mas no máximo `Partial`, com a prévia JPEG ainda íntegra quando ela estiver
  inteira **[escolha do Claude]**. Exceção: quando o sistema de arquivos dá a cadeia viva, como já hoje.
- **Busca sem fim num cartão hostil ou muito fragmentado.** Todo o trabalho de remontagem tem orçamento, com corte
  documentado; passado o orçamento, o arquivo sai como sai hoje.
- **Deixar a busca normal mais lenta.** Numa imagem sem fragmentação, a 1.6 não pode custar mais de 5% (medido com build de
  controle, como na 1.4 e na 1.5).
- **Resultado que muda com o número de threads ou o cache, ou que difere ao retomar uma sessão** (régua da 1.3 e da 1.5).

## 5. Como funciona (o que já está decidido)

- **Onde um pedaço pode começar** **[escolha do Claude]**: num volume conhecido, só em início de cluster (a grade da 1.4,
  já usada no pareamento de vídeo); sem volume, em setor de 512 B, como hoje, com orçamento menor. Só entram como
  candidatos os clusters **livres** (não reivindicados por arquivo vivo), e um pedaço nunca é usado por dois arquivos
  `Intact`.
- **Tamanho conhecido**: quando a entrada apagada do FAT/exFAT dá o tamanho, a soma dos pedaços tem de bater; quando o
  formato dá o tamanho (o índice do vídeo, a estrutura TIFF), idem.
- **Teto de pedaços** e **orçamento de trabalho** por arquivo e por busca, com valores escolhidos pelo construtor, medidos e
  documentados.
- **Saúde**: `Intact` exige que cada emenda seja validada pelo conteúdo e seja a única possível dentro do orçamento; senão
  `Partial` com os pedaços que se conseguiu juntar.
- **Testes**: cartões sintéticos (testkit) com arquivos espalhados em 3 a N pedaços, intercalados com outros arquivos
  parecidos (a mesma cena, a rajada, dois clipes da mesma câmera), para provar que nada é colado errado. Amostras reais
  CC0 de ARW e XAVC da A6700 (ou de câmeras Sony parecidas) entram no teste opcional `real_samples` se o construtor achar
  amostras com licença permitida; nunca no repositório.

### 5.1 Decisões depois da leitura do construtor **[escolha do Claude]**

- **ARW, por modo de gravação.** O Sérgio não disse qual modo usa, então os três entram. No **comprimido sem perdas**
  (blocos codificados como JPEG sem perdas, `Compression 7`), conferir que cada bloco decodifica até o fim com o número
  exato de unidades é **validação de estrutura**, não decodificar o RAW (não há demosaico nem cor): uma emenda conferida
  assim conta como prova. No **sem compressão** e no **comprimido com perdas**, uma emenda nos dados do sensor não tem
  prova: o RAW sai remontado e no máximo `Partial`.
- **Releitura da origem.** A remontagem pode ler a origem de novo, dentro de um orçamento, mas **nunca relê um setor que a
  busca já sabe ruim** (o mapa da 1.3/1.5). Gravar durante a busca uma pequena impressão por cluster, para não precisar
  reler, também é permitido, com memória limitada e documentada. A escolha entre os dois é do construtor. Num disco que
  está morrendo, o caminho continua sendo a cópia primeiro (1.5), e a busca roda na imagem.
- **JPEG com marcadores de reinício (Q16 da 1.3).** O teto `Partial` continua: o tamanho registrado e a grade de clusters
  não provam o conteúdo. O construtor mede, com amostras reais da Sony se houver, se os JPEGs e as prévias da A6700 usam
  marcadores de reinício e registra o resultado no histórico. Se usarem, o limite fica documentado como o principal custo
  da 1.6 para a câmera do Sérgio.
- **Cadeia FAT que sobrevive num arquivo exFAT apagado.** É uma pista forte, mas não basta sozinha: um arquivo posterior,
  gravado contíguo e depois apagado, pode ter reusado clusters sem reescrever a FAT. Ela vale como prova só quando todos
  estes pontos valem juntos:
  - todos os clusters estão livres;
  - o tamanho bate e a cadeia termina em "fim";
  - nenhuma outra entrada apagada da listagem reivindica esses clusters;
  - todas as conferências de conteúdo disponíveis passam (estrutura TIFF, prévia, blocos).

  Senão, ela vira só a primeira hipótese a testar.
- **Casos reais a mirar.** Três: apagar na câmera, formatar na câmera e apagar no computador. Os cartões sintéticos imitam
  dois jeitos de alocar clusters, "o próximo livre" e "o primeiro buraco", com RAW e JPEG gravados intercalados. Como o
  firmware da Sony aloca é suposição, e fica registrado como risco. A parte entrega um **roteiro de conferência manual com a
  A6700**: gravar, apagar e regravar. O Sérgio roda quando quiser, como na 1.5.

## 6. Restrições

Tudo do `CLAUDE.md`: só leitura na origem, licenças permitidas, nada copiado do PhotoRec/TestDisk, erros por código, Rust
2024 / MSRV 1.88, `unsafe` só em `source/windows.rs`, testes só com dados sintéticos, documentação no mesmo commit, nunca
`brew`, código com `std::os::unix` protegido por `cfg`. Determinismo e "retomar = busca inteira" continuam valendo.

## 7. O que fica com o construtor

- Os algoritmos de busca de emenda (ordem dos candidatos, poda), dentro das regras do §5.
- Os tetos e orçamentos, com medição.
- Como a validação de amostras de vídeo lida com H.264 e H.265 (tamanho do prefixo vem do `avcC`/`hvcC`).
- A divisão em tarefas.

## 8. Pendências com dono 1.6

- **1.3**: arquivos em 3+ pedaços (JPEG e vídeo), incluindo a peça do meio de vídeo com menos de 1/32 das amostras (C7);
  fragmentação de RAW; vídeo com `moov` antes do `mdat`, `mdat` de tamanho 0, `moof`, e dois vídeos com primeiros pedaços
  idênticos.
- **1.4**: arquivo apagado e fragmentado sai `Partial` mesmo inteiro (D3); foto cortada seguida da cauda de uma foto quase
  igual, sem metadados; e os menores do apêndice R37 com dono 1.6 (T5 limitar a fase de palavra alta e testar o corte da
  fila; T6 confiança de diretório `NoFatChain`, chunk ilegível sem custo de orçamento, arquivos que não são foto antes do
  bitmap; T9 prévia de RAW que cruza um pedaço; T10 continuação de pasta antiga exFAT e nó cortado por profundidade).

Cada uma é fechada, medida ou devolvida com dono no histórico da 1.6.

## 9. Pronto quando

- Cartões sintéticos com JPEG, ARW e MP4 (H.264 e H.265) espalhados em 3 a N pedaços, apagados ou não, voltam inteiros e
  `Intact` quando a prova existe, e `Partial` (nunca `Intact` errado) quando não existe, inclusive com arquivos parecidos
  intercalados.
- Nenhuma piora de velocidade acima de 5% em imagem sem fragmentação; tempo de remontagem limitado e documentado.
- Determinismo e retomada de sessão iguais às da 1.5.
- READMEs, CHANGELOG, roadmap, `CLAUDE.md` e histórico da parte atualizados.
