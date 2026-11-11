import 'dart:convert';

import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/ui/chat/media.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:flutter_test/flutter_test.dart';

import 'files_fake.dart';

const goodSvg =
    '<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">'
    '<rect width="10" height="10" fill="red"/></svg>';

Widget host(ChatController c, List<String> paths) => CupertinoApp(
  home: CupertinoPageScaffold(
    child: ChatScope(
      controller: c,
      child: AttachedFiles(paths: paths),
    ),
  ),
);

/// Lets the box answer and flutter_svg parse off the fake clock.
Future<void> settle(WidgetTester tester) async {
  for (var i = 0; i < 5; i++) {
    await tester.runAsync(
      () => Future.delayed(const Duration(milliseconds: 50)),
    );
    await tester.pump();
  }
}

void main() {
  setUp(ChatController.resetCaches);

  testWidgets('an svg attachment draws as a picture', (tester) async {
    final box = FakeBox(files: {'/u/logo.svg': utf8.encode(goodSvg)});
    final c = ChatController('s1', box);
    await tester.pumpWidget(host(c, ['/u/logo.svg']));
    await settle(tester);
    expect(find.byType(SvgPicture), findsOneWidget);
    expect(find.text('📎 logo.svg'), findsNothing);
  });

  testWidgets('an svg that does not parse falls back to the file chip', (
    tester,
  ) async {
    final box = FakeBox(files: {'/u/bad.svg': utf8.encode('<svg <<< nope')});
    final c = ChatController('s1', box);
    await tester.pumpWidget(host(c, ['/u/bad.svg']));
    await settle(tester);
    expect(find.text('📎 bad.svg'), findsOneWidget);
    expect(find.byType(SvgPicture), findsNothing);
  });
}
