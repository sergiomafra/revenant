# Parte 1.1 — decisões e pendências (19/10/2026)

Registro da execução do plano `docs/superpowers/plans/2026-10-19-m1-p1-fundacao-e-escultura.md`, na branch
`feat/m1-p1-engine`. O plano diz `rust-version = "1.85"`, mas o certo é **1.88**: o código usa `if … && let`.

## Decisões tomadas durante a execução (Claude, pelo Sérgio)

- **R1:** `testkit::jpeg(.., app1_extra: 0)` não gera APP1. Com valor maior que zero, gera um APP1 com uma miniatura de 16×16
  completada até esse tamanho.
- **R2:** os "300 KB" do teste de fronteira de bloco são aproximados.
- **R3:** o motor sempre recusa extrair para o volume da origem. O `revenant-dev` tem `--allow-same-volume`, só para
  desenvolvedor e só para arquivo de imagem.
- **R4:** JPEG é **Íntegro** só se o decodificador rigoroso aceita. Se só o tolerante aceita, é **Parcial**. Se os dois recusam,
  é **Danificado**.
- **R5:** a validação não decodifica imagens acima de 210 MP, que viram Danificado, e decodifica em tons de cinza para poupar
  memória. *Rever na 1.2:* o revisor sugere "não verificado" ou Parcial em vez de Danificado.
- **R6:** MP4/MOV com índice (`moov`) estragado, ou sem tabelas de amostras, é Danificado.
- **R7:** até 7 bytes zerados no fim de um container (o terminador do QuickTime) são tolerados.
- **R8:** a busca lê o disco inteiro. O achado só sai depois que a busca passa do fim dele. Um setor ilegível dentro do arquivo
  rebaixa Íntegro para Parcial.
- **R9:** erro de leitura nunca faz um arquivo sumir. O setor ilegível vira zeros e limita o estado: JPEG fica no máximo
  Parcial; ilegível dentro do `moov` deixa o vídeo Danificado, mas o tamanho cobre o `moov`.
- **R10** (emenda o R8): o MP4 procura assinaturas de outros arquivos dentro do `mdat` e termina ali. Só um achado Íntegro
  impede a busca de olhar o que há dentro dele, assim um vídeo quebrado não esconde mais o resto do disco.
- **R11:** fazer push da branch para conferir o CI nas 4 arquiteturas fica com o Sérgio.

## Pendências (dono: parte indicada)

**Parte 1.2**
- `isobmff.rs`: um vídeo completo que termina na fronteira de um setor, seguido de um setor ilegível, sai Parcial. Correção de
  uma linha: `truncated |= unreadable_header` só quando `!complete`.
- Vídeos Motion-JPEG (MOV/AVI de câmeras compactas antigas) são cortados pelo R10 quando um quadro começa num setor. Corrigir
  pulando a checagem dentro do `mdat` quando o codec for `mjpa`, `mjpb` ou `jpeg`.
- `is_segment_marker` não aceita `0xCC`/`0xC8` nem bytes de preenchimento `FF FF` antes de um marcador. Conferir com amostras
  reais de câmera.
- Rever o R5 (rótulo de imagem acima de 210 MP).

**Parte 1.3**
- `FileSource` lê o tamanho de um aparelho como 0, e o `dev()` de um nó de aparelho não é o volume dos dados. A leitura do
  aparelho bruto precisa de leituras alinhadas a setor.
- Quando a identificação do volume falha, `volume()` devolve None. Com aparelhos, isso precisa recusar a extração.
- Setores ruins já conhecidos são relidos um a um a cada `measure`. Isso pede um mapa de setores ruins compartilhado.
- A instrução de `dd` com `bs=4m conv=noerror,sync` zera 4 MiB por setor ruim: trocar por uma ferramenta de cópia tolerante.

**Desempenho, antes do Módulo 2**
- Foram medidos ~62 MB/s com muitos JPEGs, antes do leitor compartilhado; precisa medir de novo. O `mdat` é lido duas vezes,
  uma ao medir e outra na busca.

**Módulo 2**
- `extract` não chama `sync_all`, então um erro de escrita tardio passa despercebido. Decidir junto com o relatório de
  "recuperados".

**Menores, sem dono**
- O CI roda em `push` e em `pull_request`, então cada PR roda duas vezes.
- Faltam testes de: leitura curta da fonte; o limite de 10.000 nomes; erro de escrita no `extract`; `Progress{0,0}`.
- O teste de "pastas só de leitura" do `revenant-dev` roda só em Unix.
- Com a saída fechada (`| head`), o `revenant-dev` termina com o código 1 ("cancelled").
