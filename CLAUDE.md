# Revenant — instruções para o Claude

App gratuito (GPL-3.0) para Mac e Windows que recupera fotos e vídeos de cartões SD e discos externos. Feito pelo Sérgio.
**Leia antes de mexer:** a spec `docs/superpowers/specs/2026-10-19-revenant-design.md` e o plano em andamento em
`docs/superpowers/plans/`. Este arquivo é enxuto de propósito: uma frase desatualizada aqui é um bug.

## Onde o desenvolvimento está

**25/10/2026.** Módulo 1 (motor): Partes 1.1 a 1.3 mergeadas (PRs #1 a #4). Parte 1.4 (sistemas de arquivos:
FAT12/16/32 e exFAT, deduplicação, nomes e agrupamento) concluída na branch `feat/m1-p4-filesystems`, aguardando
PR/merge; decisões e pendências em `docs/superpowers/history/2026-10-25-m1-p4-pendencias.md`. Próxima: 1.5 (disco de
verdade). Roteiro até a 1.0: `docs/superpowers/roadmap.md`. MSRV: Rust 1.88. CLI de desenvolvimento: `revenant-dev`
(`bench` mede a velocidade).
Amostras reais de câmera (CC0, nunca no repositório): `scripts/fetch-samples.sh` + teste ignorado `real_samples`.

## Diretrizes fixas

- Um módulo por vez: spec → telas → plano → subagentes. O Sérgio decide o merge.
- Fim de parte = push + PR; o merge é do Sérgio.
- Código e repositório em inglês (identificadores, comentários, códigos de erro, commits, READMEs, CHANGELOG). Exceções:
  este arquivo e `docs/superpowers/**` (pt-BR).
- Documentação sempre atualizada e explicando como usar: toda tarefa que muda comportamento atualiza o README do crate e, se
  muda o que o usuário vê, o `CHANGELOG.md` (`## [Unreleased]`), no mesmo commit.
- Nunca escrever na origem: `Source` não tem método de escrita; `FileSource` abre só leitura.
- Exceção única (Sérgio, 1.5): no Windows, uma alça de **volume** pode ser aberta com escrita só para `FSCTL_LOCK_VOLUME`/
  `FSCTL_DISMOUNT_VOLUME`, quando a alça só leitura é recusada, nunca para ler ou gravar dados; fica em
  `source/windows.rs` (`lock_volume_for_dismount`), marcada, e o teste `source_has_no_write_path` confere.
- Licenças: só MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, CC0-1.0, BSL-1.0. Proibido
  GPL/AGPL/LGPL/MPL de terceiros e qualquer código copiado do PhotoRec/TestDisk. O Revenant é GPL-3.0-only.
- Erros por código: todo erro do motor tem `code() -> &'static str` em `snake_case`; nenhum texto de UI no motor.
- Rust edição 2024, `rust-version = "1.88"`. Sem `unsafe` fora de `source/windows.rs`, cada bloco com `// SAFETY:`.
- Testes nunca tocam dados reais nem discos físicos: só imagens sintéticas em `tempfile`. Duas exceções opcionais: `real_samples`, que só lê as
  amostras CC0 baixadas para `samples/cache/`; e `os_filesystems`/`os_builders` (`REVENANT_OS_FS=1`, só macOS, rodar com
  `--test-threads=1`), que usam o `hdiutil` para criar imagens FAT/exFAT numa pasta temporária e montá-las só leitura:
  nunca tocam disco físico nem dados reais, só as imagens que o próprio teste criou. Todo teste novo é visto falhando antes.
- Commits em Conventional Commits, com o trailer `Co-Authored-By`. Nunca `git add -A`.

## Ambiente

Mac Intel, Rust 1.99 instalado pelo Homebrew em `/usr/local/bin` (sem `rustup`); alvos extras só no CI. **O Homebrew não
funciona mais neste Mac** (Sérgio, 20/10/2026): nunca rodar `brew`. Ferramentas como `ddrescue` ou `cargo-deny` não se
instalam por ele; antes de compilar ou instalar qualquer ferramenta (`cargo install`, `make`, npm), conferir se o
binário já existe (`which`, `~/.cargo/bin`, `target/release`) e reaproveitar. O Node vem do `fnm`.

## Armadilhas

Regra: todo erro vira uma instrução aqui.

- O CI compila no Windows e este Mac não (sem `rustup`, sem alvo Windows). Código ou teste que usa API só de Unix/macOS
  (`std::os::unix`, `hdiutil`) fica atrás de `#[cfg(target_os = "macos")]` (num arquivo de teste: `#![cfg(...)]` no
  topo). Antes do PR, `grep -rn "os::unix" crates` e conferir que cada uso está protegido. (PR #5: `os_builders.rs`
  quebrou o CI do Windows.)
