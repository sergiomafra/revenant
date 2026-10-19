# Módulo 1 · Parte 1 — Fundação e motor de escultura (JPEG, MP4/MOV) — Plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Criar o repositório como software de verdade (workspace Rust, licença, CI, docs, CLAUDE.md) e a primeira fatia do motor:
ler uma imagem de disco só para leitura, achar por assinatura fotos JPEG e vídeos MP4/MOV (mesmo em disco formatado), medir o fim
exato de cada um, classificar em Íntegro/Parcial/Danificado e extrair para outra pasta, recusando salvar no mesmo volume.

**Architecture:** Biblioteca `revenant-core` sem interface, com quatro peças: `Fonte` (leitura posicional, só leitura), `Formato`
(um por tipo de arquivo: reconhece a assinatura e mede o arquivo pela estrutura interna), `varrer` (anda pela fonte setor a setor,
emite eventos) e `extrair`. Um crate `revenant-testkit` gera imagens de disco sintéticas com conteúdo conhecido; um executável
`revenant-dev` (só para desenvolvimento) expõe o motor por linha de comando.

**Tech Stack:** Rust 2024 (workspace Cargo), `thiserror`, `zune-jpeg` (validação de JPEG), `windows-sys` (só Windows), `serde`/
`serde_json` e `clap` (só no `revenant-dev`), `image` + `tempfile` + `assert_cmd` (só em testes). GitHub Actions + `cargo-deny`.

**Spec:** `docs/superpowers/specs/2026-10-19-revenant-design.md` (§5 motor, §7 licença, §8 software, §9 documentação).

## Roteiro dos módulos (contexto; este plano cobre só a Parte 1.1)

- **Módulo 1 — Motor.** 1.1 (este plano): fundação + escultura JPEG/MP4/MOV. 1.2: formatos RAW (CR2, CR3, NEF, ARW, RAF, DNG, ORF,
  RW2, HEIC) + prévia embutida e EXIF + escultura de arquivos fragmentados. 1.3: exFAT/FAT32 (apagados, nomes e pastas),
  deduplicação, agrupamento por sessão/câmera, cópia do cartão tolerante a setores ruins, sessão de busca salva, leitura do
  aparelho bruto (Mac/Windows) com desmontagem sem ejetar.
- **Módulo 2 — App** (Tauri 2 + React): telas em horizontal aprovadas pelo Sérgio ANTES do plano; ajudante privilegiado; fluxo
  completo; resultados com três visualizações; i18n pt-BR/es/en.
- **Módulo 3 — Lançamento:** release automático, instaladores das 4 arquiteturas, assinatura ad-hoc, atualizador.
- **Módulo 4 — Site de documentação** (Astro Starlight, 3 idiomas, GitHub Pages).

## Global Constraints

- **Nunca escrever na fonte.** `Fonte` não tem método de escrita; `FonteArquivo` abre com leitura apenas. Nenhum código do motor
  abre o caminho de origem com permissão de escrita.
- **Licenças (spec §7):** dependências só MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, CC0-1.0, BSL-1.0.
  Proibido GPL/AGPL/LGPL/MPL de terceiros e qualquer código copiado de PhotoRec/TestDisk. O próprio Revenant é **GPL-3.0-only**,
  direitos do Sérgio Mafra.
- **Identificadores, comentários, docs e mensagens de commit em português (pt-BR)**, como no Clustory. Termos de formato (box,
  marker, `ftyp`) ficam como na especificação do formato.
- **Erros por código:** todo erro do motor tem `codigo() -> &'static str` em `snake_case`; nenhum texto de interface no motor.
- **Rust:** edition 2024, `rust-version = "1.85"`. A máquina de dev é Mac **Intel**, Rust do Homebrew (sem `rustup`): **nunca**
  `brew install/upgrade` de toolchain; alvos extras só no CI.
- **Sem `unsafe`** fora de `fonte/windows.rs` (chamadas do `windows-sys`), cada bloco com comentário `// SEGURANÇA:`.
- **Documentação no mesmo commit [Sérgio]:** toda tarefa que muda comportamento atualiza o README do crate tocado e, se mudar o
  que o usuário vê, o `CHANGELOG.md` (seção "Não lançado").
- **Trailer de todo commit:** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Nunca `git add -A`; confira
  `git diff --staged --name-only` antes de cada commit.
- **Testes não tocam dado real** nem disco físico: só imagens sintéticas em `tempfile`.
- Todo teste novo é visto falhando antes de passar (Step "verify it fails").

## Review Focus

1. **Arquivo que atravessa a fronteira de bloco de leitura** (começa no fim de um bloco de 4 MiB e termina no próximo): deve sair
   inteiro e Íntegro → teste `jpeg_atravessando_blocos` na Task 5.
2. **Setor ilegível no meio da busca** (cartão com defeito): a busca não para, registra a região ilegível e acha o que vem depois
   → teste `setor_ilegivel_nao_para_a_busca` na Task 5.
3. **Lixo que parece cabeçalho** (`FF D8 FF` ou `ftyp` soltos em dado aleatório): não vira achado → testes
   `cabecalho_falso_nao_vira_jpeg` (Task 3) e `ftyp_solto_nao_vira_video` (Task 4).
4. **Imagem minúscula ou vazia** (0 bytes, 100 bytes, tamanho que não é múltiplo de 512): zero achados, sem pânico, arquivo no
   último setor incompleto ainda achado → teste `imagens_pequenas_e_desalinhadas` na Task 5.
5. **Destino no mesmo volume da origem**, ou arquivo de mesmo nome já no destino: recusa com `destino_na_origem`; nunca
   sobrescreve, ganha sufixo `_2` → testes na Task 6.

---

### Task 1: Fundação do repositório

**Files:**
- Create: `Cargo.toml` (workspace; sem `rust-toolchain.toml`, a máquina de dev usa o Rust do Homebrew), `crates/revenant-core/Cargo.toml`,
  `crates/revenant-core/src/lib.rs`, `crates/revenant-core/src/erro.rs`, `crates/revenant-core/README.md`
- Create: `LICENSE` (texto oficial da GPL-3.0), `README.md` (substitui o atual), `CHANGELOG.md`, `CLAUDE.md`, `deny.toml`,
  `.github/workflows/ci.yml`, `.gitignore` (acrescentar `/target/`; manter `/.superpowers/`)

**Interfaces:**
- Produces:
  ```rust
  // crates/revenant-core/src/erro.rs
  #[derive(Debug, thiserror::Error)]
  pub enum ErroDoMotor {
      #[error("abrir_falhou")]      Abrir { #[source] causa: std::io::Error },
      #[error("leitura_falhou")]    Leitura { deslocamento: u64, #[source] causa: std::io::Error },
      #[error("destino_na_origem")] DestinoNaOrigem,
      #[error("escrita_falhou")]    Escrita { #[source] causa: std::io::Error },
      #[error("cancelado")]         Cancelado,
  }
  impl ErroDoMotor { pub fn codigo(&self) -> &'static str }
  ```
  `lib.rs` declara `pub mod erro;` e reexporta `ErroDoMotor`.

- [ ] **Step 1: Teste que falha** — em `erro.rs`, `#[cfg(test)] mod testes`:
  ```rust
  #[test]
  fn codigos_sao_estaveis() {
      assert_eq!(ErroDoMotor::DestinoNaOrigem.codigo(), "destino_na_origem");
      assert_eq!(ErroDoMotor::Cancelado.codigo(), "cancelado");
      let io = || std::io::Error::other("x");
      assert_eq!(ErroDoMotor::Leitura { deslocamento: 512, causa: io() }.codigo(), "leitura_falhou");
      assert_eq!(ErroDoMotor::Abrir { causa: io() }.codigo(), "abrir_falhou");
      assert_eq!(ErroDoMotor::Escrita { causa: io() }.codigo(), "escrita_falhou");
  }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core` → FAIL (não compila: `codigo` não existe).
- [ ] **Step 3:** Implementar `codigo()` (um `match`). Workspace: `members = ["crates/*"]`, `resolver = "3"`,
  `[workspace.package] edition = "2024", rust-version = "1.85", license = "GPL-3.0-only", repository =
  "https://github.com/sergiomafra/revenant"`, versão `0.1.0`.
- [ ] **Step 4:** `cargo test -p revenant-core` → PASS; `cargo clippy --all-targets -- -D warnings` e `cargo fmt --check` limpos.
- [ ] **Step 5: Arquivos de projeto.**
  - `deny.toml`: `[licenses] allow = [...]` exatamente a lista das Global Constraints ; os crates do workspace levam
    `publish = false` e ficam fora da checagem por `[licenses.private] ignore = true` (o GPL-3.0 é só nosso); `[bans] multiple-versions = "warn"`.
  - `ci.yml`: em `push` e `pull_request`; matriz `macos-latest` (arm64), `macos-15-intel` (x86_64; o `macos-13` foi aposentado), `windows-latest` (x64),
    `windows-11-arm` (ARM64); passos `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`;
    job separado `EmbarkStudios/cargo-deny-action@v2` em `ubuntu-latest`.
  - `README.md`: o que é o Revenant (1 parágrafo, da spec §1), estado atual ("em construção: motor de recuperação; ainda sem
    app para baixar"), licença, link para a spec. Em pt-BR.
  - `CHANGELOG.md`: formato Keep a Changelog, seção `## [Não lançado]`.
  - `CLAUDE.md` (enxuto, no molde do Clustory): onde o desenvolvimento está; diretrizes fixas (as Global Constraints + processo
    "um módulo por vez; spec → telas → plano → subagentes; o Sérgio decide o merge" + "documentação sempre atualizada e explicando
    como usar"); seção "Armadilhas" vazia com a regra "todo erro vira uma instrução aqui"; ambiente do Mac Intel.
  - `crates/revenant-core/README.md`: o que a versão atual do motor faz (por ora: códigos de erro).
- [ ] **Step 6:** `cargo test --workspace` PASS. Commit: `chore: fundação do repositório (workspace, licença, CI, docs)`.

---

### Task 2: `Fonte` — leitura posicional só leitura

**Files:**
- Create: `crates/revenant-core/src/fonte/mod.rs`, `crates/revenant-core/src/fonte/arquivo.rs`,
  `crates/revenant-core/src/fonte/unix.rs`, `crates/revenant-core/src/fonte/windows.rs`
- Test: `crates/revenant-core/tests/fonte.rs`

**Interfaces:**
- Consumes: `ErroDoMotor` (Task 1).
- Produces:
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
  pub struct IdVolume(pub u64);

  pub trait Fonte: Send + Sync {
      fn tamanho(&self) -> u64;
      /// Lê a partir de `deslocamento`. Lê o buffer inteiro, salvo no fim da fonte (devolve menos; 0 além do fim).
      fn ler_em(&self, deslocamento: u64, buf: &mut [u8]) -> Result<usize, ErroDoMotor>;
      /// Volume onde a fonte mora (para recusar extrair para ele). None se desconhecido.
      fn volume(&self) -> Option<IdVolume>;
  }
  pub struct FonteArquivo { /* File + tamanho + volume */ }
  impl FonteArquivo { pub fn abrir(caminho: &std::path::Path) -> Result<Self, ErroDoMotor> }
  impl Fonte for FonteArquivo {}
  /// Volume de um caminho existente (arquivo ou pasta).
  pub fn volume_de(caminho: &std::path::Path) -> Result<IdVolume, ErroDoMotor>
  ```
  `ler_em` usa leitura posicional sem cursor compartilhado (`FileExt::read_at` no Unix, `seek_read` no Windows), em laço até
  encher o buffer ou chegar ao fim. `volume_de`: Unix = `MetadataExt::dev()`; Windows = `dwVolumeSerialNumber` de
  `GetFileInformationByHandle` (abrir pastas exige `FILE_FLAG_BACKUP_SEMANTICS`).

- [ ] **Step 1: Testes que falham** (`tests/fonte.rs`, com `tempfile`):
  ```rust
  #[test] fn le_no_meio_e_no_fim() {
      // arquivo de 1000 bytes, byte i = (i % 251) as u8
      let f = FonteArquivo::abrir(&caminho).unwrap();
      assert_eq!(f.tamanho(), 1000);
      let mut b = [0u8; 10];
      assert_eq!(f.ler_em(500, &mut b).unwrap(), 10); assert_eq!(b[0], (500 % 251) as u8);
      assert_eq!(f.ler_em(995, &mut b).unwrap(), 5);
      assert_eq!(f.ler_em(1000, &mut b).unwrap(), 0);
      assert_eq!(f.ler_em(5000, &mut b).unwrap(), 0);
  }
  #[test] fn nunca_altera_a_origem() {
      // conteúdo e data de modificação idênticos depois de abrir, ler tudo e soltar a fonte
  }
  #[test] fn abre_arquivo_somente_leitura() {
      // no Unix: chmod 0o444 no arquivo; FonteArquivo::abrir ainda funciona
  }
  #[test] fn mesmo_volume_para_arquivo_e_sua_pasta() {
      assert_eq!(volume_de(dir.path()).unwrap(), FonteArquivo::abrir(&caminho).unwrap().volume().unwrap());
  }
  #[test] fn abrir_inexistente_da_codigo() {
      assert_eq!(FonteArquivo::abrir(Path::new("/nao/existe")).err().unwrap().codigo(), "abrir_falhou");
  }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core --test fonte` → FAIL (tipos inexistentes).
- [ ] **Step 3:** Implementar conforme Interfaces. Varredura de convenção no mesmo arquivo de teste:
  `fonte_nao_tem_caminho_de_escrita` lê `src/fonte/*.rs` como texto e reprova `write(true)`, `OpenOptions::new().write`,
  `.append(`, `create(true)` e `GENERIC_WRITE`.
- [ ] **Step 4:** testes PASS; clippy/fmt limpos.
- [ ] **Step 5:** README do crate (seção "Fonte"). Commit: `feat(motor): fonte de leitura posicional só leitura`.

---

### Task 3: Testkit e o formato JPEG

**Files:**
- Create: `crates/revenant-testkit/Cargo.toml` (`publish = false`; dependências `image` com só o recurso `jpeg`, `tempfile`),
  `crates/revenant-testkit/src/lib.rs`
- Create: `crates/revenant-core/src/formatos/mod.rs`, `crates/revenant-core/src/formatos/jpeg.rs`
- Test: `crates/revenant-core/tests/jpeg.rs`

**Interfaces:**
- Consumes: `Fonte`, `FonteArquivo` (Task 2).
- Produces (testkit):
  ```rust
  /// JPEG baseline válido, conteúdo pseudoaleatório determinístico por `semente`, com um segmento APP1 de `extra_app1` bytes
  /// (simula o EXIF com miniatura embutida das câmeras; o conteúdo do APP1 contém um JPEG inteiro de 16×16).
  pub fn jpeg(largura: u32, altura: u32, semente: u64, extra_app1: usize) -> Vec<u8>;
  pub struct Imagem { bytes: Vec<u8> }
  impl Imagem {
      pub fn nova(tamanho: usize, semente_do_ruido: u64) -> Self; // preenchida com ruído determinístico
      pub fn zeros(tamanho: usize) -> Self;
      pub fn colocar(&mut self, deslocamento: usize, dados: &[u8]) -> &mut Self;
      pub fn salvar(&self) -> tempfile::NamedTempFile;
  }
  ```
- Produces (core):
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)] pub enum TipoDeArquivo { Jpeg, Mp4, Mov }
  impl TipoDeArquivo { pub fn extensao(self) -> &'static str } // "jpg", "mp4", "mov"
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Estado { Integro, Parcial, Danificado }
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct Medida { pub tamanho: u64, pub tipo: TipoDeArquivo, pub estado: Estado }
  pub trait Formato: Send + Sync {
      fn nome(&self) -> &'static str;
      /// `cabeca`: até 16 bytes lidos num início de setor.
      fn reconhece(&self, cabeca: &[u8]) -> bool;
      /// Mede o arquivo que começa em `inicio`. None = não é um arquivo deste formato.
      fn medir(&self, fonte: &dyn Fonte, inicio: u64) -> Option<Medida>;
  }
  pub struct Jpeg;
  pub const LIMITE_JPEG: u64 = 64 * 1024 * 1024;
  pub fn todos() -> Vec<Box<dyn Formato>>; // [Jpeg] nesta task; Task 4 acrescenta IsoBmff
  ```

Regras do `Jpeg::medir` (o algoritmo que a assinatura não determina):
- `reconhece`: `FF D8 FF` seguido de um marcador válido (`E0..=EF`, `DB`, `C0..=C3`, `C4`, `FE`).
- Anda pelos segmentos pelos comprimentos declarados (assim a miniatura dentro do APP1 é pulada). Marcador inválido ou
  comprimento < 2 antes do SOS → `None` (cabeçalho falso).
- Exige DQT e SOF antes do SOS; sem eles → `None`.
- Depois do SOS, varre os dados comprimidos: `FF 00` e `FF D0..=D7` são dados; `FF D9` é o fim → `tamanho` inclui o `FF D9`.
- Para, sem achar `FF D9`, ao chegar ao fim da fonte, a `LIMITE_JPEG`, ou a um **início de setor (512) cujos bytes são uma
  assinatura reconhecida por `todos()`** → `tamanho` até ali, `estado = Parcial`.
- Com `FF D9`: decodifica os bytes com `zune-jpeg`; sucesso → `Integro`; erro → `Danificado`.

- [ ] **Step 1: Testes que falham** (`tests/jpeg.rs`):
  ```rust
  #[test] fn jpeg_inteiro_e_medido_exato() {
      let j = testkit::jpeg(640, 480, 1, 20_000);
      let img = Imagem::zeros(1 << 20).colocar(4096, &j).salvar();
      let m = Jpeg.medir(&FonteArquivo::abrir(img.path()).unwrap(), 4096).unwrap();
      assert_eq!(m, Medida { tamanho: j.len() as u64, tipo: TipoDeArquivo::Jpeg, estado: Estado::Integro });
  }
  #[test] fn miniatura_no_app1_nao_encerra_o_arquivo() { /* extra_app1 = 60_000; tamanho == j.len() */ }
  #[test] fn jpeg_cortado_pelo_proximo_arquivo_e_parcial() {
      // j1 cortado na metade em 4096; j2 inteiro no próximo múltiplo de 512 depois de 4096 + j1.len()/2
      // medir(4096) → estado Parcial, tamanho == (início de j2) - 4096
  }
  #[test] fn jpeg_cortado_pelo_fim_da_fonte_e_parcial() {}
  #[test] fn dados_comprimidos_estragados_sao_danificados() {
      // jpeg inteiro com um trecho do meio dos dados comprimidos estragado de um jeito que o zune-jpeg recuse (ex.: um
      // marcador FF xx inválido no meio do scan); tamanho igual, estado Danificado. Se nenhuma corrupção for recusada pelo
      // decodificador, reporte DONE_WITH_CONCERNS com a prova em vez de afrouxar a regra.
  }
  #[test] fn cabecalho_falso_nao_vira_jpeg() {
      // Imagem::nova(1<<20, 7) com b"\xFF\xD8\xFF\xE1\x00\x01" em 0  → medir(0) == None
      // e em outra posição FF D8 FF E0 + comprimento válido seguido de lixo sem DQT/SOF → None
  }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core --test jpeg` → FAIL.
- [ ] **Step 3:** Implementar testkit e `Jpeg` conforme as regras. Leia a fonte em janelas de 1 MiB (nunca o arquivo inteiro de
  uma vez antes de achar o fim; a decodificação final carrega até `LIMITE_JPEG`).
- [ ] **Step 4:** PASS; clippy/fmt limpos; `cargo deny check` se disponível (senão, o CI confere).
- [ ] **Step 5:** README do crate (seção "Formatos: JPEG"). Commit: `feat(motor): formato JPEG com medição pela estrutura`.

---

### Task 4: Formato ISO BMFF (MP4/MOV)

**Files:**
- Create: `crates/revenant-core/src/formatos/isobmff.rs`; Modify: `crates/revenant-core/src/formatos/mod.rs` (`todos()`)
- Modify: `crates/revenant-testkit/src/lib.rs`
- Test: `crates/revenant-core/tests/isobmff.rs`

**Interfaces:**
- Consumes: `Formato`, `Medida`, `Estado`, `TipoDeArquivo`, `Fonte` (Tasks 2–3).
- Produces:
  ```rust
  pub struct IsoBmff;            // core; incluído em todos()
  // testkit:
  pub struct OpcoesMp4 { pub tamanho_mdat: usize, pub moov_no_fim: bool, pub mov: bool, pub mdat_64bits: bool }
  /// ftyp (marca "isom", ou "qt  " se mov) + mdat + moov (mvhd, 1 trak com stsz e stco apontando para dentro do mdat;
  /// co64 se mdat_64bits). Ordem ftyp, moov, mdat, ou ftyp, mdat, moov se moov_no_fim (o caso das câmeras).
  pub fn mp4(op: &OpcoesMp4) -> Vec<u8>;
  ```

Regras do `IsoBmff::medir`:
- `reconhece`: bytes 4..8 == `ftyp` e tamanho declarado do box entre 8 e 4096.
- Anda pelos boxes de nível superior a partir de `inicio`. Tipos aceitos: `ftyp moov mdat free skip wide uuid meta pdin moof mfra
  styp sidx udta`. Tamanho 1 → `largesize` de 64 bits; tamanho 0 → vai até o fim da fonte (só aceito para `mdat`). Para no
  primeiro box de tipo não aceito, tamanho < 8, ou fim da fonte. `tamanho` = soma dos boxes válidos; box que passa do fim da fonte
  conta só até o fim.
- `tipo`: marca principal do `ftyp` `qt  ` → `Mov`; senão `Mp4`.
- Sem `mdat` → `None`. Sem `moov` → `Danificado` (não toca sem o índice; o reparo é a 1.1).
- Com `moov`: lê `stco`/`co64` e `stsz` de cada `trak`; algum chunk começando além de `inicio + tamanho` ou fim de amostra além →
  `Parcial`; senão `Integro`. Box `mdat` declarado maior que o que existe → `Parcial`.

- [ ] **Step 1: Testes que falham** (`tests/isobmff.rs`), cada um coloca o MP4 em 8192 de uma `Imagem::nova(…, ruído)`:
  ```rust
  #[test] fn mp4_moov_no_fim_e_integro()  { /* tamanho == bytes.len(), tipo Mp4, Integro */ }
  #[test] fn mov_e_reconhecido_como_mov() { /* mov: true → tipo Mov */ }
  #[test] fn largesize_64_bits()          { /* mdat_64bits: true → tamanho exato, Integro */ }
  #[test] fn sem_moov_e_danificado()      { /* mp4 moov_no_fim cortado antes do moov, resto ruído → Danificado */ }
  #[test] fn mdat_cortado_e_parcial()     { /* moov antes do mdat; bytes cortados no meio do mdat e fonte acaba → Parcial */ }
  #[test] fn ftyp_solto_nao_vira_video()  { /* só um ftyp válido seguido de ruído → None */ }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core --test isobmff` → FAIL.
- [ ] **Step 3:** Implementar. Pare a leitura de boxes internos em 16 MiB de `moov` (moov maior → `Danificado`).
- [ ] **Step 4:** PASS + toda a suíte do crate; clippy/fmt.
- [ ] **Step 5:** README (seção "Formatos: MP4/MOV"). Commit: `feat(motor): formato MP4/MOV pela estrutura de boxes`.

---

### Task 5: Varredura

**Files:**
- Create: `crates/revenant-core/src/varredura.rs`; Modify: `lib.rs` (reexports)
- Test: `crates/revenant-core/tests/varredura.rs`

**Interfaces:**
- Consumes: `Fonte`, `Formato`, `todos()`, `Medida`, `ErroDoMotor`.
- Produces:
  ```rust
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct Achado { pub id: u64, pub inicio: u64, pub tamanho: u64, pub tipo: TipoDeArquivo, pub estado: Estado }
  pub struct OpcoesDeVarredura { pub setor: u64, pub bloco: usize }
  impl Default for OpcoesDeVarredura { /* setor 512, bloco 4 MiB */ }
  #[derive(Clone, Default)] pub struct Cancelamento(std::sync::Arc<std::sync::atomic::AtomicBool>);
  impl Cancelamento { pub fn cancelar(&self); pub fn cancelado(&self) -> bool }
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum Evento { Achado(Achado), Progresso { lidos: u64, total: u64 }, RegiaoIlegivel { inicio: u64, tamanho: u64 } }
  #[derive(Clone, Debug, PartialEq, Eq, Default)] pub struct ResumoDaVarredura { pub achados: u64, pub bytes_ilegiveis: u64 }
  pub fn varrer(fonte: &dyn Fonte, formatos: &[Box<dyn Formato>], opcoes: &OpcoesDeVarredura,
                cancelamento: &Cancelamento, ao_evento: &mut dyn FnMut(Evento)) -> Result<ResumoDaVarredura, ErroDoMotor>;
  ```
Regras:
- Lê a fonte em blocos de `opcoes.bloco`; olha a `cabeca` (16 bytes) de cada início de setor; o primeiro formato que reconhece e
  mede ganha. Achou → emite `Achado` (`id` sequencial a partir de 1) e pula para o próximo início de setor depois de
  `inicio + tamanho`. Não achou → próximo setor. A cabeça de um setor perto do fim do bloco é lida da fonte, nunca cortada.
- Bloco com erro de leitura → relê setor a setor; cada sequência de setores ilegíveis vira um `RegiaoIlegivel` e entra em
  `bytes_ilegiveis`; a busca continua.
- `Progresso` no máximo a cada bloco e uma vez no fim com `lidos == total`.
- `cancelamento.cancelado()` conferido a cada bloco → `Err(ErroDoMotor::Cancelado)`.

- [ ] **Step 1: Testes que falham** (`tests/varredura.rs`). Uma `FonteFalha` de teste embrulha `FonteArquivo` e devolve
  `Err(Leitura)` para qualquer leitura que toque um intervalo dado.
  ```rust
  #[test] fn acha_fotos_e_videos_num_cartao_formatado() {
      // Imagem::nova(16 MiB, ruído): jpeg em 0, mp4 (moov no fim) em 1 MiB, jpeg cortado em 5 MiB + jpeg em seguida,
      // mov em 9 MiB. Espera achados (inicio, tipo, estado) exatamente:
      // [(0,Jpeg,Integro),(1MiB,Mp4,Integro),(5MiB,Jpeg,Parcial),(próx. setor,Jpeg,Integro),(9MiB,Mov,Integro)]
  }
  #[test] fn jpeg_atravessando_blocos() { /* bloco = 1 MiB, jpeg de 300 KB começando em 1 MiB - 100 KB (alinhado a 512) → Integro, tamanho exato */ }
  #[test] fn setor_ilegivel_nao_para_a_busca() {
      // jpeg em 0, setores [2 MiB, 2 MiB + 4096) ilegíveis, jpeg em 3 MiB
      // → 2 achados Integro; um RegiaoIlegivel { inicio: 2 MiB, tamanho: 4096 }; resumo.bytes_ilegiveis == 4096
  }
  #[test] fn imagens_pequenas_e_desalinhadas() {
      // 0 bytes → resumo.achados 0; 100 bytes de ruído → 0;
      // imagem de 10_000 bytes (não múltiplo de 512) com jpeg pequeno no setor 9216 terminando no último byte → 1 achado Integro
  }
  #[test] fn progresso_termina_completo() { /* último Progresso tem lidos == total */ }
  #[test] fn cancelar_interrompe() { /* cancela dentro do callback no 1º Progresso → Err com codigo "cancelado" */ }
  ```
- [ ] **Step 2:** `cargo test -p revenant-core --test varredura` → FAIL.
- [ ] **Step 3:** Implementar conforme as regras.
- [ ] **Step 4:** PASS + suíte inteira; clippy/fmt.
- [ ] **Step 5:** README (seção "Varredura"). Commit: `feat(motor): varredura por setores com eventos e setores ilegíveis`.

---

### Task 6: Extrair

**Files:**
- Create: `crates/revenant-core/src/extrair.rs`; Modify: `lib.rs`
- Test: `crates/revenant-core/tests/extrair.rs`

**Interfaces:**
- Consumes: `Fonte`, `Achado`, `volume_de`, `ErroDoMotor`.
- Produces:
  ```rust
  /// Copia os bytes do achado para `pasta_destino`, com nome `revenant_<inicio em 12 dígitos hex>.<ext>`
  /// (`_2`, `_3`… se já existir; nunca sobrescreve). Recusa se a pasta estiver no volume da fonte.
  pub fn extrair(fonte: &dyn Fonte, achado: &Achado, pasta_destino: &std::path::Path) -> Result<std::path::PathBuf, ErroDoMotor>;
  ```
  Cria com `create_new(true)`; copia em blocos de 1 MiB; erro de leitura no meio → preenche o trecho com zeros e segue (o arquivo
  sai Parcial em vez de não sair), erro de escrita → `Escrita` e apaga o arquivo incompleto.

- [ ] **Step 1: Testes que falham** (`tests/extrair.rs`). Para testar o volume sem dois discos, use uma `Fonte` de teste que
  embrulha `FonteArquivo` e devolve um `volume()` escolhido.
  ```rust
  #[test] fn extrai_bytes_identicos() { /* conteúdo do arquivo == bytes do jpeg; nome "revenant_000000001000.jpg" para inicio 4096 */ }
  #[test] fn nunca_sobrescreve() { /* extrair duas vezes → segundo é "revenant_000000001000_2.jpg"; o primeiro intacto */ }
  #[test] fn recusa_destino_no_volume_da_origem() {
      // fonte com volume() == volume_de(pasta) → Err codigo "destino_na_origem" e a pasta continua vazia
  }
  #[test] fn aceita_volume_desconhecido_de_outro_lugar() { /* volume() == Some(IdVolume(u64::MAX)) → Ok */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implementar. **Step 4:** PASS + suíte; clippy/fmt.
- [ ] **Step 5:** README (seção "Extrair"). Commit: `feat(motor): extrair achados sem sobrescrever e nunca na origem`.

---

### Task 7: `revenant-dev` (linha de comando de desenvolvimento) e fechamento da parte

**Files:**
- Create: `crates/revenant-dev/Cargo.toml` (`publish = false`; `clap` derive, `serde`, `serde_json`), `crates/revenant-dev/src/main.rs`,
  `crates/revenant-dev/README.md`
- Test: `crates/revenant-dev/tests/cli.rs` (`assert_cmd`, `revenant-testkit`)
- Modify: `README.md`, `CHANGELOG.md`, `CLAUDE.md` ("Onde o desenvolvimento está"), `crates/revenant-core/README.md`

**Interfaces:**
- Consumes: `FonteArquivo`, `todos()`, `varrer`, `extrair`, `Evento`.
- Produces: `revenant-dev varrer <IMAGEM> [--extrair <PASTA>]`. Saída: uma linha JSON por evento em stdout —
  `{"evento":"achado","id":1,"inicio":0,"tamanho":12345,"tipo":"jpeg","estado":"integro"}`,
  `{"evento":"ilegivel","inicio":…,"tamanho":…}`, `{"evento":"resumo","achados":N,"bytes_ilegiveis":M}`
  (progresso vai para stderr). Com `--extrair`, extrai cada achado ao ser achado e acrescenta `"arquivo":"<caminho>"` na linha.
  Erro → stderr `erro: <codigo>` e saída 1. (`serde` só neste crate; o core não depende de `serde`.)

- [ ] **Step 1: Testes que falham:**
  ```rust
  #[test] fn varrer_lista_achados_em_json() { /* imagem com 1 jpeg + 1 mp4 → 2 linhas "achado" e "resumo" com achados 2; saída 0 */ }
  #[test] fn extrair_grava_os_arquivos() { /* --extrair tmpdir → 2 arquivos com bytes idênticos aos originais */ }
  #[test] fn imagem_inexistente_sai_com_codigo() { /* stderr contém "erro: abrir_falhou", saída 1 */ }
  ```
- [ ] **Step 2:** FAIL. **Step 3:** implementar. **Step 4:** `cargo test --workspace` PASS; clippy/fmt.
- [ ] **Step 5: Documentação da parte:**
  - `crates/revenant-dev/README.md`: o que é (ferramenta de desenvolvedor, não é o app), como compilar, exemplo de uso com uma
    imagem de cartão (`dd`/`diskutil` explicado como "leitor", nunca escrevendo no cartão), formato da saída.
  - `README.md`: estado atual e "como experimentar o motor" para desenvolvedores.
  - `CHANGELOG.md` `[Não lançado]`: "Motor: acha fotos JPEG e vídeos MP4/MOV em imagens de cartão, inclusive formatado; classifica
    em Íntegro/Parcial/Danificado; recupera para outra pasta."
  - `CLAUDE.md`: Parte 1.1 concluída na branch; próximo: 1.2.
- [ ] **Step 6: Medição final:** `cargo test --workspace 2>&1 | grep -E '^test result'` (todas `ok`), clippy e fmt limpos.
  Commit: `feat(dev): linha de comando de desenvolvimento e documentação da parte 1.1`.
