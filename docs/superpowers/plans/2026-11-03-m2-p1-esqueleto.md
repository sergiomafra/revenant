# Parte 2.1: esqueleto do app — plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** o `Revenant.app` abre com dois cliques, mostra os discos conectados de verdade, pede a permissão do sistema
ao escolher um, prova que consegue ler, e tem Ajustes (tema, idioma), Sobre e a tela de erro, em pt-BR, es e en.

**Architecture:** o motor (`revenant-core`) ganha a ligação de cada disco (USB, SD…), a dica de cartão de câmera e uma
abertura "com o pedido do sistema" (`authopen` no Mac, um relançamento elevado pelo UAC no Windows). Um crate Tauri 2
(`crates/revenant-app`) expõe comandos finos sobre o motor, testados com um `DeviceOps` falso. A interface React em
`app/` copia os tokens e o Kit dos mockups da 2.0 em componentes, com catálogos de idioma e uma varredura de regras.

**Tech Stack:** Rust 2024 (MSRV 1.88), Tauri 2, React 19, Vite, TypeScript, Tailwind 4, Radix, react-i18next,
lucide-react, Vitest + Testing Library.

**Spec:** `docs/superpowers/specs/2026-11-03-m2-p1-esqueleto-design.md`. Telas: `docs/superpowers/mockups/m2/`
(`inicio.html` telas 1 e 3; `app.html` telas 11, 12 e 13), com `tokens.css`, `kit.css` e `kit.js`.

## Global Constraints

- Nunca escrever no disco escolhido: nada de escrita, nada de desmontar nesta parte; o `authopen` sem `-w`, `-c`, `-a`
  nem `-x`; no Windows, só leitura (a exceção de `FSCTL_LOCK_VOLUME` não é usada aqui).
- `unsafe` só em `crates/revenant-core/src/source/windows.rs` e no novo `crates/revenant-core/src/source/macos_fd.rs`,
  cada bloco com `// SAFETY:`. Nenhum `unsafe` no app.
- Licenças do que vai dentro do app: MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense,
  CC0-1.0, BSL-1.0 e MPL-2.0 (sem modificação). Ferramentas só de montagem (devDependencies do npm): qualquer licença
  aberta menos GPL e AGPL. GPL/AGPL/LGPL proibidas em tudo o que vai dentro.
- Erros: o motor devolve `code()` em `snake_case`; o app manda à interface só `{ code }`; a interface traduz. Nenhum texto
  de tela no Rust.
- Textos: todo texto visível vem dos catálogos `app/src/i18n/{pt-BR,es,en}.json`, com as mesmas chaves nos três. Os
  textos pt-BR são os dos mockups. Sem emoji, ícones Lucide, cor só por tokens, sem jargão (setor, cluster, FAT, sessão)
  fora de "Detalhes técnicos".
- Idioma inicial: o do sistema (`navigator.language`); `pt*` → `pt-BR`, `es*` → `es`, `en*` → `en`, outro → `en`. Tema
  inicial: automático.
- Janela: 1280×800, mínima 1024×700, título "Revenant".
- Testes nunca tocam disco físico; todo teste novo é visto falhando antes. Teste não mede relógio.
- Código, comentários e commits em inglês; Conventional Commits com `Co-Authored-By: Claude Opus 5.5
  <noreply@anthropic.com>`; nunca `git add -A`. Nunca `brew`. `npm install` só em `app/` e só de pacotes com as licenças
  acima.
- Código só de macOS atrás de `#[cfg(target_os = "macos")]`; antes de cada commit em Rust, `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings -W clippy::incompatible_msrv` e `cargo test --workspace`.
- Se uma dependência do Tauri exigir Rust acima de 1.88, parar e relatar (não subir a MSRV).

## Review Focus

1. O cartão sai enquanto o pedido de senha está aberto: ao voltar da senha, a tela volta à lista com o aviso de disco
   não encontrado, sem tela de erro genérica. (Task 6: `request_access_disk_gone_after_prompt`; Task 7: estado na tela.)
2. A senha é cancelada e depois aceita: "Tentar de novo" pede de novo e funciona; nenhum estado preso. (Task 7:
   `retry_after_denied_asks_again`.)
3. A lista falha uma vez no meio da atualização (o `diskutil` engasga): a tela mantém a última lista e tenta de novo;
   só uma falha na primeira carga mostra o erro. (Task 7: `poll_failure_keeps_last_list`.)
4. Disco sem rótulo, sem marca ou sem modelo: o nome cai para marca + modelo, depois para "Disco" + capacidade, nunca
   vazio. (Task 6: `display_name_falls_back`.)
5. Textos es/en mais longos na janela mínima 1024×700: nada corta nem sobrepõe. (Task 7 e Task 5: capturas na janela
   mínima nos três idiomas.)

---

### Task 1: Motor — por onde o disco está ligado e a dica de cartão de câmera

**Files:**
- Modify: `crates/revenant-core/src/device/mod.rs` (novo campo, novo enum, nova função)
- Modify: `crates/revenant-core/src/device/macos_parse.rs` (ler `BusProtocol`), `crates/revenant-core/src/device/win_parse.rs`
  e `crates/revenant-core/src/source/windows.rs` (o `BusType` do `STORAGE_DEVICE_DESCRIPTOR`, se ainda não é lido)
- Modify: `crates/revenant-dev/src/devices.rs` (campo `connection` na linha `disk`), `crates/revenant-dev/README.md`,
  `crates/revenant-core/README.md`
- Test: os módulos de teste de `macos_parse.rs` e `win_parse.rs`; `crates/revenant-core/tests/camera_card.rs`

**Interfaces:**
- Produces:
  - `pub enum Connection { Usb, SdCard, Thunderbolt, Internal, Unknown }` em `revenant_core::device`, com
    `Connection::code(&self) -> &'static str` → `"usb"`, `"sd_card"`, `"thunderbolt"`, `"internal"`, `"unknown"`.
  - `DiskInfo.connection: Connection` (campo novo; todo construtor de `DiskInfo` no código e nos testes ganha o campo).
  - `pub fn looks_like_camera_card(disk: &DiskInfo) -> bool` em `revenant_core::device`: verdadeiro quando algum volume
    tem `mount_point` com uma pasta `DCIM` (qualquer caixa: `DCIM`, `dcim`) na raiz. Só `read_dir`/`metadata` na pasta
    montada; nunca abre o disco.

- [ ] **Step 1: Testes que falham.**
  - `macos_parse`: `connection_from_bus_protocol` — `BusProtocol` `"USB"` → `Usb`; `"Secure Digital"` → `SdCard`;
    `"Thunderbolt"` → `Thunderbolt`; `"PCI-Express"`/`"SATA"`/`"Apple Fabric"` com `Internal` verdadeiro →
    `Internal`; ausente ou desconhecido e não interno → `Unknown`. `Internal` verdadeiro vence qualquer protocolo.
  - `win_parse`: `connection_from_bus_type` — `BusTypeUsb` (7) → `Usb`; `BusTypeSd` (12) e `BusTypeMmc` (13) →
    `SdCard`; um disco não removível em `BusTypeSata` (11) ou `BusTypeNvme` (17) que o parser já marca como interno →
    `Internal`; outro → `Unknown`.
  - `tests/camera_card.rs` (com `tempfile`): `DiskInfo` com um volume montado num tempdir que tem `DCIM/` → `true`;
    `dcim/` → `true`; sem a pasta → `false`; um arquivo comum chamado `DCIM` → `false`; volume sem `mount_point` →
    `false`; `mount_point` que não existe → `false`.
- [ ] **Step 2:** `cargo test -p revenant-core connection camera_card` → FAIL (não existe).
- [ ] **Step 3:** implementar. O `disks` do `revenant-dev` ganha `"connection":"usb"` (o `code()`), sempre presente.
- [ ] **Step 4:** `cargo test --workspace` → PASS; fmt e clippy limpos.
- [ ] **Step 5:** READMEs (`revenant-core`: o campo e a função; `revenant-dev`: a linha `disk` do exemplo com
  `connection`). Commit `feat(core): tell how a disk is connected and whether it looks like a camera card`.

### Task 2: Motor — abrir um disco com o pedido de permissão do sistema

**Files:**
- Create: `crates/revenant-core/src/source/macos_fd.rs` (único `unsafe` do Mac: receber um descritor por `SCM_RIGHTS`)
- Create: `crates/revenant-core/src/source/elevated.rs` (o caminho comum, sem `unsafe`: `authopen` no Mac; o
  relançamento elevado no Windows, cujas chamadas de sistema ficam em `source/windows.rs`)
- Modify: `crates/revenant-core/src/source/device.rs` (o `OsDevice` do Mac passa a poder nascer de um `File` já aberto,
  com a mesma conferência de geometria e de mídia de `os::open`), `crates/revenant-core/src/source/mod.rs`,
  `crates/revenant-core/src/source/windows.rs`, `crates/revenant-core/Cargo.toml` (`libc` só no alvo macOS; features
  do `windows-sys` que faltarem: `Win32_UI_Shell`, `Win32_System_Threading`)
- Modify: `crates/revenant-core/tests/source.rs` (guarda de `unsafe`), `CLAUDE.md` (a segunda exceção de `unsafe`),
  `crates/revenant-core/README.md`
- Test: módulos de teste de `macos_fd.rs` e `elevated.rs`; `crates/revenant-core/tests/source.rs`

**Interfaces:**
- Produces (em `revenant_core::source`):
  - `pub fn open_with_system_prompt(disk: &DiskId) -> Result<Box<dyn RawDevice>, EngineError>` — bloqueia até o
    usuário responder. Erros: `permission_denied` (cancelou ou recusou), `device_not_found` (o disco sumiu ou mudou),
    `system_tool_failed` (o `authopen` não pôde rodar), `open_failed`, `unsupported_platform` (outros sistemas).
  - `pub fn elevated_entry(args: &[std::ffi::OsString]) -> Option<i32>` — no Windows, quando `args[1]` é
    `--revenant-elevated-open`, faz o trabalho do processo elevado e devolve o código de saída; senão `None`. No Mac e
    em outros sistemas, sempre `None`. O `main` do app chama isso antes de tudo (Task 4).
- **Mac:** roda `/usr/libexec/authopen` com os argumentos de `authopen_args(disk)` (`["-stdoutpipe", "/dev/rdiskN"]`,
  função pura) e o stdout ligado a uma ponta de `UnixStream::pair()`; recebe o descritor pela outra ponta
  (`macos_fd::recv_fd(&UnixStream) -> std::io::Result<OwnedFd>`); saída não zero ou nenhum descritor →
  `PermissionDenied`; não conseguiu iniciar → `SystemTool`. O `File` vira o `OsDevice` com geometria conferida.
- **Windows:** `ShellExecuteExW` com o verbo `runas` sobre `std::env::current_exe()`, argumentos
  `--revenant-elevated-open <PhysicalDriveN> <pid do app>`, e espera o processo. O elevado abre o disco só leitura (o
  `os::open` de hoje), duplica a alça para o processo do app (`DuplicateHandle`, `DUPLICATE_SAME_ACCESS`) e devolve o
  valor da alça como código de saída. Alças do Windows cabem em 32 bits e são múltiplas de 4, então os erros usam
  códigos ímpares fixos: `ELEVATED_DENIED = 1`, `ELEVATED_NOT_FOUND = 3`, `ELEVATED_OPEN_FAILED = 5`,
  `ELEVATED_BAD_ARGS = 7`. O app pega a alça (já dele) e monta o `OsDevice`. UAC cancelado (`ERROR_CANCELLED`) →
  `PermissionDenied`. As funções puras `encode_exit(Result<u32, EngineError>) -> i32`, `decode_exit(i32) ->
  Result<u32, EngineError>` e `parse_elevated_args(&[OsString]) -> Option<(DiskId, u32)>` ficam em `elevated.rs` e são
  testadas em todo sistema. O construtor pode trocar o canal (por exemplo, um pipe nomeado) se provar que o código de
  saída não serve, e registrar no relatório.

- [ ] **Step 1: Testes que falham.**
  - `authopen_args_are_read_only`: `authopen_args(&DiskId::parse("disk4")?)` == `["-stdoutpipe", "/dev/rdisk4"]`, e
    nenhum argumento é `-w`, `-c`, `-a`, `-x` nem começa por `-o`.
  - `macos_fd` (só macOS): `recv_fd_receives_a_sent_file` — o teste manda o descritor de um arquivo temporário com
    conteúdo `b"revenant"` por um `UnixStream::pair()` (o envio, também `unsafe`, fica no módulo de teste do mesmo
    arquivo) e lê `b"revenant"` do descritor recebido; `recv_fd_without_descriptor_fails` — só bytes, sem descritor →
    `Err`; `recv_fd_on_closed_stream_fails` → `Err`.
  - `elevated_exit_codes_round_trip`: `decode_exit(encode_exit(Ok(0x1A4)))` == `Ok(0x1A4)`; cada erro
    (`PermissionDenied`, `DeviceNotFound`, um `Open`) volta com o mesmo `code()`; `decode_exit(9)` (ímpar desconhecido)
    → `open_failed`; `decode_exit(0)` → `open_failed`.
  - `elevated_args_parse`: `["app.exe", "--revenant-elevated-open", "PhysicalDrive2", "4242"]` →
    `Some((PhysicalDrive2, 4242))`; `"disk4s1"`, pid ausente, pid não numérico, flag ausente → `None`;
    `elevated_entry` com argumentos comuns → `None` em todo sistema.
  - `tests/source.rs`: `unsafe_only_in_allowed_files` — varre `crates/*/src/**/*.rs` (fora comentários `//`, `///`,
    `//!`) procurando a palavra `unsafe`; só `source/windows.rs` e `source/macos_fd.rs` podem ter; em cada um, cada
    `unsafe {` tem `// SAFETY:` numa das 3 linhas acima. Ver falhando com um arquivo temporário de exemplo (como o
    `write_path_check_finds_misplaced_write_access`), e incluir `macos_fd.rs` e `elevated.rs` no
    `source_has_no_write_path` (já entram por estarem em `source/`).
- [ ] **Step 2:** `cargo test -p revenant-core authopen elevated recv_fd unsafe_only` → FAIL.
- [ ] **Step 3:** implementar. O `macos_fd.rs` usa `libc::recvmsg` com `CMSG_*`, aceita exatamente um descritor,
  fecha extras recebidos. O `OsDevice` montado de um `File` passa pela mesma conferência de `os::open` (geometria do
  `diskutil` e `confirm`), para o disco trocado no meio do pedido sair `device_not_found`.
- [ ] **Step 4:** `cargo test --workspace` → PASS; `grep -rn "os::unix" crates` com cada uso protegido; fmt e clippy.
- [ ] **Step 5:** `CLAUDE.md` (diretriz do `unsafe`: "Sem `unsafe` fora de `source/windows.rs` e `source/macos_fd.rs`
  (Sérgio, 2.1: receber o disco aberto pelo `authopen`)"), README do core (abrir com o pedido do sistema, como
  funciona em cada sistema). Commit `feat(core): open a disk through the system's permission prompt`.

### Task 3: Licenças — MPL-2.0 no que vai junto, ferramentas de montagem livres

**Files:**
- Modify: `deny.toml` (MPL-2.0 na lista, com comentário apontando a spec e a decisão do Sérgio de 03/11/2026)
- Create: `scripts/check-npm-licenses.mjs` (Node 24, sem dependências)
- Create: `scripts/check-npm-licenses.test.mjs` (`node --test`)
- Modify: `CLAUDE.md` (diretriz de licenças), `README.md` (seção Contributing)

**Interfaces:**
- Produces: `node scripts/check-npm-licenses.mjs <pasta com package-lock.json>` — exit 0 ou 1 com a lista do que
  falhou. Lê o `package-lock.json` (v3): pacote sem `"dev": true` → licença na lista de dentro do app (as do Global
  Constraints, incluindo MPL-2.0); com `"dev": true` → qualquer uma menos as que contêm `GPL` ou `AGPL` (LGPL também
  sai: ela contém `GPL`). Expressões SPDX: `A OR B` passa se uma das partes passa; `A AND B` só se as duas passam;
  licença ausente → falha. Exporta `checkLock(lock) -> string[]` para o teste.

- [ ] **Step 1: Teste que falha** (`check-npm-licenses.test.mjs`, com locks sintéticos): MIT em dependência → ok;
  MPL-2.0 em dependência → ok; GPL-3.0 em dependência → falha; LGPL-2.1 em dev → falha; CC-BY-4.0 em dev → ok;
  CC-BY-4.0 em dependência → falha; `(MIT OR GPL-3.0)` em dependência → ok; `(MIT AND GPL-3.0)` → falha; sem licença →
  falha.
- [ ] **Step 2:** `node --test scripts/check-npm-licenses.test.mjs` → FAIL.
- [ ] **Step 3:** implementar o script; `deny.toml` com MPL-2.0.
- [ ] **Step 4:** teste → PASS; `cargo deny` não roda aqui (sem binário: conferir `which cargo-deny` e
  `~/.cargo/bin`; se não houver, o CI confere).
- [ ] **Step 5:** `CLAUDE.md` (diretriz: "Licenças do que vai dentro do app: … mais MPL-2.0 sem modificação (Sérgio,
  2.1). Ferramentas só de montagem: qualquer licença aberta, menos GPL e AGPL. Proibido GPL/AGPL/LGPL de terceiros no
  que vai dentro e qualquer código copiado do PhotoRec/TestDisk."). Commit `build: allow MPL-2.0 and check npm
  licenses`.

### Task 4: O projeto do app — Tauri 2, React, a janela e o CI

**Files:**
- Create: `app/` (Vite + React 19 + TypeScript + Tailwind 4 + Radix + react-i18next + lucide-react; Vitest + Testing
  Library + jsdom), com `package.json`, `package-lock.json`, `vite.config.ts`, `tsconfig.json`, `index.html`,
  `src/main.tsx`, `src/App.tsx`, `src/styles/tokens.css` (cópia de `docs/superpowers/mockups/m2/tokens.css`, mesmos
  nomes e valores), `src/styles/app.css`
- Create: `crates/revenant-app/` (`Cargo.toml`, `build.rs`, `tauri.conf.json`, `capabilities/default.json`,
  `src/main.rs`, `src/lib.rs`, `icons/`), membro do workspace pelo `crates/*`
- Modify: `.github/workflows/ci.yml`, `.gitignore` (`app/node_modules`, `app/dist`), `crates/revenant-app/README.md`
  (novo)

**Interfaces:**
- Produces:
  - Scripts do `app/package.json`: `dev`, `build` (`tsc -b && vite build` → `app/dist`), `test` (`vitest run`),
    `check` (Task 5), `tauri` (o `@tauri-apps/cli` como devDependency; nada instalado globalmente).
  - `tauri.conf.json`: `productName` "Revenant", `identifier` "com.mafraflow.revenant", `frontendDist`
    `"../../app/dist"`, `devUrl` `http://localhost:5173`, `beforeDevCommand`/`beforeBuildCommand` rodando em `app/`,
    janela 1280×800, `minWidth` 1024, `minHeight` 700, título "Revenant", `bundle.targets` `["app"]` no Mac (sem
    `.dmg` nesta parte), versão lida do workspace (`0.1.0`).
  - `src/main.rs`: `if let Some(code) = revenant_core::source::elevated_entry(&std::env::args_os().collect::<Vec<_>>())
    { std::process::exit(code) }` antes de `revenant_app::run()`.
  - Ícones gerados por `npm run tauri icon ../assets/brand/app-icon-1024.png` (dentro de `app/`, saída em
    `crates/revenant-app/icons/`); depois os `.png`/`.ico` do Windows de `app-icon-flat-1024.png`, e os 16/32 px
    trocados pelos PNGs pequenos de `assets/brand/`. Os arquivos gerados entram no git.
- **CI:** o job `test` ganha, antes do Clippy, `actions/setup-node@v4` (Node 24) e `npm ci && npm run build` em `app/`
  (o `tauri::generate_context!` exige o `app/dist`). Um job novo `app` (macos-latest e windows-latest): `npm ci`,
  `node scripts/check-npm-licenses.mjs app`, `node --test scripts/`, `npm run check`, `npm test`, `npm run build`.
  O `cargo-deny` continua no job dele. Os jobs que rodam `-p revenant-core` no Ubuntu não mudam.

- [ ] **Step 1: Teste que falha:** `app/src/App.test.tsx` — `renders the app root` (o `App` renderiza um elemento
  com `data-testid="app"`); e no Rust `crates/revenant-app/src/lib.rs` um teste `window_config_matches_spec` que lê o
  `tauri.conf.json` (`include_str!`, `serde_json`) e confere 1280×800, mínimo 1024×700 e o título.
- [ ] **Step 2:** `npm test` em `app/` e `cargo test -p revenant-app` → FAIL.
- [ ] **Step 3:** montar o projeto; `npm install` dos pacotes da pilha.
- [ ] **Step 4:** `npm test`, `npm run build`, `cargo test --workspace`, `node scripts/check-npm-licenses.mjs app` →
  PASS; fmt e clippy. Se o Tauri exigir Rust acima de 1.88, parar e relatar (`BLOCKED`).
- [ ] **Step 5:** `npm run tauri build -- --bundles app` gera `target/release/bundle/macos/Revenant.app`; abrir com
  `open` e conferir que a janela aparece (uma captura em `.superpowers/sdd/…` para o relatório).
- [ ] **Step 6:** `crates/revenant-app/README.md` (como rodar `npm run tauri dev`, como gerar o `.app`, onde ficam os
  ícones). Commit `feat(app): Tauri 2 + React skeleton with the window and CI`.

### Task 5: A base da interface — tokens, temas, idiomas, Kit, Ajustes, Sobre e Erro

**Files:**
- Create: `app/src/i18n/{index.ts,pt-BR.json,es.json,en.json}`, `app/src/theme/ThemeProvider.tsx`,
  `app/src/settings/{settings.ts,SettingsProvider.tsx}`, `app/src/kit/` (`Button.tsx`, `Card.tsx`, `Note.tsx`,
  `Logo.tsx`, `TechDetails.tsx`, `Modal.tsx`, `Page.tsx` com o cabeçalho, a trilha de passos e o rodapé de ações do
  Kit), `app/src/screens/{Settings.tsx,About.tsx,UnexpectedError.tsx}`, `app/src/bridge.ts`
- Create: `crates/revenant-app/src/settings.rs`
- Create: `scripts/check-app.mjs` (+ `scripts/check-app.test.mjs`), ligado ao `npm run check`
- Test: `app/src/**/*.test.tsx`, testes em `settings.rs`

**Interfaces:**
- Consumes: Task 4 (o projeto). O desenho vem de `docs/superpowers/mockups/m2/kit.css`, `kit.js` (`Kit.logo`,
  `Kit.note`, `Kit.tech`) e `app.html` (telas 11, 12, 13).
- Produces:
  - `bridge.ts`: `invoke<T>(cmd, args)` que embrulha `@tauri-apps/api/core` e devolve `Promise<T>`, rejeitando com
    `{ code: string }`; e `type EngineErrorView = { code: string }`. Os testes trocam o `invoke` por um falso.
  - Comandos Rust `get_settings() -> Settings` e `set_settings(settings: Settings) -> Result<(), ErrorView>`, com
    `Settings { theme: "light" | "dark" | "auto", language: Option<"pt-BR" | "es" | "en"> }` (serde,
    `language: None` = seguir o sistema), guardados em `settings.json` na pasta de configuração do app
    (`app.path().app_config_dir()`); `ErrorView { code: &'static str }` em `crates/revenant-app/src/lib.rs`.
    Arquivo ausente ou ilegível → os padrões (auto, None), sem erro.
  - `initialLanguage(navigatorLanguage: string): "pt-BR" | "es" | "en"` em `i18n/index.ts`.
  - O tema aplica `data-theme="light"|"dark"` no `<html>` (ou nenhum, no automático), como nos mockups.
  - `t('error.<code>.title')` e `t('error.<code>.action')` para cada código que a 2.1 mostra:
    `permission_denied`, `device_not_found`, `system_tool_failed`, `open_failed`, `unsupported_platform`,
    `read_failed`, e um `error.unknown` para os demais. A tela 13 mostra o texto do código e o código em "Detalhes
    técnicos" recolhido.
- `check-app.mjs` falha quando, em `app/src/**/*.{ts,tsx,css}` (fora testes e `tokens.css`): há cor literal (`#hex`,
  `rgb(`, `hsl(`, nome de cor CSS em propriedade de cor) fora de `tokens.css`; há emoji; há texto JSX visível literal
  (um nó de texto com letras entre tags, ou `aria-label`/`title`/`placeholder`/`alt` com string literal); uma chave
  existe num catálogo e falta noutro; um `t('…')` usa chave ausente no pt-BR; `--brand-gold` aparece fora de
  `tokens.css` e `Logo.tsx`.

- [ ] **Step 1: Testes que falham.**
  - `initial_language`: `pt-BR` → pt-BR; `pt-PT` → pt-BR; `pt` → pt-BR; `es-MX` → es; `es` → es; `en-GB` → en; `de-DE`
    → en; `""` → en.
  - `settings.rs`: `settings_round_trip` (grava e lê num tempdir); `missing_file_gives_defaults`;
    `corrupt_file_gives_defaults` (conteúdo `"{{"`).
  - `Settings.test.tsx`: trocar o tema para Escuro chama `set_settings` com `theme: "dark"` e põe
    `data-theme="dark"` no `<html>`; trocar o idioma para `es` muda os textos da tela para o catálogo es.
  - `About.test.tsx`: mostra a versão `0.1.0`, "GPL-3.0" e os links do código e da documentação.
  - `UnexpectedError.test.tsx`: com `{ code: "open_failed" }` mostra o título traduzido e o código só depois de abrir
    "Detalhes técnicos"; com um código desconhecido mostra o texto de `error.unknown` e o código.
  - `check-app.test.mjs`: um arquivo de exemplo com `color: #fff` falha; com emoji falha; `<p>Olá</p>` falha;
    `aria-label="Fechar"` falha; catálogo es sem uma chave do pt-BR falha; um arquivo limpo passa.
- [ ] **Step 2:** `npm test`, `node --test scripts/check-app.test.mjs`, `cargo test -p revenant-app settings` → FAIL.
- [ ] **Step 3:** implementar; textos pt-BR copiados dos mockups; es e en novos, no mesmo tom.
- [ ] **Step 4:** tudo → PASS; `npm run check` limpo; `npm run tauri dev` e capturas das telas 11, 12 e 13 em claro e
  escuro, e da 11 na janela mínima (1024×700) em es e en, comparadas com `app.html` (`scripts/shoot-mockups.sh` dá as
  do mockup).
- [ ] **Step 5:** README do app (temas, idiomas, onde ficam os ajustes, `npm run check`). Commit `feat(app): theme,
  languages, kit, settings, about and the unexpected error screen`.

### Task 6: A ponte — comandos de discos e de acesso

**Files:**
- Create: `crates/revenant-app/src/disks.rs`, `crates/revenant-app/src/access.rs`
- Modify: `crates/revenant-app/src/lib.rs` (registrar os comandos), README do app

**Interfaces:**
- Consumes: `DeviceOps::list`, `DiskInfo` (com `connection`), `looks_like_camera_card` (Task 1);
  `open_with_system_prompt` (Task 2); `ErrorView` (Task 5).
- Produces:
  - `#[derive(Serialize)] struct DiskView { id: String, name: DiskName, size: u64, fs: Option<String>, connection:
    &'static str, kind: DiskKind, solid_state: bool, camera_card: bool, selectable: bool, highlighted: bool }` com
    `DiskName { label: Option<String>, vendor: Option<String>, model: Option<String> }` (a interface monta o texto:
    rótulo; senão marca + modelo; senão "Disco" + capacidade, do catálogo) e
    `enum DiskKind { Card, External, Internal, System }` serializado em `snake_case`.
  - `fn classify(disk: &DiskInfo, camera_card: bool) -> Option<DiskView>` (pura): `virtual_disk` → `None`; `system` →
    `System`, `selectable: false`; `removable` e ligado por `SdCard`, ou `removable` sem ser SSD → `Card`;
    não `internal` → `External`; senão `Internal`. `highlighted` = `Card` ou `External`. `solid_state` =
    `Some(true)`. O rótulo é o do primeiro volume com `label`.
  - Comando `list_disks() -> Result<Vec<DiskView>, ErrorView>` (via `fn list_with(ops: &dyn DeviceOps, camera: impl
    Fn(&DiskInfo) -> bool)`, testável), em ordem: destacados primeiro, depois internos, depois o do sistema; dentro de
    cada grupo, a ordem do motor. Roda fora da thread da janela (`async` + `spawn_blocking`).
  - Comando `request_access(disk_id: String) -> Result<AccessView, ErrorView>` com `AccessView { id: String, size: u64
    }`, via `fn access_with(ops: &dyn DeviceOps, opener: &dyn Fn(&DiskId) -> Result<Box<dyn RawDevice>, EngineError>,
    disk_id: &str)`: confere que o disco está na lista e não é do sistema (`system_disk` se for, `device_not_found` se
    não estiver), abre pelo `opener`, lê o primeiro setor lógico (`read_aligned` em `0` com o tamanho do setor), e
    solta o acesso. Depois da abertura, confere de novo que o disco está na lista (`device_not_found` se saiu enquanto
    a senha estava aberta). Roda em `spawn_blocking`.
- [ ] **Step 1: Testes que falham** (um `DeviceOps` falso com uma lista fixa; um `opener` falso que devolve um
  `RawDevice` sobre um buffer em memória, ou um erro):
  - `classify_kinds`: SD removível por `SdCard` → `Card`, destacado; HD externo USB não removível → `External`,
    destacado; SSD externo (`solid_state: Some(true)`) → `External`, `solid_state: true`; disco interno não do sistema →
    `Internal`, não destacado, selecionável; disco do sistema → `System`, não selecionável; `virtual_disk` → `None`.
  - `list_orders_highlighted_first` e `list_hides_virtual_disks`.
  - `display_name_falls_back`: `DiskName` com rótulo; sem rótulo e com marca/modelo; sem nada (os três `None`).
  - `list_error_is_a_code`: o falso devolve `system_tool_failed` → `Err(ErrorView { code: "system_tool_failed" })`.
  - `request_access_reads_the_first_sector` (o `opener` falso conta as leituras: exatamente 1, em `0`, de 512 bytes) e
    devolve `AccessView { size }`.
  - `request_access_refuses_the_system_disk` (`system_disk`, sem chamar o `opener`), `request_access_unknown_disk`
    (`device_not_found`), `request_access_denied` (o `opener` dá `PermissionDenied` → `permission_denied`),
    `request_access_disk_gone_after_prompt` (a lista muda durante o `opener` e o disco some → `device_not_found`).
- [ ] **Step 2:** `cargo test -p revenant-app` → FAIL.
- [ ] **Step 3:** implementar.
- [ ] **Step 4:** `cargo test --workspace` → PASS; fmt e clippy.
- [ ] **Step 5:** README do app (os comandos, o que cada um devolve). Commit `feat(app): disk list and access commands
  over the engine`.

### Task 7: As telas do caminho — Escolher o disco, Permissão e a tela provisória

**Files:**
- Create: `app/src/screens/ChooseDisk.tsx`, `app/src/screens/Permission.tsx`, `app/src/screens/AccessGranted.tsx`,
  `app/src/disks/{useDisks.ts,diskName.ts}`, testes ao lado
- Modify: `app/src/App.tsx` (o roteamento: Escolher o disco é a tela inicial; Ajustes e Sobre no cabeçalho, como no
  mockup), catálogos

**Interfaces:**
- Consumes: `list_disks`, `request_access` (Task 6); Kit, i18n, `bridge.ts`, tela 13 (Task 5).
- Produces:
  - `useDisks(intervalMs = 2000)`: chama `list_disks` na abertura e a cada `intervalMs`; mantém a seleção pelo `id`
    enquanto o disco continua na lista; só troca o estado quando a lista mudou (sem piscar); uma falha depois da
    primeira carga mantém a última lista e tenta no próximo ciclo; falha na primeira carga → a tela 13 com o código.
  - `diskName(d: DiskView, t): string` (rótulo; marca + modelo; `t('disk.fallback_name', { size })`).
  - Tela 1: os três estados do mockup (`lista`, `vazio`, `ssd`: o aviso de cautela abre ao escolher um SSD, com "Saiba
    por quê" e seguir mesmo assim), ícone por `connection`/`kind` (`sd` para cartão, `usb` para SSD externo, `hdd` para
    HD, como no mockup), a linha de baixo (marca · "parece um cartão de câmera" quando `camera_card`, senão marca ·
    "ligado por USB"/"num leitor de cartão"/"ligado por Thunderbolt"), o disco do sistema apagado com o motivo, a dica
    "Pare de usar o cartão agora" como no mockup.
  - Tela 3: Mac ou Windows conforme o sistema (`navigator.userAgent`/`platform`); o botão chama `request_access`;
    enquanto espera, o botão fica ocupado e a janela responde; `permission_denied` → o estado "Recusada" com Tentar de
    novo; `device_not_found` → volta à tela 1 com um aviso `Note` ("O disco não foi encontrado. Confira se ele
    continua conectado."); outro código → tela 13.
  - Tela provisória `AccessGranted`: "O Revenant consegue ler o {nome} ({capacidade})." / "A busca chega na próxima
    versão." / Voltar (texto final do construtor, nos três idiomas, tom dos mockups).
- [ ] **Step 1: Testes que falham** (Vitest, `invoke` falso, relógio falso do Vitest para os ciclos):
  - `shows_highlighted_disks_and_greys_the_system_disk`; `empty_state_when_no_disks`;
    `ssd_choice_shows_the_caution`; `camera_card_subtitle`.
  - `poll_updates_without_losing_selection` (o disco escolhido continua escolhido quando outro aparece);
    `poll_failure_keeps_last_list`; `first_load_failure_shows_error_screen`.
  - `permission_asks_and_shows_access_granted`; `retry_after_denied_asks_again` (primeiro `permission_denied`,
    depois sucesso, com 2 chamadas a `request_access`); `disk_gone_returns_to_list_with_notice`.
  - Os testes rodam em pt-BR e repetem `shows_highlighted_disks…` em es e en (nenhuma chave aparece crua).
- [ ] **Step 2:** `npm test` → FAIL.
- [ ] **Step 3:** implementar.
- [ ] **Step 4:** `npm test`, `npm run check`, `cargo test --workspace` → PASS. Gerar o `.app`, abrir e capturar a tela
  1 com os discos reais deste Mac (só listar: não escolher disco nenhum nem digitar senha), em claro e escuro e na
  janela mínima em es; comparar com `inicio.html#tela-1`.
- [ ] **Step 5:** README do app (o caminho até a permissão). Commit `feat(app): choose a disk, ask permission and
  confirm access`.

### Task 8: Documentação, mapa das telas e roteiro manual

**Files:**
- Modify: `docs/superpowers/mockups/m2/index.html` (a tela 1 passa para a 2.1 no mapa; `node scripts/check-mockups.mjs`
  passa), `docs/superpowers/roadmap.md` (2.1 com a tela 1, 2.2 sem ela, o estado da 2.1), `README.md` (o app existe:
  como gerar e abrir), `CHANGELOG.md` (`## [Unreleased]`: o app abre, lista os discos, pede a permissão, Ajustes,
  Sobre, três idiomas), `CLAUDE.md` (onde o desenvolvimento está; o comando do app no lugar do `revenant-dev` como
  ponto de partida)
- Create: `docs/superpowers/history/2026-11-03-m2-p1-conferencia-manual.md` (pt-BR): roteiro para o Sérgio no Mac —
  gerar o `.app`, abrir com dois cliques (e o que fazer se o Mac avisar que o app não é verificado), ver o cartão e o HD
  na lista, plugar e tirar um cartão e ver a lista mudar, escolher o cartão, cancelar a senha (Recusada), tentar de
  novo e digitar a senha (tela provisória), tirar o cartão com a senha aberta, trocar tema e idioma, fechar e abrir de
  novo, abrir o Sobre; e o que conferir no Windows quando houver um PC.
- Create: `docs/superpowers/history/2026-11-03-m2-p1-pendencias.md` (decisões e o que ficou para a 2.2: manter o acesso
  durante a busca; o texto do passo 3 da permissão; a tela provisória sai).

- [ ] **Step 1:** `node scripts/check-mockups.mjs` → PASS depois da mudança no `index.html` (a regra do mapa continua
  passando).
- [ ] **Step 2:** conferir que todo README citado descreve o que a versão atual faz e que os comandos documentados
  rodam (`npm run tauri build -- --bundles app`).
- [ ] **Step 3:** Commit `docs: Part 2.1 app skeleton, screen map and manual check`.
