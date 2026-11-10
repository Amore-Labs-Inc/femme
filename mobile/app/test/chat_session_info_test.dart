import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/models.dart' hide Row;
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/session_info.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

import 'fixtures.dart';

void main() {
  testWidgets('session sheet shows the session and follows the store', (
    tester,
  ) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await Prefs.load();
    final store = Store();
    final link = Link(clientInstance: 1);
    store.sessions[sid] = Session.fromJson(sessionJson());
    await tester.pumpWidget(
      Femme(
        store: store,
        link: link,
        prefs: prefs,
        child: CupertinoApp(
          theme: femmeTheme(Brightness.light),
          home: Builder(
            builder: (c) => CupertinoButton(
              onPressed: () => showSessionInfo(c, sid),
              child: const Text('open'),
            ),
          ),
        ),
      ),
    );
    await tester.tap(find.text('open'));
    await tester.pumpAndSettle();
    expect(find.text('Session'), findsOneWidget);
    expect(find.text('lovable-lesson'), findsWidgets);
    expect(find.text('opus 5.5'), findsOneWidget);
    expect(find.text('accept edits'), findsOneWidget);
    await tester.scrollUntilVisible(find.text('not met yet'), 200);
    expect(find.text('not met yet'), findsOneWidget);
    await tester.scrollUntilVisible(find.text('81,234 / 200,000 · 41%'), 200);
    expect(find.text('81,234 / 200,000 · 41%'), findsOneWidget);
    await tester.scrollUntilVisible(find.text('Close chat'), 200);
    expect(find.text('Close chat'), findsOneWidget);

    store.sessions.remove(sid);
    store.notifyListeners();
    await tester.pump();
    expect(find.text('this chat is gone'), findsOneWidget);

    await tester.tap(find.text('Done'));
    await tester.pumpAndSettle();
    expect(find.text('Session'), findsNothing);
    link.close();
  });
}
