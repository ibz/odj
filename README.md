# odj

A keyboard-driven DJ player for the Linux terminal, inspired by the Pioneer CDJ-800.

```
cargo run --release -- ~/Music          # browse a folder
cargo run --release -- track.mp3        # load a track straight away
```

Needs `librubberband` (for Master Tempo), ALSA and PipeWire; building also
needs libclang. Plays MP3, FLAC, WAV, OGG/Vorbis, AAC/M4A and AIFF.

If there is more than one stereo output (say, a USB audio interface next to
the built-in card), odj asks at start which one to play on: a stereo output,
or one side of it in mono. HDMI outputs are not listed. Audio goes through
PipeWire, so you can run one odj per output, e.g. two decks on the two line
outs of an interface. Without PipeWire, odj opens the card directly and holds
it while it runs.

Use a terminal with the kitty keyboard protocol (Ghostty, kitty, foot,
Alacritty, WezTerm) and run it outside tmux. Hold-to-preview cue, jog nudges
and search need key-release events; elsewhere those holds fall back to timers.

Press `?` in the app for all keys. Memory points, hot cues, loops and tapped
BPMs are remembered per track, one JSON file each under
`~/.local/share/odj/tracks/`. Files are named by a fingerprint of the audio, so
cues survive renaming, moving and retagging. Settings such as Auto Cue live in
`~/.config/odj/settings.json`.

## Install on Omarchy / Arch

```
git clone https://github.com/ibz/odj && cd odj/packaging/arch
makepkg -si
```

This builds the latest `master`, pulls in `rubberband` and `alsa-lib`, and
installs `odj` plus a launcher entry (Super+Space → "odj"), which opens in
your default terminal. Run `makepkg -si` again to update; `sudo pacman -R
odj-git` removes it. Once it's on the AUR: `yay -S odj-git`.

## License

MIT, see [LICENSE](LICENSE).
