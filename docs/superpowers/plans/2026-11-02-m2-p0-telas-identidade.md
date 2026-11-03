# Módulo 2 · Parte 0 — Telas e identidade — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Entregar os arquivos de marca do Revenant (logo, ícone do app) e os mockups clicáveis, na horizontal, em claro e
escuro, das 13 telas da spec, para o Sérgio aprovar antes de qualquer código do app.

**Architecture:** Os arquivos de marca são SVGs escritos à mão em `assets/brand/`, com PNGs exportados pelo Chrome em
modo headless por um script. Os mockups ficam em `docs/superpowers/mockups/m2/`: cinco páginas HTML, uma por grupo de
telas, todas usando três arquivos comuns. O `tokens.css` guarda as cores; o `kit.css` e o `kit.js` guardam a moldura da
janela, a barra de simulação, os temas, o "Texto longo", os ícones Lucide e as fotos. Um conferidor em Node, sem
dependências, garante as regras da spec. Um script de capturas gera PNGs de cada tela e estado nos dois temas para a
revisão.

**Tech Stack:** HTML/CSS/JS puro (sem build, abre com `file://`), Node 24 (do `fnm`) para o conferidor, Google Chrome
headless (`/Applications/Google Chrome.app`) para exportar PNGs e tirar capturas, `sips` do macOS para conferir PNGs.

**Spec:** `docs/superpowers/specs/2026-11-02-m2-p0-telas-identidade-design.md` (a autoridade). Acima dela,
`docs/superpowers/specs/2026-10-19-revenant-design.md` §3 (o caminho), §4 (resultados), §6 (gostos herdados). Mockups
aprovados de referência, fora do git: `.superpowers/brainstorm/65046-1792436345/content/fluxo.html`,
`resultados-v2.html` e `logo-ab.html` (símbolos `ab1`/`ab1s`).

## Global Constraints

- **Idioma:** os textos de tela dos mockups e os docs em `docs/superpowers/**` são em pt-BR. Os identificadores, os
  comentários de código (`kit.js`, scripts), os nomes de arquivo e `assets/brand/README.md` são em inglês.
- **Cores só por tokens:** nenhuma cor literal (`#hex`, `rgb(`, `rgba(`, `hsl(`) fora de `tokens.css` e de
  `assets/brand/*.svg`. O escuro vale por `:root[data-theme="dark"]` e por
  `@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) }`.
- **Ícones:** só Lucide (licença ISC, permitida), como SVG embutido no `kit.js`; nunca emoji.
- **Alertas:** sempre ícone + texto, nunca só cor. Todo problema diz o que fazer em seguida. O tom é calmo.
- **Sem jargão na tela:** nada de setor, cluster, carving, escultura ou sessão, nem código do motor (`device_gone` etc.)
  fora de `<details class="tech">` ("Detalhes técnicos", recolhido) ou de atributos `data-code`.
- **Origem nunca como destino:** nenhuma tela lista o cartão de origem como destino, nem oferece "consertar o cartão".
- **Janela:** referência de 1280×800, mínimo de 1024×700, moldura do Mac. A do Windows só na tela 3.
- **Dourado** (`--brand-gold`): só no logo, no ícone e na tela Sobre, nunca como cor de estado.
- **Ordem do caminho:** retomar (ao abrir) → disco → permissão → conferência prévia → cópia opcional → procurando →
  resultados → onde salvar → recuperando/resumo.
- **Licenças:** fotos do Unsplash por link, com crédito "Fotos: Unsplash" no `index.html`. Ícones Lucide (ISC), com o
  aviso de licença no topo do `kit.js`.
- **Commits:** Conventional Commits, com o trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Nunca
  `git add -A`. O Homebrew não funciona neste Mac: nada de `brew`, nem de `npm install` (o conferidor não tem
  dependências).
- **Todo conferidor novo é visto falhando antes.**

## Review Focus

1. **Texto 30% mais longo (es/en):** com "Texto longo" ligado, nenhum botão quebra em duas linhas nem sai da janela, e
   nenhum título é cortado. As capturas com `long=1` de cada página entram na revisão de cada tarefa de página.
2. **Tema escuro desenhado:** cada estado legível no escuro (contraste AA), com os selos e os avisos visíveis. As
   capturas `theme=dark` entram na revisão de cada tarefa de página.
3. **Janela no mínimo de 1024×700:** a galeria e a tela de Detalhes não se sobrepõem nem cortam a barra de baixo. As
   capturas com `w=1024&h=700` entram na Tarefa 5 (resultados) e na 4 (procurando).
4. **Destino no próprio cartão:** a tela 9 e a conferência prévia (tela 4) mostram a recusa com o motivo, e o cartão
   nunca aparece como opção selecionável na lista de destinos. É conferido na Tarefa 6 pelo conferidor
   (`data-source-disk` nunca dentro de `[data-destination-option]`).
5. **Fotos sem internet:** a miniatura que não carrega mostra o fundo `--img-bg` com o selo e a etiqueta, sem ícone de
   imagem quebrada. Conferido no `kit.js` (Tarefa 2) com `onerror` que esconde o `<img>`.

---

## Estrutura de arquivos

```
assets/brand/
  README.md                 usage: colors, minimum sizes, when to use the small symbol
  symbol.svg                symbol, currentColor + var(--d) gold (≥ 48 px)
  symbol-small.svg          small geometry (32 and 16 px)
  app-icon.svg              macOS grid: 824 tile on a 1024 canvas, with a shadow
  app-icon-flat.svg         full-bleed tile, no shadow (Windows master)
  wordmark-light.svg        symbol + "Revenant", for light backgrounds
  wordmark-dark.svg         same, for dark backgrounds
  app-icon-1024.png         exported from app-icon.svg
  app-icon-flat-1024.png    exported from app-icon-flat.svg
  app-icon-32.png, app-icon-16.png   exported from the flat tile with the small symbol
scripts/
  export-brand.sh           exports the PNGs with headless Chrome
  check-mockups.mjs         enforces the spec's rules on the mockups
  shoot-mockups.sh          screenshots every screen × state × theme
docs/superpowers/mockups/m2/
  tokens.css  kit.css  kit.js
  index.html                map: screen → page → Part → engine codes; logo; credits
  inicio.html               screens 2, 1, 3, 4
  busca.html                screens 5, 6, 7
  resultados.html           screen 8
  recuperar.html            screens 9, 10
  app.html                  screens 11, 12, 13
```

---

### Tarefa 1: Arquivos de marca

**Files:**
- Create: `assets/brand/*` (lista acima), `scripts/export-brand.sh`

**Interfaces:**
- Produces: `assets/brand/symbol.svg` e `symbol-small.svg` (`viewBox="0 0 100 100"`, `fill="none"`,
  `stroke="currentColor"`, dourado como `style="stroke:var(--d, #b9822a)"`), que a Tarefa 2 embute no `kit.js` como
  `Kit.logo({ small?: boolean }): string`.

- [ ] **Step 1: Escrever o script de exportação e vê-lo falhar.**
  `scripts/export-brand.sh` (bash, `set -euo pipefail`) renderiza cada SVG pelo Chrome headless, com
  `--default-background-color=00000000` (fundo transparente), `--window-size=N,N` e `--screenshot=<png>`:
  `app-icon.svg` → `app-icon-1024.png` (1024); `app-icon-flat.svg` → `app-icon-flat-1024.png` (1024); um SVG
  temporário do ladrilho plano com o símbolo pequeno → `app-icon-32.png` e `app-icon-16.png`. No fim, confere cada PNG
  com `sips -g pixelWidth -g pixelHeight -g hasAlpha` e sai com erro se o tamanho ou o alfa não baterem.
  Run: `bash scripts/export-brand.sh` → Expected: FAIL (os SVGs ainda não existem).

- [ ] **Step 2: Escrever os SVGs** com a geometria e as cores da spec §3, copiadas literalmente.
  - Ladrilho: fundo `#2f5d50`; símbolo em `#f6f3ec`, dourado `#e9c27a`, ocupando 78% do lado do ladrilho.
  - `app-icon.svg`: canvas de 1024, ladrilho de 824×824 em (100,100) com `rx` 185, e
    `feDropShadow dy="10" stdDeviation="12" flood-opacity=".28"`.
  - `app-icon-flat.svg`: ladrilho de 1024×1024 com `rx` 230, sem sombra.
  - Wordmarks: símbolo de 56 px, "Revenant" em `font-family="-apple-system, 'SF Pro Display', 'Segoe UI', system-ui,
    sans-serif"`, `font-weight="650"`, `letter-spacing="-0.025em"`. No claro: tinta `#1d1b17`, símbolo `#2f5d50`,
    dourado `#b9822a`. No escuro: tinta `#f1eee6`, símbolo `#7fb7a4`, dourado `#e2b45e`. Fundo transparente.

- [ ] **Step 3: Rodar a exportação.** Run: `bash scripts/export-brand.sh` → Expected: os PNGs gerados, as conferências
  do `sips` OK, saída 0.

- [ ] **Step 4: Olhar.** Abrir `app-icon-1024.png`, `app-icon-flat-1024.png`, `app-icon-32.png` e `app-icon-16.png`
  (ferramenta Read de imagem). O desenho tem de bater com o `ab1`/`ab1s` do `logo-ab.html`, e o 16 px tem de ser
  legível.

- [ ] **Step 5: `assets/brand/README.md`** (em inglês): a tabela de cores da spec §3, os tamanhos mínimos (símbolo
  normal a partir de 48 px, pequeno em 32 e 16), "the gold is brand only, never a status color", como regenerar
  (`bash scripts/export-brand.sh`) e o aviso de que o wordmark usa a fonte do sistema.

- [ ] **Step 6: Commit** `feat(brand): logo, app icon and wordmarks` (adicionar `assets/brand` e
  `scripts/export-brand.sh` pelo nome).

### Tarefa 2: Base dos mockups (tokens, kit, conferidor, capturas, índice)

**Files:**
- Create: `docs/superpowers/mockups/m2/tokens.css`, `kit.css`, `kit.js`, `index.html`, `scripts/check-mockups.mjs`,
  `scripts/shoot-mockups.sh`

**Interfaces:**
- Consumes: `assets/brand/symbol.svg` e `symbol-small.svg` (Tarefa 1), embutidos no `kit.js`.
- Produces (o que as páginas usam; cada página faz `<link rel="stylesheet" href="tokens.css">`,
  `<link rel="stylesheet" href="kit.css">` e `<script src="kit.js"></script>`):
  - `Kit.init({ page: string, screens: Screen[] })`, com
    `Screen = { id: number, name: string, os?: 'mac'|'win', title: string,
    states: { key: string, label: string, render: () => string, after?: (root: HTMLElement) => void }[] }`.
    Monta a barra de simulação no topo da página e, para cada tela, um cabeçalho ("Tela N · nome"), botões de estado e
    uma janela de 1280×800 (escalada para caber na largura). O `render()` do estado escolhido preenche o corpo da
    janela; o `after(root)` liga os cliques.
  - Barra de simulação: tema (Claro / Escuro / Automático, que põe ou tira `data-theme` no `<html>`) e "Texto longo"
    (põe `data-long` no `<html>` e chama `Kit.lengthen` no corpo das janelas).
  - Parâmetros de URL para as capturas: `?screen=N&state=key&theme=light|dark&long=1&w=1024&h=700`. Com `screen`, a
    página mostra só aquela janela, sem barra e sem escala, com a janela do tamanho `w`×`h` (padrão 1280×800) colada no
    canto (0,0).
  - `Kit.icon(name: string): string`: um SVG Lucide com a classe `i`. Ícones: check, search, shield, alert, x, info,
    play, pause, cal, grid, chart, list, folder, dl, hdd, sd, usb, lock, refresh, settings, unplug, clock, image,
    video, chevron-right, external-link, copy, triangle-alert, circle-check, power.
  - `Kit.photo(i: number, w?: number): string`: a URL do Unsplash, com a mesma lista `IMG` do `resultados-v2.html`.
  - `Kit.thumb(opts): string`: miniatura com etiqueta, selo, marca de seleção e duração para vídeo, como no
    `resultados-v2.html`; o `<img>` tem `onerror="this.remove()"`.
  - `Kit.badge(state: 'ok'|'mid'|'bad'): string`: selo Íntegro/Parcial/Danificado, com ícone + texto.
  - `Kit.note(kind: 'ok'|'mid'|'bad'|'info', html: string): string`: aviso com ícone + texto.
  - `Kit.tech(code: string, html?: string): string`: `<details class="tech"><summary>Detalhes técnicos</summary>…`
    com o código.
  - `Kit.logo({ small?: boolean } = {}): string`: o símbolo.
  - `Kit.lengthen(root: HTMLElement)`: em `button, .btn, h1, h2, h3, h4, h5, label, .seg button, [data-t]`, acrescenta
    ao texto ~30% a mais de caracteres, repetindo o começo do próprio texto. Desligar o "Texto longo" refaz o render.
  - `Kit.nf(n)`, `Kit.sz(mb)`: números e tamanhos em pt-BR, como no `resultados-v2.html`.
  - Classes do `kit.css` que as páginas usam: `.btn`, `.btn.ghost`, `.btn.sm`, `.seg`, `.bd`, `.note`, `.th`, `.card`,
    `.list`, `.bar`, `.foot`, `.sp`, `.modal` (camada por cima da janela, com fundo escurecido), `.banner` (faixa de
    aviso no topo do conteúdo), `.progress` (barra de progresso). As medidas e o visual são os do `resultados-v2.html`.
  - Atributos que as páginas declaram e o conferidor lê: `data-screen="N"` (posto pelo `Kit.init` em cada janela,
    também na saída estática, veja o Step 1), `data-source-disk` (o cartão de origem) e
    `data-destination-option` (cada opção de destino).

- [ ] **Step 1: Escrever o conferidor e vê-lo falhar.** `scripts/check-mockups.mjs` (Node 24, só `node:fs` e
  `node:path`, sem dependências; sai com código 1 e lista cada falha). Sobre `docs/superpowers/mockups/m2/`:
  1. Cada página, menos o `index.html`, liga `tokens.css`, `kit.css` e `kit.js`, e chama `Kit.init(`.
  2. Nenhuma cor literal (`/#[0-9a-fA-F]{3,8}\b|rgba?\(|hsla?\(/`) em `*.html`, `kit.css` ou `kit.js`, fora do
     `tokens.css`. Exceção: SVGs de marca embutidos entre `/* brand:start */` e `/* brand:end */` no `kit.js`.
  3. Nenhum emoji (`/\p{Extended_Pictographic}/u`) em nenhum arquivo.
  4. Sem jargão: depois de remover os blocos `<details class="tech">…</details>`, as chamadas
     `Kit.tech(...)` e os atributos `data-code="…"`, nenhum `*.html` contém `/\b(setor|setores|cluster|clusters|carving|escultura|sessão|sessões)\b/i`
     nem algum dos códigos do motor:
     `device_gone permission_denied device_not_found system_disk open_failed device_not_responding sample_unreadable unmount_failed destination_too_small destination_on_source destination_unknown destination_file_too_large session_other_source session_corrupt write_failed read_failed`.
  5. As telas de 1 a 13 aparecem cada uma exatamente uma vez como `id: N` nos `Kit.init` das páginas, e cada uma está
     na tabela do `index.html` como `data-map="N"`, com um link para a sua página.
  6. `--brand-gold` só aparece no `tokens.css`, no `kit.js` (logo) e no `app.html` (Sobre).
  7. Nenhum `data-source-disk` dentro de um elemento com `data-destination-option`. É uma checagem textual: no mesmo
     elemento, ou entre a abertura de um `data-destination-option` e o fechamento do seu bloco no mesmo template
     literal.
  8. O `tokens.css` define cada token nos três lugares: `:root`, `:root[data-theme="dark"]` e o bloco
     `prefers-color-scheme: dark` com `:root:not([data-theme="light"])`.
  Run: `node scripts/check-mockups.mjs` → Expected: FAIL (pasta vazia: faltam as telas de 1 a 13, o `index.html` e o
  `tokens.css`).

- [ ] **Step 2: `tokens.css`.** Copiar os nomes e valores, no claro e no escuro, do `:root` do `resultados-v2.html`:
  `--paper --surface --surface-2 --line --line-soft --ink --ink-2 --ink-3 --accent --accent-soft --accent-ink --ok
  --ok-bg --warn --warn-bg --bad --bad-bg --img-bg --shadow`. Acrescentar:
  - `--brand-gold` (`#b9822a` no claro, `#e2b45e` no escuro);
  - `--tile` (`#2f5d50`), `--tile-ink` (`#f6f3ec`) e `--tile-gold` (`#e9c27a`), iguais nos dois temas;
  - `--overlay` (camada do modal: `rgba(20,18,14,.45)` no claro, `rgba(0,0,0,.55)` no escuro);
  - `--photo-tag` (`rgba(20,18,14,.62)`) e `--on-photo` (`#fff`), iguais nos dois temas;
  - `--info` e `--info-bg` (o azul calmo dos avisos neutros: `#2f5a7a`/`#e1ebf3` no claro, `#8fbcd9`/`#1e2e3a` no
    escuro);
  - `--win-close` (`#ff5f57`), `--win-min` (`#febc2e`) e `--win-max` (`#28c840`): os três botões da janela do Mac;
  - `--font: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", system-ui, sans-serif`.

- [ ] **Step 3: `kit.css` e `kit.js`** com a interface acima. As medidas e os estilos dos componentes vêm do
  `resultados-v2.html`, com cada cor trocada pelo seu token. No topo do `kit.js`, o aviso da licença ISC do Lucide.

- [ ] **Step 4: `index.html`.** Título "Revenant · telas da 2.0"; o logo (`Kit.logo`) e os dois wordmarks (os SVGs de
  `assets/brand` por caminho relativo, `../../../../assets/brand/wordmark-light.svg`); a tabela da spec §4 com uma
  linha por tela (`data-map="N"`), com as colunas Tela, Estados, Códigos do motor, Parte e Página (link); a ordem do
  caminho; "Fotos: Unsplash". O `index.html` usa o `tokens.css` e o `kit.css`, mas não chama `Kit.init`.

- [ ] **Step 5: `scripts/shoot-mockups.sh`.** Lê os pares tela/estado das páginas: cada página expõe
  `window.KIT_MANIFEST` ao carregar, e o script obtém o manifesto com o Chrome headless e `--dump-dom` de
  `?manifest=1`, que imprime o JSON num `<pre id="manifest">`. Para cada tela × estado × tema (light, dark), tira
  `--screenshot` de `?screen=N&state=k&theme=t` em `--window-size=1280,800`. Também aceita `--long` (acrescenta
  `long=1`) e `--min` (acrescenta `w=1024&h=700` e a janela de 1024×700). Os PNGs saem em
  `${OUT:-/tmp/revenant-shots}/<page>-<N>-<state>-<theme>[-long|-min].png`, com o `OUT` vindo do ambiente.
  Run: `OUT=<scratch> bash scripts/shoot-mockups.sh` → Expected: com só o índice, sai 0 e não tira nenhuma captura.

- [ ] **Step 6: Rodar o conferidor.** Expected: FAIL só na regra 5 (faltam as telas de 1 a 13, que vêm das Tarefas 3
  a 7). As outras regras passam.

- [ ] **Step 7: Commit** `docs(mockups): Part 2.0 mockup kit, tokens, checker and index` (adicionar os arquivos pelo
  nome).

### Tarefa 3: `inicio.html` — telas 2, 1, 3 e 4

**Files:** Create: `docs/superpowers/mockups/m2/inicio.html`
**Interfaces:** Consumes: o `Kit` (Tarefa 2). Base visual: as telas "O que você quer recuperar?" e "Só falta uma
permissão do Mac" do `fluxo.html`, levadas para a horizontal.

- [ ] **Step 1: Ver o conferidor falhar para as telas 1 a 4.** Run: `node scripts/check-mockups.mjs` → Expected: falha
  da regra 5 listando 1, 2, 3 e 4.
- [ ] **Step 2: Escrever a página** com `Kit.init({ page: 'inicio', screens: [...] })` nesta ordem:
  - **Tela 2, "Retomar a busca"** (`modal` sobre a tela 1). Estados:
    - `parou`: "A busca no Cartão SD 64 GB parou em 42%." com "Continuar" e "Começar de novo".
    - `outro`: "Este não é o cartão da busca que parou." (`session_other_source` no `Kit.tech`), com "Procurar neste
      cartão do começo" e "Cancelar".
    - `ilegivel`: o registro da busca salva não pôde ser lido (`session_corrupt`), com "Começar de novo".
  - **Tela 1, "Escolher o disco".** Estados:
    - `lista`: um cartão SD de 64 GB (`data-source-disk` quando escolhido), um HD externo de 2 TB e um SSD externo T7
      de 1 TB, com o aviso de cautela e "Saiba por quê". O disco do sistema aparece apagado, com "Disco do sistema:
      não dá para procurar nele" (`system_disk` no `data-code`). A dica fixa "Pare de usar o cartão agora. Não grave
      nada nele." e "Continuar" no rodapé.
    - `vazio`: "Conecte o cartão ou o disco", com a ilustração de um ícone `sd` grande e a dica.
    - `ssd`: o SSD selecionado, com o aviso de cautela aberto.
  - **Tela 3, "Permissão".** Estados:
    - `mac`: a explicação em palavras simples, com "Pedir a senha de administrador".
    - `mac-pedido`: a mesma tela com o diálogo de senha do sistema por cima, como um desenho do diálogo do macOS.
    - `win` (`os: 'win'`, moldura do Windows): a explicação e "Continuar"; ao clicar, o diálogo do UAC desenhado por
      cima.
    - `recusada`: "Sem a permissão não dá para ler o cartão", com "Tentar de novo" (`permission_denied` no
      `Kit.tech`).
  - **Tela 4, "Conferência prévia".** Estados:
    - `conferindo`: "Conferindo o cartão…" com o progresso indeterminado.
    - `ok`: um ícone de check e "Tudo certo", que segue sozinho.
    - `problemas`: uma lista só com dois itens ao mesmo tempo. O primeiro é "O cartão não respondeu a tempo"
      (`device_not_responding`), com "Tire e coloque o cartão de novo e confira o leitor". O segundo é "A pasta escolhida
      fica no próprio cartão" (`destination_on_source`), com "Escolha uma pasta em outro disco". Cada item tem ícone +
      texto + o que fazer + `Kit.tech`. Rodapé com "Conferir de novo".
    - `todos`: a mesma lista com um item por código da linha 4 da tabela da spec, para revisar os textos.
- [ ] **Step 3: Rodar o conferidor.** Expected: as telas 1 a 4 não aparecem mais na falha; nenhuma outra regra falha.
- [ ] **Step 4: Capturas e olhada.** Run: `OUT=<scratch>/shots bash scripts/shoot-mockups.sh inicio`, depois com
  `--long`. Olhar cada PNG (Read de imagem), no claro e no escuro. Corrigir quebras, cortes e contraste.
- [ ] **Step 5: Commit** `docs(mockups): start screens, resume, disk, permission, pre-check`.

### Tarefa 4: `busca.html` — telas 5, 6 e 7

**Files:** Create: `docs/superpowers/mockups/m2/busca.html`
**Interfaces:** Consumes: o `Kit`. Base visual: "Primeiro, uma cópia de segurança?" e "Procurando…" do `fluxo.html`.

- [ ] **Step 1: Ver o conferidor falhar para as telas 5 a 7.**
- [ ] **Step 2: Escrever a página.**
  - **Tela 5, "Cópia de segurança".** Estados:
    - `escolha`: dois cartões grandes, "Copiar o cartão primeiro (mais seguro)" e "Procurar direto (mais rápido)".
    - `recomendada`: a mesma escolha com "Recomendado: a leitura deu erro" no primeiro cartão.
    - `destino`: onde salvar a cópia. As opções têm `data-destination-option`; o cartão de origem aparece fora da lista,
      com "A cópia não pode ficar no próprio cartão".
    - `copiando`: a passada ("1ª leitura, a rápida" / "Voltando nos trechos difíceis"), a barra, a velocidade
      ("38 MB/s"), o tempo restante, "41,2 de 64 GB salvos" e "3 trechos não puderam ser lidos até agora".
    - `pronta`: "Cópia pronta", com o resumo e "Procurar na cópia".
  - **Tela 6, "Procurando".** Na horizontal, a galeria ocupa o centro e se enche durante a busca. Por cima ficam a
    barra de progresso, o tempo restante e os contadores (fotos, vídeos). Dá para marcar as miniaturas. No rodapé,
    "Pausar" e "Recuperar N arquivos". Estados:
    - `andando`: 38%.
    - `pausada`.
    - `falhando`: a `.banner` `bad` "O disco está falhando: faça uma cópia agora antes de continuar", com o botão
      "Fazer a cópia", "Continuar mesmo assim" e o `data-code` do `DiskFailing`.
    - `lento`: um aviso discreto `info`, "O cartão está lento. A busca continua.".
    - `fechar`: o modal "Fechar o Revenant? A busca fica salva e continua quando você abrir de novo.".
  - **Tela 7, "O cartão saiu"** (`modal` sobre a tela 6 ou 5). Estados:
    - `busca`: "O cartão foi desconectado. Nada se perdeu: conecte de novo para continuar.", com um indicador de
      espera e `device_gone` no `Kit.tech`.
    - `copia`: o mesmo, sobre a cópia.
    - `voltou`: "Cartão de volta. Continuando…".
- [ ] **Step 3: Rodar o conferidor.** Expected: as telas 5 a 7 resolvidas; nenhuma outra regra falha.
- [ ] **Step 4: Capturas e olhada**, também com `--min` (1024×700) na tela 6.
- [ ] **Step 5: Commit** `docs(mockups): backup copy, searching, card pulled out`.

### Tarefa 5: `resultados.html` — tela 8

**Files:** Create: `docs/superpowers/mockups/m2/resultados.html`
**Interfaces:** Consumes: o `Kit`. Base: o `resultados-v2.html` inteiro (dados, três visualizações, seleção por grupo,
"Por pasta" desligado, rodapé), portado para o `Kit`, sem mudar o conteúdo.

- [ ] **Step 1: Ver o conferidor falhar para a tela 8.**
- [ ] **Step 2: Portar e completar.** Estados:
  - `galeria`, `linha`, `detalhes`: os três do `resultados-v2.html`, com a mesma interação.
  - `formatado`: a simulação "cartão formatado", com "Por pasta" desligado e o motivo.
  - `vazio`: "Nenhuma foto ou vídeo encontrado", com o que fazer: tentar a cópia e procurar nela, ou ver o guia.
  - `sem-nome`: Detalhes com um arquivo sem o nome original. O nome por data e câmera
    (`2025-12-14_07-12-05_A7IV.ARW`) vem com a marca "nome criado pelo Revenant", e a prévia fica `Parcial` com o
    aviso dela.
  As cores literais do `resultados-v2.html` viram tokens.
- [ ] **Step 3: Rodar o conferidor.**
- [ ] **Step 4: Capturas e olhada**, também com `--min` e com `--long`.
- [ ] **Step 5: Commit** `docs(mockups): results, gallery, timeline and details`.

### Tarefa 6: `recuperar.html` — telas 9 e 10

**Files:** Create: `docs/superpowers/mockups/m2/recuperar.html`
**Interfaces:** Consumes: o `Kit`. Base: "Onde salvar?" e "1.284 arquivos recuperados" do `fluxo.html`.

- [ ] **Step 1: Ver o conferidor falhar para as telas 9 e 10.**
- [ ] **Step 2: Escrever a página.**
  - **Tela 9, "Onde salvar".** Estados:
    - `escolha`: as pastas e os discos com `data-destination-option`, mais "Escolher outra pasta…". O resumo da
      seleção ("24 arquivos, 3,1 GB") e o espaço livre de cada disco.
    - `no-cartao`: a recusa "Não dá para salvar no próprio cartão: gravar nele pode apagar fotos que ainda não voltaram.
      Escolha outro disco." O cartão aparece fora da lista, marcado `data-source-disk` (`destination_on_source` no
      `data-code`).
    - `pouco-espaco`: "Faltam 1,2 GB nesse disco. Escolha outro ou recupere menos arquivos."
  - **Tela 10, "Recuperando e resumo".** Estados:
    - `recuperando`: o progresso, "812 de 1.284", e o tempo restante.
    - `fim`: "1.284 arquivos recuperados", com o resumo (íntegros, parciais e danificados, cada um com o selo), "Abrir
      pasta" e o que significa Parcial.
    - `erro-gravar`: o disco de destino ficou cheio ou saiu; nada foi perdido e dá para continuar
      (`write_failed` no `Kit.tech`).
    - `cartao-saiu`: o cartão saiu no meio da recuperação (`device_gone`): os arquivos já salvos ficam, e dá para
      continuar quando o cartão voltar.
- [ ] **Step 3: Rodar o conferidor.** A regra 7 tem de passar.
- [ ] **Step 4: Capturas e olhada.**
- [ ] **Step 5: Commit** `docs(mockups): where to save, recovering and summary`.

### Tarefa 7: `app.html` — telas 11, 12 e 13

**Files:** Create: `docs/superpowers/mockups/m2/app.html`
**Interfaces:** Consumes: o `Kit`.

- [ ] **Step 1: Ver o conferidor falhar para as telas 11 a 13.**
- [ ] **Step 2: Escrever a página.**
  - **Tela 11, "Ajustes"** (`modal` ou painel). Estado `geral`: Tema, com Claro, Escuro e Automático, que muda o tema
    da página de verdade; Idioma, com Português (Brasil), Español e English.
  - **Tela 12, "Sobre".** Estado `sobre`: o logo grande com o dourado, "Revenant", "Versão 0.9.0-beta", "Gratuito e de
    código aberto (GPL-3.0)" e os links "Código-fonte" e "Guia de uso".
  - **Tela 13, "Erro inesperado".** Estados:
    - `erro`: "Algo deu errado ao ler o cartão. Nada foi gravado nele." com o que fazer ("Tente de novo. Se continuar,
      faça uma cópia do cartão primeiro") e `Kit.tech('read_failed', …)`.
    - `aberto`: o mesmo com "Detalhes técnicos" aberto e um botão "Copiar detalhes".
- [ ] **Step 3: Rodar o conferidor.** Expected: PASS, todas as regras.
- [ ] **Step 4: Capturas e olhada.**
- [ ] **Step 5: Commit** `docs(mockups): settings, about and unexpected error`.

### Tarefa 8: Fechamento

**Files:** Modify: `docs/superpowers/roadmap.md`, `CLAUDE.md` ("Onde o desenvolvimento está"), `README.md` (raiz, uma
linha que aponta para `assets/brand/`, se o README tiver uma seção de projeto ou de contribuição; se não tiver, nada).

- [ ] **Step 1: Conferidor e capturas de tudo.** Run: `node scripts/check-mockups.mjs` → PASS. Depois
  `bash scripts/shoot-mockups.sh` com e sem `--long`: sai 0.
- [ ] **Step 2: Roteiro.** A 2.0 vira "concluída (aguardando aprovação/PR)", com o caminho dos mockups e de
  `assets/brand/`. Cada linha da 2.1, da 2.2 e da 2.3 aponta para as suas páginas de mockup.
- [ ] **Step 3: `CLAUDE.md`.** Atualizar "Onde o desenvolvimento está". Acrescentar uma linha às diretrizes: "Telas do
  app: os mockups em `docs/superpowers/mockups/m2/` são a referência; mudar uma tela começa por mudar o mockup".
- [ ] **Step 4: Commit** `docs: Part 2.0 roadmap and project notes`.
