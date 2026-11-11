import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/app.dart';
import 'package:femme_mobile/ui/chats.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/now.dart';
import 'package:femme_mobile/ui/widgets.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:package_info_plus/package_info_plus.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

S2C msg(Object json) => S2C.fromJson(jsonDecode(jsonEncode(json)) as Object);

const s3 = '01JD3Q8Z9X4K2M7N5P6R8S0T3X';
const s4 = '01JD3Q8Z9X4K2M7N5P6R8S0T4Y';

Store store() => Store()
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
      ]),
    ),
  );

Future<Widget> app(Store st, Widget home) async {
  SharedPreferences.setMockInitialValues({});
  PackageInfo.setMockInitialValues(
    appName: 'FEMME',
    packageName: 'x',
    version: '1.0.0',
    buildNumber: '1',
    buildSignature: '',
  );
  final prefs = await Prefs.load();
  return Femme(
    store: st,
    link: Link(clientInstance: 1),
    prefs: prefs,
    child: CupertinoApp(home: home),
  );
}

Future<void> settle(WidgetTester t) async {
  for (var i = 0; i < 8; i++) {
    await t.pump(const Duration(milliseconds: 100));
  }
}

void main() {
  testWidgets('a Now row greys while pressed, then lets go', (t) async {
    await t.pumpWidget(await app(store(), const NowTab()));
    // The row and, while held, the copy the long-press menu lifts over it.
    Iterable<Color> bgs() => t
        .widgetList<AnimatedContainer>(
          find.descendant(
            of: find.byKey(const ValueKey('now-row-$s4'), skipOffstage: false),
            matching: find.byKey(const ValueKey('press-bg')),
            skipOffstage: false,
          ),
        )
        .map((c) => Color((c.decoration! as BoxDecoration).color!.toARGB32()));
    Color bg() => bgs().last;
    expect(bg().a, 0);
    final g = await t.startGesture(t.getCenter(find.text('quiet')));
    await t.pump(const Duration(milliseconds: 150));
    expect(
      bgs(),
      everyElement(Color(CupertinoColors.systemFill.color.toARGB32())),
    );
    await g.cancel();
    await settle(t);
    expect(bg().a, 0);
    await t.pumpWidget(const SizedBox());
  });

  testWidgets('a session changing section fades into its new one', (t) async {
    final st = store();
    await t.pumpWidget(await app(st, const NowTab()));
    await t.pump();
    double opacity() => t
        .widget<FadeTransition>(
          find
              .ancestor(
                of: find.text('busy'),
                matching: find.byType(FadeTransition),
              )
              .first,
        )
        .opacity
        .value;
    expect(opacity(), 1); // there at launch: no fade
    st.apply(
      msg(
        changes('Global', 21, [
          {
            'Session': sessionJson(
              id: s3,
              rev: 16,
              status: 'Idle',
              attention: 'Done',
              label: 'busy',
            ),
          },
        ]),
      ),
    );
    await t.pump();
    await t.pump(const Duration(milliseconds: 40));
    expect(opacity(), inExclusiveRange(0, 1));
    await settle(t);
    expect(opacity(), 1);
    expect(find.text('OK'), findsOneWidget); // the Done row's button, no emoji
    expect(t.getSize(find.widgetWithText(CupertinoButton, 'OK')).height, 44);
    await t.pumpWidget(const SizedBox());
  });

  testWidgets('re-tapping the active tab pops to its root, then scrolls up', (
    t,
  ) async {
    t.view.physicalSize = const Size(800, 900);
    t.view.devicePixelRatio = 2;
    addTearDown(t.view.reset);
    await t.pumpWidget(await app(store(), const Shell()));
    await t.pump();
    final nav = t.state<NavigatorState>(
      find
          .ancestor(of: find.text('quiet'), matching: find.byType(Navigator))
          .first,
    );
    nav.push(
      CupertinoPageRoute<void>(
        builder: (_) => const CupertinoPageScaffold(child: Text('pushed')),
      ),
    );
    await settle(t);
    expect(find.text('pushed'), findsOneWidget);
    await t.tap(find.text('Now').last);
    await settle(t);
    expect(find.text('pushed'), findsNothing);
    expect(find.text('quiet'), findsOneWidget);

    final list = find
        .ancestor(of: find.text('quiet'), matching: find.byType(Scrollable))
        .first;
    ScrollPosition pos() => t.state<ScrollableState>(list).position;
    pos().jumpTo(pos().maxScrollExtent);
    await t.pump();
    expect(pos().pixels, greaterThan(0));
    await t.tap(find.text('Now').last);
    await settle(t);
    expect(pos().pixels, 0);
    await t.pumpWidget(const SizedBox());
  });

  testWidgets('Chats says each status in words; + is labelled, trailing', (
    t,
  ) async {
    final handle = t.ensureSemantics();
    await t.pumpWidget(await app(store(), const ChatsTab()));
    await t.pump();
    expect(find.textContaining('needs you · claude · '), findsOneWidget);
    expect(find.textContaining('working · claude · '), findsOneWidget);
    expect(find.textContaining('idle · claude · '), findsOneWidget);
    final add = find.bySemanticsLabel('New chat');
    expect(add, findsOneWidget);
    expect(t.getCenter(add).dx, greaterThan(400)); // the trailing side
    expect(find.bySemanticsLabel(RegExp('^New chat in ')), findsOneWidget);
    await t.pumpWidget(const SizedBox());
    handle.dispose();
  });

  testWidgets('Now labels its +', (t) async {
    final handle = t.ensureSemantics();
    await t.pumpWidget(await app(store(), const NowTab()));
    expect(find.bySemanticsLabel('New chat'), findsOneWidget);
    await t.pumpWidget(const SizedBox());
    handle.dispose();
  });

  test('status word follows the lanes', () {
    Session s(String status, [String attention = 'None']) =>
        Session.fromJson(sessionJson(status: status, attention: attention));
    expect(statusWord(s('Blocked')), 'needs you');
    expect(statusWord(s('Idle', 'Done')), 'done');
    expect(statusWord(s('Starting')), 'working');
    expect(statusWord(s('Idle')), 'idle');
    expect(statusWord(s('Failed')), 'failed');
  });
}
