# Parte 1.5 — roteiro de conferência manual com hardware de verdade (25/10/2026)

Para o Sérgio rodar quando tiver o hardware. Os testes automáticos só usam imagens que eles mesmos criam (`hdiutil` no Mac,
VHD no Windows); este roteiro cobre o que só um cartão, um leitor e um HD de verdade mostram. Nada aqui é obrigatório para
o PR da 1.5: os resultados vão para o fim deste arquivo (passo 6) e decidem se a 1.5 precisa de ajuste.

## Antes de começar (vale para todos os passos)

1. **Confirme o id do disco com `revenant-dev disks` antes de qualquer outro comando**, a cada vez que plugar o cartão: o
   número (`disk4`, `PhysicalDrive2`) muda de uma conexão para outra. Confira `size`, `model`, `removable: true` e
   `system: false` na linha do cartão. Se o id que você ia usar mostra `"system":true` ou `"internal":true`, **pare**.
2. **Nunca use o disco do sistema como origem** (o `disk0`/`disk1` do Mac, o `PhysicalDrive0` do Windows). O motor recusa
   (`system_disk`), mas o roteiro não deve nem tentar.
3. O `revenant-dev` só lê o cartão e nunca grava dados nele. Mas `unmount`, `preflight`, `scan` e `copy` sobre um disco
   **também o desmontam** (como o "ejetar" do Finder, sem soltar o aparelho; passe `--no-unmount` para não desmontar), e só
   `remount` o monta de volta. Montar e desmontar não apagam nem gravam dados do cartão (o macOS pode gravar
   `.fseventsd` ao montar).
4. Use um cartão de teste, de preferência com fotos que você já tem em outro lugar. Para o passo 4, algo que você aceita perder.
5. A pasta das cópias, imagens e sessões fica **em outro disco** (o disco do computador ou um HD externo), nunca no cartão
   nem em outro volume dele. O motor recusa o contrário (`destination_on_source`), mas evite.
6. Compile uma vez: `cargo build --release -p revenant-dev` (o binário fica em `target/release/revenant-dev`). Abaixo,
   `revenant-dev` é esse binário. Os caminhos de exemplo (`~/rv-teste`) são pastas suas, que você cria com `mkdir`.
7. Feche o Finder/Explorer sobre o cartão e não abra fotos dele durante o roteiro.

## 1. Cartão SD formatado, no Mac

Objetivo: o cartão intacto antes e depois, a cópia fiel, a busca no aparelho igual à busca na imagem.

1. `mkdir -p ~/rv-teste/mac && cd ~/rv-teste/mac`
2. `revenant-dev disks` e anote o id (`disk4` abaixo; troque pelo seu). Esperado: uma linha `"event":"disk"` com o tamanho
   do cartão, `"removable":true`, `"system":false` e o volume dele em `volumes`.
3. `sudo revenant-dev unmount disk4` — **desmonta** o cartão (`diskutil unmountDisk`, sem ejetar: some do Finder, o leitor
   continua ligado). Fazemos isso **antes** do resumo "antes" porque um volume montado pode ser alterado pelo Spotlight ou pelo
   `.fseventsd` entre os dois resumos, o que daria um falso alarme. Esperado: `{"event":"unmount","disk":"disk4","held":0}`.
4. `sudo revenant-dev digest disk4` — o resumo **antes**. Esperado: `{"event":"digest","bytes":…,"fnv64":"…","unreadable_bytes":0}`.
   Anote o `fnv64`. (O `sudo` é só para ler o aparelho cru.) Demora o tempo de ler o cartão inteiro.
5. `sudo revenant-dev preflight disk4 --copy-to ~/rv-teste/mac/card.img --session ~/rv-teste/mac/sessao`
   Esperado: `{"event":"preflight","disk":"disk4","issues":[]}` e saída 0. **Atenção:** a conferência desmonta o cartão se ele ainda
   estiver montado (aqui já está, do passo 3: ela só confirma). É assim de propósito, para ninguém gravar nele enquanto lemos; o passo 11 monta de volta. Se vier `issues` não vazio, copie a linha para o registro e pare.
6. `sudo revenant-dev scan disk4 --session ~/rv-teste/mac/sessao --extract ~/rv-teste/mac/achados`
   (crie `achados` antes com `mkdir`). Deixe passar alguns por cento e **tire o leitor da porta USB** no meio.
   Esperado: a busca avisa que o aparelho sumiu (`device_gone`) ou fica esperando você plugar de volta, sem marcar setores
   ruins e sem fechar com resumo. Plugue o leitor de novo.
7. Repita **o mesmo comando do passo 6** (não repita o `preflight`). Esperado: ele reconhece o mesmo cartão, desmonta de novo
   (o Mac pode ter remontado ao plugar; é a mesma desmontagem sem ejetar), retoma de onde parou e imprime as linhas já
   achadas com `"replayed":true`, depois as novas e o `summary`. Anote o `found` do resumo. Se ele disser
   `session_other_source`, o cartão não foi reconhecido: copie a linha para o registro.
8. `sudo revenant-dev copy disk4 ~/rv-teste/mac/card.img`
   Esperado: linhas `copy_progress` até `copy_summary` com `"complete":true` e `bad:0`. O arquivo `card.img` fica ao lado
   de `card.img.rvmap` (o mapa da cópia). O espaço livre precisa ser pelo menos o tamanho do cartão.
9. `revenant-dev scan ~/rv-teste/mac/card.img` (sem `sudo`; a imagem é um arquivo). Esperado: o mesmo número de `found`
   do passo 7 (os mesmos arquivos).
10. `sudo revenant-dev digest disk4` — o resumo **depois**. Esperado: o **mesmo `fnv64` do passo 4** e `unreadable_bytes` 0.
   Diferente significa que algo gravou no cartão enquanto ele estava desmontado: pare e registre.
11. `sudo revenant-dev remount disk4` — **monta o cartão de volta**, como estava. Esperado: `{"event":"remount","disk":"disk4"}`
    e o cartão aparece no Finder. O Mac pode gravar no volume ao montar (`.fseventsd`, marca de sujo): por isso o resumo
    "depois" é o do passo 10, feito **antes** do `remount`.
12. Opcional: `sudo revenant-dev digest disk4` depois do `remount`. Pode diferir do passo 4; só registre.

## 2. O mesmo no Windows

Abra o **PowerShell como administrador** (Iniciar, digitar "PowerShell", "Executar como administrador"). Use
`.\target\release\revenant-dev.exe` no lugar de `revenant-dev`, e `PhysicalDriveN` no lugar de `diskN`.

1. `mkdir $HOME\rv-teste\win; cd $HOME\rv-teste\win`
2. `revenant-dev disks` e anote o id (`PhysicalDrive2` abaixo). Mesma verificação do passo 1.2: `removable`, `system:false`,
   tamanho. `PhysicalDrive0` costuma ser o disco do sistema: não use.
3. `revenant-dev unmount PhysicalDrive2` desmonta os volumes, mas as travas só duram enquanto o comando roda; o Windows pode
   remontar o `E:` no próximo acesso. Esperado: `{"event":"unmount","disk":"PhysicalDrive2","held":N}`. Rode o digest "antes" logo
   em seguida e, se o `E:` voltou ao Explorer, não o abra. Isso reduz, mas não elimina, a chance de o cartão mudar entre os
   resumos; uma diferença deve ser anotada, não tratada como prova de gravação do Revenant. O resumo **antes**:
   `revenant-dev digest PhysicalDrive2`; anote o `fnv64`.
4. `revenant-dev preflight PhysicalDrive2 --copy-to $HOME\rv-teste\win\card.img --session $HOME\rv-teste\win\sessao`
   Esperado: `"issues":[]`. A conferência trava e desmonta os volumes do cartão (o equivalente do "ejetar sem soltar":
   o `E:` some do Explorer, o leitor continua ligado). Com o cartão já desmontado do passo 3, no Windows é uma nova trava; o
   motor a segura só até o fim do comando.
   Se vier `unmount_failed`, algum programa está usando o cartão: feche-o e repita.
5. `revenant-dev scan PhysicalDrive2 --session $HOME\rv-teste\win\sessao --extract $HOME\rv-teste\win\achados` (crie `achados`
   antes). Tire o leitor no meio, plugue de volta e repita **o mesmo comando**: mesmo comportamento do passo 1.6–1.7 do Mac
   (retoma, `"replayed":true`, resumo final).
6. `revenant-dev copy PhysicalDrive2 $HOME\rv-teste\win\card.img`; depois `revenant-dev scan $HOME\rv-teste\win\card.img`:
   mesmo número de `found` da busca no aparelho.
7. `revenant-dev digest PhysicalDrive2` — o resumo **depois**: igual ao do passo 3. Aqui nenhuma trava está ativa (as dos comandos anteriores acabaram com
   eles), então o Windows pode ter remontado o `E:`: não o abra; uma diferença deve ser anotada, não tratada como prova de
   gravação do Revenant.
8. `revenant-dev remount PhysicalDrive2` — solta as travas e deixa o Windows montar o `E:` de novo. Esperado:
   `{"event":"remount",…}` e o cartão volta ao Explorer. Depois do remount o resumo pode diferir (o Windows escreve ao montar).

## 3. HD velho com defeito lógico (partição apagada, sistema de arquivos estranho)

Objetivo: o caminho do dia a dia do app, sem a cópia: busca direta no HD, achados cedo e um tempo estimado que faça sentido.

1. `revenant-dev disks` (Mac) e confirme o HD pelo tamanho e modelo; ele **não** pode ser o disco do sistema nem o do destino.
2. Mac: `sudo revenant-dev scan disk5 --extract ~/rv-teste/hd/achados`. Windows (administrador):
   `revenant-dev scan PhysicalDrive3 --extract $HOME\rv-teste\hd\achados`. (Se o HD tem partições que o sistema monta, o
   comando desmonta antes de ler, sem ejetar.)
3. Nos primeiros minutos, anote: o tempo até a primeira linha `found`, a velocidade e o `ETA` que o stderr mostra
   (`scanned N% … MB/s, ETA …`), e se o ETA oscila muito. Esperado: achados aparecendo enquanto lê, ETA plausível para o
   tamanho do HD, nenhum `unreadable` em HD saudável.
4. Deixe terminar (ou cancele com Ctrl-C depois de alguns minutos, o que basta para a conferência). Se terminar: o número de
   fotos e se abrem.
5. `remount` do disco (Mac `sudo revenant-dev remount disk5`; Windows `revenant-dev remount PhysicalDrive3`) se você quer
   usá-lo de novo.

## 4. Cartão ou pendrive que falha (setores ruins)

Use um cartão que você já sabe que dá erro de leitura. **Não** force um cartão bom a falhar.

1. `revenant-dev disks`; confira o id como no passo 1.2.
2. `sudo revenant-dev scan disk4 --extract ~/rv-teste/falha/achados` (Windows: sem `sudo`, como administrador). Esperado:
   linhas `unreadable` e, cedo (depois de 16 trechos ou 4 MiB ilegíveis), **uma** linha `{"event":"disk_failing",…}`. Anote a que
   percentual a linha apareceu. Interrompa com Ctrl-C depois dela: o aviso é "pare e faça uma cópia".
3. `sudo revenant-dev copy disk4 ~/rv-teste/falha/card.img` (Windows: o mesmo, sem `sudo`). Esperado: passes `fast`,
   `skipped`, `trim`, `scrape`, `copy_bad` para cada região perdida e `copy_summary` com `bad` maior que 0. Se travar muito
   tempo num ponto, Ctrl-C: não há tratador de Ctrl-C, então vale o mapa do último salvamento (se perde até um intervalo de 30 s ou 256 MiB) e o mesmo comando retoma. Opcional: repita com `--retries 1` para uma segunda
   tentativa nos setores ruins (só com o cartão ainda aguentando).
4. `revenant-dev scan ~/rv-teste/falha/card.img`. Esperado: `{"event":"map",…}` primeiro; as regiões não resgatadas aparecem como `unreadable`
   (nunca como zeros) e nenhum arquivo que as toca sai `intact`. Compare o número de `found` com o da busca direta do passo 2.
5. Se a cópia chegar a milhões de regiões ruins (o teto é 2^20 trechos no mapa), anote: é o caso que os testes sintéticos
   não cobrem de verdade.

## 5. Velocidade: aparelho contra a imagem dele

Objetivo: saber se ler o cartão cru perde para ler o arquivo de imagem dele. Referências dos testes automáticos (imagens
anexadas pelo sistema, não leitores de cartão), por plataforma:
- Mac: 113–127 MB/s no aparelho contra 148–154 MB/s na imagem (primeira rodada); 76–79 contra 80–84 (última rodada, com o Mac
  esquentando).
- Windows (CI, VHD): 113 MB/s no aparelho contra 373 MB/s no arquivo.

1. Com o cartão bom do passo 1 e a imagem `card.img` dele: rode `revenant-dev scan disk4` (Windows `PhysicalDrive2`) e
   anote o `MB/s` da última linha de progresso no stderr (`scanned 100% … MB/s`).
2. Rode `revenant-dev scan ~/rv-teste/mac/card.img` e anote o `MB/s` do mesmo jeito. (Para a segunda leitura não vir do cache de
   páginas, rode a do aparelho primeiro e, se o Mac tem RAM de sobra, `sudo purge` antes da imagem; sem isso a imagem pode
   sair absurdamente rápida.)
3. Esperado: o aparelho entre 20 e 100 MB/s num leitor UHS-I, até ~300 MB/s num UHS-II. O que importa: a busca no aparelho
   não ser **muito** mais lenta que o próprio leitor entrega. Registre os dois números e o modelo do leitor.

## 6. O que registrar e onde

Registre num bloco no **fim deste arquivo**, com a data, a máquina e o hardware (modelo do cartão, tamanho, leitor, HD):

- Para cada passo: ok ou não; se não, a linha de erro exata (`error: <código>`, o JSON da conferência) e o que você fez.
- Os dois `fnv64` do passo 1 (e do 2) e se são iguais.
- Passo 3: tempo até o primeiro achado, ETA do início contra o tempo real.
- Passo 4: percentual em que veio o `disk_failing`; `bad` do `copy_summary`; quantos `found` no aparelho e na imagem.
- Passo 5: os dois `MB/s`.
- Qualquer coisa estranha, mesmo que o comando tenha dado certo (cartão que não remontou, leitor que reiniciou, travas que
  sobraram).

Se algo grave aparecer (o `digest` de depois diferente do de antes, um erro no meio da cópia, uma sessão que não retoma),
**não conserte na hora**: abra uma pendência no `docs/superpowers/history/2026-10-25-m1-p5-pendencias.md`, na seção "Em
aberto", com a linha do registro.

## Registro

(vazio: o Sérgio preenche)
