/// The terminal's drawing surface: sizes the holder's screen to the room it has, scrolls into
/// scrollback (asking for rows as it goes), pans when the screen is wider than the phone, pinches
/// to zoom, and selects rows by long press to copy them. A program that reports the mouse gets
/// taps, the wheel and drags instead (`mouse.dart`).
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/term_models.dart';
import '../../state/term.dart';
import '../theme.dart';
import 'mouse.dart';
import 'painter.dart';
import 'palette.dart';

class TermSurface extends StatefulWidget {
  const TermSurface({
    super.key,
    required this.view,
    required this.onTap,
    this.onCopied,
  });

  final TermView view;

  /// A tap on the screen (focuses the keyboard).
  final VoidCallback onTap;

  /// Rows were copied to the clipboard.
  final void Function(int lines)? onCopied;

  @override
  State<TermSurface> createState() => _TermSurfaceState();
}

class _TermSurfaceState extends State<TermSurface> {
  final _pics = RowPictures();
  double _fontSize = 13;
  double _pinchFrom = 13;
  CellMetrics? _metrics;

  /// The absolute row at the top while scrolled up; null = live (the holder's screen).
  int? _anchor;
  double _dyRest = 0;
  double _dx = 0;
  int _pointers = 0;
  (int, int)? _selection;
  int _rowsFit = 24;
  final _wheel = WheelSteps();

  /// A long press holds the program's left button down (drag mode); the last cell it was on.
  (int, int)? _held;

  TermView get _view => widget.view;

  @override
  void dispose() {
    _pics.clear();
    super.dispose();
  }

  CellMetrics get _m =>
      (_metrics?.fontSize == _fontSize ? _metrics : null) ??
      (_metrics = CellMetrics.measure(_fontSize));

  int get _top => _anchor ?? _view.grid.topAbs;

  void _scrollRows(int n) {
    final g = _view.grid;
    if (g.screen == Screen.alt || !g.hasFrame) {
      // No scrollback on the alternate screen; a pager or editor gets arrow keys when it asks.
      if (g.modes.contains(Modes.alternateScroll)) {
        for (var i = 0; i < n.abs(); i++) {
          _view.key(n < 0 ? KeyCode.up : KeyCode.down);
        }
      }
      return;
    }
    var top = _top + n;
    if (top < g.historyLoAbs) top = g.historyLoAbs;
    setState(() => _anchor = top >= g.topAbs ? null : top);
    if (_anchor != null) _view.needRows(_anchor!);
  }

  void backToLive() => setState(() => _anchor = null);

  bool get _mouse => reportsMouse(_view.grid.modes);

  (int, int) _cell(Offset p) => touchCell(
    p,
    cellW: _m.width,
    cellH: _m.height,
    cols: _view.grid.cols,
    rows: _view.grid.rows,
    dx: _dx,
  );

  /// Sends [kind] at the cell under [p]; mouse events are about the live screen, so a view
  /// scrolled into scrollback goes back to it.
  void _mouseAt(MouseKind kind, Offset p) => _mouseCell(kind, _cell(p));

  void _mouseCell(MouseKind kind, (int, int) cell) {
    if (_anchor != null) setState(() => _anchor = null);
    _view.mouse(kind, col: cell.$1, row: cell.$2);
  }

  int _rowAt(double y) => _top + (y / _m.height).floor();

  void _copySelection() {
    final s = _selection;
    if (s == null) return;
    final a = s.$1 < s.$2 ? s.$1 : s.$2;
    final b = s.$1 < s.$2 ? s.$2 : s.$1;
    Clipboard.setData(ClipboardData(text: _view.grid.text(a, b + 1)));
    HapticFeedback.mediumImpact();
    widget.onCopied?.call(b - a + 1);
    setState(() => _selection = null);
  }

  @override
  Widget build(BuildContext context) {
    final brightness = CupertinoTheme.brightnessOf(context);
    final palette = TermPalette.of(brightness);
    return LayoutBuilder(
      builder: (context, box) {
        final m = _m;
        final cols = (box.maxWidth / m.width).floor();
        _rowsFit = (box.maxHeight / m.height).floor();
        _view.setSize(cols, _rowsFit);
        final g = _view.grid;
        final maxDx = (g.cols * m.width - box.maxWidth).clamp(
          0.0,
          double.infinity,
        );
        if (_dx > maxDx) _dx = maxDx;
        // Live: the holder's screen sits at the top; its rows may be fewer than fit.
        if (_anchor != null && _anchor! >= g.topAbs) _anchor = null;
        return Listener(
          onPointerDown: (_) => _pointers++,
          onPointerUp: (_) => _pointers--,
          onPointerCancel: (_) => _pointers--,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onTapUp: (d) {
              if (_selection == null && _mouse) {
                _mouseAt(MouseKind.press, d.localPosition);
                _mouseAt(MouseKind.release, d.localPosition);
              }
            },
            onTap: () {
              if (_selection != null) {
                setState(() => _selection = null);
                return;
              }
              widget.onTap();
            },
            onScaleStart: (_) => _pinchFrom = _fontSize,
            onScaleUpdate: (d) {
              if (_pointers >= 2 || d.pointerCount >= 2) {
                final size = (_pinchFrom * d.scale).clamp(8.0, 26.0);
                if ((size - _fontSize).abs() >= 0.5) {
                  setState(() => _fontSize = size.roundToDouble());
                }
                return;
              }
              final delta = d.focalPointDelta;
              if (maxDx > 0 && delta.dx.abs() > delta.dy.abs()) {
                setState(() => _dx = (_dx - delta.dx).clamp(0.0, maxDx));
                return;
              }
              if (_mouse) {
                final n = _wheel.take(delta.dy, m.height);
                for (var i = 0; i < n.abs(); i++) {
                  _mouseAt(wheelKind(n), d.localFocalPoint);
                }
                return;
              }
              _dyRest += delta.dy;
              final rows = (_dyRest / m.height).truncate();
              if (rows != 0) {
                _dyRest -= rows * m.height;
                _scrollRows(-rows);
              }
            },
            onScaleEnd: (_) {
              _dyRest = 0;
              _wheel.reset();
            },
            onLongPressStart: (d) {
              HapticFeedback.selectionClick();
              if (reportsDrag(g.modes)) {
                final c = _held = _cell(d.localPosition);
                _mouseCell(MouseKind.press, c);
                return;
              }
              final r = _rowAt(d.localPosition.dy);
              setState(() => _selection = (r, r));
            },
            onLongPressMoveUpdate: (d) {
              final held = _held;
              if (held != null) {
                final c = _cell(d.localPosition);
                if (c != held) _mouseCell(MouseKind.move, _held = c);
                return;
              }
              final s = _selection;
              if (s == null) return;
              setState(() => _selection = (s.$1, _rowAt(d.localPosition.dy)));
            },
            onLongPressEnd: (_) {
              final held = _held;
              if (held != null) {
                _held = null;
                _mouseCell(MouseKind.release, held);
                return;
              }
              _copySelection();
            },
            child: Stack(
              children: [
                Positioned.fill(
                  child: CustomPaint(
                    painter: TermPainter(
                      grid: g,
                      pics: _pics,
                      palette: palette,
                      metrics: m,
                      topAbs: _top,
                      dx: _dx,
                      showCursor: _anchor == null && exitedNot,
                      cursorColor: FemmeColors.accent,
                      selection: _selection,
                      selectionColor: FemmeColors.accent.withValues(
                        alpha: 0.28,
                      ),
                    ),
                  ),
                ),
                if (_anchor != null)
                  Positioned(
                    right: 12,
                    bottom: 12,
                    child: _LivePill(
                      loading: _view.loadingRows,
                      onTap: backToLive,
                    ),
                  ),
              ],
            ),
          ),
        );
      },
    );
  }

  bool get exitedNot => _view.exited == null;
}

/// The `↓` pill back to the live screen while scrolled up.
class _LivePill extends StatelessWidget {
  const _LivePill({required this.loading, required this.onTap});
  final bool loading;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final ink = FemmeColors.inkC.resolveFrom(context);
    return GestureDetector(
      onTap: () {
        HapticFeedback.lightImpact();
        onTap();
      },
      child: Container(
        height: 36,
        padding: const EdgeInsets.symmetric(horizontal: 14),
        decoration: BoxDecoration(
          color: FemmeColors.paperC.resolveFrom(context),
          border: Border.all(color: ink, width: 1.2),
          boxShadow: [BoxShadow(color: ink, offset: const Offset(2, 2))],
        ),
        alignment: Alignment.center,
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (loading) ...[
              const CupertinoActivityIndicator(radius: 7),
              const SizedBox(width: 8),
            ],
            Text('↓ live', style: TextStyle(color: ink, fontSize: 15)),
          ],
        ),
      ),
    );
  }
}
