#!/bin/bash
# Regenerates the *.bin recordings (raw PTY output of real programs, captured with script(1)) that
# tests/p1.rs replays. Needs vim, htop, top, less, man, nano, tmux, git and python3; the output of
# the interactive ones depends on timing and the machine, so a re-run changes the bytes (the tests
# do not care). The file name is NAME-COLSxROWS.bin.
export PATH=$HOME/.cargo/bin:$PATH
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE
mkdir -p "$OUT"
# The scratch HOME of the claude recording (the program writes its first-run state there).
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export tmp

# rec NAME COLS ROWS 'command' ['keys-producer shell snippet']
rec() {
  local name=$1 cols=$2 rows=$3 cmd=$4 keys=${5:-}
  local f="$OUT/$name-${cols}x${rows}.bin"
  rm -f "$f"
  if [ -n "$keys" ]; then
    (eval "$keys") | TERM=xterm-256color COLUMNS=$cols LINES=$rows timeout 30 script -q -c "stty cols $cols rows $rows; $cmd" -O "$f" >/dev/null 2>&1
  else
    TERM=xterm-256color COLUMNS=$cols LINES=$rows timeout 30 script -q -c "stty cols $cols rows $rows; $cmd" -O "$f" </dev/null >/dev/null 2>&1
  fi
  local rc=$?
  # strip script's "Script started/done" lines
  python3 - "$f" <<'PY'
import sys
p = sys.argv[1]
d = open(p, 'rb').read()
if d.startswith(b'Script started'):
    d = d[d.index(b'\n') + 1:]
i = d.rfind(b'\nScript done')
if i >= 0:
    d = d[:i]
open(p, 'wb').write(d)
PY
  echo "$name: rc=$rc size=$(stat -c %s "$f")"
}

rec ls_usr 100 30 'ls -la --color=always /usr'
rec seq_5000 80 24 'seq 1 5000'
rec htop 120 40 'timeout 2 htop -d 1 -p 1' 'sleep 3'
rec vim_services 100 30 'vim -u NONE -N -i NONE /etc/services' \
  'sleep 1; printf "G"; sleep .3; printf "gg"; sleep .3; printf "/ssh\r"; sleep .3; printf "n"; sleep .3; printf "n"; sleep .3; printf ":set nu\r"; sleep .3; printf "\006"; sleep .3; printf "\006"; sleep .3; printf "\002"; sleep .3; printf "5jdd"; sleep .3; printf "u"; sleep .3; printf ":q!\r"; sleep .5'
rec less_nav 100 30 'less -R /etc/services' \
  'sleep 1; printf " "; sleep .3; printf " "; sleep .3; printf "b"; sleep .3; printf "G"; sleep .3; printf "g"; sleep .3; printf "/http\r"; sleep .3; printf "n"; sleep .3; printf "q"; sleep .5'
rec cargo_help 100 30 'cargo --help'
rec git_log 100 30 "cd $HERE && git --no-pager log --color=always -p -5"
rec nano 100 30 'nano --ignorercfiles /etc/services' \
  'sleep 1; printf "\026"; sleep .3; printf "\026"; sleep .3; printf "\027ssh\r"; sleep .3; printf "\030"; sleep .5'
rec top 100 30 'timeout 3 top -d 1 -p 1' 'sleep 4'
rec stress 100 30 "python3 $HERE/gen_stress.py"
rec tmux 100 30 "tmux -f /dev/null -L femme-rec new-session -x 100 -y 30 'ls -la --color=always /usr; sleep 1' \; split-window -v 'seq 1 200; sleep 1' \; split-window -h 'cargo --help | head -20; sleep 1'" \
  'sleep 3; printf "\002d"; sleep .5'
tmux -L femme-rec kill-server 2>/dev/null
rec man_ls 100 30 'MANPAGER="less -R" man ls' \
  'sleep 1.5; printf " "; sleep .3; printf " "; sleep .3; printf "q"; sleep .5'
# The Claude Code TUI (Ink): first-run screens in an empty HOME with a scrubbed environment, so no
# credentials or settings are involved and nothing reaches the recording but the program's own screens.
rec claude_tui 100 30 'env -i PATH="$PATH" HOME="$tmp" TERM=xterm-256color COLORTERM=truecolor claude' \
  'sleep 6; printf "\r"; sleep 2; printf "\033[B"; sleep 1; printf "\033[B"; sleep 1; printf "\033[A"; sleep 1; printf "\033[B"; sleep 1; printf "\r"; sleep 3; printf "\033[B"; sleep 1; printf "\003"; sleep 1; printf "\003"; sleep 2'
