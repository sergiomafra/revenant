# Revenant — roteiro até a versão 1.0

Atualizado em 19/10/2026. Cada parte segue o mesmo caminho: spec, depois telas (quando houver tela), depois plano, execução por
subagentes e um PR. Quem faz o merge é o Sérgio. A documentação é atualizada em toda parte.

## Marcos

| Marco | O que dá para fazer | Depois de |
|---|---|---|
| **A. Motor completo** | Um desenvolvedor recupera fotos RAW e vídeos de uma imagem de cartão pelo `revenant-dev`. | 1.2–1.5 |
| **B. Primeira tela** | O Sérgio usa o app no Mac dele, sem terminal, para recuperar um cartão de verdade. | 2.1–2.3 |
| **C. Beta público** | Qualquer pessoa baixa o instalador (Mac ou Windows), segue o guia e recupera os arquivos. | 3 + 4 |
| **D. 1.0** | O beta já foi usado em cartões reais, os problemas foram corrigidos e a documentação está completa nos 3 idiomas. | uso real |

**O app fica "usável" no marco B, para o Sérgio, e no marco C, para o público.**

## Módulo 1: Motor (Rust, sem interface)

- **1.1, concluída:** fundação do repositório; escultura de JPEG e MP4/MOV; estados Íntegro, Parcial e Danificado; extração
  segura; `revenant-dev`.
- **1.2, RAW e dados da foto:**
  - Formatos: CR2, CR3, NEF, ARW, RAF, DNG, ORF, RW2 e HEIC, além de TIFF genérico.
  - Lê os dados da foto (câmera, lente, número de série, ISO, data da foto) e a maior prévia JPEG embutida.
  - Pendências da 1.1.
  - Amostras reais de câmera nos testes.
- **1.3, arquivos fragmentados e velocidade:**
  - Remonta JPEG e vídeo partidos em dois ou mais pedaços.
  - Busca no ritmo do leitor de cartão: leitura em paralelo e mapa de setores ruins.
- **1.4, sistemas de arquivos:**
  - exFAT e FAT32: arquivos apagados com o nome e a pasta originais, cartão formatado rápido.
  - Deduplicação entre a busca por estrutura e a busca por assinatura.
  - Agrupamento por sessão e por câmera.
  - Arquivo recuperado sem nome ganha um nome por data e câmera.
- **1.5, disco de verdade:**
  - Lê o cartão ou HD direto pelo aparelho no Mac e no Windows, desmontando sem ejetar e sempre só leitura.
  - Cópia do cartão tolerante a setores ruins.
  - A busca fica salva e pode ser retomada.

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
