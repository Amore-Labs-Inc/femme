import 'package:femme_mobile/ui/chat/transcript.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

Widget _row(int ts) => Directionality(
  textDirection: TextDirection.ltr,
  child: FadeIn(ts: ts, child: const Text('row')),
);

void main() {
  testWidgets('a new row fades in and rises 4 px, once', (tester) async {
    await tester.pumpWidget(_row(DateTime.now().millisecondsSinceEpoch));
    Opacity op() => tester.widget<Opacity>(find.byType(Opacity));
    Transform tr() => tester.widget<Transform>(find.byType(Transform));
    expect(op().opacity, 0);
    expect(tr().transform.getTranslation().y, 4);
    await tester.pump(const Duration(milliseconds: 90));
    expect(op().opacity, inInclusiveRange(0.1, 0.99));
    await tester.pump(const Duration(milliseconds: 120));
    expect(op().opacity, 1);
    expect(tr().transform.getTranslation().y, 0);
  });

  testWidgets('history (older than a second) is drawn as it is', (
    tester,
  ) async {
    await tester.pumpWidget(_row(DateTime.now().millisecondsSinceEpoch - 5000));
    expect(find.byType(Opacity), findsNothing);
    expect(find.text('row'), findsOneWidget);
  });
}
