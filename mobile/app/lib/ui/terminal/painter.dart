/// Draws the terminal grid: one recorded picture per row (kept while the row is unchanged), then
/// the selection and the cursor on top. Monospace cells measured from the font.
library;

import 'dart:ui' as ui;

import 'package:flutter/widgets.dart';

import '../../net/term_models.dart';
import 'model.dart';
import 'palette.dart';

/// `JetBrains Mono NL` when the app bundles it, else the platform's monospace.
const monoFamily = 'JetBrains Mono NL';
const monoFallback = [
  'Menlo',
  'SF Mono',
  'Roboto Mono',
  'Droid Sans Mono',
  'monospace',
  'Courier New',
];
const _lineHeight = 1.18;

TextStyle monoStyle(double size) => TextStyle(
  fontFamily: monoFamily,
  fontFamilyFallback: monoFallback,
  fontSize: size,
  height: _lineHeight,
  fontFeatures: const [
    FontFeature.disable('liga'),
    FontFeature.disable('calt'),
  ],
);

/// The size of one cell for a font size.
class CellMetrics {
  const CellMetrics(this.fontSize, this.width, this.height);
  final double fontSize;
  final double width;
  final double height;

  factory CellMetrics.measure(double fontSize) {
    final tp = TextPainter(
      text: TextSpan(text: 'MMMMMMMMMM', style: monoStyle(fontSize)),
      textDirection: TextDirection.ltr,
    )..layout();
    final m = CellMetrics(fontSize, tp.width / 10, tp.height.ceilToDouble());
    tp.dispose();
    return m;
  }

  @override
  bool operator ==(Object other) =>
      other is CellMetrics && other.fontSize == fontSize;
  @override
  int get hashCode => fontSize.hashCode;
}

/// A piece of a row drawn at once: [cols] columns from [col], one style.
class Piece {
  const Piece(this.col, this.cols, this.style, this.text);
  final int col;
  final int cols;
  final int style;
  final String text;
  @override
  String toString() => 'Piece($col+$cols s$style "$text")';
}

/// Zero-width code points (combining marks, joiners, variation selectors): they ride with the
/// char before them.
bool isZeroWidth(int r) =>
    (r >= 0x300 && r <= 0x36F) ||
    (r >= 0x1AB0 && r <= 0x1AFF) ||
    (r >= 0x1DC0 && r <= 0x1DFF) ||
    (r >= 0x20D0 && r <= 0x20FF) ||
    (r >= 0x200B && r <= 0x200F) ||
    (r >= 0xFE00 && r <= 0xFE0F) ||
    (r >= 0xFE20 && r <= 0xFE2F) ||
    (r >= 0xE0100 && r <= 0xE01EF);

/// Splits a row into pieces by column. A char takes one column, two when the next column is in
/// `wide_cols` (the server decides wideness). ASCII stays together; any other char is a piece of
/// its own at its column, so a fallback font's advance never shifts the rest of the row. Nothing
/// at or past [maxCols].
List<Piece> layoutRow(RowPatch row, int maxCols) {
  final out = <Piece>[];
  final wide = row.wideCols.toSet();
  var col = 0;
  for (final (style, text) in row.runs) {
    if (col >= maxCols) break;
    final ascii = StringBuffer();
    var asciiCol = col;
    void flush() {
      if (ascii.isEmpty) return;
      out.add(Piece(asciiCol, col - asciiCol, style, ascii.toString()));
      ascii.clear();
    }

    final runes = text.runes.toList();
    for (var i = 0; i < runes.length; i++) {
      final r = runes[i];
      if (isZeroWidth(r)) continue; // taken with the char before, below
      if (col >= maxCols) break;
      final w = wide.contains(col + 1) ? 2 : 1;
      if (col + w > maxCols) {
        col = maxCols;
        break;
      }
      final marked = i + 1 < runes.length && isZeroWidth(runes[i + 1]);
      if (r < 0x80 && !marked) {
        if (ascii.isEmpty) asciiCol = col;
        ascii.writeCharCode(r);
        col += w;
        continue;
      }
      flush();
      final b = StringBuffer()..writeCharCode(r);
      while (i + 1 < runes.length && isZeroWidth(runes[i + 1])) {
        b.writeCharCode(runes[++i]);
      }
      out.add(Piece(col, w, style, b.toString()));
      col += w;
    }
    flush();
  }
  return out;
}

/// The text of the cell at [col], for drawing it inside the block cursor.
String cellAt(RowPatch? row, int col) {
  if (row == null) return ' ';
  for (final p in layoutRow(row, col + 2)) {
    if (p.col <= col && col < p.col + p.cols) {
      // ASCII pieces hold one char per column; any other piece is one char (and its marks).
      final ascii = p.text.codeUnits.every((u) => u < 0x80);
      return ascii ? p.text[col - p.col] : p.text;
    }
  }
  return ' ';
}

/// Recorded rows, valid for one palette and font size.
class RowPictures {
  final _pics = <int, (RowPatch, int, ui.Picture)>{};
  TermPalette? _palette;
  CellMetrics? _metrics;

  ui.Picture get(
    int abs,
    RowPatch row,
    int cols,
    TermGrid grid,
    TermPalette palette,
    CellMetrics m,
  ) {
    if (!identical(palette, _palette) || m != _metrics) {
      clear();
      _palette = palette;
      _metrics = m;
    }
    final hit = _pics[abs];
    if (hit != null && identical(hit.$1, row) && hit.$2 == cols) return hit.$3;
    hit?.$3.dispose();
    final rec = ui.PictureRecorder();
    _paintRow(Canvas(rec), row, cols, grid, palette, m);
    final pic = rec.endRecording();
    _pics[abs] = (row, cols, pic);
    return pic;
  }

  /// Drops the pictures of rows outside `[from, to)`.
  void keep(int from, int to) {
    _pics.removeWhere((abs, v) {
      final drop = abs < from || abs >= to;
      if (drop) v.$3.dispose();
      return drop;
    });
  }

  void clear() {
    for (final v in _pics.values) {
      v.$3.dispose();
    }
    _pics.clear();
  }
}

void _paintRow(
  Canvas c,
  RowPatch row,
  int cols,
  TermGrid grid,
  TermPalette palette,
  CellMetrics m,
) {
  final fill = Paint();
  for (final p in layoutRow(row, cols)) {
    final s = palette.resolve(grid.style(p.style));
    final x = p.col * m.width;
    if (s.bg != null) {
      c.drawRect(
        Rect.fromLTWH(x, 0, p.cols * m.width, m.height),
        fill..color = s.bg!,
      );
    }
    if (s.hidden) continue;
    final blank = p.text.trim().isEmpty;
    if (blank && !s.underline && !s.strike) continue;
    final tp = TextPainter(
      text: TextSpan(
        text: p.text,
        style: monoStyle(m.fontSize).copyWith(
          color: s.fg,
          fontWeight: s.bold ? FontWeight.w700 : FontWeight.w400,
          fontStyle: s.italic ? FontStyle.italic : FontStyle.normal,
          decoration: TextDecoration.combine([
            if (s.underline) TextDecoration.underline,
            if (s.strike) TextDecoration.lineThrough,
          ]),
          decorationColor: s.underlineColor ?? s.fg,
        ),
      ),
      textDirection: TextDirection.ltr,
      maxLines: 1,
    )..layout();
    // A glyph wider than its cells (a fallback font) is squeezed, never pushes the row.
    final room = p.cols * m.width;
    if (tp.width > room + 0.5 && p.cols <= 2) {
      c.save();
      c.translate(x, 0);
      c.scale(room / tp.width, 1);
      tp.paint(c, Offset.zero);
      c.restore();
    } else {
      tp.paint(c, Offset(x, 0));
    }
    tp.dispose();
  }
  if (row.wrapped) {
    // A soft-wrapped row: a hairline tick at the right edge.
    c.drawRect(
      Rect.fromLTWH(cols * m.width - 1.5, m.height * 0.3, 1.5, m.height * 0.4),
      fill..color = palette.ansi[8].withValues(alpha: 0.6),
    );
  }
}

class TermPainter extends CustomPainter {
  TermPainter({
    required this.grid,
    required this.pics,
    required this.palette,
    required this.metrics,
    required this.topAbs,
    required this.dx,
    required this.showCursor,
    required this.cursorColor,
    this.selection,
    this.selectionColor,
    super.repaint,
  });

  final TermGrid grid;
  final RowPictures pics;
  final TermPalette palette;
  final CellMetrics metrics;

  /// The absolute row drawn at the top.
  final int topAbs;

  /// Horizontal pan, in pixels (when the holder's screen is wider than the phone's).
  final double dx;
  final bool showCursor;
  final Color cursorColor;

  /// Selected rows, `[from, to]` absolute.
  final (int, int)? selection;
  final Color? selectionColor;

  @override
  void paint(Canvas canvas, Size size) {
    final m = metrics;
    canvas.drawRect(Offset.zero & size, Paint()..color = palette.bg);
    canvas.save();
    canvas.clipRect(Offset.zero & size);
    canvas.translate(-dx, 0);
    final n = (size.height / m.height).ceil();
    final cols = grid.cols > 0 ? grid.cols : (size.width / m.width).floor();
    for (var i = 0; i < n; i++) {
      final abs = topAbs + i;
      final row = grid.row(abs);
      if (row == null) continue;
      canvas.save();
      canvas.translate(0, i * m.height);
      canvas.drawPicture(pics.get(abs, row, cols, grid, palette, m));
      canvas.restore();
    }
    pics.keep(topAbs - 50, topAbs + n + 50);
    final sel = selection;
    if (sel != null && selectionColor != null) {
      final a = (sel.$1 < sel.$2 ? sel.$1 : sel.$2) - topAbs;
      final b = (sel.$1 < sel.$2 ? sel.$2 : sel.$1) - topAbs;
      canvas.drawRect(
        Rect.fromLTRB(dx, a * m.height, dx + size.width, (b + 1) * m.height),
        Paint()..color = selectionColor!,
      );
    }
    final cur = grid.cursor;
    final cy = grid.topAbs + cur.row - topAbs;
    if (showCursor && cur.visible && cy >= 0 && cy < n) {
      final r = Rect.fromLTWH(
        cur.col * m.width,
        cy * m.height,
        m.width,
        m.height,
      );
      final p = Paint()..color = cursorColor;
      switch (cur.shape) {
        case CursorShape.block:
          canvas.drawRect(r, p);
          final tp = TextPainter(
            text: TextSpan(
              text: cellAt(grid.row(grid.topAbs + cur.row), cur.col),
              style: monoStyle(m.fontSize).copyWith(color: palette.bg),
            ),
            textDirection: TextDirection.ltr,
          )..layout();
          tp.paint(canvas, r.topLeft);
          tp.dispose();
        case CursorShape.bar:
          canvas.drawRect(Rect.fromLTWH(r.left, r.top, 2, r.height), p);
        case CursorShape.underline:
          canvas.drawRect(Rect.fromLTWH(r.left, r.bottom - 2, r.width, 2), p);
      }
    }
    canvas.restore();
  }

  @override
  bool shouldRepaint(TermPainter old) => true;
}
