import 'package:femme_mobile/net/models.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import 'chat_items.dart';
import 'chat_strip_test.dart' show pumpChat;

void main() {
  testWidgets('tapping a read mark scrolls the transcript to its message', (
    tester,
  ) async {
    final (_, link) = await pumpChat(tester, [
      it('u1', ItemKind.userMessage, body: {'text': 'the queued question'}),
      for (var n = 0; n < 40; n++)
        it(
          'a$n',
          ItemKind.assistantMessage,
          body: {'text': 'answer number $n\n\nwith a second paragraph'},
        ),
      it(
        'ev',
        ItemKind.event,
        body: {'text': 'read the queued question', 'read': 'u1'},
      ),
    ]);
    final target = find.text('the queued question');
    expect(target, findsNothing);
    await tester.tap(find.text('↳ read the queued question'));
    await tester.pumpAndSettle();
    expect(target, findsOneWidget);
    // it ends up on screen, not just built off its edge
    final y = tester.getCenter(target).dy;
    expect(y, greaterThan(0));
    expect(
      y,
      lessThan(tester.view.physicalSize.height / tester.view.devicePixelRatio),
    );

    await tester.pumpWidget(const SizedBox());
    link.close();
  });
}
