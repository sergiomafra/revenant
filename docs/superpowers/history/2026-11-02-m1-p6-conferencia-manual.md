# Parte 1.6 — roteiro de conferência manual com a A6700 (02/11/2026)

Para o Sérgio rodar quando quiser. Os testes automáticos só usam cartões sintéticos, que imitam dois jeitos de alocar
clusters ("o próximo livre" e "o primeiro buraco"); como o firmware da Sony aloca é suposição (D19). Este roteiro confere
com a câmera de verdade: gravar, apagar, gravar de novo e ver o que volta. Nada aqui é obrigatório para o PR da 1.6; os
resultados vão para a seção "Conferência manual" de `docs/superpowers/history/2026-11-02-m1-p6-pendencias.md` (passo 8).

## Antes de começar

1. Valem as regras do roteiro da 1.5 (`docs/superpowers/history/2026-10-25-m1-p5-conferencia-manual.md`, "Antes de
   começar"): confirme o id do disco com `revenant-dev disks` **a cada vez** que plugar o cartão (`removable: true`,
   `system: false`, o tamanho certo); nunca use o disco do sistema; feche o Finder sobre o cartão.
2. Compile uma vez, na pasta do repositório: `cargo build --release -p revenant-dev`. Abaixo, `revenant-dev` é
   `target/release/revenant-dev`.
3. Pasta de trabalho: `mkdir -p ~/rv-a6700 && cd ~/rv-a6700`. Ela precisa de espaço livre para **cada** imagem do cartão
   (o tamanho do cartão inteiro, por exemplo 64 GB) mais os arquivos. Use o menor cartão SDXC que tiver (64 GB).
4. A busca na imagem com `--extract` para uma pasta no mesmo disco que a imagem precisa de `--allow-same-volume` (a imagem
   é um arquivo, não o cartão; é o uso que o README do `revenant-dev` permite). Nunca use essa opção com o cartão.
5. Use um cartão sem nada que você precise: o passo 1 o formata.

## 1. Preparar

1. Na A6700: formate o cartão **na câmera** (item "Formatar" do menu). Um cartão SDXC formatado na câmera é exFAT.
2. Ajuste a gravação para **RAW+JPEG**.
3. Anote o **tipo de arquivo RAW** que a câmera está usando (no menu de qualidade de imagem, o item do tipo
   de arquivo RAW): **sem compressão**, **compactado sem perdas** (L, M ou S) ou **compactado**. Ele decide o que
   pode sair `Intact` (passo 6.2).
4. Anote se a câmera apaga o par junto (RAW e JPEG da mesma foto) ou um de cada vez.

## 2. Gravar e guardar uma cópia

1. Fotografe **30 fotos** (cenas parecidas valem: é o caso difícil) e grave **3 clipes XAVC curtos** (10 a 20 s cada),
   na ordem: 10 fotos, clipe 1, 10 fotos, clipe 2, 10 fotos, clipe 3.
2. Ponha o cartão no Mac e copie **o cartão inteiro** pelo Finder para `~/rv-a6700/antes/` (as pastas `DCIM` e `PRIVATE`
   inteiras). É a referência de bytes dos passos seguintes.
3. Calcule os resumos dos originais:

   ```sh
   cd ~/rv-a6700
   find antes -type f \( -iname '*.ARW' -o -iname '*.JPG' -o -iname '*.MP4' \) -exec shasum -a 256 {} + | sort > antes.sha
   wc -l antes.sha
   ```

   Esperado: 63 linhas (30 ARW, 30 JPG, 3 MP4).
4. Ejete o cartão no Finder e devolva-o à câmera.

## 3. Apagar na câmera

1. Na câmera, apague as fotos **5, 10, 15 e 20** (o par RAW+JPEG de cada) e o **clipe do meio** (clipe 2).
2. Anote os nomes apagados (por exemplo `DSC00005.ARW`/`DSC00005.JPG`, `C0002.MP4`).

## 4. Gravar de novo, para encher os buracos

1. Fotografe **mais 15 fotos** e grave **um clipe longo** (2 a 3 minutos, para passar pelos buracos).
2. Não copie nada: estes arquivos novos são vivos e o próprio cartão é a referência deles (passo 5.7).

## 5. Ler o cartão, copiar para uma imagem e buscar

1. Ponha o cartão no Mac. `revenant-dev disks` e anote o id (`disk4` abaixo; troque pelo seu). Esperado: uma linha
   `"event":"disk"` com o tamanho do cartão, `"removable":true`, `"system":false`.
2. Antes de desmontar, copie pelo Finder os arquivos novos (as 15 fotos e o clipe longo) para `~/rv-a6700/depois/` e
   acrescente os resumos deles:

   ```sh
   find depois -type f \( -iname '*.ARW' -o -iname '*.JPG' -o -iname '*.MP4' \) -exec shasum -a 256 {} + | sort > depois.sha
   ```

3. `sudo revenant-dev unmount disk4`. Esperado: `{"event":"unmount","disk":"disk4","held":0}`.
4. `sudo revenant-dev digest disk4`. Esperado: `{"event":"digest","bytes":…,"fnv64":"…","unreadable_bytes":0}`. Anote o
   `fnv64`.
5. `sudo revenant-dev copy disk4 ~/rv-a6700/card1.img`. Esperado: linhas `copy_progress` e, no fim, `copy_summary` com
   `"complete":true` e `"bad":0`.
6. `revenant-dev digest ~/rv-a6700/card1.img`. Esperado: o **mesmo `fnv64`** do passo 5.4 e `"unreadable_bytes":0`.
   Depois `sudo revenant-dev remount disk4` e tire o cartão.
7. Busque na imagem e extraia:

   ```sh
   mkdir -p ~/rv-a6700/achados1
   revenant-dev scan ~/rv-a6700/card1.img --extract ~/rv-a6700/achados1 --allow-same-volume > ~/rv-a6700/scan1.jsonl
   ```

   Esperado: linhas `volume` (uma, `"fs":"exfat"`), `found`, talvez `updated`, e o `summary` no fim.
8. Confira os bytes contra as cópias (cada original procurado pelo resumo entre os extraídos):

   ```sh
   cd ~/rv-a6700
   find achados1 -type f -exec shasum -a 256 {} + | sort > achados1.sha
   cat antes.sha depois.sha | while read h f; do
     if grep -q "^$h " achados1.sha; then echo "IGUAL  $f"; else echo "FALTA  $f"; fi
   done > conferencia1.txt
   grep -c IGUAL conferencia1.txt; grep FALTA conferencia1.txt
   ```

   Esperado:
   - todo arquivo **vivo** (os 26 pares e 2 clipes que ficaram, as 15 fotos e o clipe longo) `IGUAL`;
   - um arquivo **apagado** (passo 3) só pode faltar se a gravação do passo 4 caiu em cima dele; um apagado cujos
     clusters ninguém reusou tem de voltar `IGUAL`.
   - Um `FALTA` num arquivo vivo, ou num apagado que não foi sobrescrito, é o achado que importa: registre.
   - Um arquivo que voltou `updated` foi extraído duas vezes (o segundo com `_2`); o resumo acha qualquer um dos dois.

## 6. Contar o que veio, por tipo e saúde, e medir o DRI

1. A saúde final de cada achado (uma linha `updated` substitui a `found` do mesmo `id`):

   ```sh
   jq -s -r '[.[] | select(.event=="found" or .event=="updated")] | group_by(.id) | map(last)
     | group_by(.kind + " " + .health) | map("\(length) \(.[0].kind) \(.[0].health)") | .[]' ~/rv-a6700/scan1.jsonl
   ```

   Anote as contagens `intact`, `partial` e `damaged` de `arw`, `jpeg` e `mp4`.
2. Os achados em pedaços, de onde vieram e como foram colocados (é aqui que aparece como a Sony aloca):

   ```sh
   jq -s -c '[.[] | select(.event=="found" or .event=="updated")] | group_by(.id) | map(last) | .[]
     | select(.fragments or .state == "deleted")
     | {id, name, kind, health, state, placement, pieces: ((.fragments // []) | length)}' ~/rv-a6700/scan1.jsonl
   ```

   O que esperar, pelo que a 1.6 promete:
   - vivo: `intact`, `placement` `chain`, com o nome;
   - apagado na câmera com a cadeia de clusters intacta e ninguém por cima: `intact`, `placement` `old_chain` (se nenhum
     apagado vier `old_chain`, a A6700 zera a cadeia ao apagar: registre, muda o que se espera);
   - JPEG apagado em 2 ou mais pedaços colocado só pelo conteúdo: no máximo `partial`, com os bytes certos (Q37); se
     ficar o palpite de uma corrida, `partial` com bytes errados no fim (DRI 0 em 3+ pedaços, Q59);
   - ARW **sem compressão** ou **compactado** em pedaços sem `old_chain`: `partial`, bytes no lugar; **sem perdas**:
     `intact` só com as emendas exatamente entre blocos, senão `partial`;
   - clipe em pedaços: `intact` quando os quadros provam cada emenda, senão `partial`.
3. O DRI dos JPEGs da câmera (0 = sem marcadores de reinício):

   ```sh
   cd ~/rv-a6700
   perl -0777 -ne 'print "$ARGV: DRI ", (/\xFF\xDD\x00\x04(..)/s ? unpack("n", $1) : 0), "\n"' antes/DCIM/*/*.JPG | sort -u -k3 | head
   ```

   O comando procura o primeiro marcador DRI no arquivo (a miniatura do EXIF incluída); anote os valores que aparecem.
4. O DRI das prévias de um ARW (o `revenant-dev` não tem comando para isso; usa-se o teste opcional, que lê um arquivo com
   o nome de uma amostra da lista):

   ```sh
   mkdir -p ~/rv-a6700/dri
   cp ~/rv-a6700/antes/DCIM/*/DSC00001.ARW ~/rv-a6700/dri/sony-ilce-6700.arw
   cd <pasta do repositório>
   REVENANT_SAMPLES=$HOME/rv-a6700/dri cargo test --release -p revenant-core --test real_samples sony_restart_intervals -- --ignored --nocapture
   ```

   Esperado: linhas como `sony-ilce-6700.arw: JPEG at … (… B), 6192x4128, DRI 0` (por exemplo: 6192x4128 é o tamanho
   da amostra CC0; o seu é o da sua câmera) (e as das outras prévias) e a dos blocos
   (`tiles, TileByteCounts …`, só no sem perdas). A amostra CC0 da A6700 deu DRI 0 em todas; um valor maior que 0 aqui
   muda o custo da 1.6 para a câmera: registre.

## 7. Repetir com os outros jeitos de apagar

Para cada variante, repita o passo 5 inteiro (nova imagem `cardN.img`, `scanN.jsonl`, `achadosN`, `conferenciaN.txt`) e o
passo 6.1–6.2, sempre partindo do cartão como ficou no passo anterior. As fotos novas de cada variante não têm resumo
ainda: no passo 5.2, antes de desmontar, copie-as pelo Finder para `depois/` e rode de novo o comando do `depois.sha`
(ele refaz a lista com tudo o que está em `depois/`), senão a contagem de `FALTA` não as cobre.

1. **Apagar no computador (Mac):** com o cartão montado no Mac, apague no Finder as fotos 25 e 30 (os dois arquivos de
   cada) e o clipe 3, e **esvazie o Lixo** com o cartão ainda conectado (senão os arquivos só vão para `.Trashes` do
   cartão). Esvaziar o Lixo com o cartão conectado faz o macOS gravar no cartão (`.Trashes`, `.fseventsd`): num cartão
   de teste isso é aceitável, mas muda o experimento (são escritas do sistema, não da câmera); anote. Antes de apagar,
   confira que eles já estão em `antes/`. Ejete, fotografe mais 10 fotos na câmera e faça o
   passo 5 com `card2.img`. O que muda: o macOS pode zerar a cadeia; anote o `placement` desses apagados.
2. **Apagar no computador (Windows), se houver um PC:** no Explorer, apague com **Shift+Delete** as fotos 35 e 40 e o
   clipe longo; fotografe mais 10 na câmera e faça o passo 5 no Mac com `card3.img` (a cópia pode ser no Mac).
3. **Formatar na câmera:** formate o cartão na A6700 (a formatação comum, que é rápida) e fotografe **5 fotos novas**. Passo 5
   com `card4.img`. Esperado: as 5 novas `intact` e vivas; as antigas que sobraram voltam pela escultura, e algumas com o
   nome pela pasta antiga (`state` `orphaned`, `path` começando com `?/`). Nenhuma antiga pode vir `intact` com bytes
   que não sejam dela: confira pelo resumo.

## 8. O que registrar e onde

Registre na seção **"Conferência manual"** de `docs/superpowers/history/2026-11-02-m1-p6-pendencias.md`, com a data, o
Mac, o cartão (marca, tamanho) e o leitor:

- O tipo de arquivo RAW (passo 1.3) e se a câmera apaga o par junto (1.4).
- Por variante (passos 5 e 7): o `fnv64` do cartão e o da imagem (iguais?), o número de `IGUAL` e cada `FALTA` com o
  nome, e se o arquivo que faltou era vivo, apagado e sobrescrito ou apagado e não sobrescrito.
- As contagens `intact` / `partial` / `damaged` por tipo (passo 6.1).
- A lista do passo 6.2 (ou só as linhas dos apagados e dos em pedaços): quantos pedaços, que `placement`. Isso diz se a
  Sony aloca como "o próximo livre" ou "o primeiro buraco" e se mantém a cadeia ao apagar.
- Os DRI dos JPEGs e das prévias (passos 6.3 e 6.4).
- **Qualquer achado `intact` cujo arquivo não bate com nenhum original** (um `FALTA` com um extraído `intact` no lugar):
  é o pior erro possível (antiobjetivo do desenho). Não conserte na hora: abra uma pendência na seção "Em aberto" do
  histórico com a linha do `scanN.jsonl`.
