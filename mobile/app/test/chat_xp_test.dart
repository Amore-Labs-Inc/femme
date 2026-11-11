import 'package:femme_mobile/net/models.dart' hide Row;
import 'package:femme_mobile/ui/chat/strips.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

Widget _bar(Context c) => CupertinoApp(
  theme: femmeTheme(Brightness.light),
  home: CupertinoPageScaffold(child: Center(child: XpBar(c))),
);

void main() {
  test("the tap card carries the desktop's numbers", () {
    expect(xpRows(const Context(used: 10328, window: 12900, compactions: 2)), [
      ('current context', '10,328 / 12,900'),
      ('level progress', '80.1%'),
      ('room left', '2,572'),
      ('level', '3 (2 compactions)'),
    ]);
    final unknown = xpRows(const Context(used: 4300, window: 0));
    expect(unknown.map((r) => r.$2), [
      '4,300 / ?',
      '?',
      '?',
      '1 (never compacted)',
    ]);
    expect(
      xpRows(const Context(used: 1, window: 2, compactions: 1))[3].$2,
      '2 (1 compaction)',
    );
  });

  testWidgets('five segments: four notches', (tester) async {
    await tester.pumpWidget(_bar(const Context(used: 50, window: 100)));
    for (var i = 1; i < 5; i++) {
      expect(find.byKey(ValueKey('xp-notch-$i')), findsOneWidget);
    }
    expect(find.byKey(const ValueKey('xp-notch-5')), findsNothing);
  });

  testWidgets(
    'a compaction seen on screen rings once; opening at a level does not',
    (tester) async {
      final ring = find.byKey(const ValueKey('xp-ring'));
      await tester.pumpWidget(
        _bar(const Context(used: 90, window: 100, compactions: 2)),
      );
      expect(ring, findsNothing);
      await tester.pumpWidget(
        _bar(const Context(used: 5, window: 100, compactions: 3)),
      );
      await tester.pump(const Duration(milliseconds: 100));
      expect(ring, findsOneWidget);
      expect(find.text('L4'), findsOneWidget);
      await tester.pump(const Duration(milliseconds: 700));
      expect(ring, findsNothing);
      // more context, same level: no ring
      await tester.pumpWidget(
        _bar(const Context(used: 50, window: 100, compactions: 3)),
      );
      await tester.pump(const Duration(milliseconds: 100));
      expect(ring, findsNothing);
    },
  );
}
