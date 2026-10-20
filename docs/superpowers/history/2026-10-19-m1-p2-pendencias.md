# Parte 1.2 — decisões e pendências (19/10/2026)

Registro da execução do plano `docs/superpowers/plans/2026-10-19-m1-p2-raw-e-metadados.md`, na branch `feat/m1-p2-raw`.
Cada decisão traz o custo se estiver errada.

## Decisões tomadas durante a execução (Claude, pelo Sérgio)

- **P1:** o testkit ganhou `jpeg_with_exif` (JPEG com APP1 `Exif` de Make, Model e campos Exif), usado pelo RAF e pelos
  testes de detalhes da foto. Custo se errada: nenhum.
- **P2:** a exceção do Motion-JPEG só vale se toda cabeça que cortou o `mdat` cai exatamente no início de um chunk de uma
  trilha de vídeo `mjpa`/`mjpb`/`jpeg` e o índice sem corte é Íntegro. Custo: um vídeo MJPEG com quadros fora do início dos
  chunks é cortado (Parcial e quadros soltos, como na 1.1).
- **P3:** o `confirms` do JPEG anda no máximo 1 MiB de cabeçalho. Custo: um JPEG com mais de 1 MiB de APPn antes do SOS não
  corta outro arquivo (a busca ainda o acha).
- **P4:** JPEG aritmético (DAC) com estrutura completa é Parcial ("não verificável"), como o acima de 210 MP. Custo: nenhum.
- **P5:** até 65.536 strips por tag e 262.144 por leitura do TIFF; passar de um limite marca `truncated` (Parcial), não
  `garbled`. Custo: um RAW legítimo enorme aparece Parcial.
- **P6:** o primeiro valor vence para marca, modelo, lente, série, ISO e data (IFD0/Exif antes de IFD1/SubIFDs); o próximo
  IFD de Exif/GPS não é seguido. Custo: nenhum.
- **P7:** o tipo de JPEG é decidido uma vez por IFD (pelo SOF do primeiro strip/tile). Custo: um IFD com compressões
  misturadas (fora do padrão) é mal classificado.
- **P8:** tag repetida no mesmo IFD é ignorada sem marcar `garbled`. Custo: nenhum (continua limitado).
- **P9:** cabeçalho TIFF cujo IFD0 não confirma não vira achado. Custo: um RAW com o IFD0 destruído não aparece (não abriria
  mesmo).
- **P10:** estrutura que só falta porque a origem acabou é Parcial ("cortado"), não Danificado. Custo: nenhum.
- **P11** (emenda o P10): faixa que começa dentro do limite é registrada cortada no limite; a que começa depois não conta; o
  tamanho do RAW é o fim da estrutura, nunca "tudo o que sobra". Custo: nenhum.
- **P12:** um só `SOF_SEGMENTS = 64` para saúde e inspeção. Custo: nenhum.

**Tarefa 6 (amostras reais).** As amostras reais acharam três defeitos de verdade, corrigidos com testes sintéticos que
reproduzem a estrutura:
- **RW2 da Panasonic:** o StripOffsets é um marcador (`0xFFFFFFFF`) e os dados brutos começam na tag 0x0118, sem tamanho. O
  tamanho vem do sensor (tags 0x0002/0x0003) e do empacotamento (pacotes de 16 bytes, em blocos de 0x4000), com aritmética
  verificada e no máximo 10^9 pixels; tamanho hostil ou estouro dá tamanho desconhecido (Parcial).
- **ORF da Olympus/OM System:** a miniatura e a prévia ficam dentro do MakerNote e começavam em setor, cortando o arquivo.
  Agora o MakerNote é lido (cabeçalhos `OLYMPUS` e `OM SYSTEM`). Custo: o MakerNote antigo `OLYMP` (E-10 e afins) não é lido.
- **P13 (estreitada na revisão):** uma cabeça de JPEG dentro de um JPEG do próprio RAW (a miniatura EXIF dentro da prévia)
  não corta o RAW só se todo JPEG próprio que a contém (até `JPEG_MAX_LEN`) ainda decodifica no modo rigoroso; uma
  decodificação por JPEG, guardada. Se não decodifica, corta, e o arquivo de dentro aparece sozinho. Vale para RAW em
  TIFF, RAF e CR3 (`PRVW` e JPEG de tamanho cheio), num ajudante compartilhado. Custo: uma prévia que não decodifica no
  modo rigoroso e guarda uma miniatura aninhada agora corta o RAW ali (Parcial, e a miniatura aparece como outro achado);
  e uma decodificação a mais quando há cabeça dentro de um JPEG próprio.
- **P14:** só valem os empacotamentos da Panasonic conferidos em arquivo real: formato 4 com 12 bits, 14 pixels por pacote
  (DMC-FZ28, DMC-GF1, DMC-LX7 duas vezes, DC-G100) e formato 5 com 14 bits, 9 pixels (DC-GH5S). Qualquer outro, inclusive o
  formato 5 com 12 bits, dá tamanho desconhecido: o RW2 sai Parcial. Custo: um corpo com outro empacotamento aparece
  Parcial até ser conferido.
- **P16** (emenda o P14, revisão final): com empacotamento desconhecido o RW2 continua Parcial, mas os dados brutos são
  reivindicados a partir da tag 0x0118 até um limite seguro (2 bytes por pixel, arredondado ao bloco de 0x4000, com
  aritmética verificada e o mesmo teto de pixels); o corte por outro arquivo e o fim da origem aparam. Sem isso o RW2
  recuperado não abria (o achado acabava antes dos dados). Custo se errada: alguns bytes do que vem depois entram no
  arquivo (inofensivo para leitores de RAW).
- **P15:** `tiff.rs` usa o mesmo limite de 64 segmentos de marcador (`jpeg::SOF_SEGMENTS`) para achar o SOF; a tolerância
  de zeros no fim do teste de amostras reais vale só para ARW. Custo: nenhum.
- **Preenchimento do ARW:** a Sony completa alguns ARW com zeros depois dos dados (até múltiplo de 4 KiB ou 64 KiB). Nada
  aponta para esses zeros, então o achado termina no último byte referenciado, e o teste aceita até 64 KiB de zeros no fim.
  Custo: o arquivo recuperado fica sem esse enchimento (abre igual).

**Revisão final (onda de correções).**
- O MJPEG não anda mais o `mdat` inteiro depois de um corte: a segunda leitura só lê cabeçalhos de caixa, e numa trilha
  MJPEG com índice Íntegro uma terceira leitura pula os inícios de chunk e para na primeira cabeça que não é um deles
  (mesma regra do P2). Medido em 1,5 GiB com 96 MP4 de `mdat` tamanho 0: 21,8 s → 2,6 s.
- CR3: a saúde é julgada sobre os mesmos JPEGs que a inspeção mostra (`PRVW` e o JPEG de tamanho cheio), com a regra do
  RAW em TIFF (o maior decide; um não mensurável deixa Parcial). Sem `PRVW` continua Parcial. Custo: um `PRVW` quebrado
  ao lado de um JPEG cheio bom não rebaixa mais o CR3.
- RAW em TIFF: só o início de strip/tile com compressão JPEG (6, 7, 34892) é isento de corte; uma foto gravada no início
  de um strip sem compressão agora corta o RAW e aparece.
- Num RW2 Parcial (empacotamento desconhecido) a busca continua olhando cabeças dentro dele (regra da 1.1: um arquivo
  quebrado não esconde outros), então o JpgFromRaw, que começa em setor, ainda aparece como achado próprio. Evitar isso
  pede que a medida diga à busca quais inícios são do próprio arquivo: fica para a 1.3.

## Pendências (dono: parte indicada)

**Parte 1.3 (arquivos fragmentados e velocidade)**
- O `moov` é carregado mais de uma vez por medida de um vídeo cortado no `mdat` (`judge`, `own_starts`, os JPEGs do CR3).
- A busca relata os JPEGs próprios de um achado Parcial que começam em setor (ex.: o JpgFromRaw de um RW2 com
  empacotamento desconhecido). A medida poderia devolver os inícios próprios para a busca pular.
- A saúde depende da posição perto do fim da origem: o mesmo offset corrompido dá Parcial quando a origem acaba antes
  dele e Danificado quando ela continua (revisão final, item 4).
- ISO BMFF: `confirms` aceita `ftyp` seguido de qualquer caixa válida, mas `measure` recusa um arquivo sem `mdat`; um
  `ftyp` solto pode cortar outro arquivo sem virar achado (revisão final, item 8).
- `all_formats()` é reconstruído a cada `measure`.
- Um COUNT ou tamanho corrompido num offset válido ainda é cortado no limite; numa origem menor que 1 GiB o achado pode ir
  até o fim dela (Parcial). Os comentários de `claim()`/`TiffInfo.data` devem dizer que o corte vale para toda faixa.
- O ARW da Sony DSLR-A100 (2006) sai Danificado: o ponteiro de SubIFD aponta para dados embaralhados. Pede tratamento por
  modelo.
- RW2 com empacotamento não conferido sai Parcial (P14, P16): o formato 5 com 12 bits (seriam 10 pixels por pacote) e os
  formatos 6 e 7 dos corpos mais novos. Conferir com amostras reais quando houver.
- O MakerNote antigo `OLYMP\0` da Olympus (E-10 e afins, offsets relativos ao TIFF) não é lido.
- Não há HEIC real: nenhuma amostra com licença CC0/domínio público verificável foi achada.

**Parte 1.4 (sistemas de arquivos)**
- Uma foto gravada exatamente no início de uma prévia ou de um strip com compressão JPEG de RAW, de um chunk de CR3 ou de
  uma extensão de HEIC não se distingue dos dados do próprio arquivo. Os metadados do sistema de arquivos resolvem isso.

**Parte 1.5 (disco de verdade)**
- O HEIC nunca fica "completo" na leitura: um setor ilegível logo depois de um HEIC completo o deixa Parcial, e um `moov`
  solto pode mudar o tipo.
- Um MP4 que lista `heic` como marca compatível e é cortado antes do `moov` vira HEIC Danificado.

**Módulo 2 (app)**
- RW2 não mostra largura, altura nem ISO (ficam nas tags próprias da Panasonic, 0x0002/0x0003 e 0x0017).
- A Sony grava `----` como lente quando não sabe; hoje aparece assim.
- JPEG acima de 64 MiB perde as dimensões na inspeção.

**Menores, sem dono**
- Testes que faltam: o ramo do limite de iterações do `iloc`; a guarda `room()`.
  O teste `zero_size` do HEIC aceita qualquer coisa diferente de Íntegro (mais frouxo que Parcial).
- `random_head_inside_mdat` depende do ruído semeado.
- O ponteiro de próximo IFD é reivindicado antes da checagem de `MAX_ENTRIES` e pode marcar `garbled` à toa quando o limite
  cai no meio da tabela.
- `offset_in` (aritmética de ponteiros no CR3) é frágil.
- Linhas longas e quebras irregulares em alguns READMEs e comentários.
