#!/usr/bin/env bash
# Launches the installed odj-mixer and two odj-player decks in a tmux
# session, split screen: odj-mixer on the left, the two decks stacked on
# the right.
set -euo pipefail

session=odj

if tmux has-session -t "$session" 2>/dev/null; then
    exec tmux attach -t "$session"
fi

# Plain panes first, addressed by their pane id (stable regardless of the
# user's tmux `base-index`/window numbering) rather than a window index,
# and with remain-on-exit so one that fails stays put showing why instead
# of vanishing and taking the window down with it — then the real
# commands go in via send-keys.
tmux new-session -d -s "$session"
tmux set-option -t "$session" remain-on-exit on
mixer=$(tmux display-message -p -t "$session" '#{pane_id}')
deck1=$(tmux split-window -h -P -F '#{pane_id}' -t "$mixer")
deck2=$(tmux split-window -v -P -F '#{pane_id}' -t "$deck1")

tmux send-keys -t "$mixer" 'odj-mixer' Enter
tmux send-keys -t "$deck1" 'odj-player --deck 1' Enter
tmux send-keys -t "$deck2" 'odj-player --deck 2' Enter
tmux select-pane -t "$mixer"

exec tmux attach -t "$session"
