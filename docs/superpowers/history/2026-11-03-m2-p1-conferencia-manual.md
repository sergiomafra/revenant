# Parte 2.1 — roteiro de conferência manual do app (03/11/2026)

Para o Sérgio rodar quando quiser, no Mac (e no Windows, quando houver um PC). Os testes automáticos e as capturas de
tela não abrem o app de verdade, não pedem a senha de verdade e não escolhem disco de verdade; este roteiro confere o que
só o app aberto mostra. Nada aqui apaga ou grava no cartão: o app só lê. Os resultados vão para o fim deste arquivo.

## Antes de começar

1. Use um cartão SD (ou HD externo) **que não tenha nada de que você precise**. A 2.1 não grava nada, mas é a primeira
   vez que o app lê um disco de verdade pelo pedido de senha do sistema.
2. Na pasta do repositório: `cd app && npm ci && npm run tauri build -- --bundles app`. O app sai em
   `target/release/bundle/macos/Revenant.app`.

## No Mac

1. **Abrir com dois cliques.** Dê dois cliques em `Revenant.app` (ou `open target/release/bundle/macos/Revenant.app`).
   O Mac pode avisar que o app "não foi verificado" ou "não pode ser aberto" (não há assinatura paga nesta fase). Para
   abrir: clique com o botão direito no app, **Abrir**, e confirme **Abrir**; ou vá em Ajustes do Sistema, **Privacidade
   e Segurança**, e use **Abrir mesmo assim** ao lado do aviso. Anote qual dos dois caminhos funcionou.
2. **A janela.** Conferir que a janela aparece com o conteúdo (não em branco: a política de segurança do conteúdo é nova
   e só o app aberto prova que ela deixa a tela desenhar), com o título "Revenant" e o ícone do app. O tamanho de
   1280×800 é o da área interna; a janela inteira, com a barra de título, fica um pouco maior. Tente diminuir: ela não
   deve passar de 1024×700 para menos.
3. **A lista.** A tela "O que você quer recuperar?" mostra o cartão e o HD externo que estiverem plugados, cada um
   com nome, tamanho, sistema de arquivos e como está ligado. O disco do Mac aparece apagado, com o motivo, e não se
   deixa escolher. Se o cartão tem uma pasta `DCIM`, deve dizer "parece um cartão de câmera".
4. **Plugar e tirar.** Com o app aberto, tire o cartão (puxe, ou ejete): em até uns 2 segundos ele some da lista. Plugue
   de novo: ele volta. Sem fechar o app e sem a lista "piscar" se nada mudou. Se o cartão estava escolhido e saiu, a
   escolha some.
5. **Nenhum disco.** Com tudo desplugado, a tela diz "Conecte o cartão ou o disco".
6. **Escolher o cartão.** Clique no cartão e em **Continuar**. Aparece a tela da permissão (texto do Mac). Clique no
   botão que pede a senha.
7. **Cancelar a senha.** No pedido de senha do Mac, cancele. A tela deve mostrar o estado "Recusada", com
   **Tentar de novo**. (Foi nunca visto com o pedido de verdade: é o ponto principal deste roteiro.)
8. **Tentar de novo.** Clique em **Tentar de novo** e digite a senha de administrador. Deve aparecer a tela provisória
   "O Revenant consegue ler <nome do cartão> (<tamanho>)" e "a busca chega na próxima versão", com **Voltar**. Anote se
   o pedido de senha mostra o nome do `authopen` ou outro texto estranho.
9. **Tirar o cartão com a senha aberta.** Volte à lista, escolha o cartão, **Continuar**, deixe o pedido de senha aberto
   e tire o cartão. Digite a senha (ou cancele). O app deve voltar à lista com o aviso de que o disco não foi
   encontrado, sem travar.
10. **Tema e idioma.** Abra **Ajustes** (botão no canto superior direito de toda tela). Troque o tema (claro, escuro,
    automático) e o idioma (Português, Español, English): a tela muda na hora. Nenhum texto deve ficar em outro idioma.
11. **Fechar e abrir de novo.** Feche o app (Cmd+Q) e abra: tema e idioma continuam como você deixou.
12. **Sobre.** Abra **Sobre**: logo, versão, licença GPL-3.0, e os dois links (código e guia) abrem no navegador.
13. **Idioma do sistema.** (Opcional.) Em Ajustes, escolha o idioma marcado "Padrão": ele volta a seguir o do Mac.
14. Confira no Monitor de Atividade que nenhum processo `authopen` ficou aberto depois de tudo, e que o disco não foi
    montado de novo nem aparece "não ejetado corretamente".

## No Windows (quando houver um PC)

O CI só compila no Windows; nunca foi aberto lá. Conferir:

1. Gerar e abrir o app (`npm ci` e `npm run tauri build`), e os passos 2 a 5 e 10 a 12 acima (a lista de discos, o
   disco do Windows apagado, tema, idioma, Sobre).
2. **O pedido do UAC.** Escolha um cartão e **Continuar**: o Windows mostra o pedido de confirmação. Ele pode abrir **atrás**
   da janela do app (o app ainda não passa a janela dele); se nada aparecer, procure o escudo piscando na barra de tarefas.
   Recusar deve mostrar "Recusada" com **Tentar de novo**; aceitar, a tela provisória.
3. **O app reabre elevado.** Ao aceitar, o app abre o disco por um segundo processo elevado (o mesmo programa chamado
   de novo): anote se aparece uma segunda janela por um instante, ou se o app pisca.
4. Tire o cartão com o pedido aberto (passo 9 do Mac).
5. No CI, o `npm ci` no `windows-11-arm` ainda não foi exercitado: se o job do app falhar lá, o plano é gerar a parte do
   `app/dist` só no x64.

## O que anotar

Para cada passo que não bateu: o que apareceu, e uma captura de tela se puder. Resultados abaixo.

## Conferência manual

(vazio: o Sérgio preenche)
