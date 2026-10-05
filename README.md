# odj

A keyboard-driven DJ player for the Linux terminal, inspired by the Pioneer CDJ-800.

```
cargo run --release -- ~/Music          # browse a folder
cargo run --release -- track.mp3        # load a track straight away
```

Needs `librubberband` (for Master Tempo) and ALSA/PipeWire. Plays MP3, FLAC,
WAV, OGG/Vorbis, AAC/M4A and AIFF.

Use a terminal with the kitty keyboard protocol (Ghostty, kitty, foot,
Alacritty, WezTerm) and run it outside tmux. Hold-to-preview cue, jog nudges
and search need key-release events; elsewhere those holds fall back to timers.

Press `?` in the app for all keys. Memory points, hot cues, loops and tapped
BPMs are remembered per track in `~/.local/share/odj/memory.json`; settings
such as Auto Cue live in `settings.json` next to it.
