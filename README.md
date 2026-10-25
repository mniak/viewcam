# viewcam

Visualizador de webcam em tempo real, escrito em Rust. Captura pela API nativa
do sistema (AVFoundation no macOS, V4L2 no Linux, Media Foundation no Windows)
e desenha os quadros em uma janela leve.

![fluxo](https://img.shields.io/badge/captura-~30%20fps%20em%201280x720-green)

## Requisitos

- Rust estável (testado com 1.99)
- Uma webcam
- No macOS, permissão de câmera para o terminal/editor que executa o binário
  (Ajustes do Sistema → Privacidade e Segurança → Câmera)

## Compilar e executar

```sh
cargo run --release
```

A primeira execução compila as dependências (`nokhwa` + `minifb`), o que leva
alguns minutos. Depois disso o binário fica em `target/release/viewcam`:

```sh
./target/release/viewcam
```

## Uso

```text
viewcam [OPTIONS] [INDEX]

-l, --list          Lista as câmeras e sai
    --formats       Lista os formatos suportados pela câmera escolhida e sai
    --probe         Captura ~20 quadros, mostra estatísticas, salva um PNG e sai
-i, --index <N>     Câmera a abrir (padrão: 0)
-n, --name <TEXTO>  Abre a primeira câmera cujo nome contenha TEXTO
    --id <ID>       Abre a câmera pelo id do sistema (veja --list)
-W, --width <N>     Largura preferida (padrão: 1280)
-H, --height <N>    Altura preferida (padrão: 720)
    --fps <N>       Taxa de quadros preferida (padrão: 30)
-m, --mirror        Começa espelhado, como um espelho (padrão)
    --no-mirror     Começa sem espelhar
-h, --help          Ajuda
```

Exemplos:

```sh
viewcam --list                 # quais câmeras existem
viewcam --name face            # a FaceTime HD Camera
viewcam --formats -n face      # o que essa câmera sabe entregar
viewcam --probe -n face        # mede o fps real e salva snapshots/probe-*.png
viewcam -W 1920 -H 1080        # pede 1080p
```

### Teclas

| Tecla   | Ação                                    |
| ------- | --------------------------------------- |
| `Esc`/`Q` | Fecha                                 |
| `M`     | Alterna espelhamento                    |
| `S`     | Salva um PNG em `./snapshots`           |
| `Espaço`| Congela / retoma a imagem               |

O título da janela mostra resolução, fps medido e o estado atual (`live`,
`mirrored`, `frozen`). A janela pode ser redimensionada; a imagem mantém a
proporção.

## Como o formato é escolhido

O programa pede à câmera a lista de formatos suportados e escolhe o mais
próximo de `--width/--height/--fps`, nesta ordem de preferência:

1. não ultrapassar a resolução pedida (4K acidental é lento demais);
2. menor distância até a resolução pedida;
3. taxa de quadros ≥ 20 quando existir;
4. MJPEG antes de YUYV, e ambos antes de NV12/GRAY.

Se a câmera recusar o formato escolhido, ele cai para o padrão do dispositivo e
avisa na saída.

Esse passo explícito existe porque o `RequestedFormatType::Closest` do `nokhwa`
0.10 procura a taxa de quadros usando a resolução *pedida* — se ela não existir
exatamente, a busca falha e a biblioteca escolhe o primeiro formato da lista
(às vezes 160x120@25). Por isso também usamos `--name`/`--id`: índices numéricos
mudam quando uma câmera USB ou um iPhone via Continuity Camera entra ou sai.

## Problemas comuns

**A imagem fica preta.** A permissão de câmera foi negada. Rode
`viewcam --probe` para confirmar: ele avisa quando os quadros estão quase
pretos. Autorize o app que executa o binário em Ajustes do Sistema →
Privacidade e Segurança → Câmera e abra o binário de novo.

**"could not open camera … Already in use".** Outro programa está com a câmera
aberta (Zoom, navegador, OBS). Feche-o.

**Poucos fps.** Rode `viewcam --probe`: ele mostra o tempo gasto em captura e em
decodificação. Se a captura já vem lenta, a câmera/iluminação é o limite
(muitas webcams caem para 15 fps com pouca luz). Se a decodificação for o
gargalo, use `--release` — a build de debug é bem mais lenta.

**Tela cheia.** A versão atual do `minifb` (0.29) não expõe fullscreen; maximize
a janela pelo botão verde do macOS.

## Estrutura

```text
src/main.rs   CLI, seleção de formato, loop de captura e desenho
```

Testes dos helpers de imagem e da escolha de formato:

```sh
cargo test
```
