# Revenant — roteiro até a versão 1.0

Atualizado em 02/11/2026. Cada parte segue o mesmo caminho: spec, depois telas (quando houver tela), depois plano, execução por
subagentes e um PR. Quem faz o merge é o Sérgio. A documentação é atualizada em toda parte.

## Marcos

| Marco | O que dá para fazer | Depois de |
|---|---|---|
| **A. Motor completo** (fechado em 25/10/2026, com a 1.5) | Um desenvolvedor recupera fotos RAW e vídeos de uma imagem de cartão pelo `revenant-dev`. | 1.2–1.5 |
| **B. Primeira tela** | O Sérgio usa o app no Mac dele, sem terminal, para recuperar um cartão de verdade. | 2.1–2.3 |
| **C. Beta público** | Qualquer pessoa baixa o instalador (Mac ou Windows), segue o guia e recupera os arquivos. | 3 + 4 |
| **D. 1.0** | O beta já foi usado em cartões reais, os problemas foram corrigidos e a documentação está completa nos 3 idiomas. | uso real |

**O app fica "usável" no marco B, para o Sérgio, e no marco C, para o público.**

## Módulo 1: Motor (Rust, sem interface)

- **1.1, concluída:** fundação do repositório; escultura de JPEG e MP4/MOV; estados Íntegro, Parcial e Danificado; extração
  segura; `revenant-dev`.
- **1.2, RAW e dados da foto, concluída:**
  - Formatos: CR2, CR3, NEF, ARW, RAF, DNG, ORF, RW2 e HEIC, além de TIFF genérico.
  - Lê os dados da foto (câmera, lente, número de série, ISO, data da foto) e a maior prévia JPEG embutida.
  - Pendências da 1.1.
  - Amostras reais de câmera nos testes.
- **1.3, arquivos fragmentados e velocidade, concluída** (PR #4):
  - Remonta o JPEG partido em dois pedaços e o vídeo de câmera cujo `moov` ficou longe do `mdat` (dois pedaços).
  - Busca no ritmo do leitor de cartão: cada setor lido uma vez pela busca e pelos detectores (a busca de lacuna de um
    JPEG pode reler a sua janela de 64 MiB), conferência em paralelo e mapa de setores ruins (de 21,8 para 503,9 MB/s
    em JPEG denso; números em `crates/revenant-core/README.md`).
  - Não cobre: arquivos em 3 ou mais pedaços e fragmentação de RAW (foram para a 1.6).
- **1.4, sistemas de arquivos, concluída** (PR #5):
  - FAT12/16/32 e exFAT, em MBR e GPT: arquivos vivos e apagados com o nome, a pasta e as datas originais, até com o
    arquivo fragmentado (cadeia viva) e a FAT com setor ruim.
  - Cartão formatado rápido: os nomes e as pastas voltam pelas pastas antigas que sobraram.
  - Deduplicação: a mesma foto achada pelo sistema de arquivos e pela escultura sai uma vez; JPEGs próprios de um RAW
    `Partial` não são mais relatados à parte; o vídeo pareia na grade de clusters.
  - Agrupamento por sessão, dia, câmera e pasta.
  - Arquivo recuperado sem nome ganha um nome por data e câmera; a extração sanea os nomes.
  - Não cobre: arquivo apagado e fragmentado (sai `Partial`, vai para a 1.6); NTFS, APFS e HFS+ (a escultura segue
    cobrindo). Pendências e decisões em `docs/superpowers/history/2026-10-25-m1-p4-pendencias.md`.
- **1.5, disco de verdade, concluída** (PR #6):
  - Lê o cartão ou HD direto pelo aparelho no Mac e no Windows, desmontando sem ejetar e sempre só leitura; lista os
    discos e recusa o disco do sistema.
  - Conferência prévia: tudo o que já se sabe estar errado (permissão, aparelho mudo, setor ilegível, desmontagem, destino
    pequeno ou no próprio cartão, sessão de outro cartão) numa lista só, antes da leitura longa.
  - Cópia do cartão tolerante a setores ruins, com mapa próprio, retomável; a busca na imagem lê o que não foi resgatado
    como ilegível, nunca como zeros.
  - A busca fica salva em uma sessão e retoma depois de queda, cabo puxado ou energia, com os mesmos resultados.
  - Sinais cedo: aviso "disco falhando, faça uma cópia", velocidade e tempo restante.
  - Não cobre: janela de remontagem e leitura sem root/administrador pelo ajudante privilegiado (2.1); impedir o sono do
    computador e os textos de cada aviso (Módulo 2); copiar e buscar ao mesmo tempo (ideia para depois). Pendências em
    `docs/superpowers/history/2026-10-25-m1-p5-pendencias.md`; conferência com hardware em
    `docs/superpowers/history/2026-10-25-m1-p5-conferencia-manual.md`.
- **1.6, fragmentação avançada, concluída** (revisão final feita e corrigida; aguardando o PR/merge):
  - JPEG, vídeo e RAW em 3 ou mais pedaços, buscados no espaço livre do cartão na grade de clusters que a 1.4 traz, com
    orçamento por arquivo e por busca.
  - Vídeo de câmera H.264/H.265: com o `moov` no fim, antes do `mdat`, com `mdat` de tamanho 0 e fragmentado (`moof`);
    cada quadro conferido onde o índice o põe, inclusive a peça do meio pequena.
  - RAW (ARW primeiro; CR2 só `Partial`; NEF, DNG, ORF, RW2) onde a emenda é provada: estrutura TIFF, prévia e, no sem perdas, cada
    bloco decodificado; `Intact` só com cortes exatamente entre blocos.
  - Apagados e fragmentados no FAT/exFAT voltam no lugar, com nome e tamanho (sem nome quando a estrutura termina antes
    do tamanho, Q53); no exFAT, a cadeia que sobrevive ao apagar
    na câmera é prova (`old_chain`) quando nada no cartão a contradiz.
  - Nunca cola duas fotos e chama de `Intact`: um JPEG em 2 ou mais pedaços posto só pelo conteúdo sai `Partial`.
  - Não cobre (para depois):
    - RAW sem compressão (ou com perdas) espalhado nos dados do sensor: volta com os bytes no lugar, mas `Partial`.
    - Continuação a mais de 256 MiB do pedaço anterior, ou depois de 4096 candidatos numa emenda (um clipe de 761 MiB em 3
      pedaços fora de ordem não volta).
    - JPEG sem marcadores de reinício (DRI 0) em 3 ou mais pedaços, colocado só pelo conteúdo, muitas vezes não volta
      (fica o palpite de uma corrida, Parcial, como na 1.4: 6 das 9 prévias reais na medição da Tarefa 11); falta um
      jeito de achar o corte verdadeiro quando os dados fora de ordem decodificam sem erro.
    - Um apagado sobrescrito por outro arquivo de exatamente o mesmo tamanho sai com os bytes certos e o nome do antigo:
      conferir a identidade pelo EXIF (data da foto contra as datas da entrada).
    - Um RAW, HEIC, RAF, CR3 ou vídeo com trilha sem conferência (ProRes) gravado contíguo sobre o primeiro pedaço de um
      apagado e passando dele não volta inteiro (os dados depois desse pedaço não são conferidos; o JPEG e o vídeo com
      todas as amostras conferidas voltam, sem nome, Q61/Q62); no RAW sem perdas, conferir bloco a bloco resolveria.
    - O padrão de alocação real da Sony: a conferir pelo roteiro com a A6700.
  - Decisões, resíduos aceitos e pendências em `docs/superpowers/history/2026-11-02-m1-p6-pendencias.md`; conferência
    com a câmera em `docs/superpowers/history/2026-11-02-m1-p6-conferencia-manual.md`.

## Módulo 2: App (Tauri 2 + React; telas aprovadas antes de cada parte)

- **2.0, telas e identidade:**
  - Mockups em horizontal de todas as telas, em claro e escuro.
  - Logo e ícone.
  - Aprovação do Sérgio.
- **2.1, esqueleto:**
  - Janela, temas e 3 idiomas.
  - Ajudante privilegiado só de leitura: senha de administrador no Mac, UAC no Windows.
  - Ponte entre o app e o motor.
- **2.2, o caminho:** escolher o disco, dar a permissão, fazer a cópia opcional e acompanhar a busca com a galeria se enchendo
  ao vivo.
- **2.3, resultados e recuperação:**
  - Galeria, Linha do tempo e Detalhes.
  - "Selecionar tudo" por grupo.
  - Prévia real das fotos e dos vídeos pelos decodificadores do sistema.
  - Recuperar, com o resumo no fim.

## Módulo 3: Lançamento

- Release automático por tag `vX.Y.Z`, com os instaladores das 4 arquiteturas (.dmg, .msi/.exe).
- Assinatura ad-hoc no Mac. Pedido de assinatura gratuita à SignPath para o Windows.
- Atualização automática dentro do app.
- Primeiro lançamento: `0.9.0-beta`.

## Módulo 4: Site de documentação

- Astro Starlight no GitHub Pages, em pt-BR, es e en.
- Guia para quem usa, com capturas de tela: instalar, passar pelo alerta, dar a permissão, recuperar, "perdi meu cartão: o que
  fazer agora" e por que SSD é diferente.
- Parte para quem desenvolve.

## Depois da 1.0 (versões 1.x)

Reparo de vídeo com arquivo de referência (o próximo da fila), MXF/BRAW, documentos, SSD interno e o que o uso real pedir.
