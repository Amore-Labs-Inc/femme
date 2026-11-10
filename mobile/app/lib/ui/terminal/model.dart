/// The client half of a terminal (femme-proto `term.rs`): a row cache keyed by absolute row number,
/// the style table, cursor, modes and title, fed by [Frame]s under the apply rule. Plain data; the
/// stream and its protocol live in `term.dart`.
library;

import '../../net/term_models.dart';

/// What [TermGrid.apply] did with a frame.
enum ApplyResult {
  /// Applied: the grid now holds the frame's `rev`.
  applied,

  /// The frame's `base_rev` is not what the grid holds: re-`Open` with [TermGrid.rev].
  mismatch,
}

class TermGrid {
  /// Holder instance (0 before the first frame).
  int epoch = 0;

  /// The rev held (0 = nothing).
  int rev = 0;

  int cols = 0;
  int rows = 0;

  /// Absolute row number of the first visible row.
  int topAbs = 0;

  /// Oldest scrollback row the holder still has.
  int historyLoAbs = 0;

  Cursor cursor = const Cursor();
  Set<String> modes = const {};
  Screen screen = Screen.primary;

  /// The title the program gave its terminal, kept until another one comes.
  String? title;

  /// Highest input seq the holder wrote to the pty.
  int appliedSeq = 0;

  /// Style id → style. Id 0 is the default and never stored.
  final styles = <int, Style>{};

  /// Absolute row number → row. Rows above [topAbs] are scrollback (immutable); the others are
  /// the visible screen.
  final cache = <int, RowPatch>{};

  /// Rows the holder said it does not have any more (a `GetRows` answered with none): no row at
  /// or below this is asked for again.
  int noRowsAtOrBelow = -1;

  bool get hasFrame => rev != 0;
  int get bottomAbs => topAbs + rows;

  Style style(int id) => styles[id] ?? const Style();
  RowPatch? row(int abs) => cache[abs];

  /// Forgets everything (a new epoch, or a fresh open).
  void reset() {
    epoch = 0;
    rev = 0;
    styles.clear();
    cache.clear();
    noRowsAtOrBelow = -1;
    title = null;
  }

  /// Applies one frame. `base_rev == 0` is a full snapshot: the style table and the visible rows
  /// are dropped first (all of the cache when the epoch changed). Otherwise the grid must hold
  /// `base_rev` of the same epoch.
  ApplyResult apply(Frame f) {
    if (f.isSnapshot) {
      if (f.epoch != epoch) {
        cache.clear();
        noRowsAtOrBelow = -1;
      } else {
        final from = hasFrame && topAbs < f.topAbs ? topAbs : f.topAbs;
        cache.removeWhere((abs, _) => abs >= from);
      }
      styles.clear();
    } else if (f.epoch != epoch || f.baseRev != rev || !hasFrame) {
      return ApplyResult.mismatch;
    }
    epoch = f.epoch;
    rev = f.rev;
    appliedSeq = f.appliedSeq;
    screen = f.screen;
    cols = f.cols;
    rows = f.rows;
    topAbs = f.topAbs;
    historyLoAbs = f.historyLoAbs;
    cursor = f.cursor;
    modes = f.modes;
    if (f.title != null) title = f.title;
    for (final (id, s) in f.newStyles) {
      if (id != 0) styles[id] = s;
    }
    for (final p in f.rowPatches) {
      cache[p.abs] = p;
    }
    // Rows below the screen (it shrank) and scrollback the holder dropped are gone for good.
    cache.removeWhere((abs, _) => abs >= bottomAbs || abs < historyLoAbs);
    return ApplyResult.applied;
  }

  /// Adds scrollback rows (a `GetRows` answer to [asked], `[from, to)`). Only rows above the
  /// screen are taken: visible rows come from frames. When the answer lacks the top row asked
  /// for, the holder has nothing at or below it and it is not asked for again.
  void addRows(List<RowPatch> got, {(int, int)? asked}) {
    for (final r in got) {
      if (r.abs < topAbs && r.abs >= historyLoAbs) cache[r.abs] = r;
    }
    if (asked != null) {
      final top = asked.$2 - 1;
      if (!cache.containsKey(top) && top > noRowsAtOrBelow) {
        noRowsAtOrBelow = top;
      }
    }
  }

  /// The scrollback chunk to ask for so that rows from [wantTop] up to the screen are held:
  /// `[from, to)`, at most [chunk] rows, never below [historyLoAbs]; null when nothing is missing.
  (int, int)? missing(int wantTop, {int chunk = 200}) {
    final floor = [
      wantTop,
      historyLoAbs,
      noRowsAtOrBelow + 1,
    ].reduce((a, b) => a > b ? a : b);
    for (var abs = topAbs - 1; abs >= floor; abs--) {
      if (cache.containsKey(abs)) continue;
      final to = abs + 1;
      var from = to - chunk;
      if (from < historyLoAbs) from = historyLoAbs;
      if (from <= noRowsAtOrBelow) from = noRowsAtOrBelow + 1;
      return from < to ? (from, to) : null;
    }
    return null;
  }

  /// Plain text of rows `[fromAbs, toAbs)`, soft-wrapped rows joined (for copy).
  String text(int fromAbs, int toAbs) {
    final b = StringBuffer();
    for (var abs = fromAbs; abs < toAbs; abs++) {
      final r = cache[abs];
      final line = (r?.text ?? '').replaceAll(RegExp(r' +$'), '');
      b.write(line);
      if (abs < toAbs - 1 && !(r?.wrapped ?? false)) b.write('\n');
    }
    return b.toString();
  }
}
