import 'dart:async';
import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/state/links.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/app.dart';
import 'package:femme_mobile/ui/chat.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:package_info_plus/package_info_plus.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

S2C msg(Object json) => S2C.fromJson(jsonDecode(jsonEncode(json)) as Object);

/// A session the store has never seen.
const unseen = '01JD3Q8Z9X4K2M7N5P6R8S0T9Q';

/// A link that never dials: the app's connect and pause calls only record where it would go.
class QuietLink extends Link {
  QuietLink() : super(clientInstance: 1);
  String? host;

  @override
  void connect(String host, String token) => this.host = host;
  @override
  void pause() => host = null;
}

Finder chatOf(String s) =>
    find.byWidgetPredicate((w) => w is ChatScreen && w.sid == s);

void mocks([Map<String, Object> prefs = const {}]) {
  SharedPreferences.setMockInitialValues(prefs);
  ChatController.resetCaches();
  PackageInfo.setMockInitialValues(
    appName: 'FEMME',
    packageName: 'x',
    version: '1.0.0',
    buildNumber: '1',
    buildSignature: '',
  );
}

Future<void> settle(WidgetTester t) async {
  for (var i = 0; i < 6; i++) {
    await t.pump(const Duration(milliseconds: 200));
  }
}

void main() {
  testWidgets('an OpenSession link opens that chat on the Now tab', (t) async {
    mocks();
    final prefs = await Prefs.load();
    final store = Store()
      ..apply(
        msg(
          changes('Global', 10, [
            {'Session': sessionJson()},
          ]),
        ),
      );
    final open = ValueNotifier<String?>(null);
    await t.pumpWidget(
      Femme(
        store: store,
        link: Link(clientInstance: 1),
        prefs: prefs,
        child: CupertinoApp(home: Shell(open: open)),
      ),
    );
    // Start on another tab: the link must bring Now back.
    await t.tap(find.text('Chats').last);
    await settle(t);
    final tabs = t.widget<CupertinoTabScaffold>(
      find.byType(CupertinoTabScaffold),
    );

    open.value = sid;
    await settle(t);
    expect(tabs.controller!.index, 0);
    expect(chatOf(sid), findsOneWidget);
    expect(open.value, isNull);

    // A session the store does not know yet still opens; it replaces the first chat.
    open.value = unseen;
    await settle(t);
    expect(chatOf(unseen), findsOneWidget);
    expect(chatOf(sid), findsNothing);

    await t.pumpWidget(const SizedBox());
  });

  testWidgets('a session link fed to the app before its first frame opens', (
    t,
  ) async {
    mocks({'host': 'ws://127.0.0.1:9', 'token': 'tok'});
    final prefs = await Prefs.load();
    final links = StreamController<DeepLink>()..add(const OpenSession(sid));
    await t.pumpWidget(
      FemmeApp(prefs: prefs, links: links.stream, link: QuietLink()),
    );
    await settle(t);
    expect(find.byType(Shell), findsOneWidget);
    expect(chatOf(sid), findsOneWidget);
    await t.pumpWidget(const SizedBox());
    unawaited(links.close());
  });

  testWidgets('a pair link pairs an unpaired app', (t) async {
    mocks();
    final prefs = await Prefs.load();
    final links = StreamController<DeepLink>();
    await t.pumpWidget(
      FemmeApp(prefs: prefs, links: links.stream, link: QuietLink()),
    );
    expect(find.text('Pair with your box'), findsOneWidget);

    links.add(
      parseDeepLink(
        Uri.parse('femme://pair?host=box.example.com&tls=1&name=hq&token=abc'),
      )!,
    );
    await settle(t);
    expect(prefs.host, 'wss://box.example.com');
    expect(prefs.token, 'abc');
    expect(find.byType(Shell), findsOneWidget);
    await t.pumpWidget(const SizedBox());
    unawaited(links.close());
  });

  testWidgets('a pair link for another box asks before replacing it', (
    t,
  ) async {
    mocks({'host': 'ws://127.0.0.1:9', 'token': 'old'});
    final prefs = await Prefs.load();
    final links = StreamController<DeepLink>();
    await t.pumpWidget(
      FemmeApp(prefs: prefs, links: links.stream, link: QuietLink()),
    );
    final other = parseDeepLink(
      Uri.parse('femme://pair?host=box.example.com&name=hq&token=new'),
    )!;

    links.add(other);
    await settle(t);
    expect(find.text('Pair with hq?'), findsOneWidget);
    expect(find.text('This replaces the current box.'), findsOneWidget);
    await t.tap(find.text('Cancel'));
    await settle(t);
    expect(prefs.token, 'old');

    links.add(other);
    await settle(t);
    await t.tap(find.text('Pair'));
    await settle(t);
    expect(prefs.host, 'wss://box.example.com');
    expect(prefs.token, 'new');
    await t.pumpWidget(const SizedBox());
    unawaited(links.close());
  });
}
