import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/outbox.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'store_test.dart' show FakeLink;

const sid = 'S1';

void main() {
  late Prefs prefs;
  setUp(() async {
    SharedPreferences.setMockInitialValues({});
    prefs = await Prefs.load();
  });

  test('a prompt still on its way survives a new Store and is sent on the next connection', () async {
    final a = Store(keeper: prefs);
    final link = FakeLink();
    a.attachRequester(link);
    a
        .send(
          sid,
          'ship it',
          how: PromptHow.steer,
          attachments: [Attachment('a.png', List.filled(32, 7))],
        )
        .ignore();
    final (op, _) = link.sent.single;

    final b = Store(keeper: prefs);
    final o = b.outbox.single;
    expect(o.op, op);
    expect(o.text, 'ship it');
    expect(o.how, PromptHow.steer);
    expect(o.attachments.single.name, 'a.png');
    expect(o.attachments.single.blob, List.filled(32, 7));
    expect(o.unsent, isTrue);
    expect(b.items(sid).single.itemId, 'local:$op');

    final link2 = FakeLink();
    b.attachRequester(link2);
    b.onOnline();
    final prompts = link2.sent.where((s) => s.$1 != null).toList();
    expect(prompts.single.$1, op);
    final req = prompts.single.$2 as Map;
    expect(req['Prompt']['text'], 'ship it');
  });

  test('a failed entry keeps its reason and retryability', () async {
    final a = Store(keeper: prefs);
    final link = FakeLink();
    a.attachRequester(link);
    final f = a.send(sid, 'hello');
    link.answers[link.sent.single.$1]!.completeError(const LinkLost('gone'));
    await expectLater(f, throwsA(isA<LinkLost>()));

    final o = Store(keeper: prefs).outbox.single;
    expect(o.state, OutboxState.failed);
    expect(o.retryable, isTrue);
    expect(o.error, contains('gone'));
  });

  test(
    'accepted entries are not written; the key goes when nothing waits',
    () async {
      final a = Store(keeper: prefs);
      final link = FakeLink();
      a.attachRequester(link);
      final f = a.send(sid, 'hello');
      expect(prefs.outboxJson, isNotNull);
      link.answers[link.sent.single.$1]!.complete(const Resp('Ok', null));
      await f;
      expect(a.outbox.single.state, OutboxState.accepted);
      expect(prefs.outboxJson, isNull);
      expect(Store(keeper: prefs).outbox, isEmpty);
    },
  );

  test('discard removes it from disk; a corrupt value is ignored', () {
    final a = Store(keeper: prefs);
    a.attachRequester(FakeLink());
    a.send(sid, 'x').ignore();
    a.discard(a.outbox.single.op);
    expect(Store(keeper: prefs).outbox, isEmpty);

    prefs.saveOutbox(
      jsonEncode([
        'nope',
        {'op': 1},
      ]),
    );
    expect(Store(keeper: prefs).outbox, isEmpty);
    prefs.saveOutbox('{not json');
    expect(Store(keeper: prefs).outbox, isEmpty);
  });
}
