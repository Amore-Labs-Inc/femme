import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/app.dart';
import 'package:femme_mobile/ui/now.dart';
import 'package:package_info_plus/package_info_plus.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

S2C msg(Object json) => S2C.fromJson(jsonDecode(jsonEncode(json)) as Object);

const s3 = '01JD3Q8Z9X4K2M7N5P6R8S0T3X';
const s4 = '01JD3Q8Z9X4K2M7N5P6R8S0T4Y';
const s5 = '01JD3Q8Z9X4K2M7N5P6R8S0T5Z';

Store fakeStore() => Store()
  ..apply(
    msg(
      changes('Global', 20, [
        {
          'Session': sessionJson(
            id: sid,
            rev: 11,
            status: 'Blocked',
            label: 'blocked',
          ),
        },
        {
          'Session': sessionJson(
            id: sid2,
            rev: 12,
            status: 'Idle',
            attention: 'Done',
            label: 'finished',
          ),
        },
        {
          'Session': sessionJson(
            id: s3,
            rev: 13,
            status: 'Working',
            label: 'busy',
          ),
        },
        {
          'Session': sessionJson(
            id: s4,
            rev: 14,
            status: 'Idle',
            label: 'quiet',
          ),
        },
        {
          'Session': {
            ...sessionJson(id: s5, rev: 15, status: 'Closed', label: 'gone'),
            'closed': 1,
          },
        },
      ]),
    ),
  );

void main() {
  test('Now sections from the store, in order', () {
    final secs = nowSections(fakeStore());
    expect(secs.map((x) => x.$1), ['Needs you', 'Done', 'Working', 'Idle']);
    expect(
      [for (final x in secs) x.$2.map((s) => s.title).toList()],
      [
        ['blocked'],
        ['finished'],
        ['busy'],
        ['quiet'],
      ],
    );
  });

  test('the pending question is the newest open one', () {
    Item q(String id, int seq, String status, {String? answered}) =>
        Item.fromJson(
          itemJson(
            id: id,
            seq: seq,
            kind: 'Question',
            status: status,
            body: answered == null
                ? questionBody
                : questionBody.replaceFirst(
                    '"answered":null',
                    '"answered":$answered',
                  ),
          ),
        );
    expect(pendingQuestion([]), isNull);
    expect(
      pendingQuestion([q('a', 1, 'Answered', answered: '{"Option":0}')]),
      isNull,
    );
    final p = pendingQuestion([q('a', 1, 'Pending'), q('b', 2, 'Failed')]);
    expect(p!.prompt, 'Fold it into one lesson?');
    expect(p.options, ['yes, fold it', 'no, keep two']);
  });

  test('last line of the newest assistant message', () {
    final items = [
      Item.fromJson(
        itemJson(id: 'a', seq: 1, body: '{"text":"first\\n\\nsecond line"}'),
      ),
      Item.fromJson(itemJson(id: 'b', seq: 2, kind: 'ToolCall', body: '{}')),
    ];
    expect(lastLine(items, {ItemKind.assistantMessage}), 'second line');
    expect(lastLine(items, {ItemKind.toolCall}), 'Done: the build is green.');
  });

  testWidgets('Now renders the sections and the question options', (t) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await Prefs.load();
    final store = fakeStore();
    store.apply(
      msg(
        changes(
          {'Session': sid},
          30,
          [
            {
              'Item': itemJson(
                id: 'q1',
                seq: 5,
                kind: 'Question',
                status: 'Pending',
                body: questionBody,
              ),
            },
          ],
        ),
      ),
    );
    final link = Link(clientInstance: 1);
    await t.pumpWidget(
      Femme(
        store: store,
        link: link,
        prefs: prefs,
        child: const CupertinoApp(home: NowTab()),
      ),
    );
    expect(find.text('Needs you'.toUpperCase()), findsNothing);
    expect(find.text('blocked'), findsOneWidget);
    expect(find.text('Fold it into one lesson?'), findsOneWidget);
    expect(find.text('yes, fold it'), findsOneWidget);
    expect(find.text('finished'), findsOneWidget);
    expect(find.text('busy'), findsOneWidget);
    expect(find.text('quiet'), findsOneWidget);
    expect(find.text('gone'), findsNothing);
    await t.pumpWidget(const SizedBox());
  });

  testWidgets('every tab of the shell renders', (t) async {
    SharedPreferences.setMockInitialValues({});
    PackageInfo.setMockInitialValues(
      appName: 'FEMME',
      packageName: 'x',
      version: '1.0.0',
      buildNumber: '1',
      buildSignature: '',
    );
    final prefs = await Prefs.load();
    final link = Link(clientInstance: 1);
    await t.pumpWidget(
      Femme(
        store: fakeStore(),
        link: link,
        prefs: prefs,
        child: const CupertinoApp(home: Shell()),
      ),
    );
    expect(find.text('1'), findsWidgets); // the Now badge
    for (final tab in ['Chats', 'Box', 'Me', 'Now']) {
      await t.tap(find.text(tab).last);
      await t.pump(const Duration(milliseconds: 500));
      expect(t.takeException(), isNull, reason: tab);
    }
    expect(find.text('Platform'), findsNothing); // Chats is offstage now
    await t.pumpWidget(const SizedBox());
  });

  testWidgets('a multi-select question on Now ticks options and sends them', (
    t,
  ) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await Prefs.load();
    final store = fakeStore();
    store.apply(
      msg(
        changes(
          {'Session': sid},
          30,
          [
            {
              'Item': itemJson(
                id: 'q1',
                seq: 5,
                kind: 'Question',
                status: 'Pending',
                body: questionBody
                    .replaceFirst('"multi":false', '"multi":true')
                    .replaceFirst(
                      '"no, keep two"',
                      '"no, keep two","split in three"',
                    ),
              ),
            },
          ],
        ),
      ),
    );
    final link = _RecordingLink();
    await t.pumpWidget(
      Femme(
        store: store,
        link: link,
        prefs: prefs,
        child: const CupertinoApp(home: NowTab()),
      ),
    );
    expect(find.text('Fold it into one lesson?'), findsOneWidget);
    expect(find.text('☐  yes, fold it'), findsOneWidget);
    expect(find.text('☐  split in three'), findsOneWidget);
    expect(find.text('Other…'), findsNothing);

    final send = find.widgetWithText(CupertinoButton, 'Send');
    expect(t.widget<CupertinoButton>(send).onPressed, isNull);
    await t.tap(find.text('☐  yes, fold it'));
    await t.pump();
    await t.tap(find.text('☐  split in three'));
    await t.pump();
    expect(find.text('☑  yes, fold it'), findsOneWidget);
    expect(find.text('☑  split in three'), findsOneWidget);
    expect(link.sent, isEmpty, reason: 'ticking sends nothing');

    await t.tap(send);
    await t.pump();
    expect(
      link.sent.single,
      Req.answerQuestion(sid, 'toolu_01ABC', Answer.options([0, 2])),
    );
    await t.pumpWidget(const SizedBox());
  });

  testWidgets('a single-select question keeps the one-tap buttons', (t) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await Prefs.load();
    final store = fakeStore();
    store.apply(
      msg(
        changes(
          {'Session': sid},
          30,
          [
            {
              'Item': itemJson(
                id: 'q1',
                seq: 5,
                kind: 'Question',
                status: 'Pending',
                body: questionBody,
              ),
            },
          ],
        ),
      ),
    );
    final link = _RecordingLink();
    await t.pumpWidget(
      Femme(
        store: store,
        link: link,
        prefs: prefs,
        child: const CupertinoApp(home: NowTab()),
      ),
    );
    expect(find.textContaining('☐'), findsNothing);
    await t.tap(find.text('no, keep two'));
    await t.pump();
    expect(
      link.sent.single,
      Req.answerQuestion(sid, 'toolu_01ABC', Answer.option(1)),
    );
    await t.pumpWidget(const SizedBox());
  });
}

/// A link that answers every request `Ok` at once and keeps what was asked.
class _RecordingLink extends Link {
  _RecordingLink() : super(clientInstance: 1);
  final sent = <Object>[];

  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
  }) {
    if (req is Map && req.containsKey('AnswerQuestion')) sent.add(req);
    return Future.value(const Resp('Ok', 'Ok'));
  }
}
