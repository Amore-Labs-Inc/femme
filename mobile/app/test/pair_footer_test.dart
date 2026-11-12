import 'package:femme_mobile/ui/me/pairing.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  testWidgets('the first-run footers are 13 pt footnotes, not body text', (
    t,
  ) async {
    await t.pumpWidget(
      const CupertinoApp(
        home: CupertinoPageScaffold(
          child: SingleChildScrollView(child: PairForm()),
        ),
      ),
    );
    final footer = t.widget<Text>(find.textContaining('femmed pair --name'));
    expect(footer.style!.fontSize, 13);

    await t.enterText(find.byType(CupertinoTextField).first, 'nope');
    await t.testTextInput.receiveAction(TextInputAction.done);
    await t.pump();
    expect(
      t.widget<Text>(find.text('not a femme://pair link')).style!.fontSize,
      13,
    );
  });
}
