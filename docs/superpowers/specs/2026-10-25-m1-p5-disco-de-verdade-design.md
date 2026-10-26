# Parte 1.5: disco de verdade (desenho)

Data: 25/10/2026. Autoridade acima deste documento: a spec `2026-10-19-revenant-design.md` (§3 passos 1 a 4, §5 "Somente
leitura, sempre" e "Desempenho", §6 "Acesso ao disco bruto"). Este documento detalha a parte 1.5 do roadmap; o que não
está aqui segue a spec. Marcas: **[Sérgio]** é decisão dele; **[escolha do Claude]** é decisão minha, que ele pode trocar.

## 1. Por que agora, e para quem

Até a 1.4 o motor só lê arquivos de imagem de disco. A 1.5 o faz ler o **aparelho de verdade** (o cartão ou o HD inteiro),
fechando o marco A do roadmap: um desenvolvedor recupera fotos e vídeos de um cartão real pelo `revenant-dev`.

Os dois casos reais do Sérgio guiam a parte **[Sérgio]**:

- **Cartão formatado** antes de salvar tudo. O cartão está fisicamente bom; o que importa é lê-lo inteiro sem que o sistema
  grave nada nele (a 1.4 já devolve os nomes depois de uma formatação rápida).
- **HD corrompido**: o PC o reconhecia, mas não mostrava nada dentro (defeito lógico, não físico). Tamanho desconhecido;
  um HD de 1 a 2 TB leva horas para ser lido.

O que faria o Sérgio desistir no meio: **saber que vai dar errado** **[Sérgio]**. Esperar horas e descobrir no fim que não
deu é o pior resultado possível. Daí as três regras que guiam a parte **[escolha do Claude, confirmada pelo Sérgio]**:

1. **O que pode estragar tudo é conferido antes de começar** (permissão, disco respondendo, desmontagem, espaço para a
   cópia), com aviso na hora.
2. **Os primeiros minutos já dizem se vale esperar**: achados e progresso desde o início, e aviso cedo quando o disco
   começa a falhar em série, sugerindo a cópia.
3. **Nada se perde no meio**: cabo solto, computador dormindo ou programa fechado não perdem o que já foi lido nem o que já
   foi achado; a busca e a cópia retomam de onde pararam.

## 2. Objetivos

- **G1. Ler o aparelho**, no Mac e no Windows, sempre só leitura, inteiro (o disco todo, não uma partição).
- **G2. Desmontar sem ejetar** os volumes do aparelho antes de ler, para o sistema parar de gravar no cartão (spec §5).
  Se não der, seguir montado e avisar.
- **G3. Listar os discos** conectados, com o que a tela 1 da spec vai precisar (nome, capacidade, marca/modelo, se é
  removível, se é o disco do sistema, sistemas de arquivos que tem).
- **G4. Conferência prévia**: uma chamada que diz, antes de começar, tudo o que já se sabe que vai falhar.
- **G5. Cópia tolerante a setores ruins** (imagem do disco): passada rápida pulando erros, depois volta nos buracos (spec
  §3 passo 3); retomável; a busca roda na imagem e trata o que não foi resgatado como ilegível, nunca como zeros.
- **G6. Busca salva e retomável**: os achados e o ponto da busca ficam gravados num arquivo de sessão; uma busca
  interrompida continua de onde parou e chega ao **mesmo resultado** que teria sem a interrupção.
- **G7. Sinais cedo**: progresso, velocidade e aviso de "disco falhando, faça a cópia" nos primeiros minutos.
- **G8. `revenant-dev`** ganha tudo isso na linha de comando (o app vem no Módulo 2).
- **G9. Pendências herdadas com dono 1.5** fechadas ou medidas (§9).

## 3. Fora da 1.5 (não-objetivos)

- **Telas, textos para o usuário e o pedido de senha bonito**: Módulo 2. O motor só devolve códigos e dados.
- **O ajudante privilegiado** (processo separado só de leitura, iniciado com senha/UAC, spec §6): Módulo 2.1. Na 1.5 quem
  lê o aparelho é o próprio `revenant-dev`, rodado como administrador (`sudo` no Mac; "Executar como administrador" no
  Windows). O desenho da API deve permitir que o ajudante do 2.1 a use sem mudança (§5.1).
- **Impedir o computador de dormir**: é do app (Módulo 2). A 1.5 garante que dormir não perde nada (G6), não que não durma.
- **NTFS, APFS, HFS+**: continuam fora (a escultura segue cobrindo; o HD do Sérgio é coberto por ela).
- **Linux**: fora (a spec é Mac e Windows).
- **Fragmentação avançada**: 1.6.

## 4. Antiobjetivos (o que faria a parte falhar mesmo "funcionando")

- **Qualquer escrita no aparelho de origem**, por qualquer caminho (inclusive o sistema gravando num volume que deixamos
  montado sem avisar).
- **Ler o disco errado**, ou retomar uma sessão em outro disco e misturar resultados.
- **Tratar um cabo solto como um disco cheio de setores ruins**: um aparelho que sumiu não é um aparelho com defeito.
- **Martelar um disco que está morrendo**: reler setores ruins à toa desgasta o que resta.
- **Marcar como Íntegro um arquivo lido de buracos da cópia** (zeros no lugar do que não foi resgatado).
- **Descobrir na quinta hora** algo que dava para saber no primeiro minuto (falta de espaço, falta de permissão, disco
  que não responde).
- **Uma retomada que dá resultado diferente** de uma busca sem interrupção.

## 5. Como funciona

### 5.1 Abrir o aparelho

- **Mac** **[escolha do Claude]**: lê `/dev/rdiskN` (o nó "cru", sem cache do sistema, bem mais rápido em leitura
  sequencial), aberto só para leitura. Exige root.
- **Windows** **[escolha do Claude]**: lê `\\.\PhysicalDriveN`, aberto só com `GENERIC_READ`, compartilhando leitura e
  escrita (para não brigar com o sistema). Exige administrador.
- Um aparelho exige leituras alinhadas ao tamanho de setor dele (512 ou 4096 B) e não informa o tamanho pelos metadados de
  arquivo comuns. A origem de aparelho do motor resolve isso por dentro: aceita qualquer `offset`/tamanho (alinha e recorta
  sozinha), informa o tamanho real e o tamanho de setor lógico e físico. Todo o motor (busca, sistemas de arquivos,
  extração, pareamento) continua usando a mesma interface `Source`, sem método de escrita.
- **Identidade do aparelho**: tamanho, modelo, número de série quando houver, e um resumo dos primeiros e últimos MiB.
  Serve para a sessão (§5.5) recusar retomar em outro disco e para o `extract` recusar gravar em qualquer volume desse disco.
- **Aparelho que sumiu** (cabo solto, leitor desconectado) é um erro próprio do motor, diferente de setor ruim: a busca e a
  cópia param, guardam o ponto e devolvem um código (`device_gone` ou parecido), sem registrar os setores como ruins.
- **Fronteira com o ajudante do Módulo 2.1** **[escolha do Claude]**: o ajudante só serve bytes e as operações de
  aparelho (listar, desmontar, remontar); o motor (busca, cópia, sessão) roda no processo do app, sem privilégio. Isso
  mantém pequeno o código que roda como administrador (spec §6: "processo ajudante pequeno, só de leitura"). Na 1.5 não se
  constrói o canal entre processos, mas a leitura do aparelho e as operações de aparelho ficam atrás de interfaces que
  esse canal possa servir, e os erros (inclusive o de aparelho que sumiu) são códigos que atravessam um canal.

### 5.2 Listar os discos e desmontar sem ejetar

- **Listar**: no Mac pelas ferramentas do sistema (`diskutil` com saída estruturada) **[escolha do Claude]**; no Windows
  pelas consultas do sistema ao aparelho. O disco do sistema vem marcado (a tela o mostra apagado; a 1.5 só o marca).
- **Desmontar sem ejetar**: no Mac `diskutil unmountDisk` **[escolha do Claude]**; no Windows, travar e desmontar cada
  volume do disco e **manter a trava** enquanto a busca durar, para o sistema não remontar.
- Se a desmontagem falhar (volume em uso), a chamada diz isso com um código, e a busca pode seguir montada; o motor
  continua sem escrever, e o aviso (que o sistema pode gravar) é do app.
- **Ao reconectar** (cabo solto, depois de dormir), o sistema remonta o cartão sozinho e pode gravar nele antes de o
  `revenant-dev` desmontar de novo. A 1.5 desmonta outra vez assim que vê o aparelho, antes de ler, e documenta essa
  janela **[escolha do Claude]**. Fechar a janela de vez (recusar a montagem enquanto a sessão dura) pede integração com o
  sistema que é do ajudante do Módulo 2.1.
- **No fim** a 1.5 **não remonta sozinha** **[escolha do Claude]**: remontar devolve ao sistema a chance de gravar no cartão.
  O motor oferece a chamada de remontar; o `revenant-dev` diz como remontar e só remonta se pedirem.

### 5.3 Conferência prévia

Uma chamada, antes de qualquer leitura longa, devolve a lista do que já se sabe (códigos, não textos):

- falta permissão para ler o aparelho;
- o aparelho não responde ou não lê o primeiro, o do meio e o último setor (amostra pequena);
- não deu para desmontar;
- para a cópia: o destino não tem espaço (o tamanho total do disco, conferido antes de começar; nunca começar uma cópia
  que se sabe que não cabe) **[escolha do Claude]**; o destino fica no próprio disco de origem (recusado, como no
  `extract`);
- para retomar: a sessão é de outro disco.

### 5.4 A cópia tolerante a setores ruins

- **Fases** (no espírito do `ddrescue`, sem copiar código dele, que é GPL de terceiros): (1) passada rápida em blocos
  grandes, pulando à frente quando dá erro e aumentando o pulo em erros seguidos; (2) volta pelas bordas dos buracos;
  (3) setor a setor no que sobrou; (4) novas tentativas só se pedidas (padrão: nenhuma, para não martelar o disco)
  **[escolha do Claude]**.
- **Mapa**: um arquivo ao lado da imagem diz o estado de cada trecho (não tentado, resgatado, ruim, a raspar). Gravado com
  frequência e de um jeito que aguenta queda no meio da gravação. A cópia retoma pelo mapa.
- **A busca na imagem usa o mapa**: trechos não resgatados aparecem para o motor como **ilegíveis** (o mesmo caminho dos
  setores ruins de hoje), nunca como zeros. Um arquivo que cai num buraco nunca sai Íntegro.
- A imagem é um arquivo comum no destino (pode ser esparso onde ainda não foi lido) **[escolha do Claude]**. Um destino
  que não aguenta um arquivo desse tamanho (FAT32, limite de 4 GiB) é recusado na conferência prévia, com código próprio;
  a imagem não é dividida em pedaços **[escolha do Claude]**.
- Progresso: bytes resgatados, ruins, ainda não tentados, velocidade, estimativa.
- Quando não há espaço para a cópia, o caminho é procurar direto (§5.3): o motor já lê cada setor uma vez e não insiste nos
  ruins, o que basta para defeito lógico.
- **Cópia e busca são etapas separadas, uma depois da outra** **[Sérgio: "busque fazer algo que esteja no mercado, sem
  inventar moda"]**. É o padrão dos programas para o público (Disk Drill: cópia byte a byte em várias passadas, depois a
  busca na imagem) e o que a spec §3 passo 3 já descreve. Disco com defeito lógico: procurar direto, e os achados aparecem
  desde o primeiro minuto. Disco falhando de verdade: a cópia mostra desde o início o progresso e os setores ruins; a busca
  vem depois, na imagem. Copiar e buscar ao mesmo tempo, lendo cada trecho uma vez (a "imagem em tempo de execução" do
  R-Studio e do UFS Explorer, recurso de programa de técnico), fica fora da 1.5 como ideia para depois.

### 5.5 Busca salva e retomável

- Uma **sessão** é uma pasta (ou um arquivo) num lugar escolhido por quem chama, nunca no disco de origem. Guarda: a
  identidade da origem (§5.1), as opções da busca, os achados já emitidos, e de tempos em tempos um **ponto de retomada**.
- **Requisito central**: retomar uma busca interrompida em qualquer ponto produz exatamente os mesmos eventos (mesmos
  achados, mesmas saúdes, mesmos ids, mesmas atualizações) que a busca sem interrupção, com qualquer número de threads e
  tamanho de cache. É a mesma régua de determinismo das partes 1.3 e 1.4.
- O ponto de retomada precisa guardar o que a busca carrega de um bloco para o outro (achados pendentes, trechos a pular,
  candidatos a `moov` e a pastas antigas, inícios de prévias de RAW, o mapa de setores ruins já conhecidos, o que for
  preciso), ou reconstruir esse estado de forma comprovadamente idêntica. Como, é do construtor (§8).
- **O que "mesmos eventos" quer dizer** **[escolha do Claude]**: a igualdade é exigida (e testada) para uma origem que
  devolve os mesmos bytes e os mesmos erros a cada leitura, como as origens sintéticas e uma imagem. Num disco que está
  morrendo, uma releitura pode dar diferente; por isso a retomada **nunca relê um setor que a sessão já sabe ruim** (o
  mapa de setores ruins vai na sessão) e evita reler o que já foi lido, salvo o mínimo de metadados que o construtor
  provar necessário (por exemplo, as estruturas do sistema de arquivos), com o custo documentado.
- **Uma sessão pertence a uma origem** **[escolha do Claude]**: uma busca começada no aparelho não é retomada na imagem
  dele, nem o contrário; depois de um aviso de "faça a cópia", a busca na imagem é uma sessão nova. A identidade de um
  aparelho é tamanho, modelo e número de série (os primeiros MiB podem mudar se o disco ficou montado); a de uma imagem
  inclui um resumo do conteúdo.
- A gravação aguenta queda no meio (não deixa sessão corrompida; no pior caso volta ao ponto anterior).
- Abrir uma sessão terminada devolve os resultados sem ler o disco de novo (spec §3: "Fechar o app não perde a busca").
- Retomar em outro disco é recusado (§5.1, identidade).

### 5.6 Sinais cedo

- A busca e a cópia emitem progresso com velocidade e estimativa desde o começo (a estimativa pode ser crua no início).
- **Aviso de disco falhando** **[escolha do Claude]**: quando os erros de leitura passam de um limite nos primeiros trechos
  (ou a velocidade despenca de um jeito típico de disco morrendo), o motor emite um evento recomendando a cópia. Os
  limites exatos são do construtor, documentados e testados com uma origem que simula falhas.
- Nada disso tem texto de interface: são eventos com códigos e números.

### 5.7 `revenant-dev`

Novos comandos ou opções (nomes do construtor): listar discos; conferência prévia; desmontar/remontar; copiar com mapa
(retomável); buscar num aparelho, numa imagem com mapa, ou retomar uma sessão. Saída em linhas JSON como hoje. Os READMEs
explicam como rodar com `sudo` no Mac e como administrador no Windows, e como não estragar o cartão (não abrir no
Finder/Explorer).

## 6. Restrições

- Tudo o que está no `CLAUDE.md` vale: só leitura, `Source` sem escrita, licenças permitidas, erros por código sem texto,
  Rust 2024, MSRV 1.88, **sem `unsafe` fora de `source/windows.rs`**, testes nunca tocam disco físico, documentação no mesmo
  commit, nunca `brew`.
- Chamadas de sistema do Mac que pediriam `unsafe` (consultar tamanho de setor e de disco do aparelho) devem preferir o
  que não exige: ferramentas do sistema com saída estruturada (`diskutil info -plist`) ou o que a biblioteca padrão
  oferece. Se o construtor provar que alguma é inevitável, ele para e pergunta antes (é exceção a uma regra fixa do
  Sérgio) **[escolha do Claude]**.
- **Exceção no Windows** **[Sérgio, aprovou a recomendação]**: travar e desmontar um volume (para o Windows não gravar
  nem remontar durante a busca) pode exigir abrir o **volume** com permissão de escrita. É permitido só para isso: essa
  abertura serve apenas aos pedidos de trava e desmontagem, nunca lê nem grava dados, e a leitura dos dados continua por
  uma abertura só de leitura do aparelho. Se houver um jeito sem escrita, o construtor usa esse. A exceção entra no
  `CLAUDE.md` junto com o código.
- Desempenho: ler um aparelho deve ser tão rápido quanto ler a imagem dele, e a sessão não pode atrasar a busca de forma
  perceptível (o construtor mede e documenta).
- Memória limitada mesmo em HD de vários TB (spec §5): o mapa da cópia e o estado da sessão crescem com o número de
  trechos ruins e de achados, com tetos documentados.

## 7. Como se testa sem o hardware do Sérgio

- **Testes automáticos** continuam sem tocar aparelho físico: origens sintéticas que simulam falhas (setores ruins,
  lentidão, aparelho que some no meio), e interrupções em pontos sorteados seguidas de retomada, comparando os eventos
  com os de uma busca inteira.
- **Teste opcional no Mac** (como o `os_filesystems` da 1.4): imagens de disco anexadas pelo sistema viram aparelhos
  de verdade (`/dev/diskN`) sem tocar disco físico. Cobre abrir o aparelho, listar, desmontar sem ejetar, buscar, copiar e
  retomar, e **confere que os bytes do aparelho não mudaram** depois de tudo (spec §5).
- **Windows no CI** **[escolha do Claude, a confirmar pelo construtor]**: o mesmo com um disco virtual anexado pelo
  sistema, se os executores do CI permitirem; se não, o caminho do Windows fica coberto por compilação, testes de unidade
  e pela conferência manual.
- **Conferência manual em hardware real**: um roteiro curto para o Sérgio rodar quando tiver o cartão SD e o HD/pendrive
  velhos à mão (ele tem, mas não agora **[Sérgio]**). Inclui conferir que o cartão não mudou (resumo antes e depois).

## 8. O que fica com o construtor

- O formato da sessão e do ponto de retomada, e como provar a igualdade dos eventos retomados.
- O formato do mapa da cópia e os tamanhos de bloco e de pulo.
- Os limites do aviso de disco falhando.
- Os nomes dos comandos e opções do `revenant-dev`, das funções e dos códigos de erro (em `snake_case`).
- Como listar discos no Windows, e se o Windows no CI dá para anexar disco virtual.
- A divisão em tarefas.

## 9. Pendências herdadas com dono 1.5

Do arquivo de cada parte (`docs/superpowers/history/`):

- **1.1**: `FileSource` com aparelho (tamanho 0, leituras alinhadas) e `volume()` vazio com aparelho → resolvidos por §5.1;
  cópia própria no lugar do `dd`/`ddrescue` do README → §5.4.
- **1.2**: o HEIC nunca fica "completo" na leitura (um setor ilegível logo depois de um HEIC completo o deixa Parcial; um
  `moov` solto pode mudar o tipo); um MP4 que lista `heic` como marca e é cortado antes do `moov` vira HEIC Danificado.
- **1.3**: setor de varredura maior que 512 B (conferir com aparelho 4Kn); origem que encolhe no meio da busca (aqui vira
  o caso "aparelho sumiu"); cancelamento atrasado durante uma medida ou carga do cache num aparelho lento; teto de trechos
  ruins (medir num cartão que está morrendo).
- **1.4** (apêndice R37): T1, T2, T3, T5 (teto de fotos compartilhado entre volumes), T6 (nota `InUse`), T7 (notas).

Cada uma é fechada, medida ou devolvida com dono no arquivo de histórico da 1.5.

## 10. Pronto quando

- `revenant-dev` lista discos, faz a conferência prévia, desmonta sem ejetar, copia com mapa e busca num aparelho, numa
  imagem com mapa ou numa sessão retomada, no Mac e no Windows (Windows conferido no CI e/ou na conferência manual).
- Os testes provam: nenhuma escrita (bytes do aparelho iguais antes e depois, no teste opcional do Mac); retomada em
  qualquer ponto igual à busca inteira; buracos da cópia nunca Íntegros; aparelho que some não vira setor ruim.
- READMEs, CHANGELOG, histórico da parte e roadmap atualizados; o roteiro de conferência manual entregue ao Sérgio.
