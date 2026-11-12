// The chat screen's iOS look: a rounded bubble without border or shadow, capsule chips, a
// capsule composer field, rounded code blocks, a soft toast, labelled icon buttons, and the
// session info as an iOS sheet.
import 'dart:convert';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/chat.dart';
import 'package:femme_mobile/ui/chat/look.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

S2C msg(Object json) => S2C.fromJson(jsonDecode(jsonEncode(json)) as Object);

BoxDecoration decorationOf(WidgetTester t, Finder f) =>
    t.widget<Container>(f).decoration! as BoxDecoration;

/// The nearest Container above [f] that has a decoration.
Finder decorated(Finder f) => find
    .ancestor(
      of: f,
      matching: find.byWidgetPredicate(
        (w) => w is Container && w.decoration is BoxDecoration,
      ),
    )
    .first;

void main() {
  late Link link;

  Future<void> pumpChat(WidgetTester tester) async {
    SharedPreferences.setMockInitialValues({});
    ChatController.resetCaches();
    final prefs = await Prefs.load();
    final store = Store();
    link = Link(clientInstance: 1);
    store.apply(
      msg(
        changes('Global', 10, [
          {'Session': sessionJson(status: 'Idle')},
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
                body: jsonEncode({'text': 'hello there'}),
              ),
            },
            {
              'Item': itemJson(
                id: 'msg_1:0',
                seq: 2,
                body: jsonEncode({
                  'text': 'Sure.\n\n```rust\nfn main() {}\n```',
                }),
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
  }

  Future<void> done(WidgetTester tester) async {
    await tester.pumpWidget(const SizedBox());
    link.close();
  }

  testWidgets('your bubble is rounded, with no border and no shadow', (
    tester,
  ) async {
    await pumpChat(tester);
    final d = decorationOf(tester, find.byKey(const ValueKey('user-bubble')));
    expect(d.border, isNull);
    expect(d.boxShadow, isNull);
    expect(d.color, FemmeColors.accent);
    final r = d.borderRadius! as BorderRadius;
    expect(r.topLeft, const Radius.circular(bubbleRadius));
    await done(tester);
  });

  testWidgets('chips are capsules without border; the field is a capsule', (
    tester,
  ) async {
    await pumpChat(tester);
    for (final label in ['accept edits', 'opus 5.5 · high']) {
      final d = decorationOf(tester, decorated(find.text(label)));
      expect(d.border, isNull, reason: label);
      expect(d.borderRadius, BorderRadius.circular(999), reason: label);
    }
    final field = tester.widget<CupertinoTextField>(
      find.byType(CupertinoTextField),
    );
    final fd = field.decoration!;
    expect(fd.borderRadius, BorderRadius.circular(composerRadius));
    expect(fd.border!.top.width, lessThan(1.2));
    await done(tester);
  });

  testWidgets('code blocks are rounded, borderless', (tester) async {
    await pumpChat(tester);
    final d = decorationOf(tester, decorated(find.text('fn main() {}')));
    expect(d.border, isNull);
    expect(d.borderRadius, BorderRadius.circular(codeRadius));
    await done(tester);
  });

  testWidgets('icon buttons carry labels', (tester) async {
    final semantics = tester.ensureSemantics();
    await pumpChat(tester);
    for (final l in ['more actions', 'attach', 'earlier prompts', 'send']) {
      expect(find.bySemanticsLabel(l), findsOneWidget, reason: l);
    }
    semantics.dispose();
    await done(tester);
  });

  testWidgets('the session info opens as an iOS sheet', (tester) async {
    await pumpChat(tester);
    await tester.tap(find.byIcon(CupertinoIcons.info_circle));
    await tester.pumpAndSettle();
    final title = find.text('Session');
    expect(title, findsOneWidget);
    expect(
      ModalRoute.of(tester.element(title)),
      isA<CupertinoSheetRoute<void>>(),
    );
    expect(find.byType(CupertinoListSection), findsWidgets);
    await tester.tap(find.text('Done'));
    await tester.pumpAndSettle();
    expect(find.text('Session'), findsNothing);
    await done(tester);
  });

  testWidgets('the toast is a rounded pill with a soft shadow', (tester) async {
    await pumpChat(tester);
    toast(tester.element(find.byType(ChatScreen)), 'copied');
    await tester.pump();
    final d = decorationOf(tester, find.byKey(const ValueKey('toast')));
    expect(d.borderRadius, BorderRadius.circular(cardRadius));
    expect(d.boxShadow!.every((s) => s.blurRadius > 0), isTrue);
    await tester.pump(const Duration(seconds: 4));
    await done(tester);
  });
}
