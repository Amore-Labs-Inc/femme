import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/chat.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

S2C msg(Object json) => S2C.fromJson(jsonDecode(jsonEncode(json)) as Object);

void main() {
  testWidgets(
    'chat shows a user bubble, assistant markdown and a pending question',
    (tester) async {
      SharedPreferences.setMockInitialValues({});
      ChatController.resetCaches();
      final prefs = await Prefs.load();
      final store = Store();
      final link = Link(clientInstance: 1);
      store.apply(
        msg(
          changes('Global', 10, [
            {'Session': sessionJson(status: 'Blocked', attention: 'Blocked')},
          ]),
        ),
      );
      store.apply(
        msg(
          changes(
            {'Session': sid},
            60,
            [
              {
                'Item': itemJson(
                  id: 'u1',
                  seq: 1,
                  kind: 'UserMessage',
                  body: jsonEncode({
                    'op': '01JC9W6K7Q2R3S4T5V6W7X8Y9Z',
                    'read_at': 1794300002000,
                    'text': 'fold the lessons\n\nAttached: /home/u/notes.txt',
                  }),
                ),
              },
              {
                'Item': itemJson(
                  id: 'msg_1:0',
                  seq: 2,
                  body: jsonEncode({
                    'text': 'Here is the **plan**.\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nsee https://x.dev',
                  }),
                ),
              },
              {
                'Item': itemJson(
                  id: 'toolu_01ABC',
                  seq: 3,
                  kind: 'Question',
                  status: 'Pending',
                  body: questionBody,
                ),
              },
            ],
          ),
        ),
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
      await tester.pumpAndSettle();

      // header: title, model chip, permission chip
      expect(find.text('lovable-lesson'), findsWidgets);
      expect(find.text('opus 5.5 · high'), findsOneWidget);
      expect(find.text('accept edits'), findsOneWidget);

      // the question came up as a sheet as well as inline
      expect(find.text('Fold it into one lesson?'), findsNWidgets(2));
      expect(find.text('yes, fold it'), findsNWidgets(2));
      await tester.tapAt(const Offset(20, 120)); // outside the sheet
      await tester.pumpAndSettle();
      expect(find.text('Fold it into one lesson?'), findsOneWidget);

      final transcript = find.byWidgetPredicate(
        (w) => w is Scrollable && w.axisDirection == AxisDirection.up,
      );
      await tester.scrollUntilVisible(
        find.text('fold the lessons'),
        200,
        scrollable: transcript,
      );
      // user bubble without its Attached line, the file as a chip, read time
      expect(find.text('fold the lessons'), findsOneWidget);
      expect(find.textContaining('notes.txt'), findsOneWidget);
      expect(find.textContaining('read '), findsOneWidget);

      // assistant markdown
      expect(
        find.textContaining('Here is the', findRichText: true),
        findsOneWidget,
      );

      // composer placeholder while not connected
      expect(find.text('not connected'), findsOneWidget);

      await tester.pumpWidget(const SizedBox());
      link.close();
    },
  );

  testWidgets('a chat whose session closes says so and offers Close', (
    tester,
  ) async {
    SharedPreferences.setMockInitialValues({});
    ChatController.resetCaches();
    final prefs = await Prefs.load();
    final store = Store();
    final link = Link(clientInstance: 1);
    final nav = GlobalKey<NavigatorState>();
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
          navigatorKey: nav,
          home: const Text('home'),
        ),
      ),
    );
    openChat(nav.currentContext!, sid);
    await tester.pump();
    await tester.pump(const Duration(seconds: 1));
    await tester.pump(const Duration(seconds: 1));
    expect(find.byType(ChatScreen), findsOneWidget);
    expect(find.text('session closed'), findsNothing);
    expect(find.byType(CupertinoTextField), findsWidgets); // the composer

    store.apply(
      msg(
        changes('Global', 11, [
          {
            'Tombstone': {'entity': 'Session', 'id': sid, 'rev': 11},
          },
        ]),
      ),
    );
    await tester.pump(const Duration(seconds: 1));
    expect(find.text('session closed'), findsOneWidget);
    expect(find.text('loading…'), findsNothing);
    expect(find.text('lovable-lesson'), findsOneWidget); // title kept
    expect(find.byType(CupertinoTextField), findsNothing); // no composer

    await tester.tap(find.text('Close'));
    await tester.pump(const Duration(seconds: 1));
    expect(find.text('home'), findsOneWidget);
    await tester.pump(const Duration(seconds: 1));
    expect(store.isSubscribed(sid), isFalse);

    await tester.pumpWidget(const SizedBox());
    link.close();
  });
}
