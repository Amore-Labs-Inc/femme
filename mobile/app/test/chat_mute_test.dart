import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/push.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/chat/header.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

class _Keeper implements PushKeeper {
  @override
  String? pushTopic = 'femme-t';
  @override
  bool pushOn = true;
  @override
  String? pushRulesJson;
  @override
  Future<void> setPush({String? topic, bool? on, String? rulesJson}) async {
    if (topic != null) pushTopic = topic;
    if (on != null) pushOn = on;
    if (rulesJson != null) pushRulesJson = rulesJson;
  }
}

void main() {
  late List<Object> sent;
  late _Keeper keeper;
  late PushSync push;

  // `settle`: wait for the first registration (a plain test; a widget test's fake clock never
  // runs a `Future.delayed`, it pumps instead)
  Future<void> setUpPush({bool on = true, bool settle = true}) async {
    sent = [];
    keeper = _Keeper()..pushOn = on;
    final link = ValueNotifier(const LinkState(LinkPhase.online));
    push = PushSync(
      (r) async {
        sent.add(r);
        return const Resp('Ok', null);
      },
      link,
      keeper,
    );
    if (settle) await Future<void>.delayed(Duration.zero);
  }

  test(
    'muting one chat registers it in the rules and keeps the others',
    () async {
      await setUpPush();
      await push.setRules(const PushRules(done: false, quiet: (23, 7)));
      sent.clear();
      await push.setMuted('s1', true);
      expect(push.isMuted('s1'), isTrue);
      expect(push.isMuted('s2'), isFalse);
      final rules = (sent.single as Map)['PushRegister']['rules'] as Map;
      expect(rules['muted'], ['s1']);
      expect(rules['done'], false);
      expect(rules['quiet'], [23, 7]);
      // already muted: nothing sent
      await push.setMuted('s1', true);
      expect(sent, hasLength(1));
      await push.setMuted('s2', true);
      expect(push.rules.muted, {'s1', 's2'});
      await push.setMuted('s1', false);
      expect(push.rules.muted, {'s2'});
      expect(((sent.last as Map)['PushRegister']['rules'] as Map)['muted'], [
        's2',
      ]);
    },
  );

  Future<void> pumpMenu(WidgetTester tester) async {
    SharedPreferences.setMockInitialValues({});
    ChatController.resetCaches();
    final prefs = await Prefs.load();
    final store = Store();
    store.apply(
      S2C.fromJson(
        jsonDecode(
          jsonEncode(
            changes('Global', 10, [
              {'Session': sessionJson()},
            ]),
          ),
        ) as Object,
      ),
    );
    await tester.pumpWidget(
      Femme(
        store: store,
        link: Link(clientInstance: 1),
        prefs: prefs,
        push: push,
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
    await tester.tap(find.text('menu'));
    await tester.pumpAndSettle();
  }

  testWidgets('the chat menu mutes and unmutes the chat', (tester) async {
    await setUpPush(settle: false);
    await pumpMenu(tester);
    expect(find.text('Mute notifications'), findsOneWidget);
    await tester.tap(find.text('Mute notifications'));
    await tester.pumpAndSettle();
    expect(push.isMuted(sid), isTrue);
    expect(find.text('Notifications for this chat are off'), findsOneWidget);
    await tester.pump(const Duration(seconds: 5));
    await tester.pumpAndSettle();
    await tester.tap(find.text('menu'));
    await tester.pumpAndSettle();
    expect(find.text('Unmute notifications'), findsOneWidget);
  });

  testWidgets('with push off the chat menu has no mute line', (tester) async {
    await setUpPush(on: false, settle: false);
    await pumpMenu(tester);
    expect(find.text('Mute notifications'), findsNothing);
    expect(find.text('Unmute notifications'), findsNothing);
  });
}
