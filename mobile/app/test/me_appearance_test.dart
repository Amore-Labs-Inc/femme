import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/state/store.dart';
import 'package:femme_mobile/ui/femme.dart';
import 'package:femme_mobile/ui/me.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

void main() {
  testWidgets('the appearance picker fills its row', (t) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await Prefs.load();
    await t.pumpWidget(
      Femme(
        store: Store(),
        link: Link(clientInstance: 1),
        prefs: prefs,
        child: const CupertinoApp(home: MeTab()),
      ),
    );
    await t.pump();
    final picker = find.byKey(const ValueKey('appearance'));
    final screen = t.getSize(find.byType(MeTab)).width;
    // the inset card (20 a side) less the row's own 16 a side
    expect(t.getSize(picker).width, closeTo(screen - 2 * 20 - 2 * 16, 1));
  });
}
