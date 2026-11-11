import 'package:femme_mobile/ui/theme.dart';
import 'package:femme_mobile/ui/widgets.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

Widget _app(Widget child, {Brightness b = Brightness.light}) =>
    CupertinoApp(theme: femmeTheme(b), home: child);

void main() {
  testWidgets('the status dot is a borderless-looking circle VoiceOver names', (
    t,
  ) async {
    final h = t.ensureSemantics();
    await t.pumpWidget(
      _app(
        const Center(
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              StatusDot(FemmeColors.queue),
              StatusDot(FemmeColors.work, label: 'online'),
            ],
          ),
        ),
      ),
    );
    expect(find.bySemanticsLabel('idle'), findsOneWidget);
    expect(find.bySemanticsLabel('online'), findsOneWidget);
    final box = t.widget<Container>(
      find.descendant(
        of: find.byType(StatusDot).first,
        matching: find.byType(Container),
      ),
    );
    final d = box.decoration! as BoxDecoration;
    expect(d.shape, BoxShape.circle);
    expect(d.color, FemmeColors.queue);
    expect(d.border!.top.width, lessThanOrEqualTo(.5));
    h.dispose();
  });

  test('lane words', () {
    expect(laneWord(FemmeColors.work), 'working');
    expect(laneWord(FemmeColors.pause), 'needs you');
    expect(laneWord(FemmeColors.bad), 'failed');
    expect(laneWord(FemmeColors.done), 'closed');
  });

  testWidgets('a section footer is footnote: 13 pt, secondary label', (
    t,
  ) async {
    await t.pumpWidget(
      _app(
        CupertinoPageScaffold(
          child: Builder(
            builder: (c) => CupertinoListSection.insetGrouped(
              footer: sectionFooter(c, 'checked just now'),
              children: const [CupertinoListTile(title: Text('row'))],
            ),
          ),
        ),
      ),
    );
    final text = t.widget<Text>(find.text('checked just now'));
    expect(text.style!.fontSize, 13);
    expect(
      text.style!.color,
      isSameColorAs(CupertinoColors.secondaryLabel.color),
    );
  });

  for (final b in Brightness.values) {
    testWidgets('a tab page is grouped grey all over ($b)', (t) async {
      await t.pumpWidget(
        _app(
          const TabPage(title: 'Box', status: 'online', slivers: []),
          b: b,
        ),
      );
      final want = b == Brightness.light
          ? CupertinoColors.systemGroupedBackground.color
          : CupertinoColors.systemGroupedBackground.darkColor;
      final scaffold = t.widget<CupertinoPageScaffold>(
        find.byType(CupertinoPageScaffold),
      );
      expect(scaffold.backgroundColor, isSameColorAs(want));
      final bar = t.widget<CupertinoSliverNavigationBar>(
        find.byType(CupertinoSliverNavigationBar),
      );
      expect(bar.backgroundColor, isSameColorAs(want));
    });
  }
}
