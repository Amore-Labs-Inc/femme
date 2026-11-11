import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/net/wire.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/chat.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'chat_items.dart';
import 'fixtures.dart';

S2C msg(Object json) => S2C.fromJson(jsonDecode(jsonEncode(json)) as Object);

Map<String, dynamic> row(Item i) => {
  'Item': {
    'sid': i.sid,
    'item_id': i.itemId,
    'seq': i.seq,
    'rev': i.rev,
    'kind': wireName(i.kind),
    'status': wireName(i.status),
    'preview': i.preview,
    'body': i.body,
    'blob_hash': null,
    'ts': i.ts,
    'parent': i.parent,
  },
};

/// A chat screen over [items], idle so no working-line timer runs.
Future<(Store, Link)> pumpChat(WidgetTester tester, List<Item> items) async {
  SharedPreferences.setMockInitialValues({});
  ChatController.resetCaches();
  final prefs = await Prefs.load();
  final store = Store();
  final link = Link(clientInstance: 1);
  store.apply(
    msg(
      changes('Global', 10, [
        {'Session': sessionJson(status: 'Idle')},
      ]),
    ),
  );
  store.apply(
    msg(changes({'Session': sid}, 60, [for (final i in items) row(i)])),
  );
  await tester.pumpWidget(
    Femme(
      store: store,
      link: link,
      prefs: prefs,
      child: CupertinoApp(
        theme: femmeTheme(Brightness.light),
        home: const ChatScreen(sid: sid),
      ),
    ),
  );
  await tester.pump();
  return (store, link);
}

void main() {
  testWidgets('the strip shows main and each subagent; a chip opens it', (
    tester,
  ) async {
    final (_, link) = await pumpChat(tester, [
      it('u1', ItemKind.userMessage, body: {'text': 'go'}),
      call('ag1', 'Agent', {
        'description': 'review the diff',
        'prompt': '…',
      }, status: ItemStatus.inProgress),
      it(
        'sub1',
        ItemKind.assistantMessage,
        body: {'text': 'looking at the diff'},
        parent: 'ag1',
      ),
    ]);
    expect(find.text('‹ main'), findsOneWidget);
    expect(find.text('review the diff'), findsWidgets);
    expect(find.textContaining(RegExp(r'^\d+m \d\ds$|^\d+s$')), findsWidgets);

    await tester.tap(find.text('review the diff').first);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 600));
    expect(
      find.textContaining('looking at the diff', findRichText: true),
      findsOneWidget,
    );
    // ‹ main goes back
    await tester.tap(find.text('‹ main').last);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 600));
    expect(
      find.textContaining('looking at the diff', findRichText: true),
      findsNothing,
    );

    await tester.pumpWidget(const SizedBox());
    link.close();
  });

  testWidgets('no subagents, no strip', (tester) async {
    final (_, link) = await pumpChat(tester, [
      it('u1', ItemKind.userMessage, body: {'text': 'go'}),
    ]);
    expect(find.text('‹ main'), findsNothing);
    await tester.pumpWidget(const SizedBox());
    link.close();
  });
}
