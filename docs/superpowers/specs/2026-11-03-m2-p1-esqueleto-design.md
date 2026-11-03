# Revenant 2.1: esqueleto do app (documento de projeto)

Data: 03/11/2026. Parte 2.1 do Módulo 2 (`docs/superpowers/roadmap.md`). Spec do produto:
`docs/superpowers/specs/2026-10-19-revenant-design.md` (§3 o caminho, §6 como é feito, §8 sem assinatura paga). Telas:
`docs/superpowers/mockups/m2/` (aprovadas na 2.0, `docs/superpowers/specs/2026-11-02-m2-p0-telas-identidade-design.md`).

## 1. O que é e por quê

O motor está pronto e as telas estão desenhadas. A 2.1 é a primeira versão do app de verdade: a janela, os temas, os
três idiomas, a ponte com o motor e a permissão de administrador. **[Sérgio]** No fim dela, o Sérgio abre o app no Mac e
vê os HDs, SSDs e cartões conectados, para escolher um. Por isso a tela 1 (Escolher o disco), que estava na 2.2, entra
aqui. O Sérgio aceitou a ideia de ir "evoluindo aos poucos": o app só fica funcional por inteiro no fim da 2.3 (marco B).
Até lá, cada parte já trabalha com dados reais, nada de tela de faz de conta.

**Sucesso:**
1. O Sérgio dá dois cliques no `Revenant.app` (sem terminal). O app abre na lista de discos, com os discos que estão
   conectados de verdade, e a lista muda sozinha quando ele pluga ou tira um cartão.
2. Ele escolhe o cartão, o Mac pede a senha de administrador na janela do próprio sistema e o app confirma que consegue
   ler o cartão. Não lê nada além disso e não escreve nada.
3. Ele troca o tema e o idioma nos Ajustes, fecha e abre o app, e a escolha continua lá. Ele vê o Sobre e, se algo der
   errado, a tela de erro inesperado.

## 2. Objetivos, não-objetivos e anti-objetivos

**Objetivos**
- O projeto Tauri 2 + React com a pilha da spec do produto (§6): React 19, Vite, TypeScript, Tailwind 4, Radix,
  react-i18next e ícones Lucide.
- As telas 1 (Escolher o disco), 3 (Permissão), 11 (Ajustes), 12 (Sobre) e 13 (Erro inesperado), iguais aos mockups,
  nos dois temas e nos três idiomas.
- A ponte com o motor: o app chama o `revenant-core`. Cada erro chega à interface como **código**, e a interface traduz
  o código.
- O acesso de leitura ao disco com a permissão padrão do sistema: a senha de administrador no Mac e o UAC no Windows.
- Um `Revenant.app` que o Sérgio abre com dois cliques, gerado por um comando documentado.
- O CI compila e testa o app no Mac e no Windows, e confere a licença das dependências do npm.

**Não-objetivos**
- Retomar uma busca, a conferência prévia, a cópia, a busca e os avisos dela: ficam para a 2.2.
- Os resultados e a recuperação: ficam para a 2.3.
- O instalador `.dmg`/`.msi`, a assinatura e a atualização automática: ficam para o Módulo 3.
- O Linux e o app no celular.

**Anti-objetivos (se acontecer, falhou)**
- Qualquer escrita no disco escolhido. O app só abre o disco para leitura; nesta parte nem desmonta.
- O usuário digitar um comando, ou ver um pedido de senha que não seja o do sistema.
- Texto de tela vindo do motor, um texto fora do catálogo de idiomas, uma cor fora dos tokens, um emoji.
- Um ajudante privilegiado instalado de forma permanente no computador.
- O app travar, ou a janela congelar, enquanto lista os discos ou espera a senha.

## 3. As telas desta parte

A referência é o mockup. Mudar uma tela começa por mudar o mockup (o `node scripts/check-mockups.mjs` tem de passar).
Os textos em pt-BR dos mockups viram o catálogo pt-BR; as versões es e en são novas e saem nesta parte.

| # | Tela | Mockup | O que entra na 2.1 |
| --- | --- | --- | --- |
| 1 | Escolher o disco | `inicio.html#tela-1` | Os três estados: a lista, nenhum disco e o SSD escolhido (aviso de cautela). Com os dados reais do motor (§4). |
| 3 | Permissão | `inicio.html#tela-3` | Mac, Windows e recusada. O pedido é o do sistema (§5). |
| 11 | Ajustes | `app.html#tela-11` | Tema e idioma, guardados entre aberturas. |
| 12 | Sobre | `app.html#tela-12` | Logo, versão (a do workspace), licença e links. |
| 13 | Erro inesperado | `app.html#tela-13` | O padrão para todo código sem tela própria. |

- **O que vem depois da permissão.** Na 2.2, a conferência prévia começa sozinha. Na 2.1, uma tela provisória diz, em
  palavras simples, que o Revenant consegue ler o disco escolhido (com o nome e a capacidade), que a busca chega na
  próxima versão, e oferece Voltar. **[escolha do Claude]** Ela usa os componentes das outras telas, não ganha mockup
  próprio (some na 2.2) e não lê nada além do necessário para provar o acesso (§5).
- **O passo 3 da permissão** ("A conferência do cartão começa sozinha") continua como no mockup: é o texto da 2.2 em
  diante. Na 2.1 ele fica igual, e a tela provisória explica o resto. **[escolha do Claude]**
- **Idioma inicial:** o do sistema, quando é pt, es ou en; qualquer outro abre em inglês. **[Sérgio aceitou]** O pt-PT
  vai para o pt-BR e qualquer variante do espanhol vai para o es. **[escolha do Claude]**
- **Tema inicial:** automático (segue o sistema).
- **O mapa do `index.html`** passa a tela 1 para a 2.1, e o roteiro também.

## 4. A lista de discos (tela 1)

Vem de `DeviceOps::list` (`crates/revenant-core/src/device/mod.rs`), que já existe e, no Mac, não pede senha.

- **Em destaque:** cartões e discos removíveis ou externos (`removable` ou não `internal`), que não são do sistema.
- **Apagado, com o motivo:** o disco do sistema (`system`). Não dá para escolher.
- **Habilitado com aviso de cautela:** estado sólido (`solid_state == Some(true)`) que não é do sistema: CFexpress, SSD
  externo. É o estado "SSD escolhido" do mockup, com o "Saiba por quê".
- **Outro disco interno que não é do sistema:** aparece, habilitado, sem destaque.
- **Imagens de disco anexadas pelo sistema** (`virtual_disk`, como o `.dmg` de um instalador): não aparecem.
  **[escolha do Claude]** Elas confundiriam a lista. Se um dia for preciso recuperar de uma imagem, isso entra com
  "abrir um arquivo de imagem".
- **Nome no cartão:** o rótulo do volume quando existe, senão a marca e o modelo; capacidade em GB/TB legíveis; o sistema
  de arquivos (o mockup mostra a etiqueta).
- **A linha de baixo e o ícone de cada cartão [escolha do Claude]:** o mockup mostra "ligado por USB", "parece um
  cartão de câmera" e um ícone por tipo (cartão SD, pen drive, HD). O `DiskInfo` não sabe por onde o disco está ligado,
  então o motor ganha esse campo (USB, cartão SD, Thunderbolt, interno, desconhecido), lido do `diskutil` no Mac e do
  tipo de barramento no Windows, com os testes dos parsers nos dois. "Parece um cartão de câmera" aparece quando um
  volume montado do disco tem a pasta `DCIM` na raiz (só olhar a pasta montada, nunca ler o disco). Sem informação, a
  linha mostra só a marca.
- **Atualização sozinha:** quando um disco é conectado ou tirado, a lista muda em até uns 3 segundos, sem piscar e sem
  perder a seleção do disco que continua lá. O jeito (consultar de tempos em tempos ou ouvir o sistema) é do construtor.
  Listar nunca trava a janela.
- **Erro ao listar:** a tela de erro inesperado, com o código. Nenhum disco é o estado "nenhum disco", não um erro.

## 5. A permissão e o acesso ao disco

A spec do produto (§6) pede um processo pequeno, só de leitura, aberto com o pedido padrão do sistema, sem o ajudante
instalado da Apple (que exige assinatura paga). Na 2.1:

- **Mac [escolha do Claude]:** o `/usr/libexec/authopen`, que já vem no macOS, mostra o pedido de senha do próprio sistema
  e abre o disco **só para leitura**. Depois passa o arquivo aberto ao app. Assim não há nada instalado de forma
  permanente; o app lê o disco pelo arquivo que recebeu. **[Sérgio, 03/11/2026]** Receber o arquivo aberto exige
  `unsafe`: fica num único arquivo do Mac no motor, a segunda exceção depois de `source/windows.rs`, com cada bloco
  marcado com `// SAFETY:` e um teste de guarda como o `source_has_no_write_path`. O `CLAUDE.md` passa a dizer isso.
- **Uma senha por escolha [escolha do Claude]:** na 2.1, cada vez que o Sérgio escolhe um disco, o Mac pede a senha
  (voltar e escolher de novo pede outra vez). Manter o acesso aberto durante a busca toda, para não pedir de novo ao
  retomar ou depois de o cartão sair e voltar, é desenho da 2.2. Se ficar provado que esse caminho não serve, o construtor volta
  ao que a spec do produto prevê (um processo ajudante iniciado com a senha) e registra o porquê.
- **Windows:** um processo pequeno, iniciado com o pedido do UAC, abre o disco só para leitura e serve o app. Ele pode,
  por exemplo, passar ao app uma alça só de leitura, ou responder leituras por um canal local. O jeito é do construtor,
  dentro das regras do `CLAUDE.md` (sem escrita, `unsafe` só em `source/windows.rs` ou no módulo equivalente do app,
  cada bloco com `// SAFETY:`).
- **Prova do acesso:** o app lê o tamanho do disco e o primeiro setor pelo acesso recebido e mostra a tela provisória
  (§3). Nada é guardado e nada é escrito. Depois o acesso é fechado.
- **Recusada ou cancelada:** o estado "Recusada" da tela 3 (`permission_denied`), com Tentar de novo.
- **O disco saiu ou mudou entre a escolha e a senha:** volta à lista com o aviso correspondente (`device_not_found`).
  O texto do aviso é do construtor.
- **O motor ganha uma forma de ler um disco já aberto por outro processo**, se ainda não tiver. Ela continua sem nenhum
  método de escrita, e o teste `source_has_no_write_path` continua valendo.

## 6. Como é feito

- **Onde fica [escolha do Claude]:** o app em `app/` (a interface) e o lado Rust do Tauri como um crate do workspace
  (`crates/revenant-app`, ou em `app/src-tauri` incluído no workspace; o construtor escolhe um e documenta). Rust edição
  2024 e MSRV 1.88, como o resto. Se uma dependência do Tauri exigir um Rust mais novo, o construtor para e registra; não
  sobe a MSRV sozinho.
- **Componentes num lugar só:** os tokens de `docs/superpowers/mockups/m2/tokens.css` viram os tokens do app, com os
  mesmos nomes. Os componentes do Kit dos mockups (cartão, aviso, botão, logo, "Detalhes técnicos") viram componentes
  React reutilizáveis.
- **Idiomas:** um catálogo por idioma (pt-BR, es, en). Cada código de erro que a 2.1 mostra tem texto nos três. Uma
  varredura no CI falha se houver texto visível fora do catálogo, chave faltando num idioma, cor fora dos tokens ou emoji
  (como o `check-mockups.mjs` faz com os mockups).
- **Ajustes guardados** numa pasta de configuração do app, no computador do usuário.
- **Ícone:** gerado pelo `tauri icon` a partir de `assets/brand/app-icon-1024.png` (Mac) e `app-icon-flat-1024.png`
  (Windows). Os de 16 e 32 px usam os PNGs pequenos já desenhados.
- **Dependências [Sérgio, 03/11/2026: "se o MPL-2.0 for a solução, eu aceito"]:** a pilha (Tailwind 4 pelo
  `lightningcss`, o Tauri por `cssparser`, `selectors`, `option-ext` e outros) traz peças MPL-2.0. A regra passa a ser:
  - o que vai dentro do app: as licenças do `CLAUDE.md` e mais a **MPL-2.0**, só em dependência usada sem modificação. A
    MPL vale por arquivo, é compatível com a GPL-3.0 e não impede a versão paga. GPL, AGPL e LGPL continuam proibidas;
  - o que só serve para montar o app e não vai dentro dele (ferramentas do npm, dados como o `caniuse-lite`): qualquer
    licença aberta, menos GPL e AGPL. **[escolha do Claude]**
  - o `deny.toml`, o `CLAUDE.md` e uma conferência de licença do npm no CI (como o `cargo-deny`) passam a dizer isso. Instalar pacotes do npm é permitido nesta parte (é o projeto do app), mas o `brew` continua proibido.
- **Abrir o app:** um comando documentado gera o `Revenant.app` (sem assinatura paga; a ad-hoc, se o Tauri exigir). Para
  desenvolver, o modo de desenvolvimento do Tauri.
- **Testes:**
  - Rust: a ponte (os comandos que a interface chama) é testada com um `DeviceOps` falso, sem disco nenhum. O acesso de
    leitura é testado com um arquivo temporário no lugar do disco.
  - Interface: os estados das telas (lista com cada tipo de disco, nenhum disco, recusada, erro) com o motor falso, em
    pt-BR, es e en.
  - Testes nunca tocam disco físico. A conferência real (`authopen` com um cartão de verdade, UAC no Windows) fica num
    roteiro manual para o Sérgio, em `docs/superpowers/history/`. O Windows é compilado e testado no CI; a conferência
    com as mãos é no Mac. **[escolha do Claude]** O Sérgio não tem um PC à mão.
- **Documentação:** um README em `app/` (como rodar, como gerar o `.app`, como funciona a permissão), o README do
  repositório e o `CHANGELOG.md` (`## [Unreleased]`).

## 7. O que fica para o construtor

- A estrutura das pastas do app e dos componentes, o roteador entre as telas e o jeito de guardar os ajustes.
- Como a lista se atualiza sozinha (§4).
- Os detalhes do canal com o processo do Windows (§5).
- Os textos es e en (tom calmo, frases curtas, sem jargão, como o pt-BR) e os textos que não estão nos mockups.

## 8. Aprovação

O Sérgio aprova esta spec; então vem o plano e a execução por subagentes. No fim, push e PR. O merge é do Sérgio.
