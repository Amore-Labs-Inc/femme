import 'dart:async';
import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/outbox.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

class FakeLink implements Requester {
  final sent = <(String?, Object)>[];
  final answers = <String, Completer<Resp>>{};
  final completers = <Completer<Resp>>[];
  @override
  bool isOnline = true;

  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
  }) {
    sent.add((op, req));
    final c = Completer<Resp>();
    completers.add(c);
    if (op != null) answers[op] = c;
    return c.future;
  }
}

S2C msg(Object json) => S2C.fromJson(jsonDecode(jsonEncode(json)) as Object);

Object itemRow({
  String id = 'msg_01',
  int seq = 10,
  int rev = 50,
  String kind = 'AssistantMessage',
  String status = 'Completed',
  String? body,
}) => {
  'Item': itemJson(
    id: id,
    seq: seq,
    rev: rev,
    kind: kind,
    status: status,
    body: body ?? '{"text":"t-$id"}',
  ),
};

void main() {
  late Store store;
  late FakeLink link;
  setUp(() {
    store = Store();
    link = FakeLink();
    store.attachRequester(link);
  });

  test(
    'global Changes apply, reset drops the scope first, tombstones delete',
    () {
      store.apply(
        msg(
          changes('Global', 10, [
            {'Session': sessionJson(rev: 9)},
            {
              'Session': sessionJson(
                id: sid2,
                rev: 10,
                status: 'Idle',
                attention: 'Done',
              ),
            },
          ]),
        ),
      );
      expect(store.sessions.length, 2);
      expect(store.globalRev, 10);
      expect(store.working.single.id, sid);
      expect(store.done.single.id, sid2);

      // A stale row (lower rev) does not overwrite.
      store.apply(
        msg(
          changes('Global', 11, [
            {'Session': sessionJson(rev: 3, status: 'Idle')},
          ]),
        ),
      );
      expect(store.sessions[sid]!.status, SessionStatus.working);

      store.apply(
        msg(
          changes('Global', 12, [
            {
              'Tombstone': {'entity': 'Session', 'id': sid2, 'rev': 12},
            },
          ]),
        ),
      );
      expect(store.sessions.keys, [sid]);

      store.apply(
        msg(
          changes('Global', 20, [
            {'Session': sessionJson(id: sid2, rev: 20, status: 'Blocked')},
          ], reset: true),
        ),
      );
      expect(store.sessions.keys, [sid2]);
      expect(store.globalRev, 20);
      expect(store.needsYou.single.id, sid2);
    },
  );

  test('session Changes: items ordered by seq, reset, item tombstone', () {
    store.subscribe(sid);
    store.apply(
      msg(
        changes(
          {'Session': sid},
          52,
          [
            itemRow(id: 'b', seq: 2, rev: 52),
            itemRow(id: 'a', seq: 1, rev: 51),
          ],
        ),
      ),
    );
    expect(store.items(sid).map((i) => i.itemId), ['a', 'b']);
    expect(store.cursors().sessions, {sid: 52});

    store.apply(
      msg(
        changes(
          {'Session': sid},
          53,
          [
            {
              'Tombstone': {'entity': 'Item', 'id': '$sid/a', 'rev': 53},
            },
          ],
        ),
      ),
    );
    expect(store.items(sid).map((i) => i.itemId), ['b']);

    store.apply(
      msg(
        changes(
          {'Session': sid},
          60,
          [itemRow(id: 'z', seq: 9, rev: 60)],
          reset: true,
        ),
      ),
    );
    expect(store.items(sid).map((i) => i.itemId), ['z']);
    expect(store.cursors().sessions, {sid: 60});
  });

  test('Delta: append at offset, resend cuts back, gap dropped, row replaces buffer', () {
    store.subscribe(sid);
    store.apply(
      msg(
        changes(
          {'Session': sid},
          5,
          [
            itemRow(
              id: 'm',
              seq: 1,
              rev: 5,
              status: 'InProgress',
              body: '{"text":"Hel"}',
            ),
          ],
        ),
      ),
    );
    Item item() => store.items(sid).single;
    store.apply(const Delta(sid, 'm', 3, 'lo'));
    expect(store.textOf(item()), 'Hello');
    store.apply(const Delta(sid, 'm', 5, ' wörld'));
    expect(store.textOf(item()), 'Hello wörld');
    // Offsets count UTF-8 bytes: "Hello wörld" is 12 (ö is two).
    store.apply(const Delta(sid, 'm', 12, '!'));
    expect(store.textOf(item()), 'Hello wörld!');
    // A resend from an earlier offset replaces the tail.
    store.apply(const Delta(sid, 'm', 5, ', you'));
    expect(store.textOf(item()), 'Hello, you');
    // A gap is ignored.
    store.apply(const Delta(sid, 'm', 99, 'x'));
    expect(store.textOf(item()), 'Hello, you');
    expect(store.streaming(item()), isTrue);
    // The completed row wins.
    store.apply(
      msg(
        changes(
          {'Session': sid},
          6,
          [itemRow(id: 'm', seq: 1, rev: 6, body: '{"text":"Hello, world."}')],
        ),
      ),
    );
    expect(store.streaming(item()), isFalse);
    expect(store.textOf(item()), 'Hello, world.');
  });

  test('outbox: optimistic row, then reconciled by the UserMessage with the same op', () async {
    store.subscribe(sid);
    store.apply(
      msg(changes({'Session': sid}, 5, [itemRow(id: 'a', seq: 4, rev: 5)])),
    );
    final sending = store.send(sid, 'ship it');
    final local = store.items(sid).last;
    expect(local.kind, ItemKind.userMessage);
    expect(local.status, ItemStatus.pending);
    expect(local.seq, 5);
    final (op, req) = link.sent.last;
    expect((req as Map)['Prompt']['text'], 'ship it');
    expect(local.op, op);

    link.answers[op]!.complete(const Resp('Ok', null));
    await sending;
    expect(store.outbox.single.state, OutboxState.accepted);

    store.apply(
      msg(
        changes(
          {'Session': sid},
          6,
          [
            itemRow(
              id: 'U1',
              seq: 5,
              rev: 6,
              kind: 'UserMessage',
              status: 'Pending',
              body: jsonEncode({'text': 'ship it', 'op': op}),
            ),
          ],
        ),
      ),
    );
    expect(store.outbox, isEmpty);
    expect(store.items(sid).map((i) => i.itemId), ['a', 'U1']);
  });

  test(
    'outbox: a link failure marks it failed and retry reuses the op',
    () async {
      final f = store.send(sid, 'hello');
      final (op, _) = link.sent.last;
      link.answers[op]!.completeError(const LinkLost('gone'));
      await expectLater(f, throwsA(isA<LinkLost>()));
      expect(store.items(sid).single.status, ItemStatus.failed);
      expect(store.outbox.single.retryable, isTrue);

      final r = store.retry(op!);
      expect(link.sent.last.$1, op);
      link.answers[op]!.complete(const Resp('Ok', null));
      await r;
      expect(store.outbox.single.state, OutboxState.accepted);
    },
  );

  test(
    'subscribe refcount, resume cursors, epoch change forgets everything',
    () {
      store.subscribe(sid);
      store.subscribe(sid);
      expect(
        link.sent.where((s) => (s.$2 as Map).containsKey('Subscribe')).length,
        1,
      );
      store.unsubscribe(sid);
      expect(
        link.sent.where((s) => (s.$2 as Map).containsKey('Unsubscribe')),
        isEmpty,
      );
      store.unsubscribe(sid);
      expect(
        link.sent.where((s) => (s.$2 as Map).containsKey('Unsubscribe')).length,
        1,
      );

      store.onWelcome(7);
      store.apply(
        msg(
          changes('Global', 10, [
            {'Session': sessionJson()},
          ]),
        ),
      );
      expect(store.cursors().toJson(), {
        'epoch': 7,
        'global_rev': 10,
        'sessions': [],
      });
      store.onWelcome(8);
      expect(store.sessions, isEmpty);
      expect(store.globalRev, isNull);
      link.sent.clear();
      store.onOnline();
      expect(link.sent.single.$2, Req.subscribe(Scope.global));
    },
  );

  test('loadOlder pages backwards from the oldest seq', () async {
    store.subscribe(sid);
    store.apply(
      msg(changes({'Session': sid}, 5, [itemRow(id: 'n', seq: 120, rev: 5)])),
    );
    link.sent.clear();
    final f = store.loadOlder(sid);
    final (op, req) = link.sent.single;
    expect(req, Req.transcriptPage(sid, 120));
    expect(op, isNull); // the link picks the op
    link.completers.last.complete(
      Resp('Page', {
        'items': [
          itemJson(id: 'o1', seq: 100, rev: 3),
          itemJson(id: 'o2', seq: 119, rev: 4),
        ],
        'next_before': 100,
      }),
    );
    expect(await f, 2);
    expect(store.items(sid).map((i) => i.seq), [100, 119, 120]);
    expect(store.hasOlder(sid), isTrue);
    // The cursor of the live scope is not moved by history.
    expect(store.cursors().sessions, {sid: 5});

    final g = store.loadOlder(sid);
    expect(link.sent.last.$2, Req.transcriptPage(sid, 100));
    link.completers.last.complete(
      const Resp('Page', {'items': [], 'next_before': null}),
    );
    expect(await g, 0);
    expect(store.hasOlder(sid), isFalse);
  });

  int subscribes() =>
      link.sent.where((s) => (s.$2 as Map).containsKey('Subscribe')).length;

  test(
    'a refused session subscribe is tried again while the chat is open',
    () async {
      store.subscribeRetry = const Duration(milliseconds: 20);
      store.subscribe(sid);
      expect(subscribes(), 1);
      link.completers.last.completeError(const FemmeError('NotFound', 'no'));
      await Future<void>.delayed(const Duration(milliseconds: 60));
      expect(subscribes(), 2);
      link.completers.last.completeError(const FemmeError('NotFound', 'no'));
      await Future<void>.delayed(const Duration(milliseconds: 60));
      expect(subscribes(), 3);
      link.completers.last.complete(const Resp('Subscribed', null));
      await Future<void>.delayed(const Duration(milliseconds: 60));
      expect(subscribes(), 3);
    },
  );

  test(
    'a refused subscribe stops being retried once the chat closes',
    () async {
      store.subscribeRetry = const Duration(milliseconds: 20);
      store.subscribe(sid);
      link.completers.last.completeError(const FemmeError('NotFound', 'no'));
      await Future<void>.delayed(Duration.zero);
      store.unsubscribe(sid);
      await Future<void>.delayed(const Duration(milliseconds: 60));
      expect(subscribes(), 1);
    },
  );

  test('a subscribe lost with the link waits for the reconnection', () async {
    store.subscribeRetry = const Duration(milliseconds: 20);
    store.subscribe(sid);
    link.completers.last.completeError(const LinkLost('gone'));
    await Future<void>.delayed(const Duration(milliseconds: 60));
    expect(subscribes(), 1);
  });

  test('a tombstoned or closed session is known as closed, not unknown', () {
    store.apply(
      msg(
        changes('Global', 10, [
          {'Session': sessionJson(rev: 9)},
          {'Session': sessionJson(id: sid2, rev: 10, status: 'Idle')},
        ]),
      ),
    );
    expect(store.isClosed(sid), isFalse);
    expect(store.isClosed('01UNKNOWN'), isFalse);
    expect(store.session('01UNKNOWN'), isNull);

    store.apply(
      msg(
        changes('Global', 11, [
          {'Session': sessionJson(rev: 11, status: 'Closed')},
          {
            'Tombstone': {'entity': 'Session', 'id': sid2, 'rev': 11},
          },
        ]),
      ),
    );
    expect(store.isClosed(sid), isTrue);
    expect(store.isClosed(sid2), isTrue);
    // the last row stays readable for the chat header; the lists drop both
    expect(store.session(sid2)!.id, sid2);
    expect(store.sessions.containsKey(sid2), isFalse);
    expect(store.live, isEmpty);

    // a tombstone for a session never held still says closed
    store.apply(
      msg(
        changes('Global', 12, [
          {
            'Tombstone': {'entity': 'Session', 'id': '01NEVERSEEN', 'rev': 12},
          },
        ]),
      ),
    );
    expect(store.isClosed('01NEVERSEEN'), isTrue);
    expect(store.session('01NEVERSEEN'), isNull);

    // a new epoch forgets the markers
    store.onWelcome(1);
    store.onWelcome(2);
    expect(store.isClosed(sid2), isFalse);
  });

  test('an open chat keeps its transcript when its session is tombstoned', () {
    store.subscribe(sid);
    store.apply(
      msg(
        changes(
          {'Session': sid},
          60,
          [
            {'Item': itemJson()},
          ],
        ),
      ),
    );
    store.apply(
      msg(
        changes('Global', 61, [
          {
            'Tombstone': {'entity': 'Session', 'id': sid, 'rev': 61},
          },
        ]),
      ),
    );
    expect(store.isClosed(sid), isTrue);
    expect(store.items(sid), hasLength(1));
  });

  test('a closed session is not resubscribed', () async {
    store.subscribeRetry = const Duration(milliseconds: 20);
    store.subscribe(sid);
    link.completers.last.completeError(const FemmeError('NotFound', 'no'));
    store.apply(
      msg(
        changes('Global', 5, [
          {
            'Tombstone': {'entity': 'Session', 'id': sid, 'rev': 5},
          },
        ]),
      ),
    );
    await Future<void>.delayed(const Duration(milliseconds: 60));
    expect(subscribes(), 1);
  });
}
