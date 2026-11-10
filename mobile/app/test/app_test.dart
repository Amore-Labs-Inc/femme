import 'package:femme_mobile/state/prefs.dart';
import 'package:femme_mobile/ui/app.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:shared_preferences/shared_preferences.dart';

void main() {
  testWidgets('first run shows the pair screen and keeps a client instance', (
    tester,
  ) async {
    SharedPreferences.setMockInitialValues({});
    final prefs = await Prefs.load();
    expect(prefs.paired, isFalse);
    expect(prefs.clientInstance, greaterThan(0));
    await tester.pumpWidget(FemmeApp(prefs: prefs));
    expect(find.text('Pair with your box'), findsOneWidget);
    expect(find.text('Connect'), findsOneWidget);
  });
}
