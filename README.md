# odj

Keyboard-driven DJ tools for the Linux terminal:

- **odj-player**, a DJ player inspired by the Pioneer CDJ-800.
- **odj-sampler**, which plays the cues stored by odj-player from 16 pads.

```
cargo run --release -p odj-player -- ~/Music      # browse a folder
cargo run --release -p odj-player -- track.mp3    # load a track straight away
cargo run --release -p odj-sampler                # 16 pads of stored cues
```

Needs `librubberband` (for Master Tempo), ALSA and PipeWire; building also
needs libclang. Plays MP3, FLAC, WAV, OGG/Vorbis, AAC/M4A and AIFF.

If there is more than one stereo output (say, a USB audio interface next to
the built-in card), odj-player and odj-sampler ask at start which one to play on: a stereo
output, or one side of it in mono. HDMI outputs are not listed. Audio goes
through PipeWire, so you can run one odj-player per output, e.g. two decks on
the two line outs of an interface. Without PipeWire, odj-player opens the card
directly and holds it while it runs.

Use a terminal with the kitty keyboard protocol (Ghostty, kitty, foot,
Alacritty, WezTerm) and run it outside tmux. Hold-to-preview cue, jog nudges
and search need key-release events; elsewhere those holds fall back to timers.

Press `?` in the app for all keys. Memory points, hot cues, loops and tapped
BPMs are remembered per track, one JSON file each under
`~/.local/share/odj/tracks/`. Files are named by a fingerprint of the audio, so
cues survive renaming, moving and retagging. Settings such as Auto Cue live in
`~/.config/odj/settings.json`.

## odj-sampler

Sixteen pads on the keys `1234 / QWER / ASDF / ZXCV`. Press Enter to put one
of your stored cues (hot cues, memory points, last loops) on the selected pad.
A loop goes on as it is. A cue without an end opens the sample editor, which
starts at 4 bars and lets you move the start and the end by beats, bars or
milliseconds and preview it; `T` brings a pad back into it. Each pad can be one-shot, gate (plays while held) or toggle, loop
or not, and has its own gain.

A pad holds its own copy of the audio, cut out and converted to the output
rate, so the track file isn't read again while you play. The header and the
memory panel show how much memory the pads take. The kit (which cue is on which
pad, the trimmed end and the pad settings) is saved as you go to
`~/.local/share/odj/kit.json`, and the pads are cut from the files again at the
next start. A pad whose file has moved or changed shows so.

## Layout

A Cargo workspace: `crates/odj-core` holds what both apps share (decoding and
fingerprinting, the cue library, audio output, the file browser);
`crates/odj-player` and `crates/odj-sampler` are the two binaries.

## Install on Omarchy / Arch

```
git clone https://github.com/ibz/odj && cd odj/packaging/arch
makepkg -si
```

This builds the latest `master`, pulls in `rubberband` and `alsa-lib`, and
installs `odj-player` and `odj-sampler` plus a launcher entry (Super+Space →
"odj player"), which opens in your default terminal. Run `makepkg -si` again
to update; `sudo pacman -R odj-git` removes it. Once it's on the AUR: `yay -S odj-git`.

## License

MIT, see [LICENSE](LICENSE).
