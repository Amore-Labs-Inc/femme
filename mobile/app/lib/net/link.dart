/// One connection to `femmed` over the `/femme/ws` WebSocket carrier, reconnecting forever.
///
/// The socket multiplexes streams as JSON text frames: `{"s":N,"open":KIND}`, `{"s":N,"m":MSG}`,
/// `{"s":N,"fin":true}`, `{"s":N,"rst":true}`. Client-opened stream ids are odd, server-opened
/// even. Ctrl (s=1) carries Hello/Welcome and requests; the server then opens Events.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';

import 'package:flutter/foundation.dart';

import 'ids.dart';
import 'models.dart';
import 'rpc.dart';
import 'term_models.dart';

enum LinkPhase { connecting, online, offline }

@immutable
class LinkState {
  const LinkState(this.phase, {this.reason, this.serverVersion, this.retryAt});
  final LinkPhase phase;

  /// Why the link is offline (the last error), or null.
  final String? reason;

  /// The server's version from its last `Welcome`.
  final String? serverVersion;

  /// When the next attempt starts, while offline and waiting.
  final DateTime? retryAt;

  bool get online => phase == LinkPhase.online;
  @override
  String toString() => '${phase.name}${reason == null ? '' : ' ($reason)'}';
}

/// Thrown into a request's future when the connection it was sent on went away and the request
/// is not one to resend (see [Link.req]'s `resend`), or when the link is closed.
class LinkLost implements Exception {
  const LinkLost(this.reason);
  final String reason;
  @override
  String toString() => 'link lost: $reason';
}

/// What the store needs from a link; tests fake it.
abstract interface class Requester {
  Future<Resp> req(Object req, {String? op, Duration? timeout, bool resend});
  bool get isOnline;
}

class _Pending {
  _Pending(this.op, this.req, this.resend);
  final String op;
  final Object req;
  final bool resend;
  final completer = Completer<Resp>();
  Timer? timer;

  /// Sent on the live connection (cleared when it drops).
  bool sent = false;
}

/// Builds the WebSocket URL from what the user typed: a full `ws(s)://` or `http(s)://` URL
/// (`/femme/ws` added when it has no path), or a bare host (`wss://<host>/femme/ws`).
Uri linkUrl(String host) {
  var h = host.trim();
  if (h.startsWith('http://')) h = 'ws://${h.substring(7)}';
  if (h.startsWith('https://')) h = 'wss://${h.substring(8)}';
  if (!h.startsWith('ws://') && !h.startsWith('wss://')) h = 'wss://$h';
  final u = Uri.parse(h);
  return (u.path.isEmpty || u.path == '/') ? u.replace(path: '/femme/ws') : u;
}

class Link implements Requester {
  Link({
    required this.clientInstance,
    this.reqTimeout = const Duration(seconds: 30),
    this.helloTimeout = const Duration(seconds: 10),
    this.minBackoff = const Duration(seconds: 1),
    this.maxBackoff = const Duration(seconds: 30),
    this.pingInterval = const Duration(seconds: 20),
  });

  final int clientInstance;
  final Duration reqTimeout;
  final Duration helloTimeout;
  final Duration minBackoff;
  final Duration maxBackoff;
  final Duration pingInterval;

  /// The cursors to put in `Hello`; the store sets it.
  Cursors Function() cursors = () => const Cursors(epoch: 0);

  /// Called with the `Welcome`'s epoch before any event of the new connection.
  void Function(int epoch)? onWelcome;

  /// Called once the Events stream is open and unanswered requests were resent.
  void Function()? onOnline;

  final state = ValueNotifier(
    const LinkState(LinkPhase.offline, reason: 'not connected'),
  );
  final _events = StreamController<S2C>.broadcast();
  final _pending = <String, _Pending>{};
  final _terms = <int, TermStream>{};

  Uri? _url;
  String? _token;
  WebSocket? _ws;
  int _gen = 0;
  bool _paused = false;
  Duration? _backoff;
  Completer<void>? _wake;
  int _nextStream = 1;
  int? _eventsStream;
  Completer<void>? _helloDone;
  Completer<void>? _eventsOpen;
  Completer<void>? _socketDone;
  String? _closeReason;

  /// Server pushes from the Events stream (`Changes`, `Delta`, `SnapshotOnBulk`), across reconnects.
  Stream<S2C> get events => _events.stream;

  @override
  bool get isOnline => state.value.online;

  /// Starts (or restarts, with new credentials) the connect loop. Returns at once; watch [state].
  void connect(String host, String token) {
    _url = linkUrl(host);
    _token = token;
    _paused = false;
    _restart();
  }

  /// Drops the socket and stops reconnecting until [resume] (app in the background).
  void pause() {
    if (_paused) return;
    _paused = true;
    _gen++;
    _wakeUp();
    _drop('paused');
    _setState(LinkPhase.offline, reason: 'paused');
  }

  /// Reconnects at once after [pause], or skips the wait of a pending retry.
  void resume() {
    if (_url == null) return;
    if (_paused || !isOnline) {
      _paused = false;
      _restart();
    }
  }

  /// Stops for good: fails every pending request and closes [events].
  Future<void> close() async {
    _paused = true;
    _gen++;
    _wakeUp();
    _drop('closed');
    for (final p in _pending.values.toList()) {
      _fail(p, const LinkLost('closed'));
    }
    await _events.close();
    state.dispose();
  }

  /// Sends a request on Ctrl and completes with its response, or throws [FemmeError] (the server
  /// refused it), [TimeoutException] or [LinkLost].
  ///
  /// While offline the request waits. When the connection drops before the answer, a request with
  /// [resend] is sent again on the next connection with the same [op] (the server dedups it); one
  /// without fails with [LinkLost]. Connection-scoped requests (`Subscribe`) must not be resent:
  /// the dedup would answer them without subscribing the new connection.
  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
  }) {
    op ??= Ulid.next();
    final existing = _pending[op];
    if (existing != null) return existing.completer.future;
    final p = _Pending(op, req, resend);
    _pending[op] = p;
    p.timer = Timer(timeout ?? reqTimeout, () {
      _fail(p, TimeoutException('no answer to $op', timeout ?? reqTimeout));
    });
    if (isOnline) _sendReq(p);
    return p.completer.future;
  }

  /// Attaches a terminal pane: opens a Term stream and sends `TermUp::Open`. The stream ends when
  /// the pane exits, [TermStream.close] is called or the connection drops (reopen with the rev
  /// held then). Throws [LinkLost] while offline.
  TermStream openTerm(PaneId pane, {int epoch = 0, int haveRev = 0}) {
    if (!isOnline) throw const LinkLost('offline');
    final id = _allocStream();
    final t = TermStream._(this, id, pane);
    _terms[id] = t;
    _send({
      's': id,
      'open': {'Term': pane.toJson()},
    });
    t.send(
      TermUp.open(
        pane,
        clientInstance: clientInstance,
        epoch: epoch,
        haveRev: haveRev,
      ),
    );
    return t;
  }

  // ---- connection loop ----

  void _restart() {
    final gen = ++_gen;
    _backoff = null;
    _wakeUp();
    _drop('reconnecting');
    unawaited(_loop(gen));
  }

  Future<void> _loop(int gen) async {
    while (gen == _gen && !_paused) {
      _setState(LinkPhase.connecting);
      String reason;
      try {
        reason = await _session(gen);
      } on FemmeError catch (e) {
        reason = e.toString();
        _backoff = maxBackoff; // refused (unsupported protocol, unauthorized): no fast loop
      } catch (e) {
        reason = _describe(e);
      }
      if (gen != _gen) return;
      _drop(reason);
      if (_paused) return;
      final wait = _nextBackoff();
      _setState(
        LinkPhase.offline,
        reason: reason,
        retryAt: DateTime.now().add(wait),
      );
      final wake = _wake = Completer<void>();
      final timer = Timer(wait, () {
        if (!wake.isCompleted) wake.complete();
      });
      await wake.future;
      timer.cancel();
    }
  }

  void _wakeUp() {
    final w = _wake;
    if (w != null && !w.isCompleted) w.complete();
  }

  Duration _nextBackoff() {
    final b = _backoff == null ? minBackoff : _backoff! * 2;
    _backoff = b > maxBackoff ? maxBackoff : b;
    // Up to 20 % jitter so phones do not reconnect in lockstep after a box restart.
    final jitter = _backoff!.inMilliseconds * Random().nextDouble() * 0.2;
    return _backoff! + Duration(milliseconds: jitter.round());
  }

  /// One connection, from dial to close. Returns why it ended.
  Future<String> _session(int gen) async {
    final ws = await WebSocket.connect(
      _url.toString(),
      headers: {'Authorization': 'Bearer $_token'},
    ).timeout(helloTimeout);
    if (gen != _gen) {
      unawaited(ws.close());
      return 'superseded';
    }
    ws.pingInterval = pingInterval;
    _ws = ws;
    _nextStream = 1;
    _eventsStream = null;
    _closeReason = null;
    final helloDone = _helloDone = Completer<void>();
    final eventsOpen = _eventsOpen = Completer<void>();
    final done = _socketDone = Completer<void>();
    // Settled with an error when the socket ends early; awaited below only on the happy path.
    helloDone.future.ignore();
    eventsOpen.future.ignore();
    ws.listen(
      // A dropped socket can still flush frames; only the live one speaks.
      (data) {
        if (identical(_ws, ws)) _onFrame(data);
      },
      onError: (Object e) {
        if (identical(_ws, ws)) _closeReason ??= _describe(e);
      },
      onDone: () {
        if (identical(_ws, ws)) {
          _closeReason ??= ws.closeReason?.isNotEmpty == true
              ? 'closed: ${ws.closeReason}'
              : 'connection closed';
        }
        final why = LinkLost(_closeReason ?? 'connection closed');
        if (!done.isCompleted) done.complete();
        for (final c in [helloDone, eventsOpen]) {
          if (!c.isCompleted) c.completeError(why);
        }
      },
    );
    final ctrl = _allocStream();
    _send({'s': ctrl, 'open': 'Ctrl'});
    _send({'s': ctrl, 'm': hello(clientInstance, cursors())});
    await helloDone.future.timeout(helloTimeout);
    await eventsOpen.future.timeout(helloTimeout);
    if (gen != _gen) return 'superseded';
    _backoff = null;
    _setState(LinkPhase.online);
    for (final p in _pending.values.toList()) {
      if (!p.sent) _sendReq(p);
    }
    onOnline?.call();
    await done.future;
    return _closeReason ?? 'connection closed';
  }

  void _onFrame(Object? data) {
    if (data is! String) return _protocolError('binary frame');
    final Map<String, dynamic> f;
    try {
      f = jsonDecode(data) as Map<String, dynamic>;
    } catch (_) {
      return _protocolError('bad JSON frame');
    }
    final s = f['s'];
    if (s is! int) return _protocolError('frame without stream id');
    if (f.containsKey('open')) return _onOpen(s, f['open']);
    if (f['fin'] == true || f['rst'] == true) {
      return _onEnd(s, f['rst'] == true);
    }
    if (!f.containsKey('m')) return;
    final m = f['m'] as Object;
    try {
      if (s == 1) return _onCtrl(S2C.fromJson(m));
      if (s == _eventsStream) return _events.add(S2C.fromJson(m));
      _terms[s]?._down(TermDown.fromJson(m));
    } on FormatException catch (e) {
      debugPrint('femme link: skipped a message on stream $s: $e');
    }
  }

  void _onOpen(int s, Object? kind) {
    if (kind == 'Events' && _eventsStream == null && s.isEven) {
      _eventsStream = s;
      if (!(_eventsOpen?.isCompleted ?? true)) _eventsOpen!.complete();
      return;
    }
    // Bulk and anything newer: not handled yet, so the server hears no at once.
    _send({'s': s, 'rst': true});
  }

  void _onEnd(int s, bool reset) {
    if (s == 1 || s == _eventsStream) {
      _closeReason =
          '${s == 1 ? 'ctrl' : 'events'} stream ${reset ? 'reset' : 'ended'}';
      unawaited(_ws?.close());
      return;
    }
    _terms.remove(s)?._end(reset ? 'reset' : null);
  }

  void _onCtrl(S2C msg) {
    switch (msg) {
      case Welcome(:final epoch, :final serverVersion):
        _setState(LinkPhase.connecting, serverVersion: serverVersion);
        onWelcome?.call(epoch);
        if (!(_helloDone?.isCompleted ?? true)) _helloDone!.complete();
      case RespMsg r when r.op == Ulid.nil:
        // A refusal before Welcome (or of no request): it ends this connection.
        final c = _helloDone;
        try {
          r.unwrap();
        } on FemmeError catch (e) {
          if (c != null && !c.isCompleted) c.completeError(e);
        }
      case RespMsg(:final op):
        final p = _pending.remove(op);
        if (p == null) return;
        p.timer?.cancel();
        try {
          p.completer.complete(msg.unwrap());
        } catch (e) {
          p.completer.completeError(e);
        }
      default:
        break;
    }
  }

  void _protocolError(String why) {
    _closeReason = 'protocol error: $why';
    unawaited(_ws?.close());
  }

  /// Closes the socket and settles what hung on it.
  void _drop(String reason) {
    final ws = _ws;
    _ws = null;
    _eventsStream = null;
    if (ws != null) unawaited(ws.close());
    final done = _socketDone;
    if (done != null && !done.isCompleted) done.complete();
    for (final p in _pending.values.toList()) {
      if (!p.sent) continue;
      p.sent = false;
      if (!p.resend) _fail(p, LinkLost(reason));
    }
    for (final t in _terms.values.toList()) {
      t._end(reason);
    }
    _terms.clear();
  }

  void _fail(_Pending p, Object error) {
    if (_pending[p.op] == p) _pending.remove(p.op);
    p.timer?.cancel();
    if (!p.completer.isCompleted) p.completer.completeError(error);
  }

  void _sendReq(_Pending p) {
    p.sent = true;
    _send({'s': 1, 'm': reqMsg(p.op, p.req)});
  }

  int _allocStream() {
    final id = _nextStream;
    _nextStream += 2;
    return id;
  }

  void _send(Object frame) => _ws?.add(jsonEncode(frame));

  void _setState(
    LinkPhase phase, {
    String? reason,
    DateTime? retryAt,
    String? serverVersion,
  }) {
    state.value = LinkState(
      phase,
      reason: reason,
      retryAt: retryAt,
      serverVersion: serverVersion ?? state.value.serverVersion,
    );
  }

  static String _describe(Object e) => switch (e) {
    WebSocketException(:final message) => message,
    SocketException(:final message, :final osError) =>
      osError == null ? message : '$message: ${osError.message}',
    TimeoutException() => 'timed out',
    LinkLost(:final reason) => reason,
    _ => e.toString(),
  };
}

/// One attached terminal pane on a Term stream: `TermUp` up, `TermDown` down.
class TermStream {
  TermStream._(this._link, this.id, this.pane);
  final Link _link;
  final int id;
  final PaneId pane;
  final _downs = StreamController<TermDown>();
  final _closed = Completer<String?>();

  Stream<TermDown> get down => _downs.stream;

  /// Completes when the stream ends: null for a clean end, else why (connection lost, reset).
  Future<String?> get closed => _closed.future;
  bool get isClosed => _closed.isCompleted;

  /// Sends one `TermUp` (see [TermUp]'s builders).
  void send(Object up) {
    if (isClosed) return;
    _link._send({'s': id, 'm': up});
  }

  /// Finishes the client's direction; the server ends its own after.
  void close() {
    if (isClosed) return;
    _link._send({'s': id, 'fin': true});
    _link._terms.remove(id);
    _end(null);
  }

  void _down(TermDown d) {
    if (!_downs.isClosed) _downs.add(d);
  }

  void _end(String? reason) {
    if (!_closed.isCompleted) _closed.complete(reason);
    unawaited(_downs.close());
  }
}
