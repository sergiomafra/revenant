# Revenant — documento de projeto (versão 1.0)

**Data:** 19/10/2026 · **Dono:** Sérgio Mafra (desenvolvedor, fotógrafo/filmmaker) · **Repositório:** `github.com/sergiomafra/revenant`

Marcações: **[Sérgio]** = decidido por ele nesta conversa; **[escolha do Claude]** = decisão minha, que ele pode vetar; sem marcação = consequência direta das duas.

---

## 1. Por que existe

Quem trabalha com audiovisual perde cartão o tempo todo: SD corrompido, formatado sem querer, arquivo sobrescrito na hora de
descarregar. Em dezembro de 2025, depois de fotografar uma corrida, o Sérgio achou que tinha passado todas as fotos para o
computador, mas umas sobrescreveram as outras. Para recuperar, precisou de um programa pago. **O preço fazia a recuperação não
valer a pena**: ele gastaria mais recuperando do que ganharia com as fotos.

Do programa pago, ele gostou da **qualidade da recuperação** ("espetacular"). Odiou duas coisas:
1. **A prévia**: mostravam pouco e mal o que podia ser recuperado (provavelmente para evitar print).
2. **A organização dos arquivos na tela**: "horrível". Ele quer algo **fácil de visualizar**.

O Revenant é a resposta: um app **gratuito**, para Mac e Windows, que **qualquer pessoa usa sem terminal**, com a recuperação no
nível dos melhores e uma tela de resultados que dá gosto de olhar.

**Sucesso**, para a 1.0: um fotógrafo que formatou ou corrompeu um SD abre o Revenant, sem ler nada antes, e sai com as fotos e os
vídeos de volta numa pasta, entendendo durante o processo o que é recuperável e o que não é.

## 2. Objetivos, não-objetivos e anti-objetivos

### Objetivos da 1.0
- **[Sérgio]** Recuperar **fotos e vídeos de câmera** de **cartões SD e HDs externos**: apagados, cartão formatado, sistema de
  arquivos corrompido. **[escolha do Claude]** "Cartões e discos externos" inclui também **microSD, CompactFlash, CFexpress** e
  **SSDs externos** (ex.: Samsung T7): é o que esse público usa. Só o disco do sistema fica de fora.
- **[Sérgio]** Rodar em **macOS e Windows**, em várias arquiteturas: Mac Apple Silicon (arm64) e **Intel (x86_64; a máquina de
  desenvolvimento do Sérgio é Intel)**, Windows x64 e Windows ARM64.
- **[Sérgio]** **User friendly** para quem nunca abriu um terminal. Instalador comum (.dmg; .msi/.exe).
- **[Sérgio]** **Tecnologia atual de recuperação** (detalhes na §5).
- **[Sérgio]** **Prévia de verdade** e **organização fácil de ver** (§4).
- **[Sérgio]** **Tratado como software**: versionamento, lançamentos, changelog, documentação bonita, tudo no GitHub dele.
- **[Sérgio]** **Gratuito e funcional para todos.** No futuro pode haver uma versão paga com recursos sofisticados para empresas.

### Não-objetivos da 1.0 (ficam para depois, "uma coisa de cada vez" [Sérgio])
- SSD interno e disco do sistema; documentos, áudio solto e outros tipos fora foto/vídeo de câmera; celulares; RAID; nuvem; Linux.
- Reparo de vídeo com arquivo de referência (reconstruir o índice de um MP4/MOV quebrado a partir de um clipe saudável da mesma
  câmera): **[escolha do Claude]** fica para a 1.1. É o recurso pelo qual a concorrência mais cobra e o próximo da fila.
- Versão paga, contas, telemetria.

### Anti-objetivos (se acontecer, falhou, mesmo funcionando)
- **Escrever qualquer byte no disco de origem.** Nunca, em nenhuma tela, em nenhum modo.
- Esconder, borrar ou travar a prévia. Paywall de qualquer tipo na versão gratuita.
- Prometer o que não dá: mostrar como "íntegro" um arquivo que não abre; insinuar que um SSD com TRIM tem salvação.
- Exigir terminal, linha de comando ou conhecimento técnico em qualquer passo.
- Tela poluída ou "cara de programa de técnico". A régua é a da Apple de Steve Jobs [Sérgio, herdado do Clustory].
- Depender de código cuja licença impeça a futura versão paga (ver §7).

## 3. O caminho do usuário

Mockups aprovados (fora do git): `.superpowers/brainstorm/*/content/fluxo.html` e `resultados-v2.html`. **[Sérgio]** O app é
**em janela horizontal** (paisagem, como um app de fotos de desktop); os mockups do fluxo foram desenhados em pé só para caberem
lado a lado: o layout real é horizontal.

1. **Escolha o disco.** Discos conectados em cartões grandes (nome, capacidade, marca, sistema de arquivos). SD e HD externo em
   destaque. O disco do sistema aparece apagado. Mídia de estado sólido (CFexpress, SSD externo) fica habilitada, com um aviso de
   cautela, sem bloquear ("Em SSD, arquivos apagados podem sumir em minutos. Saiba por quê", link para a documentação). Dica fixa: "Pare de usar o cartão agora. Não grave nada nele."
2. **Permissão.** Mac: explica em palavras simples por que precisa de acesso ao disco e leva ao lugar certo dos Ajustes, ou pede
   a senha de administrador. Windows: o pedido de administrador. Nunca pede para digitar comando.
3. **Cópia de segurança, opcional** **[escolha do Claude]**. "Copiar o cartão primeiro (mais seguro)" × "Procurar direto (mais
   rápido)". A cópia é uma imagem do disco feita com leitura tolerante a setores ruins (passada rápida pulando erros, depois
   volta nos buracos); a busca roda na imagem. Recomendado automaticamente quando a leitura dá erro.
4. **Procurando.** Progresso, tempo restante, contadores (fotos, vídeos). **A galeria vai se enchendo durante a busca**; dá para
   olhar e marcar antes de terminar. Pausar e retomar. **[escolha do Claude]** Fechar o app não perde a busca: o resultado fica
   salvo e reabre.
5. **Resultados** (§4).
6. **Recuperar.** "Onde salvar?" O app **recusa** salvar no próprio disco de origem (aviso explicando por quê). Fim: "1.284
   arquivos recuperados", resumo (íntegros, parciais, danificados), "Abrir pasta". **[escolha do Claude]** Os arquivos
   recuperados mantêm o nome original quando ele existe; senão, um nome legível por data e câmera (`2025-12-14_07-12-05_A7IV.ARW`).

## 4. A tela de resultados [Sérgio]

Uma tela, **três visualizações**, com um seletor na barra de cima:
- **Galeria** (padrão): miniaturas grandes, estilo Fotos da Apple, agrupadas por sessão de fotos.
- **Linha do tempo** (intermediária): histograma de arquivos por dia; clicar no dia mostra a grade daquele dia.
- **Detalhes** (para quem quer mais): lateral de grupos, lista compacta, **prévia grande em resolução total** com EXIF (câmera,
  lente, ISO, data).

Regras:
- **Selecionar tudo por grupo** (dia, sessão ou pasta), com estado parcial e contagem ("412 arquivos · 24 selecionados").
- **Agrupar por data ou por pasta.** "Por pasta" só existe quando a estrutura original sobreviveu (arquivo apagado sem formatar);
  senão aparece desligado com o motivo ("Pastas não encontradas: o cartão foi formatado").
- Todo arquivo tem **prévia real** e um **selo de estado** com ícone + texto: **Íntegro** (abre por inteiro), **Parcial** (abre,
  mas falta um pedaço: foto cortada, vídeo que trava; recupera-se assim mesmo, com aviso), **Danificado** (não abre).
- Filtros Tudo / Fotos / Vídeos; busca por nome.
- Barra fixa embaixo: "Recuperar 24 arquivos (3,1 GB)", atualizada ao vivo.
- **[escolha do Claude]** O app lembra a última visualização usada.
- **[Sérgio]** Requisitos vão crescer com o uso: "vamos desenvolvendo e, conforme as necessidades aparecerem, a gente vai criando
  e modificando".

## 5. O motor de recuperação

**[escolha do Claude]** Escrito do zero, em **Rust**, como uma biblioteca independente da interface. Nenhum código de PhotoRec,
TestDisk ou outro GPL (ver §7). A interface só conversa com o motor por uma API.

Duas estratégias que rodam juntas e se completam:

1. **Leitura do sistema de arquivos** (rápida, guarda nomes e pastas). Na 1.0: **exFAT e FAT32** (o que câmeras usam em SD).
   Acha entradas de diretório apagadas, reconstrói cadeias de clusters (exFAT: arquivos contíguos marcados `NoFatChain` e o
   bitmap de alocação; FAT32: FAT + heurística de contiguidade, já que apagar zera a cadeia), detecta formatação rápida e
   procura vestígios das estruturas antigas.
2. **Escultura de arquivos por assinatura** ("carving", funciona em disco formatado ou corrompido e em qualquer sistema de
   arquivos, inclusive NTFS, APFS e HFS+ de HDs externos). Varre o disco inteiro por cabeçalhos e **entende a estrutura interna
   de cada formato** para achar o fim exato e detectar fragmentação, em vez de cortar por tamanho fixo:
   - Fotos: JPEG, HEIC/HEIF, DNG, **CR2, CR3** (Canon), **NEF** (Nikon), **ARW** (Sony), **RAF** (Fujifilm), ORF/ORI
     (OM/Olympus), RW2 (Panasonic).
   - Vídeos: **MP4/MOV** (ISO BMFF; inclui XAVC da Sony, Canon, GoPro, DJI, iPhone), com tratamento do caso comum de câmera que
     grava `mdat` antes do `moov`. **[escolha do Claude]** MXF e BRAW ficam para depois.
   - Escultura com fragmentação: para formatos com estrutura validável, tentar remontar arquivos fragmentados em 2+ pedaços
     (validação por decodificação/estrutura nos pontos de quebra), não só arquivos contíguos.

Junto disso:
- **Deduplicação**: o mesmo arquivo achado pelas duas estratégias aparece uma vez, preferindo a versão com nome/pasta.
- **Prévia e metadados**: para RAW, a **maior** JPEG embutida + EXIF (o app não revela o RAW; "resolução total" na
  visualização Detalhes quer dizer a maior prévia que a câmera gravou) **[escolha do Claude]**; para vídeo, um quadro e a duração. **[escolha do Claude]** A
  decodificação de vídeo usa os decodificadores do próprio sistema (AVFoundation no Mac, Media Foundation no Windows), para não
  carregar FFmpeg e problemas de licença/patente. Sem o decodificador no sistema (HEVC/HEIF no Windows sem a extensão
  instalada), mostra a miniatura gravada no próprio arquivo quando houver e um estado "Sem prévia neste PC" com a explicação e
  o link da extensão gratuita do fabricante. O estado (Íntegro/Parcial/Danificado) não depende do decodificador: vem da
  validação estrutural.
- **Estado** (Íntegro/Parcial/Danificado): validação real do arquivo (JPEG decodifica até o fim; estrutura do RAW coerente; caixas
  do MP4 consistentes e amostras dentro do arquivo), não um palpite.
- **Agrupamento**: sessões inferidas por data/hora do EXIF (intervalo entre fotos) e câmera (modelo + número de série quando houver).
- **Somente leitura, sempre**: o disco de origem é aberto só para leitura no nível do sistema operacional; o código do motor não
  tem caminho de escrita para ele. Teste automatizado garante isso. **[escolha do Claude]** O próprio sistema grava no cartão
  montado (Spotlight, `.fseventsd`, System Volume Information, a marca de "volume sujo"). Antes de procurar, o app **desmonta o
  volume sem ejetar** e lê o aparelho bruto, explicando isso numa frase; se não conseguir desmontar, segue com o volume montado
  e avisa. O guia ensina a não abrir o cartão no Finder/Explorer depois do problema. O teste de "nenhuma escrita" confere também
  os bytes do aparelho (imagem de teste) antes e depois de uma busca.
- **Desempenho**: leitura sequencial em blocos grandes, análise em paralelo; um SD de 64 GB deve ser varrido no tempo de leitura
  do leitor de cartão, não muito mais. Uso de memória limitado mesmo em HD de vários TB.

## 6. Como é feito [escolha do Claude, aceito pelo Sérgio: "deixo você escolher a tecnologia"]

- **Motor:** Rust (biblioteca `revenant-core`), mais um pequeno executável de linha de comando **só para testes e
  desenvolvimento**, nunca exigido do usuário.
- **App:** **Tauri 2** (janela nativa, instaladores leves). Interface com a mesma pilha do Clustory, que o Sérgio já conhece:
  React 19, Vite, TypeScript, Tailwind 4, Radix, react-i18next, ícones Lucide.
- **Acesso ao disco bruto:** precisa de privilégio de administrador nos dois sistemas. Como o app **não terá assinatura paga**
  (§8), não dá para usar o instalador de ajudante privilegiado da Apple (que exige app assinado). Caminho previsto: um processo
  ajudante pequeno, só de leitura, iniciado com a senha de administrador do sistema (Mac) ou elevação UAC (Windows), conversando
  com o app por canal local. O construtor valida a melhor forma em cada sistema; o requisito é que o usuário só veja o pedido de
  senha/permissão padrão do sistema.
- **Gostos herdados do Clustory [Sérgio]:** temas claro/escuro/automático; **cor só por tokens**; contraste WCAG AA; alertas com
  ícone + texto; **ícones Lucide, nunca emoji**; componentes de interface reutilizáveis num lugar só; **três idiomas desde o início:
  pt-BR, es, en**, nenhum texto de interface fora do catálogo; o motor devolve **códigos** de erro e a interface traduz.
- **Testes:** imagens de disco sintéticas geradas por script (exFAT/FAT32 com arquivos apagados, formatados, fragmentados,
  truncados) com o resultado esperado conhecido; amostras reais de cada formato de câmera (licença que permita uso em teste);
  varreduras de convenção (tokens, ícones, i18n, "nenhuma escrita na origem"). CI roda em Mac e Windows.

## 7. Licença e o futuro pago

Para a versão paga continuar possível, todo o código é do Sérgio ou de dependências com licença permissiva (MIT, Apache-2.0,
BSD, Zlib e afins). **Proibido:** GPL/AGPL/LGPL em código embutido (PhotoRec, TestDisk, FFmpeg com partes GPL, etc.). Uma
varredura de licenças das dependências roda no CI (`cargo-deny` e o equivalente do npm).

**[Sérgio, 19/10/2026]** Licença do Revenant gratuito: **GPL-3.0**, com o Sérgio como único dono dos direitos (ele
pode lançar a versão paga com outra licença; um concorrente não pode fechar um fork). Contribuições de terceiros, se aceitas,
exigem um termo de cessão (CLA), para não travar a versão paga.

## 8. Software de verdade: versão, lançamento, distribuição

- **[Sérgio]** O repositório `sergiomafra/revenant` **já é público**: GitHub Pages, Releases para o atualizador, minutos de CI
  gratuitos (inclusive Mac e Windows ARM) e o pedido à SignPath dependem disso.
- **Versionamento semântico** (1.2.3). **CHANGELOG.md** em português, voltado ao usuário. Commits no padrão Conventional Commits
  para gerar o changelog e o número da versão. **[escolha do Claude]**
- **Lançamento automático:** tag `vX.Y.Z` → GitHub Actions compila e publica nos **Releases** os instaladores: Mac arm64, Mac
  x86_64 (ou um universal), Windows x64, Windows ARM64.
- **Atualização dentro do app:** o app avisa que há versão nova (atualizador do Tauri, com a chave de assinatura própria dele,
  gratuita).
- **Sem assinatura paga [Sérgio: "não tenho dinheiro"]:**
  - Mac: assinatura **ad-hoc** (gratuita; sem ela o app nem abre em Apple Silicon). A documentação ensina a abrir passando pelo
    alerta (Ajustes do Sistema → Privacidade e Segurança → "Abrir Mesmo Assim"), com imagens.
  - Windows: o guia ensina a passar pelo SmartScreen ("Mais informações → Executar assim mesmo"). **[escolha do Claude]** Pedir
    assinatura gratuita para código aberto à SignPath Foundation depois que o projeto estiver público.
- **Processo como o do Clustory [Sérgio]:** um módulo por vez; spec → plano → execução por subagentes; telas mostradas antes de
  construir; o Sérgio decide o merge. Um `CLAUDE.md` enxuto no repositório com onde o desenvolvimento está, diretrizes e
  armadilhas.

## 9. Documentação [Sérgio: "toda bonitinha", "sempre atualizar e explicar como usar"]

- **Site no GitHub Pages**, em pt-BR, es e en. **[escolha do Claude]** Feito com Astro Starlight (bonito, busca e idiomas prontos).
- **Para quem usa** (prioridade): instalar no Mac e no Windows (passando pelo alerta, com imagens); dar a permissão; recuperar
  passo a passo com capturas de tela; o que significa Íntegro/Parcial/Danificado; **"Perdi meu cartão: o que fazer (e não fazer)
  agora"**; por que SSD é diferente (TRIM); perguntas frequentes; formatos e câmeras suportados.
- **Para quem desenvolve:** arquitetura, como o motor funciona, como compilar, como lançar versão.
- **README** do repositório: o que é, download, link para o site. README por pacote dizendo o que a **versão atual** faz.
- **Regra fixa [Sérgio]:** toda mudança no app atualiza a documentação **na mesma entrega** (mesmo PR). Fica escrita no
  `CLAUDE.md` do projeto.

## 10. O que fica para o construtor

- A divisão em módulos/partes e a ordem de construção (sugestão: motor + testes primeiro; depois app e fluxo; depois lançamento
  e site). Cada módulo passa por telas aprovadas antes de ser construído.
- Desenho interno do motor (formatos de índice, paralelismo, formato do arquivo de sessão salvo).
- Mecânica exata do ajudante privilegiado em cada sistema, respeitando a §6.
- Escolha das bibliotecas, respeitando a §7.
- Identidade visual (logo, ícone, nome do tema), a ser mostrada ao Sérgio antes de construir.
