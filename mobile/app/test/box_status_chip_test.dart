import 'package:femme_mobile/ui/box/prs.dart';
import 'package:femme_mobile/ui/theme.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('status tags are capsules without a border: a green tint for '
      'in use, the grey fill for disabled', (t) async {
    await t.pumpWidget(
      const CupertinoApp(
        home: Column(
          children: [
            StatusChip('in use', FemmeColors.work),
            StatusChip('disabled', FemmeColors.done, faint: true),
          ],
        ),
      ),
    );
    BoxDecoration deco(String label) =>
        t
                .widget<Container>(
                  find.ancestor(
                    of: find.text(label),
                    matching: find.byType(Container),
                  ),
                )
                .decoration!
            as BoxDecoration;

    for (final label in ['in use', 'disabled']) {
      final d = deco(label);
      expect(d.border, isNull);
      expect(d.borderRadius, const BorderRadius.all(Radius.circular(999)));
      expect(t.widget<Text>(find.text(label)).style!.fontSize, 13);
    }
    expect(deco('in use').color, FemmeColors.work.withValues(alpha: 0.16));
    expect(
      deco('disabled').color,
      CupertinoColors.tertiarySystemFill.resolveFrom(
        t.element(find.text('disabled')),
      ),
    );
  });
}
