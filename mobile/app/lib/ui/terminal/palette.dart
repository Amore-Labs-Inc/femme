/// The terminal's colours: the desktop's FEMME skins (`desktop/src/terminal/runs.rs`), ink on
/// paper in both themes, 16 ANSI colours tuned to read on that paper, the xterm 256 cube and ramp,
/// and RGB as is.
library;

import 'dart:ui';

import '../../net/term_models.dart';

class TermPalette {
  const TermPalette._(this.fg, this.bg, this.ansi);

  final Color fg;
  final Color bg;
  final List<Color> ansi;

  static const light = TermPalette._(Color(0xFF1E1E1E), Color(0xFFFEFEFE), [
    Color(0xFF1E1E1E), // 0 black: ink
    Color(0xFFD93036), // 1 red
    Color(0xFF027A42), // 2 green
    Color(0xFF9C6A00), // 3 yellow
    Color(0xFF2D5BD0), // 4 blue
    Color(0xFFB8379A), // 5 magenta
    Color(0xFF00827A), // 6 cyan
    Color(0xFF858585), // 7 white: grey
    Color(0xFF5A5A5A), // 8 bright black
    Color(0xFFE8474D), // 9
    Color(0xFF029E55), // 10
    Color(0xFFB98000), // 11
    Color(0xFF4A7DF0), // 12
    Color(0xFFD45BB6), // 13 pink
    Color(0xFF08968B), // 14
    Color(0xFFA3A3A3), // 15
  ]);

  static const dark = TermPalette._(Color(0xFFECECEC), Color(0xFF1E1E1E), [
    Color(0xFF5F5F5F), // 0 black: a grey that still reads
    Color(0xFFFF6B70), // 1
    Color(0xFF2FD07A), // 2
    Color(0xFFF5C24B), // 3
    Color(0xFF6C9BFF), // 4
    Color(0xFFD45BB6), // 5
    Color(0xFF2CD3C4), // 6
    Color(0xFFC4C4C4), // 7
    Color(0xFF8C8C8C), // 8
    Color(0xFFFF8A8E), // 9
    Color(0xFF5BE69A), // 10
    Color(0xFFFFD66E), // 11
    Color(0xFF8FB4FF), // 12
    Color(0xFFF28FD8), // 13
    Color(0xFF5CE3D6), // 14
    Color(0xFFF5F5F5), // 15
  ]);

  static TermPalette of(Brightness b) => b == Brightness.dark ? dark : light;

  static const _cube = [0, 95, 135, 175, 215, 255];

  /// A wire colour: default → [orElse], 0-15 the ANSI table, 16-231 the 6×6×6 cube, 232-255 the
  /// grey ramp `8 + 10n`, RGB as is.
  Color color(TermColor c, Color orElse) {
    final rgb = c.rgb;
    if (rgb != null) return Color.fromARGB(255, rgb.$1, rgb.$2, rgb.$3);
    final i = c.index;
    if (i == null) return orElse;
    if (i < 16) return ansi[i];
    if (i < 232) {
      final n = i - 16;
      return Color.fromARGB(
        255,
        _cube[n ~/ 36],
        _cube[n ~/ 6 % 6],
        _cube[n % 6],
      );
    }
    final g = 8 + 10 * (i - 232);
    return Color.fromARGB(255, g, g, g);
  }

  /// A style as drawn: INVERSE swaps fg and bg, DIM blends fg halfway to the bg, a bg equal to
  /// the paper is none. BOLD only picks the bold face.
  Resolved resolve(Style s) {
    final a = s.attrs;
    var f = color(s.fg, fg);
    var b = color(s.bg, bg);
    if (a.contains(Attrs.inverse)) (f, b) = (b, f);
    if (a.contains(Attrs.dim)) f = Color.lerp(f, b, 0.5)!;
    final under =
        a.contains(Attrs.underline) ||
        a.contains(Attrs.doubleUnderline) ||
        a.contains(Attrs.curlyUnderline) ||
        a.contains(Attrs.dottedUnderline) ||
        a.contains(Attrs.dashedUnderline);
    return Resolved(
      fg: f,
      bg: b == bg ? null : b,
      bold: a.contains(Attrs.bold),
      italic: a.contains(Attrs.italic),
      underline: under,
      underlineColor: s.underline.isDefault ? null : color(s.underline, f),
      strike: a.contains(Attrs.strikethrough),
      hidden: a.contains(Attrs.hidden),
    );
  }
}

class Resolved {
  const Resolved({
    required this.fg,
    this.bg,
    this.bold = false,
    this.italic = false,
    this.underline = false,
    this.underlineColor,
    this.strike = false,
    this.hidden = false,
  });
  final Color fg;
  final Color? bg;
  final bool bold, italic, underline, strike, hidden;
  final Color? underlineColor;
}
