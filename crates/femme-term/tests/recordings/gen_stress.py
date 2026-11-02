#!/usr/bin/env python3
"""Deterministic terminal stress output for the femme-term recordings (100x30)."""
import sys

w = sys.stdout.write
ESC = "\x1b"


def csi(s):
    w(ESC + "[" + s)


def line(s=""):
    w(s + "\r\n")


csi("2J"); csi("H")
w(ESC + "]0;femme stress\x07")
line("== wide, emoji, combining ==")
line("あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわをん" * 2)
line("日本語のテキスト、全角文字がラインの最後の列で折り返す様子をテストします。" * 3)
line("emoji: \U0001F600 \U0001F44D\U0001F3FD \U0001F468‍\U0001F469‍\U0001F467 \U0001F1E9\U0001F1EA flags and zwj")
line("combining: é ä ñ ộ नमस्ते x⃝")
line("box: ┌──┐ │  │ └──┘ █▓▒░ ⠀⣿")
line("x" * 99 + "あ" + "y" * 98 + "\U0001F600z")
line("== sgr matrix ==")
for bg in list(range(40, 48)) + list(range(100, 108)):
    w("".join(f"{ESC}[{fg};{bg}m{fg:3}" for fg in list(range(30, 38)) + list(range(90, 98))) + f"{ESC}[0m")
    line()
w("".join(f"{ESC}[48;5;{i}m " for i in range(256)) + f"{ESC}[0m")
line()
w("".join(f"{ESC}[38;2;{i};{255 - i};{(i * 7) % 256}mX" for i in range(256)) + f"{ESC}[0m")
line()
for code, name in [(1, "bold"), (2, "dim"), (3, "italic"), (4, "underline"), (9, "strike"), (7, "inverse"),
                   (8, "hidden"), (5, "blink"), ("4:3", "curly"), ("4:4", "dotted"), ("4:5", "dashed"), (21, "double")]:
    w(f"{ESC}[{code}m{name}{ESC}[0m ")
w(f"{ESC}[1;3;4;31;44mall at once{ESC}[22;23;24;39;49m then none{ESC}[58;2;255;0;0;4:3mcolored underline{ESC}[0m")
line()
line("== regions, scrolling ==")
csi("5;12r")
csi("12;1H")
for i in range(30):
    line(f"region line {i}")
csi("r")
csi("H")
csi("3S")
csi("2T")
csi("20;1H")
for i in range(25):
    line(f"primary scroll {i} " + "." * (i * 3))
w(ESC + "7")
csi("3;3H"); w("saved/restore")
w(ESC + "8")
line("== edit ==")
w("abcdefghij"); csi("5D"); csi("3@"); csi("2P"); csi("X"); line()
csi("2L"); csi("M"); csi("4;4H"); csi("1K"); csi("0J")
line("tabs:\ta\tb\tc\td"); w("back\x08\x08ck"); line()
csi("?7l"); line("nowrap " + "z" * 150); csi("?7h")
csi("?25l"); csi("?25h")
w(ESC + "(0" + "lqqqqk" + ESC + "(B"); line()
csi("?1049h"); csi("2J"); csi("H")
for i in range(30):
    csi(f"{i + 1};1H"); w(f"alt screen row {i} " + "#" * i)
csi("?1049l")
line("back on primary")
csi("2J"); csi("H")
for i in range(40):
    line(f"after clear {i}")
csi("3J")
line("end")
