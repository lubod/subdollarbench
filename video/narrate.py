#!/usr/bin/env python3
"""R80 F2/F3: turn a text file into narration in the operator's cloned voice.

Usage:
  video/narrate.py <text-file> <out.mp3>
  video/narrate.py --show <text-file>
  video/narrate.py --self-test

The reference is ~/sdb-video/voice/ref.wav + ref.txt (video/voice-ref.sh
installs it). Chunk audio is cached under ~/sdb-video/cache/ by a hash of
the spoken chunk, the reference files and the model id, so an interrupted
render resumes and a one-sentence edit regenerates one chunk.

The model is Qwen/Qwen3-TTS-12Hz-1.7B-Base on CPU. Run it with
~/sdb-video/venv-qwen/bin/python and HF_HOME=~/sdb-video/hf; --show and
--self-test need neither the model nor that venv.
"""
import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HOME = Path.home()
VIDEO_DIR = Path(os.environ.get("SDB_VIDEO_DIR", HOME / "sdb-video"))
VOICE_DIR = Path(os.environ.get("SDB_VOICE_DIR", VIDEO_DIR / "voice"))
CACHE_DIR = Path(os.environ.get("SDB_CACHE_DIR", VIDEO_DIR / "cache"))
REF_WAV = VOICE_DIR / "ref.wav"
REF_TXT = VOICE_DIR / "ref.txt"
MODEL_ID = "Qwen/Qwen3-TTS-12Hz-1.7B-Base"
LEXICON = ROOT / "video" / "lexicon.tsv"
MAX_WORDS = 40
PAUSE_SENTENCE = 0.25
PAUSE_PARAGRAPH = 0.60

DIGIT_WORDS = ["zero", "one", "two", "three", "four", "five", "six", "seven",
               "eight", "nine"]
ONES = DIGIT_WORDS + ["ten", "eleven", "twelve", "thirteen", "fourteen",
                      "fifteen", "sixteen", "seventeen", "eighteen", "nineteen"]
TENS = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy",
        "eighty", "ninety"]
ABBREVIATIONS = ["e.g.", "i.e.", "etc.", "vs.", "Mr.", "Dr.", "Ms.", "Prof.",
                 "St.", "No."]


def number_words(n):
    """0..999999 in words, hyphenated as sixty-five."""
    if n < 0:
        return str(n)
    if n < 20:
        return ONES[n]
    if n < 100:
        return TENS[n // 10] + (("-" + ONES[n % 10]) if n % 10 else "")
    if n < 1000:
        return ONES[n // 100] + " hundred" + (
            (" and " + number_words(n % 100)) if n % 100 else "")
    if n < 1_000_000:
        return number_words(n // 1000) + " thousand" + (
            (" " + number_words(n % 1000)) if n % 1000 else "")
    return str(n)


def amount_words(match):
    dollars = int(match.group(1))
    cents = int(match.group(2))
    if dollars == 0:
        if cents == 0:
            return "zero dollars"
        return number_words(cents) + (" cent" if cents == 1 else " cents")
    text = number_words(dollars) + (" dollar" if dollars == 1 else " dollars")
    if cents:
        text += " and " + number_words(cents) + (
            " cent" if cents == 1 else " cents")
    return text


def decimal_words(match):
    whole = number_words(int(match.group(1)))
    digits = " ".join(DIGIT_WORDS[int(d)] for d in match.group(2))
    return whole + " point " + digits


def load_lexicon(path=None):
    path = Path(path) if path else LEXICON
    entries = []
    if not path.exists():
        return entries
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        written, _, spoken = line.partition("\t")
        if written and spoken:
            entries.append((written, spoken))
    # Longest first so luna-pro is not eaten by luna, SQLite by SQL.
    entries.sort(key=lambda pair: len(pair[0]), reverse=True)
    return entries


def apply_lexicon(text, entries=None):
    """Whole-word replacements in one pass, then the amount and decimal rules."""
    if entries is None:
        entries = load_lexicon()
    if entries:
        pattern = re.compile(
            r"(?<![A-Za-z0-9])("
            + "|".join(re.escape(written) for written, _ in entries)
            + r")(?![A-Za-z0-9])",
            re.IGNORECASE)
        lookup = {written.lower(): spoken for written, spoken in entries}
        text = pattern.sub(lambda m: lookup[m.group(1).lower()], text)
    text = re.sub(r"\$(\d+)\.(\d{2})\b", amount_words, text)
    text = re.sub(r"\b(\d+)\.(\d+)\b", decimal_words, text)
    return text


def split_sentences(paragraph):
    """Split at sentence ends; decimals and abbreviations do not end one."""
    protected = paragraph
    for i, abbrev in enumerate(ABBREVIATIONS):
        protected = protected.replace(abbrev, "\x02%d\x03" % i)
    protected = re.sub(r"(\d)\.(\d)",
                       lambda m: m.group(1) + "\x01" + m.group(2), protected)
    parts = re.split(r"(?:(?<=[.!?])|(?<=[.!?][\"'\u201d\u2019]))\s+", protected)
    for i, abbrev in enumerate(ABBREVIATIONS):
        parts = [p.replace("\x02%d\x03" % i, abbrev) for p in parts]
    return [p.replace("\x01", ".").strip() for p in parts if p.strip()]


def split_long(sentence, max_words=MAX_WORDS):
    """A sentence over the limit is split at commas, then at spaces."""
    if len(sentence.split()) <= max_words:
        return [sentence]
    pieces = [p.strip() for p in re.split(r"(?<=,)\s+", sentence) if p.strip()]
    out = []
    for piece in pieces:
        words = piece.split()
        while len(words) > max_words:
            out.append(" ".join(words[:max_words]))
            words = words[max_words:]
        if words:
            out.append(" ".join(words))
    return out


def chunk_text(text, max_words=MAX_WORDS):
    """Whole sentences, <= max_words each; a flag on a paragraph's last chunk.

    A sentence over the limit is split at commas into its own standalone
    sub-chunks: the fragments never merge with the sentences before or
    after it, so no pause lands between two unrelated sentences.
    """
    paragraphs = [p for p in re.split(r"\n\s*\n", text.strip()) if p.strip()]
    chunks = []
    for pi, paragraph in enumerate(paragraphs):
        paragraph = " ".join(paragraph.split())
        current = []
        words = 0
        for sentence in split_sentences(paragraph):
            n = len(sentence.split())
            if n > max_words:
                if current:
                    chunks.append({"text": " ".join(current),
                                   "paragraph_break_after": False})
                    current = []
                    words = 0
                group = []
                group_words = 0
                for piece in split_long(sentence, max_words):
                    piece_words = len(piece.split())
                    if group and group_words + piece_words > max_words:
                        chunks.append({"text": " ".join(group),
                                       "paragraph_break_after": False})
                        group = []
                        group_words = 0
                    group.append(piece)
                    group_words += piece_words
                if group:
                    chunks.append({"text": " ".join(group),
                                   "paragraph_break_after": False})
                continue
            if current and words + n > max_words:
                chunks.append({"text": " ".join(current),
                               "paragraph_break_after": False})
                current = []
                words = 0
            current.append(sentence)
            words += n
        if current:
            chunks.append({
                "text": " ".join(current),
                "paragraph_break_after": pi < len(paragraphs) - 1,
            })
        elif chunks and pi < len(paragraphs) - 1:
            # The paragraph ended with a long sentence's sub-chunks.
            chunks[-1]["paragraph_break_after"] = True
    return chunks


def ref_hash(ref_wav, ref_txt):
    h = hashlib.sha256()
    h.update(Path(ref_wav).read_bytes())
    h.update(b"\0")
    if Path(ref_txt).exists():
        h.update(Path(ref_txt).read_text().encode())
    return h.hexdigest()


def chunk_key(spoken, ref_digest):
    return hashlib.sha256(
        (MODEL_ID + "\0" + ref_digest + "\0" + spoken).encode()).hexdigest()


def make_model_synth(ref_wav, ref_txt):
    """Load the Base model and build the clone prompt once."""
    try:
        import torch
        import soundfile as sf
        from qwen_tts import Qwen3TTSModel
    except ImportError as exc:
        raise SystemExit(
            "[narrate] the TTS stack is missing (%s); run with "
            "~/sdb-video/venv-qwen/bin/python" % exc)
    torch.set_num_threads(16)
    model = Qwen3TTSModel.from_pretrained(
        MODEL_ID, device_map="cpu", dtype=torch.float32)
    prompt = model.create_voice_clone_prompt(
        ref_audio=str(ref_wav), ref_text=Path(ref_txt).read_text())

    def synth(text, path):
        wavs, sr = model.generate_voice_clone(
            text=text, language="English", voice_clone_prompt=prompt)
        sf.write(path, wavs[0], sr)

    return synth


def join_audio(rendered, out_path):
    """Join chunk wavs with a short sentence pause and a long paragraph one."""
    import numpy as np
    import soundfile as sf
    pieces = []
    sr = None
    for chunk, path in rendered:
        wav, this_sr = sf.read(str(path), dtype="float32")
        if sr is None:
            sr = this_sr
        pieces.append(wav)
        pause = PAUSE_PARAGRAPH if chunk["paragraph_break_after"] else PAUSE_SENTENCE
        pieces.append(np.zeros(int(sr * pause), dtype=np.float32))
    audio = np.concatenate(pieces) if pieces else np.zeros(0, dtype=np.float32)
    out_path = Path(out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = out_path.with_suffix(".tmp.wav")
    sf.write(str(tmp), audio, sr or 24000)
    subprocess.run(
        ["ffmpeg", "-y", "-v", "error", "-i", str(tmp),
         "-codec:a", "libmp3lame", "-q:a", "2", str(out_path)],
        check=True)
    tmp.unlink()
    return len(audio) / (sr or 24000)


def narrate_text(text, out_path, ref_wav, ref_txt, cache_dir,
                 synth=None, join=True):
    """Render text; a cache hit reuses its chunk. Returns the run stats."""
    chunks = chunk_text(text)
    cache_dir = Path(cache_dir)
    cache_dir.mkdir(parents=True, exist_ok=True)
    digest = ref_hash(ref_wav, ref_txt)
    entries = load_lexicon()
    rendered = []
    generated = 0
    reused = 0
    start = time.time()
    for chunk in chunks:
        spoken = apply_lexicon(chunk["text"], entries)
        path = cache_dir / (chunk_key(spoken, digest) + ".wav")
        if path.exists() and path.stat().st_size > 0:
            reused += 1
        else:
            if synth is None:
                synth = make_model_synth(ref_wav, ref_txt)
            synth(spoken, path)
            generated += 1
        rendered.append((chunk, path))
    audio_seconds = join_audio(rendered, out_path) if join else 0.0
    wall = time.time() - start
    words = len(text.split())
    wpm = (words / (audio_seconds / 60.0)) if audio_seconds > 0 else 0.0
    return {
        "chunks": len(chunks),
        "generated": generated,
        "reused": reused,
        "audio_seconds": audio_seconds,
        "wall_seconds": wall,
        "words": words,
        "wpm": wpm,
    }


def repo_roots():
    """The worktree and the main checkout of the same repo."""
    roots = [ROOT]
    try:
        common = subprocess.run(
            ["git", "-C", str(ROOT), "rev-parse", "--path-format=absolute",
             "--git-common-dir"], capture_output=True, text=True,
            check=True).stdout.strip()
        main = Path(common).parent.resolve()
        if main != ROOT:
            roots.append(main)
    except Exception:
        pass
    return roots


def refuse_repo_output(path):
    """Every checkout of the repo is refused; portal/ is the samples' exception."""
    target = Path(path).resolve()
    for root in repo_roots():
        try:
            rel = target.relative_to(root)
        except ValueError:
            continue
        if rel.parts and rel.parts[0] == "portal":
            continue
        raise SystemExit(
            "[narrate] refusing to write inside the repo: %s (only portal/ is "
            "allowed)" % target)


def run_self_test():
    tmp = Path(tempfile.mkdtemp(prefix="sdb-narrate-"))
    try:
        # The splitter: whole sentences, the word limit, paragraph marks,
        # and decimals/abbreviations that do not end a sentence.
        text = (
            "The best model scored 65.0 points, e.g. the top one. It spent "
            "$0.99.\n\n"
            "This paragraph has a deliberately long sentence that keeps "
            "going, with clause after clause, because the splitter has to "
            "respect the word limit without losing any of the words, and it "
            "still has to keep the paragraph break at the end of the block."
        )
        chunks = chunk_text(text)
        assert chunks, "the splitter produced no chunks"
        assert all(len(c["text"].split()) <= MAX_WORDS for c in chunks), chunks
        assert chunks[0]["text"].find("65.0") >= 0, chunks[0]
        assert chunks[0]["text"].find("e.g.") >= 0, chunks[0]
        assert any(c["paragraph_break_after"] for c in chunks[:-1]), chunks
        source_words = text.split()
        joined_words = " ".join(c["text"] for c in chunks).split()
        assert joined_words == source_words, (joined_words, source_words)

        # A long sentence between two short ones: its chunks stay its
        # own, never merged with the neighbours.
        mixed = (
            "Short before. When the answers differ, even by a single row, "
            "in a single query, on a single ticket, the ticket fails, the "
            "points are lost, and the model has to decide whether to spend "
            "what is left of its dollar fixing the bug or moving on. Short "
            "after."
        )
        mixed_chunks = chunk_text(mixed)
        long_chunks = [c["text"] for c in mixed_chunks if "answers differ" in c["text"]]
        assert long_chunks, mixed_chunks
        assert all(
            "Short before." not in t and "Short after." not in t
            for t in long_chunks), mixed_chunks

        # The lexicon: whole words, amounts and decimals as rules.
        spoken = apply_lexicon("$0.99 and 65.0 and 76")
        assert "ninety-nine cents" in spoken, spoken
        assert "sixty-five point zero" in spoken, spoken
        assert "76" in spoken, spoken
        assert apply_lexicon("SQLite") == "sequel lite", apply_lexicon("SQLite")
        assert apply_lexicon("lunatic") == "lunatic", apply_lexicon("lunatic")
        assert apply_lexicon("DeepSeek and MiMo") == "deep seek and mimo", \
            apply_lexicon("DeepSeek and MiMo")
        assert apply_lexicon("subdollarbench.org") == "sub dollar bench dot org", \
            apply_lexicon("subdollarbench.org")
        assert apply_lexicon("glm-5.3-flash") == "g l m five point three flash", \
            apply_lexicon("glm-5.3-flash")
        assert apply_lexicon("mimo-flash") == "mimo flash", \
            apply_lexicon("mimo-flash")
        assert apply_lexicon("luna-pro") == "luna pro", \
            apply_lexicon("luna-pro")

        # The cache: a second run generates nothing; one changed sentence
        # regenerates one chunk.
        video = tmp / "video"
        voice = video / "voice"
        cache = video / "cache"
        voice.mkdir(parents=True)
        (voice / "ref.wav").write_bytes(b"RIFF-fake")
        (voice / "ref.txt").write_text("hello")
        story = (
            "First paragraph with a sentence that has a few words.\n\n"
            "Second paragraph with another sentence."
        )

        def stub(text, path):
            Path(path).write_bytes(b"RIFF-stub")

        first = narrate_text(story, tmp / "out.mp3", voice / "ref.wav",
                             voice / "ref.txt", cache, synth=stub, join=False)
        assert first["generated"] == 2 and first["reused"] == 0, first
        second = narrate_text(story, tmp / "out.mp3", voice / "ref.wav",
                              voice / "ref.txt", cache, synth=stub, join=False)
        assert second["generated"] == 0 and second["reused"] == 2, second
        edited = story.replace("another sentence", "a different sentence")
        third = narrate_text(edited, tmp / "out.mp3", voice / "ref.wav",
                             voice / "ref.txt", cache, synth=stub, join=False)
        assert third["generated"] == 1 and third["reused"] == 1, third

        # --show prints the spoken text without a model.
        show_file = tmp / "show.txt"
        show_file.write_text("It scored 65.0 and cost $0.99.")
        shown = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--show",
             str(show_file)], capture_output=True, text=True)
        assert shown.returncode == 0, shown.stderr
        assert "sixty-five point zero" in shown.stdout, shown.stdout
        assert "ninety-nine cents" in shown.stdout, shown.stdout

        # A missing ref.wav names the path and video/voice-ref.sh.
        env = dict(os.environ, SDB_VIDEO_DIR=str(video),
                   SDB_VOICE_DIR=str(voice), SDB_CACHE_DIR=str(cache))
        (voice / "ref.wav").unlink()
        text_file = tmp / "text.txt"
        text_file.write_text("One sentence.")
        missing = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), str(text_file),
             str(tmp / "out.mp3")], env=env, capture_output=True, text=True)
        assert missing.returncode != 0, missing.stdout
        assert "ref.wav" in missing.stderr, missing.stderr
        assert "video/voice-ref.sh" in missing.stderr, missing.stderr

        # An output inside the repo is refused; portal/ is allowed.
        inside = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), str(text_file),
             str(ROOT / "not-portal.mp3")], env=env, capture_output=True,
            text=True)
        assert inside.returncode != 0, inside.stdout
        assert "inside the repo" in inside.stderr, inside.stderr
        for root in repo_roots():
            try:
                refuse_repo_output(root / "not-portal.mp3")
            except SystemExit as exc:
                assert "inside the repo" in str(exc), exc
            else:
                raise AssertionError("a repo path was allowed: %s" % root)
            refuse_repo_output(root / "portal" / "video" / "sample.mp3")

        print("[narrate self-test] the splitter keeps sentences and the word "
              "limit, the lexicon speaks amounts and decimals, the cache "
              "reuses and regenerates one chunk, --show works, and a missing "
              "ref or a repo output is refused")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(
        description="Narrate a text file in the operator's cloned voice.")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--show", metavar="TEXT_FILE",
                        help="print the text as it will be spoken")
    parser.add_argument("text_file", nargs="?")
    parser.add_argument("out", nargs="?")
    args = parser.parse_args()
    if args.self_test:
        run_self_test()
        return
    if args.show:
        print(apply_lexicon(Path(args.show).read_text()), end="")
        return
    if not args.text_file or not args.out:
        parser.error("a text file and an output are needed (or --show/--self-test)")
    refuse_repo_output(args.out)
    if not REF_WAV.exists():
        raise SystemExit(
            "[narrate] ref.wav is missing: %s\n"
            "[narrate] record and install it with video/voice-ref.sh" % REF_WAV)
    text = Path(args.text_file).read_text()
    stats = narrate_text(text, args.out, REF_WAV, REF_TXT, CACHE_DIR)
    print("[narrate] %d chunks generated, %d reused; audio %.1f s; wall "
          "%.1f s; %.1f wpm" % (stats["generated"], stats["reused"],
                                stats["audio_seconds"], stats["wall_seconds"],
                                stats["wpm"]))


if __name__ == "__main__":
    main()
