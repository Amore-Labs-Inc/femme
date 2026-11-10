import 'dart:async';

import 'package:flutter/foundation.dart';

import '../net/ids.dart';
import '../net/link.dart';
import '../net/models.dart';
import '../net/rpc.dart';
import 'outbox.dart';
import 'transcript.dart';

/// The app's copy of the box: sessions, panes, issues, the transcripts of open sessions and the
/// outbox. Fed by a [Link]'s events; screens listen to it.
class Store extends ChangeNotifier {
  Store();

  Requester? _link;
  StreamSubscription<S2C>? _sub;

  /// The daemon database's epoch from the last `Welcome`; null before the first.
  int? epoch;

  /// Highest rev held for the global scope; null until its first `Changes`.
  int? globalRev;

  final sessions = <String, Session>{};
  final issues = <int, Issue>{};

  /// Keyed by [PaneId.key] (`sid:no`).
  final panes = <String, Pane>{};
  final _transcripts = <String, Transcript>{};
  final _outbox = <String, OutboxEntry>{};

  /// Connects the store to [link]: its cursors, its Welcome, its events.
  void attach(Link link) {
    attachRequester(link);
    link.cursors = cursors;
    link.onWelcome = onWelcome;
    link.onOnline = onOnline;
    _sub = link.events.listen(apply);
  }

  /// Uses [r] for requests (tests pass a fake).
  void attachRequester(Requester r) => _link = r;

  @override
  void dispose() {
    _sub?.cancel();
    super.dispose();
  }

  // ---- resume ----

  /// What `Hello` carries: every scope held, from where it is held. Scopes not held yet are
  /// subscribed explicitly in [onOnline].
  Cursors cursors() => Cursors(
    epoch: epoch ?? 0,
    globalRev: globalRev,
    sessions: {
      for (final e in _transcripts.entries)
        if (e.value.refs > 0 && e.value.rev != null) e.key: e.value.rev!,
    },
  );

  /// A new epoch means the daemon's database is not the one the cursors came from: forget it all.
  void onWelcome(int newEpoch) {
    if (epoch != null && epoch != newEpoch) {
      sessions.clear();
      issues.clear();
      panes.clear();
      globalRev = null;
      for (final t in _transcripts.values) {
        t.clear();
      }
      notifyListeners();
    }
    epoch = newEpoch;
  }

  /// Subscribes what the `Hello` cursors did not resume, and resends failed outbox entries.
  void onOnline() {
    if (globalRev == null) _subscribe(Scope.global, null);
    for (final e in _transcripts.entries) {
      if (e.value.refs > 0 && e.value.rev == null) {
        _subscribe(Scope.session(e.key), null);
      }
    }
    for (final o in _outbox.values) {
      if (o.state == OutboxState.failed && o.retryable) retry(o.op).ignore();
    }
  }

  void _subscribe(Scope scope, int? after) {
    final link = _link;
    if (link == null || !link.isOnline) return;
    link.req(Req.subscribe(scope, after: after), resend: false).ignore();
  }

  // ---- events ----

  void apply(S2C msg) {
    switch (msg) {
      case Changes():
        _applyChanges(msg);
      case Delta(:final sid, :final itemId, :final offset, :final text):
        final t = _transcripts[sid];
        if (t != null && t.applyDelta(itemId, offset, text)) notifyListeners();
      default:
        break;
    }
  }

  void _applyChanges(Changes c) {
    final sid = c.scope.sid;
    if (sid == null) {
      if (c.reset) {
        sessions.clear();
        panes.clear();
        issues.clear();
      }
      for (final r in c.rows) {
        _applyGlobal(r);
      }
      globalRev = c.reset ? c.rev : _max(globalRev, c.rev);
    } else {
      final t = _transcripts.putIfAbsent(sid, Transcript.new);
      if (c.reset) t.clear();
      for (final r in c.rows) {
        switch (r) {
          case ItemRow(:final item):
            t.put(item);
            _reconcile(item);
          case Tombstone(entity: Entity.item, :final id):
            t.remove(id.substring(id.indexOf('/') + 1));
          default:
            break;
        }
      }
      t.rev = c.reset ? c.rev : _max(t.rev, c.rev);
    }
    notifyListeners();
  }

  void _applyGlobal(Row r) {
    switch (r) {
      case SessionRow(:final session):
        final old = sessions[session.id];
        if (old == null || old.rev <= session.rev) {
          sessions[session.id] = session;
        }
      case PaneRow(:final pane):
        final old = panes[pane.id.key];
        if (old == null || old.rev <= pane.rev) panes[pane.id.key] = pane;
      case IssueRow(:final issue):
        final old = issues[issue.id];
        if (old == null || old.rev <= issue.rev) issues[issue.id] = issue;
      case Tombstone(:final entity, :final id):
        switch (entity) {
          case Entity.session:
            sessions.remove(id);
            _transcripts[id]?.clear();
            panes.removeWhere((_, p) => p.id.sid == id);
          case Entity.pane:
            panes.remove(id);
          case Entity.issue:
            issues.remove(int.tryParse(id));
          case Entity.item:
            break;
        }
      default:
        break;
    }
  }

  static int _max(int? a, int b) => a == null || b > a ? b : a;

  // ---- transcripts ----

  /// Opens a session's transcript (counted: every [subscribe] needs one [unsubscribe]).
  void subscribe(String sid) {
    final t = _transcripts.putIfAbsent(sid, Transcript.new);
    if (t.refs++ == 0) _subscribe(Scope.session(sid), t.rev);
  }

  void unsubscribe(String sid) {
    final t = _transcripts[sid];
    if (t == null || t.refs == 0) return;
    if (--t.refs == 0) {
      final link = _link;
      if (link != null && link.isOnline) {
        link.req(Req.unsubscribe(Scope.session(sid)), resend: false).ignore();
      }
    }
  }

  bool isSubscribed(String sid) => (_transcripts[sid]?.refs ?? 0) > 0;

  /// The session's items by seq, then its outbox entries as `Pending` user messages.
  List<Item> items(String sid) {
    _sweepOutbox();
    final t = _transcripts[sid];
    final real = t?.sorted ?? const <Item>[];
    final local = _outbox.values.where((o) => o.sid == sid).toList()
      ..sort((a, b) => a.ts.compareTo(b.ts));
    if (local.isEmpty) return real;
    final after = (t?.newestSeq ?? 0) + 1;
    return [...real, for (final o in local) o.asItem(after)];
  }

  /// The text to show for an item: its streaming text while it streams.
  String textOf(Item item) => _transcripts[item.sid]?.textOf(item) ?? item.text;

  bool streaming(Item item) =>
      _transcripts[item.sid]?.streaming(item.itemId) ?? false;

  /// More history exists (or might) before the oldest item held.
  bool hasOlder(String sid) {
    final t = _transcripts[sid];
    return t == null || t.olderUnknown || t.nextBefore != null;
  }

  /// Loads one page of history before the oldest item held. Returns how many items it added.
  Future<int> loadOlder(String sid, {int limit = 60}) async {
    final t = _transcripts.putIfAbsent(sid, Transcript.new);
    final before = t.olderUnknown ? t.oldestSeq : t.nextBefore;
    final link = _link;
    if (before == null || t.loadingOlder || link == null) return 0;
    t.loadingOlder = true;
    try {
      final page = (await link.req(
        Req.transcriptPage(sid, before, limit: limit),
      )).page;
      var added = 0;
      for (final i in page.items) {
        if (t.byId[i.itemId] == null && t.put(i)) added++;
      }
      t.nextBefore = page.nextBefore;
      t.olderUnknown = false;
      if (added > 0) notifyListeners();
      return added;
    } finally {
      t.loadingOlder = false;
    }
  }

  // ---- outbox ----

  List<OutboxEntry> get outbox => _outbox.values.toList();

  /// Sends a prompt. The bubble shows at once (as a local `Pending` item) and stays until the
  /// box's `UserMessage` row with the same op arrives. Throws what the request threw; the entry
  /// then shows as failed until [retry] or [discard].
  Future<void> send(
    String sid,
    String text, {
    PromptHow how = PromptHow.queue,
    List<Attachment> attachments = const [],
  }) {
    final o = OutboxEntry(
      op: Ulid.next(),
      sid: sid,
      text: text,
      how: how,
      attachments: attachments,
      ts: DateTime.now().millisecondsSinceEpoch,
    );
    _outbox[o.op] = o;
    notifyListeners();
    return _deliver(o);
  }

  /// Sends a failed entry again under the same op (the box dedups it).
  Future<void> retry(String op) {
    final o = _outbox[op];
    if (o == null) return Future.value();
    o.state = OutboxState.sending;
    o.error = null;
    notifyListeners();
    return _deliver(o);
  }

  void discard(String op) {
    if (_outbox.remove(op) != null) notifyListeners();
  }

  Future<void> _deliver(OutboxEntry o) async {
    final link = _link;
    if (link == null) {
      _failed(o, 'not connected');
      throw const LinkLost('not connected');
    }
    try {
      await link.req(o.request, op: o.op, timeout: const Duration(minutes: 2));
      if (_outbox[o.op] == o) {
        // A `/command` leaves no UserMessage behind (the daemon stores none), so nothing would
        // ever reconcile its bubble: it goes with the answer.
        if (o.text.trimLeft().startsWith('/')) {
          _outbox.remove(o.op);
        } else {
          o.state = OutboxState.accepted;
        }
        notifyListeners();
      }
    } catch (e) {
      o.retryable = e is TimeoutException || e is LinkLost;
      _failed(o, e.toString());
      rethrow;
    }
  }

  /// An accepted entry the box never echoed (a message to a session that was closed meanwhile)
  /// is not shown forever: ten minutes, as the desktop does.
  void _sweepOutbox() {
    final cutoff = DateTime.now().millisecondsSinceEpoch - 10 * 60 * 1000;
    _outbox.removeWhere(
      (_, o) => o.state == OutboxState.accepted && o.ts < cutoff,
    );
  }

  void _failed(OutboxEntry o, String why) {
    if (_outbox[o.op] != o) return;
    o.state = OutboxState.failed;
    o.error = why;
    notifyListeners();
  }

  /// The box's row of a message we sent replaces our local one.
  void _reconcile(Item item) {
    if (item.kind != ItemKind.userMessage) return;
    final op = item.op;
    if (op != null) _outbox.remove(op);
  }

  // ---- derived lists ----

  /// Sessions waiting on the user, most recently changed first.
  List<Session> get needsYou => _sorted((s) => s.needsYou);

  /// Sessions that finished and want a look.
  List<Session> get done =>
      _sorted((s) => !s.needsYou && s.attention == Attention.done);

  List<Session> get working => _sorted(
    (s) =>
        !s.needsYou &&
        s.attention != Attention.done &&
        (s.status == SessionStatus.working ||
            s.status == SessionStatus.starting),
  );

  /// Everything else still open (idle, paused, interrupted, crashed, failed).
  List<Session> get idle => _sorted(
    (s) =>
        !s.needsYou &&
        s.attention != Attention.done &&
        s.status != SessionStatus.working &&
        s.status != SessionStatus.starting &&
        s.status != SessionStatus.closed,
  );

  /// The sessions the lists show: live ones, as the desktop's sidebar (a closed row stays until
  /// retention tombstones it).
  Iterable<Session> get live => sessions.values.where(
    (s) => s.closed == null && s.status != SessionStatus.closed,
  );

  List<Session> _sorted(bool Function(Session) where) =>
      live.where(where).toList()..sort((a, b) => b.rev.compareTo(a.rev));
}
