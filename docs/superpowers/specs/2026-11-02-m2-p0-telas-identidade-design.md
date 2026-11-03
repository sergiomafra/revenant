# Revenant 2.0: telas e identidade (documento de projeto)

Data: 02/11/2026. Parte 2.0 do Módulo 2 (`docs/superpowers/roadmap.md`). Spec do produto:
`docs/superpowers/specs/2026-10-19-revenant-design.md` (§3 o caminho, §4 os resultados, §6 os gostos herdados do Clustory).

## 1. O que é e por quê

O motor (Módulo 1) está pronto. Antes de escrever o app (Tauri 2 + React), o Sérgio aprova **como cada tela fica**. A 2.0
entrega isso: os mockups clicáveis de todas as telas, na horizontal, em claro e escuro, mais o logo e o ícone. Nenhuma
linha do app sai nesta parte. Com tudo aprovado, as Partes 2.1 a 2.3 só constroem o que já está desenhado, e cada uma
aponta para o mockup da sua tela.

**Sucesso:** o Sérgio abre os mockups, passa pelo caminho inteiro (do cartão aos arquivos de volta) e por cada aviso, nos
dois temas, e não acha uma tela que falte nem uma que "pareça programa de técnico".

## 2. Objetivos, não-objetivos e anti-objetivos

**Objetivos**
- Todas as telas da §4 abaixo, em HTML clicável, janela horizontal, claro e escuro.
- O logo (§3) como SVG final, em arquivos, mais o ícone do app pronto para ser convertido no Mac e no Windows.
- Um mapa: cada tela → a Parte que a constrói (2.1, 2.2 ou 2.3) e os códigos do motor que ela mostra.

**Não-objetivos**
- Código do app, projeto Tauri, componentes React. (2.1)
- Textos finais dos avisos nos 3 idiomas: os mockups trazem o texto em pt-BR, que vale como rascunho e pode mudar até
  a 2.3. Os catálogos es/en são da 2.1 em diante.
- Telas do site de documentação (Módulo 4).
- Converter o ícone para `.icns`/`.ico`: o `tauri icon` faz isso na 2.1 a partir do PNG mestre.

**Anti-objetivos (se acontecer, falhou)**
- Uma tela poluída ou com "cara de programa de técnico" (régua da Apple, spec §2).
- Mostrar código de erro cru (`device_gone`) ou jargão (setor, cluster, carving, FAT) na frente de quem usa. O código pode
  aparecer só num "Detalhes técnicos" recolhido.
- Qualquer tela que sugira gravar algo no cartão de origem, ou que deixe escolher o cartão como destino.
- Cor fora dos tokens, emoji no lugar de ícone, alerta só com cor (sem ícone + texto).

## 3. Identidade [Sérgio]

**Logo escolhido:** o anel com a seta que dá a volta (o "desfazer": o arquivo volta), com o R em três pedaços dentro (o
arquivo partido que volta inteiro). Dourado em dois pontos: a **ponta da seta** e a **perna do R**, o pedaço que chega.
O resto na cor principal.

Geometria final (viewBox 0 0 100 100, traços com ponta e junção arredondadas; `currentColor` é a cor principal, `--d` o
dourado):

```svg
<!-- tamanho normal (≥ 48 px) -->
<path d="M17.1 31 A38 38 0 1 0 48 12.05" stroke-width="6"/>
<polyline points="56 4 48 12 56 20" stroke-width="6" stroke="var(--d)"/>
<g transform="translate(50 51) scale(.58) translate(-52 -51)" stroke-width="12">
  <path d="M32 42 V18 H52 A16 16 0 0 1 52 50 H44"/>
  <path d="M32 62 V82"/>
  <path d="M58 62 L72 84" stroke="var(--d)"/>
</g>

<!-- tamanho pequeno (32 e 16 px): traço mais grosso, R inteiro, porque os vãos somem abaixo de 1 px -->
<path d="M19 33 A36 36 0 1 0 47 14.2" stroke-width="9"/>
<polyline points="57 5 47 14 57 23" stroke-width="9" stroke="var(--d)"/>
<g transform="translate(50 51) scale(.56) translate(-52 -51)" stroke-width="17">
  <path d="M32 84 V18 H52 A16 16 0 0 1 52 50 H32"/>
  <path d="M54 56 L70 84" stroke="var(--d)"/>
</g>
```

**Cores do logo:**

| Uso | Principal | Dourado | Fundo |
| --- | --- | --- | --- |
| Ícone do app (ladrilho) | `#f6f3ec` | `#e9c27a` | `#2f5d50` |
| Sobre fundo claro | `#2f5d50` | `#b9822a` | `#f6f3ec` |
| Sobre fundo escuro | `#7fb7a4` | `#e2b45e` | `#17161a` |

**Arquivos a entregar** (no repositório, `assets/brand/`, nomes em inglês):
- `symbol.svg` e `symbol-small.svg`: só o desenho, em `currentColor` + dourado por variável, para uso no app.
- `app-icon.svg`: o ladrilho com o símbolo, no grid de ícone do macOS (forma arredondada de 824 px centrada em 1024, com
  a sombra padrão); `app-icon-1024.png` exportado dele, mestre do ícone do Mac.
- `app-icon-flat.svg` e `app-icon-flat-1024.png`: o mesmo ladrilho ocupando o quadro inteiro, **sem sombra**, mestre do
  Windows [escolha do Claude: sombra embutida fica borrada no Windows e rouba área nos tamanhos pequenos].
- `app-icon-16.png` e `app-icon-32.png`: exportados com a geometria pequena, para a 2.1 pôr no lugar dos que o
  `tauri icon` gera desses tamanhos [escolha do Claude].
- `wordmark-light.svg` e `wordmark-dark.svg`: símbolo + "Revenant".
- Um `README.md` curto em `assets/brand/` com as cores, os tamanhos mínimos e quando usar a versão pequena.

O ícone usa o ladrilho também no Windows (o Windows não impõe forma; o ladrilho arredondado é aceito e é o mesmo nos dois).

**Paleta e tipografia do app:** as dos mockups aprovados (`resultados-v2.html`), agora como tokens nomeados que a 2.1
copia: papel `#f6f3ec`/`#17161a`, superfície, linhas, tinta em 3 níveis, verde principal `#2f5d50`/`#7fb7a4`, e as cores
de estado (Íntegro verde, Parcial âmbar, Danificado vermelho, cada uma com fundo suave). O dourado do logo entra como
token de marca, usado só na identidade (logo, ícone, tela Sobre), nunca como cor de estado (o âmbar do Parcial é outro
token). Fonte do sistema (SF no Mac, Segoe UI no Windows). Ícones Lucide.

## 4. As telas

Janela horizontal, tamanho de referência 1280×800, mínimo 1024×700 [escolha do Claude]. Cada tela em claro e escuro.
Moldura de janela do Mac nos mockups; a do Windows aparece onde a tela muda de verdade entre os sistemas (permissão).

O caminho e a tela de resultados seguem os mockups aprovados no início (`fluxo.html`, `resultados-v2.html`), **sem
mudanças de conteúdo** [Sérgio: "por enquanto nada"]: só passam para a horizontal e ganham o tema escuro desenhado (não só
o automático). As telas novas cobrem o que o motor das Partes 1.5 e 1.6 entrega e ainda não tinha desenho [lista aprovada
pelo Sérgio].

A ordem do caminho é a da spec do produto (§3): disco → permissão → cópia opcional → procurando → resultados → recuperar.
As telas novas entram nela sem mudar a ordem [escolha do Claude]: "Retomar" aparece ao abrir o app, antes do disco; a
conferência prévia vem logo depois da permissão e antes da escolha da cópia; "O cartão saiu" e os avisos aparecem por
cima da tela em que acontecem.

A moldura do Windows aparece só na permissão (tela 3), porque é ali que o pedido muda de verdade. Os nomes de disco e
pasta seguem o padrão do Mac nos mockups; a diferença de nomes (`D:\`) é da 2.2 [escolha do Claude].

| # | Tela | Estados a desenhar | Códigos do motor | Parte |
| --- | --- | --- | --- | --- |
| 1 | **Escolher o disco** | Lista de discos em cartões grandes (nome, capacidade, marca, sistema de arquivos); SD e HD externo em destaque; disco do sistema apagado com o motivo; SSD/CFexpress habilitado com aviso de cautela e "Saiba por quê"; nenhum disco ("Conecte o cartão"); dica fixa "Pare de usar o cartão agora". | `system_disk` | 2.2 |
| 2 | **Retomar a busca** | Ao abrir, uma busca salva: "A busca no Cartão SD 64 GB parou em 42%. Continuar ou começar de novo?"; e o caso da sessão de outro cartão. | `session_other_source`, `session_corrupt` | 2.2 |
| 3 | **Permissão** | Mac: explicação simples + pedido da senha de administrador do sistema; Windows: explicação + o pedido do UAC; recusada ("Sem a permissão não dá para ler o cartão", tentar de novo). | `permission_denied` | 2.1 |
| 4 | **Conferência prévia** | "Conferindo o cartão…" (curto); tudo certo (segue sozinho); com problemas: **uma lista só** de tudo o que já se sabe estar errado, cada item com ícone + texto + o que fazer. Desenhar com 2 itens ao mesmo tempo. | `device_not_found`, `device_not_responding`, `sample_unreadable`, `open_failed`, `unmount_failed`, `destination_too_small`, `destination_on_source`, `destination_unknown`, `destination_file_too_large` | 2.2 |
| 5 | **Cópia de segurança** | A escolha "Copiar o cartão primeiro (mais seguro)" × "Procurar direto (mais rápido)", com a recomendação automática quando a leitura deu erro; onde salvar a cópia; a cópia em andamento (passada, velocidade, tempo restante, quanto já foi salvo, trechos ilegíveis em palavras simples); cópia concluída. | `CopyEvent::Progress`, `destination_*` | 2.2 |
| 6 | **Procurando** | Progresso, tempo restante, contadores (fotos, vídeos), galeria se enchendo, dá para marcar antes do fim; pausar e retomar; aviso "O disco está falhando: faça uma cópia agora" (com o botão); aviso discreto "O cartão está lento"; fechar o app sem perder a busca. | `DiskFailing`, `Rate` (lento) | 2.2 |
| 7 | **O cartão saiu** | No meio da busca ou da cópia: "O cartão foi desconectado. Nada se perdeu: conecte de novo para continuar." Espera e retoma sozinho quando volta. | `device_gone` | 2.2 |
| 8 | **Resultados** | As três visualizações aprovadas (Galeria, Linha do tempo, Detalhes), os selos Íntegro/Parcial/Danificado, "Por pasta" desligado com o motivo; nada encontrado; um arquivo sem o nome original (nome por data e câmera) mostrado como tal. | estados do motor | 2.3 |
| 9 | **Onde salvar** | Escolher a pasta; recusa no próprio cartão (explicando por quê); espaço insuficiente. | `destination_on_source`, `destination_too_small` | 2.3 |
| 10 | **Recuperando e resumo** | Progresso da recuperação; fim: "1.284 arquivos recuperados", resumo (íntegros, parciais, danificados), "Abrir pasta". | `write_failed`, `device_gone` | 2.3 |
| 11 | **Ajustes** | Tema (claro, escuro, automático), idioma (pt-BR, es, en). | — | 2.1 |
| 12 | **Sobre** | Logo, versão, licença GPL-3.0, link do código e da documentação. | — | 2.1 |
| 13 | **Erro inesperado** | Um padrão único para o que não tem tela própria: o que aconteceu em palavras simples, o que fazer, e "Detalhes técnicos" recolhido com o código. | qualquer outro | 2.1 |

**Regras que valem para todas as telas**
- Todo aviso tem ícone + texto, nunca só cor; o tom é calmo ("nada se perdeu"), sem pânico.
- Todo texto que mostra um problema diz o que fazer em seguida.
- O cartão de origem nunca aparece como destino possível; nenhuma tela oferece "consertar o cartão".
- Nada de jargão na tela principal (setor, cluster, FAT, carving, sessão). "Detalhes técnicos" recolhido pode ter.

## 5. Como é entregue

- Os mockups ficam **no repositório** [escolha do Claude: as Partes 2.1 a 2.3 precisam apontar para eles, e os do início
  ficaram fora do git], em `docs/superpowers/mockups/m2/`, um HTML por grupo de telas, autocontido (CSS e JS embutidos,
  ícones Lucide como SVG embutido), mais um `index.html` que liga tudo e mostra o mapa da §4.
- Cada página tem a chave claro/escuro/automático e passa pelos estados de cada tela (os estados da coluna "Estados" da
  §4) por botões de simulação fora da janela, como no `resultados-v2.html`.
- Tokens de cor num arquivo só, `docs/superpowers/mockups/m2/tokens.css`, ligado por todas as páginas (caminho relativo,
  abre sem servidor), com os nomes que a 2.1 vai copiar [escolha do Claude]. Os valores do escuro vêm do
  `resultados-v2.html`, que já os tem; o que faltar (cores das telas novas) é derivado deles.
- Fotos de exemplo: as mesmas do Unsplash dos mockups aprovados, carregadas pela rede, com crédito "Fotos: Unsplash" no
  `index.html`. A licença do Unsplash permite esse uso sem atribuição obrigatória [escolha do Claude].
- Texto em pt-BR. Na barra de simulação de cada página, uma chave "Texto longo" aumenta os botões e títulos em ~30%
  (o tamanho típico do espanhol e do inglês), para conferir que nada quebra [escolha do Claude].
- Os mockups do início são preservados como estão; os novos os substituem como referência.

## 6. O que fica para o construtor

- O desenho exato das telas novas (2, 4, 5, 7, 9, 10 a 13), dentro da estética e das regras acima.
- A disposição horizontal das telas do fluxo que foram desenhadas em pé.
- O texto em pt-BR de cada aviso (rascunho; o Sérgio revisa na aprovação).
- Como exportar o PNG do ícone (o Chrome em modo headless está instalado neste Mac e serve).

## 7. Aprovação

O Sérgio aprova os mockups e o logo; só então a 2.0 vira PR (mockups, `assets/brand/`, esta spec e o roteiro
atualizado). A aprovação de cada tela pode vir com mudanças; elas entram antes do PR.
