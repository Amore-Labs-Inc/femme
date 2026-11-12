/// Terminal state on the wire (femme-proto `term.rs`): frames the holder sends, keys the client
/// sends. The emulator runs on the box; the client applies [Frame]s to a row cache.
library;

import 'models.dart';
import 'wire.dart';

/// Flag names of `Mods`, `Attrs` and `Modes`, as bitflags serializes them.
abstract final class Mods {
  static const shift = 'SHIFT', alt = 'ALT', ctrl = 'CTRL', super_ = 'SUPER';
}

abstract final class Attrs {
  static const bold = 'BOLD',
      dim = 'DIM',
      italic = 'ITALIC',
      underline = 'UNDERLINE';
  static const doubleUnderline = 'DOUBLE_UNDERLINE',
      curlyUnderline = 'CURLY_UNDERLINE';
  static const dottedUnderline = 'DOTTED_UNDERLINE',
      dashedUnderline = 'DASHED_UNDERLINE';
  static const strikethrough = 'STRIKETHROUGH',
      inverse = 'INVERSE',
      hidden = 'HIDDEN';
  static const blink = 'BLINK';
}

abstract final class Modes {
  static const appCursor = 'APP_CURSOR', appKeypad = 'APP_KEYPAD';
  static const bracketedPaste = 'BRACKETED_PASTE', mouseClick = 'MOUSE_CLICK';
  static const mouseDrag = 'MOUSE_DRAG',
      mouseMotion = 'MOUSE_MOTION',
      sgrMouse = 'SGR_MOUSE';
  static const focusInOut = 'FOCUS_IN_OUT',
      altScreen = 'ALT_SCREEN',
      lineWrap = 'LINE_WRAP';
  static const alternateScroll = 'ALTERNATE_SCROLL',
      kittyKeyboard = 'KITTY_KEYBOARD';
  static const insert = 'INSERT', origin = 'ORIGIN';
}

/// `term::KeyCode`.
abstract final class KeyCode {
  static Object char(String c) => {'Char': c};
  static Object f(int n) => {'F': n};
  static const Object enter = 'Enter', tab = 'Tab', backspace = 'Backspace';
  static const Object escape = 'Escape',
      up = 'Up',
      down = 'Down',
      left = 'Left';
  static const Object right = 'Right',
      home = 'Home',
      end = 'End',
      pageUp = 'PageUp';
  static const Object pageDown = 'PageDown',
      insert = 'Insert',
      delete = 'Delete';
}

enum MouseKind { press, release, move, scrollUp, scrollDown }

/// `term::TermUp` builders.
abstract final class TermUp {
  static Object open(
    PaneId pane, {
    required int clientInstance,
    int epoch = 0,
    int haveRev = 0,
  }) => {
    'Open': {
      'pane': pane.toJson(),
      'epoch': epoch,
      'have_rev': haveRev,
      'client_instance': clientInstance,
    },
  };
  static Object key(int seq, Object code, {Set<String> mods = const {}}) => {
    'Key': {
      'seq': seq,
      'key': {'code': code, 'mods': flagsToJson(mods)},
    },
  };
  static Object paste(int seq, String text) => {
    'Paste': {'seq': seq, 'text': text},
  };
  static Object bytes(int seq, List<int> data) => {
    'Bytes': {'seq': seq, 'data': data},
  };
  static Object mouse(
    int seq,
    MouseKind kind, {
    int button = 0,
    required int col,
    required int row,
    Set<String> mods = const {},
  }) => {
    'Mouse': {
      'seq': seq,
      'ev': {
        'kind': wireName(kind),
        'button': button,
        'col': col,
        'row': row,
        'mods': flagsToJson(mods),
      },
    },
  };
  static Object resize(int cols, int rows) => {
    'Resize': {'cols': cols, 'rows': rows},
  };
  static Object focus(bool on) => {'Focus': on};
  static Object ack(int rev) => {
    'Ack': {'rev': rev},
  };
  static Object getRows(int fromAbs, int toAbs) => {
    'GetRows': {'from_abs': fromAbs, 'to_abs': toAbs},
  };
}

/// `term::Color`: default, a palette index, or RGB.
class TermColor {
  const TermColor._(this.index, this.rgb);
  static const defaultColor = TermColor._(null, null);
  final int? index;
  final (int, int, int)? rgb;
  bool get isDefault => index == null && rgb == null;

  factory TermColor.fromJson(Object? json) {
    final (tag, v) = tagOf(json);
    return switch (tag) {
      'Indexed' => TermColor._(v as int, null),
      'Rgb' => () {
        final c = (v as List).cast<int>();
        return TermColor._(null, (c[0], c[1], c[2]));
      }(),
      _ => defaultColor,
    };
  }
}

class Style {
  const Style({
    this.fg = TermColor.defaultColor,
    this.bg = TermColor.defaultColor,
    this.underline = TermColor.defaultColor,
    this.attrs = const {},
  });
  final TermColor fg;
  final TermColor bg;
  final TermColor underline;
  final Set<String> attrs;

  factory Style.fromJson(Json j) => Style(
    fg: TermColor.fromJson(j['fg']),
    bg: TermColor.fromJson(j['bg']),
    underline: TermColor.fromJson(j['underline']),
    attrs: flagsFromJson(j['attrs']),
  );
}

enum CursorShape { block, underline, bar }

enum Screen { primary, alt }

class Cursor {
  const Cursor({
    this.row = 0,
    this.col = 0,
    this.shape = CursorShape.block,
    this.visible = true,
  });
  final int row;
  final int col;
  final CursorShape shape;
  final bool visible;

  factory Cursor.fromJson(Json j) => Cursor(
    row: j['row'] as int,
    col: j['col'] as int,
    shape: wireEnum(CursorShape.values, j['shape'], CursorShape.block),
    visible: j['visible'] as bool? ?? true,
  );
}

/// One row of cells by absolute row number; `runs` are `(style id, text)` left to right.
class RowPatch {
  const RowPatch({
    required this.abs,
    this.wrapped = false,
    this.runs = const [],
    this.wideCols = const [],
  });
  final int abs;
  final bool wrapped;
  final List<(int, String)> runs;
  final List<int> wideCols;

  factory RowPatch.fromJson(Json j) => RowPatch(
    abs: j['abs'] as int,
    wrapped: j['wrapped'] as bool? ?? false,
    runs: [
      for (final r in j['runs'] as List)
        ((r as List)[0] as int, r[1] as String),
    ],
    wideCols: (j['wide_cols'] as List? ?? const []).cast<int>(),
  );

  String get text => runs.map((r) => r.$2).join();
}

class Timing {
  const Timing(
    this.seq,
    this.inputRecvNs,
    this.ptyWriteNs,
    this.ptyReadNs,
    this.frameSentNs,
  );
  final int seq;
  final int inputRecvNs;
  final int ptyWriteNs;
  final int ptyReadNs;
  final int frameSentNs;
  factory Timing.fromJson(Json j) => Timing(
    j['seq'] as int,
    j['input_recv_ns'] as int,
    j['pty_write_ns'] as int,
    j['pty_read_ns'] as int,
    j['frame_sent_ns'] as int,
  );
}

/// A screen update: valid on top of [baseRev] (0 = full snapshot), leaves the client at [rev].
class Frame {
  const Frame({
    required this.epoch,
    required this.rev,
    required this.baseRev,
    required this.appliedSeq,
    required this.echoSeq,
    required this.screen,
    required this.cols,
    required this.rows,
    required this.topAbs,
    required this.historyLoAbs,
    required this.cursor,
    required this.modes,
    this.title,
    this.newStyles = const [],
    this.rowPatches = const [],
    this.timing,
  });
  final int epoch;
  final int rev;
  final int baseRev;
  final int appliedSeq;
  final int echoSeq;
  final Screen screen;
  final int cols;
  final int rows;
  final int topAbs;
  final int historyLoAbs;
  final Cursor cursor;
  final Set<String> modes;
  final String? title;
  final List<(int, Style)> newStyles;
  final List<RowPatch> rowPatches;
  final Timing? timing;

  bool get isSnapshot => baseRev == 0;

  factory Frame.fromJson(Json j) => Frame(
    epoch: j['epoch'] as int,
    rev: j['rev'] as int,
    baseRev: j['base_rev'] as int,
    appliedSeq: j['applied_seq'] as int? ?? 0,
    echoSeq: j['echo_seq'] as int? ?? 0,
    screen: wireEnum(Screen.values, j['screen'], Screen.primary),
    cols: j['cols'] as int,
    rows: j['rows'] as int,
    topAbs: j['top_abs'] as int,
    historyLoAbs: j['history_lo_abs'] as int? ?? 0,
    cursor: Cursor.fromJson(j['cursor'] as Json),
    modes: flagsFromJson(j['modes']),
    title: j['title'] as String?,
    newStyles: [
      for (final s in j['new_styles'] as List? ?? const [])
        ((s as List)[0] as int, Style.fromJson(s[1] as Json)),
    ],
    rowPatches: listOf(j['row_patches'] ?? const [], RowPatch.fromJson),
    timing: j['timing'] == null ? null : Timing.fromJson(j['timing'] as Json),
  );
}

/// `term::TermDown`.
sealed class TermDown {
  const TermDown();

  factory TermDown.fromJson(Object json) {
    final (tag, v) = tagOf(json);
    return switch (tag) {
      'Frame' => FrameDown(Frame.fromJson(v as Json)),
      'Rows' => RowsDown(listOf(v, RowPatch.fromJson)),
      'Exited' => Exited((v as Json)['status'] as int),
      'Bell' => const Bell(),
      _ => throw FormatException('unknown TermDown $tag'),
    };
  }
}

class FrameDown extends TermDown {
  const FrameDown(this.frame);
  final Frame frame;
}

class RowsDown extends TermDown {
  const RowsDown(this.rows);
  final List<RowPatch> rows;
}

class Exited extends TermDown {
  const Exited(this.status);
  final int status;
}

class Bell extends TermDown {
  const Bell();
}
