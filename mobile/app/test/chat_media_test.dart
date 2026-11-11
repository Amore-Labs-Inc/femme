import 'dart:convert';

import 'package:femme_mobile/state/chat.dart';
import 'package:femme_mobile/ui/chat/media.dart';
import 'package:femme_mobile/ui/media/actions.dart';
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

  group('thumbnail menu', () {
    final png = base64Decode(
      'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
    );
    tearDown(() => canCopyImageOverride = null);

    testWidgets('long-press on a picture: Open, Share, Save, Copy', (
      tester,
    ) async {
      canCopyImageOverride = true;
      final box = FakeBox(files: {'/u/a.png': png});
      await tester.pumpWidget(host(ChatController('s1', box), ['/u/a.png']));
      await settle(tester);
      await tester.longPress(find.byType(GestureDetector).first);
      await tester.pumpAndSettle();
      for (final a in ['Open', 'Share', 'Save to Photos', 'Copy image']) {
        expect(find.text(a), findsOneWidget, reason: a);
      }
    });

    testWidgets('no Copy image where the platform cannot; svg not saved', (
      tester,
    ) async {
      canCopyImageOverride = false;
      final box = FakeBox(files: {'/u/a.svg': utf8.encode(goodSvg)});
      await tester.pumpWidget(host(ChatController('s1', box), ['/u/a.svg']));
      await settle(tester);
      await tester.longPress(find.byType(SvgPicture));
      await tester.pumpAndSettle();
      expect(find.text('Open'), findsOneWidget);
      expect(find.text('Share'), findsOneWidget);
      expect(find.text('Save to Photos'), findsNothing);
      expect(find.text('Copy image'), findsNothing);
    });

    testWidgets('Save to Photos without the plugin says so', (tester) async {
      final box = FakeBox(files: {'/u/a.png': png});
      await tester.pumpWidget(host(ChatController('s1', box), ['/u/a.png']));
      await settle(tester);
      await tester.longPress(find.byType(GestureDetector).first);
      await tester.pumpAndSettle();
      await tester.tap(find.text('Save to Photos'));
      await settle(tester);
      expect(
        find.text("Save to Photos isn't available on this device"),
        findsOneWidget,
      );
      await tester.pump(const Duration(seconds: 5));
    });
  });
}
