import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/net/wire.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/chat/header.dart';
import 'package:femme_mobile/ui/chat/turns.dart';
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
    'parent': null,
  },
};

void main() {
  test('a running foreground call is a task to background; others are not', () {
    Json? b(Item i) => i.bodyJson;
    final fg = call(
      'a',
      'Bash',
      {'command': 'sleep 60'},
      status: ItemStatus.inProgress,
      task: 'foreground',
    );
    expect(hasForegroundTask([fg], b), isTrue);
    // answered, already in the background, or no task at all
    expect(hasForegroundTask([fg, result('a', 'done')], b), isFalse);
    expect(
      hasForegroundTask([
        call(
          'c',
          'Bash',
          {},
          status: ItemStatus.inProgress,
          task: 'background',
        ),
      ], b),
      isFalse,
    );
    expect(
      hasForegroundTask([
        call('d', 'Read', {}, status: ItemStatus.inProgress),
      ], b),
      isFalse,
    );
  });

  testWidgets('the chat menu offers Background all only while a task waits', (
    tester,
  ) async {
    SharedPreferences.setMockInitialValues({});
    ChatController.resetCaches();
    final prefs = await Prefs.load();
    final store = Store();
    final link = Link(clientInstance: 1);
    store.apply(
      msg(
        changes('Global', 10, [
          {'Session': sessionJson()},
        ]),
      ),
    );
    await tester.pumpWidget(
      Femme(
        store: store,
        link: link,
        prefs: prefs,
        child: CupertinoApp(
          theme: femmeTheme(Brightness.light),
          home: Builder(
            builder: (context) => CupertinoButton(
              onPressed: () => showChatMenu(context, sid),
              child: const Text('menu'),
            ),
          ),
        ),
      ),
    );
    Future<void> openMenu() async {
      await tester.tap(find.text('menu'));
      await tester.pumpAndSettle();
    }

    await openMenu();
    expect(find.text('Background all'), findsNothing);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();

    store.apply(
      msg(
        changes(
          {'Session': sid},
          60,
          [
            row(
              call(
                'toolu_1',
                'Bash',
                {'command': 'cargo test'},
                status: ItemStatus.inProgress,
                task: 'foreground',
              ),
            ),
          ],
        ),
      ),
    );
    await openMenu();
    expect(find.text('Background all'), findsOneWidget);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();

    await tester.pumpWidget(const SizedBox());
    link.close();
  });
}
