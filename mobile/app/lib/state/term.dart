/// One attached terminal pane: the `Term` stream, the apply rule, acks, input seqs, scrollback
/// paging, resize and reconnection. The screen listens to it; the grid is in
/// `ui/terminal/model.dart`.
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../net/link.dart';
import '../net/models.dart';
import '../net/term_models.dart';
import '../ui/terminal/model.dart';

/// What [TermView] needs of a `Term` stream; [TermStream] in the app, a fake in tests.
abstract interface class TermPort {
  Stream<TermDown> get down;

  /// Completes when the stream ends: null for a clean end, else why.
  Future<String?> get closed;
  void send(Object up);
  void close();
}

/// Opens a `Term` stream for [pane] and sends its `Open` with [epoch] and [haveRev]. Throws
/// [LinkLost] while offline.
typedef TermOpener = TermPort Function(PaneId pane, int epoch, int haveRev);

class _LinkPort implements TermPort {
  _LinkPort(this._s);
  final TermStream _s;
  @override
  Stream<TermDown> get down => _s.down;
  @override
  Future<String?> get closed => _s.closed;
  @override
  void send(Object up) => _s.send(up);
  @override
  void close() => _s.close();
}

/// Scrollback rows asked for at once.
const rowsChunk = 200;

class TermView extends ChangeNotifier {
  TermView({
    required this.pane,
    required this.opener,
    required this.clientInstance,
    required this.isOnline,
    this.online,
    this.resizeDebounce = const Duration(milliseconds: 200),
    this.retryDelay = const Duration(seconds: 1),
  });

  /// A view on [link]: reopens when the link comes back online.
  factory TermView.forLink(Link link, PaneId pane) => TermView(
    pane: pane,
    opener: (p, epoch, haveRev) =>
        _LinkPort(link.openTerm(p, epoch: epoch, haveRev: haveRev)),
    clientInstance: link.clientInstance,
    isOnline: () => link.isOnline,
    online: link.state,
  );

  final PaneId pane;
  final TermOpener opener;
  final int clientInstance;
  final bool Function() isOnline;

  /// Notifies when the link's state changes (null in tests that reopen by hand).
  final Listenable? online;
  final Duration resizeDebounce;
  final Duration retryDelay;

  final grid = TermGrid();

  /// The exit status once the program ended; the view stays as it was.
  int? exited;

  /// Bells rung since the view opened (the screen buzzes when it grows).
  int bells = 0;

  /// The stream is open and its first frame applied.
  bool get live => _port != null && _ready;

  TermPort? _port;
  StreamSubscription<TermDown>? _sub;
  bool _ready = false;
  bool _resyncing = false;
  bool _disposed = false;
  bool _waiting = false;
  Timer? _retry;

  int _seq = 0;
  final _unacked = <(int, Object Function(int))>[];
  final _queued = <Object Function(int)>[];

  (int, int)? _wantSize;
  (int, int)? _sentSize;
  Timer? _resizeTimer;

  (int, int)? _rowsAsked;
  int? _wantTop;

  /// Opens the stream (once; reconnection is automatic after).
  void start() {
    online?.addListener(_onLink);
    _open();
  }

  void _open() {
    if (_disposed || exited != null || _port != null) return;
    if (!isOnline()) {
      _waiting = true;
      return;
    }
    final TermPort port;
    try {
      port = opener(pane, grid.epoch, grid.rev);
    } on LinkLost {
      _waiting = true;
      return;
    }
    _waiting = false;
    _port = port;
    _ready = false;
    _resyncing = false;
    _sentSize = null;
    _rowsAsked = null;
    _sub = port.down.listen(_onDown);
    port.closed.then((why) {
      if (identical(_port, port)) _onClosed(why);
    });
  }

  void _onLink() {
    if (_waiting && isOnline()) _open();
  }

  void _onClosed(String? why) {
    _sub?.cancel();
    _sub = null;
    _port = null;
    _ready = false;
    if (_disposed) return;
    notifyListeners();
    if (exited != null) return;
    if (isOnline()) {
      // The stream ended but the link is up (reset, or the holder restarted): try again shortly.
      _retry?.cancel();
      _retry = Timer(retryDelay, _open);
    } else {
      _waiting = true;
    }
  }

  void _onDown(TermDown d) {
    switch (d) {
      case FrameDown(:final frame):
        _onFrame(frame);
      case RowsDown(:final rows):
        grid.addRows(rows, asked: _rowsAsked);
        _rowsAsked = null;
        final want = _wantTop;
        if (want != null) needRows(want);
        notifyListeners();
      case Exited(:final status):
        exited = status;
        notifyListeners();
      case Bell():
        bells++;
        notifyListeners();
    }
  }

  void _onFrame(Frame f) {
    final port = _port;
    if (port == null) return;
    if (grid.apply(f) == ApplyResult.mismatch) {
      if (_resyncing) return;
      _resyncing = true;
      if (f.epoch != grid.epoch) grid.reset();
      port.send(
        TermUp.open(
          pane,
          clientInstance: clientInstance,
          epoch: grid.epoch,
          haveRev: grid.rev,
        ),
      );
      return;
    }
    _resyncing = false;
    port.send(TermUp.ack(grid.rev));
    _unacked.removeWhere((u) => u.$1 <= grid.appliedSeq);
    if (!_ready) {
      _ready = true;
      if (grid.appliedSeq > _seq) _seq = grid.appliedSeq;
      // Input the holder never wrote goes again under its own seq (it dedups the rest).
      for (final (seq, build) in _unacked) {
        port.send(build(seq));
      }
      final q = List.of(_queued);
      _queued.clear();
      q.forEach(_sendInput);
      _flushResize();
    }
    notifyListeners();
  }

  // ---- input ----

  void _input(Object Function(int seq) build) {
    if (exited != null) return;
    if (!live) {
      _queued.add(build);
      return;
    }
    _sendInput(build);
  }

  void _sendInput(Object Function(int seq) build) {
    final seq = ++_seq;
    _unacked.add((seq, build));
    if (_unacked.length > 512) _unacked.removeAt(0);
    _port?.send(build(seq));
  }

  /// Sends one key ([KeyCode] value) with modifier names ([Mods]).
  void key(Object code, {Set<String> mods = const {}}) =>
      _input((seq) => TermUp.key(seq, code, mods: mods));

  /// Sends a mouse event at a cell of the visible screen (0-based); [button] 0 left, 1 middle,
  /// 2 right, ignored for the wheel.
  void mouse(
    MouseKind kind, {
    required int col,
    required int row,
    int button = 0,
    Set<String> mods = const {},
  }) => _input(
    (seq) =>
        TermUp.mouse(seq, kind, button: button, col: col, row: row, mods: mods),
  );

  void paste(String text) {
    if (text.isEmpty) return;
    _input((seq) => TermUp.paste(seq, text));
  }

  /// The highest seq handed out (for tests and diagnostics).
  int get seq => _seq;

  // ---- size ----

  /// The size the screen has room for; sent (debounced) when it differs from the last sent.
  void setSize(int cols, int rows) {
    if (cols < 2 || rows < 1) return;
    if (_wantSize == (cols, rows)) return;
    _wantSize = (cols, rows);
    _resizeTimer?.cancel();
    _resizeTimer = Timer(resizeDebounce, _flushResize);
  }

  void _flushResize() {
    final want = _wantSize;
    if (want == null || !live || want == _sentSize) return;
    if (_sentSize == null && grid.cols == want.$1 && grid.rows == want.$2) {
      _sentSize = want;
      return;
    }
    _sentSize = want;
    _port!.send(TermUp.resize(want.$1, want.$2));
  }

  // ---- scrollback ----

  /// The screen shows rows from [topAbs]: asks for the missing scrollback above the screen, one
  /// chunk at a time.
  void needRows(int topAbs) {
    _wantTop = topAbs;
    if (!live || _rowsAsked != null || topAbs >= grid.topAbs) return;
    final m = grid.missing(topAbs, chunk: rowsChunk);
    if (m == null) return;
    _rowsAsked = m;
    _port!.send(TermUp.getRows(m.$1, m.$2));
  }

  /// A scrollback request is out.
  bool get loadingRows => _rowsAsked != null;

  @override
  void dispose() {
    _disposed = true;
    online?.removeListener(_onLink);
    _retry?.cancel();
    _resizeTimer?.cancel();
    _sub?.cancel();
    final port = _port;
    _port = null;
    port?.close();
    super.dispose();
  }
}
