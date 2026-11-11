import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/chat.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/follow.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

S2C msg(Object json) => S2C.fromJson(jsonDecode(jsonEncode(json)) as Object);

Map<String, dynamic> row({
  String id = sid,
  String status = 'Idle',
  String agent = 'Claude',
  String? label = 'lovable-lesson',
  int created = 1794300000000,
}) => {
  'Session': {
    ...sessionJson(id: id, status: status, label: label),
    'agent': agent,
    'created': created,
  },
};

Future<Store> pump(WidgetTester tester, Widget child, Store store) async {
  SharedPreferences.setMockInitialValues({});
  ChatController.resetCaches();
  final prefs = await Prefs.load();
  await tester.pumpWidget(
    Femme(
      store: store,
      link: Link(clientInstance: 1),
      prefs: prefs,
      child: CupertinoApp(theme: femmeTheme(Brightness.light), home: child),
    ),
  );
  await tester.pumpAndSettle();
  return store;
}

void main() {
  test('newestLive picks the newest live session by created, then id', () {
    final store = Store();
    store.apply(
      msg(
        changes('Global', 1, [
          row(id: sid, created: 5),
          row(id: sid2, label: 'other', created: 9),
          row(
            id: '01JD3Q8Z9X4K2M7N5P6R8S0T3X',
            label: 'gone',
            created: 99,
            status: 'Closed',
          ),
        ]),
      ),
    );
    expect(newestLive(store), sid2);
  });

  test('claudeFor: only a live shell on show follows a running Claude', () {
    final store = Store();
    store.apply(
      msg(
        changes('Global', 1, [
          row(id: sid, agent: 'Shell', label: 'sh'),
          row(id: sid2, label: 'c', created: 7),
        ]),
      ),
    );
    expect(claudeFor(store, sid, [sid2]), sid2);
    expect(claudeFor(store, sid, []), isNull);
    // a Claude chat on show is left alone
    expect(claudeFor(store, sid2, [sid]), isNull);
  });

  testWidgets('a chat whose session closes moves to the newest live one', (
    tester,
  ) async {
    final store = Store()
      ..apply(msg(changes('Global', 1, [row(id: sid, status: 'Working')])));
    await pump(
      tester,
      const Following(
        sid: sid,
        child: ChatScreen(sid: sid),
      ),
      store,
    );
    expect(find.text('lovable-lesson'), findsWidgets);

    store.apply(
      msg(
        changes('Global', 2, [
          row(id: sid, status: 'Closed'),
          row(id: sid2, label: 'other', created: 1794300009000),
        ]),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('other'), findsWidgets);
    expect(find.text('lovable-lesson closed'), findsOneWidget);
    await tester.pump(const Duration(seconds: 4)); // the toast goes
  });

  testWidgets('a chat opened on an already closed session stays', (
    tester,
  ) async {
    final store = Store()
      ..apply(
        msg(
          changes('Global', 1, [
            row(id: sid, status: 'Closed'),
            row(id: sid2, label: 'other'),
          ]),
        ),
      );
    await pump(
      tester,
      const Following(
        sid: sid,
        child: ChatScreen(sid: sid),
      ),
      store,
    );
    expect(find.text('session closed'), findsOneWidget);
    expect(find.text('other'), findsNothing);
  });

  testWidgets('a Claude session arriving over a live shell takes over', (
    tester,
  ) async {
    final store = Store()
      ..apply(msg(changes('Global', 1, [row(id: sid, agent: 'Shell')])));
    await pump(
      tester,
      const Following(
        sid: sid,
        child: Center(child: Text('the shell')),
      ),
      store,
    );
    expect(find.text('the shell'), findsOneWidget);

    store.apply(
      msg(
        changes('Global', 2, [
          row(id: sid2, label: 'typed claude', status: 'Working', created: 9),
        ]),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('the shell'), findsNothing);
    expect(find.text('typed claude'), findsWidgets);
  });
}
