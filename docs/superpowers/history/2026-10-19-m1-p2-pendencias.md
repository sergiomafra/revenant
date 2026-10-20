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

**Tarefa 6 (amostras reais), aguardando revisão.** As amostras reais acharam três defeitos de verdade, corrigidos com testes
sintéticos que reproduzem a estrutura:
- **RW2 da Panasonic:** o StripOffsets é um marcador (`0xFFFFFFFF`) e os dados brutos começam na tag 0x0118, sem tamanho. O
  tamanho vem do sensor (tags 0x0002/0x0003) e do empacotamento (pacotes de 16 bytes com 14 pixels no formato 4, 10 ou 9 no
  formato 5, em blocos de 0x4000). Conferido em 6 arquivos de 5 câmeras. Outro formato fica Parcial. Custo: um corpo novo com outro
  empacotamento aparece Parcial até ser conferido.
- **ORF da Olympus/OM System:** a miniatura e a prévia ficam dentro do MakerNote e começavam em setor, cortando o arquivo.
  Agora o MakerNote é lido (cabeçalhos `OLYMPUS` e `OM SYSTEM`). Custo: o MakerNote antigo `OLYMP` (E-10 e afins) não é lido.
- **Miniatura dentro da prévia:** uma cabeça de JPEG dentro de um JPEG do próprio RAW não corta mais o RAW. Custo: um
  arquivo gravado por cima de um JPEG menor do RAW é tomado como parte dele (sobre a prévia maior, a decodificação falha e o
  arquivo continua aparecendo).
- **Preenchimento do ARW:** a Sony completa alguns ARW com zeros depois dos dados (até múltiplo de 4 KiB ou 64 KiB). Nada
  aponta para esses zeros, então o achado termina no último byte referenciado, e o teste aceita até 64 KiB de zeros no fim.
  Custo: o arquivo recuperado fica sem esse enchimento (abre igual).

## Pendências (dono: parte indicada)

**Parte 1.3 (arquivos fragmentados e velocidade)**
- O `moov` é relido em `is_motion_jpeg`, e o MJPEG anda o `mdat` inteiro mesmo depois de um corte cedo (poderia parar na
  primeira cabeça fora de chunk).
- `all_formats()` é reconstruído a cada `measure`.
- Um COUNT ou tamanho corrompido num offset válido ainda é cortado no limite; numa origem menor que 1 GiB o achado pode ir
  até o fim dela (Parcial). Os comentários de `claim()`/`TiffInfo.data` devem dizer que o corte vale para toda faixa.
- `tiff.rs` ainda olha 16 segmentos atrás do SOF (`MAX_SEGMENTS`), contra 64 no resto (P12).
- O ARW da Sony DSLR-A100 (2006) sai Danificado: o ponteiro de SubIFD aponta para dados embaralhados. Pede tratamento por
  modelo.
- RW2 com formato de empacotamento diferente de 4 e 5 (corpos mais novos) sai Parcial; conferir com amostras quando houver.
- Não há HEIC real: nenhuma amostra com licença CC0/domínio público verificável foi achada.

**Parte 1.4 (sistemas de arquivos)**
- Uma foto gravada exatamente no início de um strip/prévia de RAW, de um chunk de CR3 ou de uma extensão de HEIC não se
  distingue dos dados do próprio arquivo. Os metadados do sistema de arquivos resolvem isso.

**Parte 1.5 (disco de verdade)**
- O HEIC nunca fica "completo" na leitura: um setor ilegível logo depois de um HEIC completo o deixa Parcial, e um `moov`
  solto pode mudar o tipo.
- Um MP4 que lista `heic` como marca compatível e é cortado antes do `moov` vira HEIC Danificado.

**Módulo 2 (app)**
- RW2 não mostra largura, altura nem ISO (ficam nas tags próprias da Panasonic, 0x0002/0x0003 e 0x0017).
- A Sony grava `----` como lente quando não sabe; hoje aparece assim.
- JPEG acima de 64 MiB perde as dimensões na inspeção.

**Menores, sem dono**
- Testes que faltam: tirar a condição `hdlr 'vide'` do MJPEG; o ramo do limite de iterações do `iloc`; a guarda `room()`.
  O teste `zero_size` do HEIC aceita qualquer coisa diferente de Íntegro (mais frouxo que Parcial).
- `random_head_inside_mdat` depende do ruído semeado.
- O ponteiro de próximo IFD é reivindicado antes da checagem de `MAX_ENTRIES` e pode marcar `garbled` à toa quando o limite
  cai no meio da tabela.
- `offset_in` (aritmética de ponteiros no CR3) é frágil.
- Linhas longas e quebras irregulares em alguns READMEs e comentários.
