# Revenant — instruções para o Claude

App gratuito (GPL-3.0) para Mac e Windows que recupera fotos e vídeos de cartões SD e discos externos. Feito pelo Sérgio.
**Leia antes de mexer:** a spec `docs/superpowers/specs/2026-10-19-revenant-design.md` e o plano em andamento em
`docs/superpowers/plans/`. Este arquivo é enxuto de propósito: uma frase desatualizada aqui é um bug.

## Onde o desenvolvimento está

**20/10/2026.** Módulo 1 (motor): Parte 1.1 (fundação e escultura) mergeada (PR #1, CI verde nas 4 arquiteturas);
Parte 1.2 (RAW e dados da foto) mergeada (PRs #2 e #3). Parte 1.3 (arquivos fragmentados e velocidade) concluída na
branch `feat/m1-p3-fragments-speed`, aguardando PR/merge; decisões e pendências em
`docs/superpowers/history/2026-10-20-m1-p3-pendencias.md`. Próxima: 1.4 (sistemas de arquivos). Roteiro até a 1.0:
`docs/superpowers/roadmap.md`. MSRV: Rust 1.88. CLI de desenvolvimento: `revenant-dev` (`bench` mede a velocidade).
Amostras reais de câmera (CC0, nunca no repositório): `scripts/fetch-samples.sh` + teste ignorado `real_samples`.

## Diretrizes fixas

- Um módulo por vez: spec → telas → plano → subagentes. O Sérgio decide o merge.
- Fim de parte = push + PR; o merge é do Sérgio.
- Código e repositório em inglês (identificadores, comentários, códigos de erro, commits, READMEs, CHANGELOG). Exceções:
  este arquivo e `docs/superpowers/**` (pt-BR).
- Documentação sempre atualizada e explicando como usar: toda tarefa que muda comportamento atualiza o README do crate e, se
  muda o que o usuário vê, o `CHANGELOG.md` (`## [Unreleased]`), no mesmo commit.
- Nunca escrever na origem: `Source` não tem método de escrita; `FileSource` abre só leitura.
- Licenças: só MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, Unicode-3.0, Unlicense, CC0-1.0, BSL-1.0. Proibido
  GPL/AGPL/LGPL/MPL de terceiros e qualquer código copiado do PhotoRec/TestDisk. O Revenant é GPL-3.0-only.
- Erros por código: todo erro do motor tem `code() -> &'static str` em `snake_case`; nenhum texto de UI no motor.
- Rust edição 2024, `rust-version = "1.88"`. Sem `unsafe` fora de `source/windows.rs`, cada bloco com `// SAFETY:`.
- Testes nunca tocam dados reais nem discos físicos: só imagens sintéticas em `tempfile`. Única exceção: o teste opcional
  `real_samples`, que só lê as amostras CC0 baixadas para `samples/cache/`. Todo teste novo é visto falhando antes.
- Commits em Conventional Commits, com o trailer `Co-Authored-By`. Nunca `git add -A`.

## Ambiente

Mac Intel, Rust do Homebrew (sem `rustup`). Nunca rodar `brew install/upgrade` de toolchain; alvos extras só no CI.

## Armadilhas

(vazio) Regra: todo erro vira uma instrução aqui.
