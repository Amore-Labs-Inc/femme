/// The terminal's drawing surface: sizes the holder's screen to the room it has, scrolls into
/// scrollback (asking for rows as it goes), pans when the screen is wider than the phone, pinches
/// to zoom, and selects rows by long press to copy them. A program that reports the mouse gets
/// taps, the wheel and drags instead (`mouse.dart`). A vertical pan flung away keeps scrolling
/// (or turning the wheel) and slows down as iOS lists do, until the end or the next touch.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/physics.dart';
import 'package:flutter/scheduler.dart';
import 'package:flutter/services.dart';

import '../../net/term_models.dart';
import '../../state/term.dart';
import '../theme.dart';
import 'keys.dart';
import 'mouse.dart';
import 'painter.dart';
import 'palette.dart';

class TermSurface extends StatefulWidget {
  const TermSurface({
    super.key,
    required this.view,
    required this.onTap,
    this.onCopied,
    this.mods,
    this.onModsUsed,
  });

  final TermView view;

  /// A tap on the screen (focuses the keyboard).
  final VoidCallback onTap;

  /// Rows were copied to the clipboard.
  final void Function(int lines)? onCopied;

  /// The extra-keys row's sticky modifiers: mouse events carry them, as keys do, and a gesture
  /// that sent any uses up the armed ones (then [onModsUsed]).
  final StickyMods? mods;
  final VoidCallback? onModsUsed;

  @override
  State<TermSurface> createState() => _TermSurfaceState();
}

class _TermSurfaceState extends State<TermSurface>
    with SingleTickerProviderStateMixin {
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

  /// The current one-finger gesture scrolled vertically (a fling may follow) / pinched.
  bool _scrolled = false, _pinched = false;

  /// Where the finger last was (the wheel turns at that cell).
  Offset _focal = Offset.zero;

  /// The fling in flight: its simulation, the distance it has fed so far, and whether the touch
  /// that stopped it should do nothing else (iOS: a tap on a moving list only stops it).
  late final Ticker _flingTicker = createTicker(_flingTick);
  Simulation? _fling;

  /// The modifiers held when the fling started (the gesture that flung has used them up).
  Set<String> _flingMods = const {};

  /// A mouse event went out during the current gesture.
  bool _sentMouse = false;
  double _flungTo = 0;
  bool _swallowTap = false;

  TermView get _view => widget.view;

  @override
  void dispose() {
    _flingTicker.dispose();
    _pics.clear();
    super.dispose();
  }

  CellMetrics get _m =>
      (_metrics?.fontSize == _fontSize ? _metrics : null) ??
      (_metrics = CellMetrics.measure(_fontSize));

  int get _top => _anchor ?? _view.grid.topAbs;

  /// Scrolls [n] rows (negative: up into scrollback); false when there was nowhere to go.
  bool _scrollRows(int n) {
    final g = _view.grid;
    if (g.screen == Screen.alt || !g.hasFrame) {
      // No scrollback on the alternate screen; a pager or editor gets arrow keys when it asks.
      if (!g.modes.contains(Modes.alternateScroll)) return false;
      for (var i = 0; i < n.abs(); i++) {
        _view.key(n < 0 ? KeyCode.up : KeyCode.down);
      }
      return true;
    }
    final from = _top;
    var top = from + n;
    if (top < g.historyLoAbs) top = g.historyLoAbs;
    setState(() => _anchor = top >= g.topAbs ? null : top);
    if (_anchor != null) _view.needRows(_anchor!);
    return _top != from;
  }

  /// [dy] of vertical finger travel (positive: down, toward older rows) at [at]: wheel notches
  /// in mouse mode, scrollback rows otherwise. False when the scrollback is at its end.
  bool _scrollBy(double dy, Offset at) {
    final h = _m.height;
    if (_mouse) {
      final n = _wheel.take(dy, h);
      for (var i = 0; i < n.abs(); i++) {
        _mouseAt(wheelKind(n), at);
      }
      return true;
    }
    _dyRest += dy;
    final rows = (_dyRest / h).truncate();
    if (rows == 0) return true;
    _dyRest -= rows * h;
    return _scrollRows(-rows);
  }

  /// Keeps scrolling after a lift at [vy] px/s, slowing down as a `UIScrollView` does
  /// (`BouncingScrollSimulation`'s friction), fed through [_scrollBy] frame by frame.
  void _startFling(double vy) {
    _fling = FrictionSimulation(
      0.135,
      0,
      vy,
      tolerance: const Tolerance(velocity: 20),
    );
    _flungTo = 0;
    _flingTicker
      ..stop()
      ..start();
  }

  void _flingTick(Duration elapsed) {
    final sim = _fling;
    if (sim == null) return;
    final t = elapsed.inMicroseconds / Duration.microsecondsPerSecond;
    final x = sim.x(t);
    final dy = x - _flungTo;
    _flungTo = x;
    if (!_scrollBy(dy, _focal) || sim.isDone(t)) _stopFling();
  }

  void _stopFling() {
    _flingTicker.stop();
    _fling = null;
    _dyRest = 0;
    _wheel.reset();
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
    _sentMouse = true;
    final mods = _fling != null ? _flingMods : widget.mods?.mods ?? const {};
    _view.mouse(kind, col: cell.$1, row: cell.$2, mods: mods);
  }

  /// The gesture is over: if it sent mouse events, its one-shot modifiers are used up, as a key
  /// press uses them.
  void _useMods() {
    if (!_sentMouse) return;
    _sentMouse = false;
    final m = widget.mods;
    if (m == null || m.mods.isEmpty) return;
    m.take();
    widget.onModsUsed?.call();
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
          onPointerDown: (_) {
            _pointers++;
            _swallowTap = _fling != null;
            if (_swallowTap) _stopFling();
          },
          onPointerUp: (_) => _pointers--,
          onPointerCancel: (_) => _pointers--,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onTapUp: (d) {
              if (_swallowTap) return;
              if (_selection == null && _mouse) {
                _mouseAt(MouseKind.press, d.localPosition);
                _mouseAt(MouseKind.release, d.localPosition);
                _useMods();
              }
            },
            onTap: () {
              if (_swallowTap) return;
              if (_selection != null) {
                setState(() => _selection = null);
                return;
              }
              widget.onTap();
            },
            onScaleStart: (d) {
              _stopFling();
              _pinchFrom = _fontSize;
              _scrolled = _pinched = false;
              _focal = d.localFocalPoint;
            },
            onScaleUpdate: (d) {
              if (_pointers >= 2 || d.pointerCount >= 2) {
                _pinched = true;
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
              _scrolled = true;
              _focal = d.localFocalPoint;
              _scrollBy(delta.dy, _focal);
            },
            onScaleEnd: (d) {
              final vy = d.velocity.pixelsPerSecond.dy;
              if (_scrolled && !_pinched && vy.abs() >= kMinFlingVelocity) {
                _flingMods = widget.mods?.mods ?? const {};
                _startFling(vy);
              } else {
                _stopFling();
              }
              _useMods();
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
                _useMods();
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
        key: const ValueKey('term-live-pill'),
        height: 36,
        padding: const EdgeInsets.symmetric(horizontal: 14),
        decoration: BoxDecoration(
          color: CupertinoColors.secondarySystemBackground.resolveFrom(context),
          borderRadius: const BorderRadius.all(Radius.circular(999)),
          boxShadow: const [
            BoxShadow(
              color: Color(0x26000000),
              blurRadius: 12,
              offset: Offset(0, 3),
            ),
          ],
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
