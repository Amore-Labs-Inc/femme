/// Touches as mouse events for a program that asked for the mouse (`MOUSE_CLICK`, `MOUSE_DRAG`,
/// `MOUSE_MOTION`), as the desktop sends them (`desktop/src/terminal/pane.rs` `wheel`,
/// `input.rs` `cell`): cells of the visible screen, 0-based, clamped to it. The holder encodes
/// them per the active mode (SGR or not), so the phone only says what happened where.
///
/// On the phone: a tap is a left press + release; a one-finger vertical pan turns the wheel, one
/// notch per cell height (finger down = `ScrollUp`, as the scrollback pan); and while the program
/// wants drags (`MOUSE_DRAG` / `MOUSE_MOTION`) a long press holds the left button down and moving
/// drags it. Without a mouse mode none of this happens and the surface scrolls its scrollback.
library;

import 'dart:ui';

import '../../net/term_models.dart';

/// The program reports the mouse.
bool reportsMouse(Set<String> modes) =>
    modes.contains(Modes.mouseClick) ||
    modes.contains(Modes.mouseDrag) ||
    modes.contains(Modes.mouseMotion);

/// The program also wants the button held down while moving.
bool reportsDrag(Set<String> modes) =>
    modes.contains(Modes.mouseDrag) || modes.contains(Modes.mouseMotion);

/// The cell under [p] (surface pixels; [dx] = how far the screen is panned right), clamped to a
/// [cols] x [rows] screen of [cellW] x [cellH] cells.
(int col, int row) touchCell(
  Offset p, {
  required double cellW,
  required double cellH,
  required int cols,
  required int rows,
  double dx = 0,
}) {
  int idx(double d, double size, int n) =>
      ((d / size).floor()).clamp(0, n < 1 ? 0 : n - 1);
  return (idx(p.dx + dx, cellW, cols), idx(p.dy, cellH, rows));
}

/// Adds vertical finger travel and takes out whole wheel notches of [cellH] each (positive:
/// `ScrollUp`, the finger went down), keeping the rest for the next move.
class WheelSteps {
  double _rest = 0;

  int take(double dy, double cellH) {
    _rest += dy;
    final n = (_rest / cellH).truncate();
    _rest -= n * cellH;
    return n;
  }

  void reset() => _rest = 0;
}

/// One notch of [n] (from [WheelSteps.take]).
MouseKind wheelKind(int n) => n > 0 ? MouseKind.scrollUp : MouseKind.scrollDown;
