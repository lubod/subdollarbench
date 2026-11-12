# Video narration

The videos use the operator's own cloned voice. Only that voice is
cloned: no stock or vendor voice, and no reference clip of anyone else.

## Where things live

- **In the repo:** `video/voice/reading.txt` (the text to read aloud),
  `video/voice/test-sentences.txt`, `video/lexicon.tsv`,
  `video/narrate.py`, `video/voice-ref.sh`, this README.
- **Outside the repo, under `~/sdb-video/`:**
  - `voice/incoming/` — put the recording here; `voice-ref.sh` installs
    the newest file.
  - `voice/ref.wav`, `voice/ref.txt` — the installed reference (mono
    24 kHz) and its transcript.
  - `cache/` — per-chunk audio, keyed by the spoken chunk, the
    reference files and the model id.
  - `hf/` — the Hugging Face cache; always set `HF_HOME=$HOME/sdb-video/hf`.
  - `venv-qwen/` — Python 3.12 with torch and qwen-tts.
  - `out/` — scratch outputs.
- **Samples for the operator:** `portal/video/` (ignored by git).
- **The recording and anything derived from it is never committed.**

## Record

Read `video/voice/reading.txt` aloud: only the paragraph before the
`---`, from "I gave" to "can buy". Use a quiet room, a normal speaking
pace and one take; keep it between 10 and 60 seconds; WAV or any format
`ffmpeg` reads. Put the file in `~/sdb-video/voice/incoming/`.

## Check and install

```sh
video/voice-ref.sh ~/sdb-video/voice/incoming/<recording>
```

It refuses a clip shorter than 10 s or longer than 60 s, a silent or a
clipped one, and a sample rate below 16 kHz, naming the measured value.
A good clip becomes `~/sdb-video/voice/ref.wav` (mono 24 kHz) and
`ref.txt` (the spoken part of the reading text).

## Narrate

```sh
HF_HOME=$HOME/sdb-video/hf nice -n 19 \
  ~/sdb-video/venv-qwen/bin/python video/narrate.py \
  video/voice/test-sentences.txt portal/video/clone-1.mp3
```

`--show <text-file>` prints the text as it will be spoken (the lexicon,
amounts and decimals applied) without loading the model. `--self-test`
checks the splitter, the lexicon, the cache and the refusals without
the model.

The model is `Qwen/Qwen3-TTS-12Hz-1.7B-Base` (Apache 2.0), on CPU; the
model load takes about two minutes. The script prints the chunks
generated and reused, the audio seconds, the wall seconds and the words
per minute.

## Resume

Each chunk's audio is cached under `~/sdb-video/cache/` by a hash of the
spoken chunk, the reference files and the model id. Rerun the same
command after an interruption; only the missing chunks are generated. A
one-sentence edit regenerates exactly the chunk that contains it.

## What must not be committed

`video/voice-ref.sh` and `video/narrate.py` refuse a recording or an
output inside the repo's tree (`portal/` is the samples' exception).
`git ls-files` must never list a `.wav`, `.mp3`, `.flac`, `.m4a`, `.ogg`,
`.opus`, `.pt` or `.safetensors` file.
