import 'package:femme_mobile/ui/terminal/extra_keys.dart';
import 'package:femme_mobile/ui/terminal/keys.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

/// The key under [label]: its rounded fill.
BoxDecoration keyFill(WidgetTester t, String label) =>
    t
            .widgetList<AnimatedContainer>(
              find.ancestor(
                of: find.text(label),
                matching: find.byType(AnimatedContainer),
              ),
            )
            .single
            .decoration!
        as BoxDecoration;

void main() {
  testWidgets('keys look like the keyboard\'s: a rounded fill, no border, '
      'a darker fill while pressed, no offset, 44 pt', (t) async {
    final sent = <ExtraKey>[];
    await t.pumpWidget(
      CupertinoApp(
        home: CupertinoPageScaffold(
          child: Align(
            alignment: Alignment.bottomCenter,
            child: ExtraKeysRow(mods: StickyMods(), onKey: sent.add),
          ),
        ),
      ),
    );
    final label = ExtraKey.esc.label;
    final up = keyFill(t, label);
    expect(up.border, isNull);
    expect(up.boxShadow, isNull);
    expect(up.borderRadius, const BorderRadius.all(Radius.circular(7)));
    expect(
      up.color,
      CupertinoColors.tertiarySystemFill.resolveFrom(
        t.element(find.text(label)),
      ),
    );
    final key = find.ancestor(
      of: find.text(label),
      matching: find.byType(AnimatedContainer),
    );
    expect(t.getSize(key).height, greaterThanOrEqualTo(44));
    expect(t.getSize(key).width, greaterThanOrEqualTo(44));
    final at = t.getTopLeft(key);

    final g = await t.startGesture(t.getCenter(key));
    await t.pumpAndSettle();
    expect(
      keyFill(t, label).color,
      CupertinoColors.systemFill.resolveFrom(t.element(find.text(label))),
    );
    expect(t.getTopLeft(key), at);
    expect(t.widget<AnimatedContainer>(key).transform, isNull);
    await g.up();
    await t.pumpAndSettle();
    expect(sent, [ExtraKey.esc]);
  });
}
