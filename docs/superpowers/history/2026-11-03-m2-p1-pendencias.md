# Parte 2.1 — decisões e pendências (03/11/2026)

Registro da execução do plano `docs/superpowers/plans/2026-11-03-m2-p1-esqueleto.md`, na branch `feat/m2-p1-skeleton`
(de `main` depois do merge da 2.0). O desenho é `docs/superpowers/specs/2026-11-03-m2-p1-esqueleto-design.md`. O roteiro
para o Sérgio conferir o app aberto está em `docs/superpowers/history/2026-11-03-m2-p1-conferencia-manual.md`.
Cada decisão traz o custo se estiver errada.

## O que vai para a 2.2

- **Manter o acesso aberto durante a busca.** Na 2.1, cada escolha de disco pede a senha uma vez, o app lê o primeiro
  setor para provar que funciona e solta o acesso. A 2.2 mantém o acesso aberto durante a busca toda, para não pedir de
  novo ao retomar ou quando o cartão sai e volta. Se esse caminho não servir, volta ao ajudante iniciado com a senha
  (spec do produto §6) e registra o porquê.
- **O texto do passo 3 da permissão** ("A conferência do cartão começa sozinha") ficou como no mockup, mas na 2.1 não
  é verdade ainda. Na 2.2 passa a ser.
- **A tela provisória sai.** "O Revenant consegue ler o disco" (`AccessGranted`) não tem mockup e dá lugar à conferência
  prévia.

## Decisões do Sérgio na 2.1

- **MPL-2.0 permitida, sem modificação** (vale para o que vai dentro do app). Já está no `CLAUDE.md` e no `deny.toml`.
- **Segunda exceção de `unsafe`**: `crates/revenant-core/src/source/macos_fd.rs`, para receber o disco aberto pelo
  `authopen`; cada bloco com `// SAFETY:`, e o teste `unsafe_only_in_allowed_files` confere. O `authopen` é chamado sem
  `-w`, `-c`, `-a` nem `-x`: só leitura.

## Decisões do controlador (Claude, pelo Sérgio)

- **Tauri 2.11 e tauri-plugin-opener 2.5**: a linha mais nova que compila no Rust 1.88. A MSRV é do Sérgio, não sobe
  por causa de um Tauri. Custo: uma subida de versão quando a MSRV subir.
- **`deny.toml`**: só mira macOS e Windows (não entregamos nada para Linux) e `unmaintained = "workspace"` (crate sem
  manutenção só falha se for dependência direta nossa; vulnerabilidades continuam falhando em todo lugar);
  `Apache-2.0 WITH LLVM-exception` permitida. O `cargo-deny` não se instala neste Mac: **a CI é quem confirma**; empurrar a
  branch e ver verde antes de abrir o PR.
- **Cartão SD vence "interno"**: leitor SD embutido em notebook aparece como interno; a tela mostra como cartão.
  Custo: um disco interno ligado por SD aparecer como leitor de cartão (raro).
- **Mudanças nos mockups feitas na 2.1** (a regra é mudar o mockup primeiro): a abertura da tela 13 virou "Nada foi
  gravado no cartão." e a frase de explicação nos detalhes técnicos saiu; os botões **Ajustes** e **Sobre** entraram no
  canto superior direito do cabeçalho; a dica de "janela fechada" do `app.html` passou de "use o menu Revenant" para
  "use os botões no alto da janela, à direita". O mapa do `index.html` leva a tela 1 para a 2.1.
- **Modal próprio, sem Radix Dialog**: o Radix puxa `tslib` (0BSD), licença que não está na lista do Sérgio.

## Perguntas para o Sérgio

1. **0BSD.** Permitir 0BSD deixaria usar o Radix Dialog (foco preso, acessibilidade pronta) no lugar do Modal próprio.
2. **Nota `save_failed`** ("não deu para guardar a sua escolha; vale até fechar o app") não está em nenhum mockup. Entra
   num mockup ou sai?
3. **"Os arquivos que você já recuperou…"** aparece também em erros que acontecem antes de qualquer recuperação
   (por exemplo ao listar os discos), onde não faz sentido. Trocar o texto por código de erro?
4. **Posição dos botões Ajustes e Sobre** no cabeçalho (canto superior direito de toda tela): confirmar ou mover.
5. **O disco do sistema aparece pelo modelo**, não por "Macintosh HD". O nome do volume APFS mora num disco sintetizado, e
   chegar nele é trabalho no motor. Fica assim até lá, ou entra na 2.2?

## Pequenos itens adiados

Nada disso bloqueia o PR; entram quando o arquivo for tocado de novo.

- **Motor/guarda de `unsafe`** (`tests/source.rs`): o leitor de literal de caractere erra em `'\''` (buscar a partir de
  `i+3`); o caminho relativo vem depois do primeiro `src` do caminho absoluto (usar `strip_prefix`); a guarda não olha
  `tests/`, `benches/` nem `build.rs`; dois blocos `unsafe` a até 3 linhas podem dividir um `// SAFETY:`.
- **Windows**: `run_elevated` sem `CoInitializeEx`; janela do UAC sem janela-mãe (pode abrir atrás do app; passar o
  handle da janela); espera sem limite pelo processo elevado; o Thunderbolt nunca é detectado no Windows (a interface
  aceita "desconhecido"). Um comentário em `win_parse.rs` passa de 120 colunas.
- **Scripts do app**: `check-app` pode ser burlado por `?:`, `&&`, `+` ou constante, e tem falsos positivos com hex e
  comentário; `check-npm-licenses` trata pacote sem licença como bloqueio e é sensível a maiúsculas (falha para o lado
  seguro); `small-icons.mjs` sem proteção para entrada densa.
- **Interface**: sem setas do teclado na lista de discos; a nota "disco não encontrado" fica até outra ação; o valor do
  provider é recriado a cada render; depois de Voltar e Continuar com o pedido de senha ainda aberto, o botão da nova tela
  de permissão não faz nada até o pedido ser respondido (poderia aparecer ocupado ao montar).
- **App em Rust**: o número de bytes lidos no teste de acesso é ignorado; a última afirmação de `csp_is_strict` é
  redundante; `img-src asset:` só é necessário no macOS.
- **CI**: o `npm ci` no `windows-11-arm` nunca rodou (plano B: gerar `app/dist` só no x64).
