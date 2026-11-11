import 'dart:convert';
import 'dart:typed_data';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/ui/chat/attachments.dart';
import 'package:femme_mobile/ui/media/actions.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

class PutOk implements Requester {
  @override
  bool isOnline = true;
  @override
  Future<Resp> req(
    Object req, {
    String? op,
    Duration? timeout,
    bool resend = true,
  }) async => Resp('BlobHash', List.filled(32, 7));
}

/// A real 1x1 PNG.
final png = base64Decode(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
);

Future<DraftFiles> chips(WidgetTester tester) async {
  final d = DraftFiles(PutOk());
  await tester.pumpWidget(
    CupertinoApp(
      home: CupertinoPageScaffold(
        child: Align(alignment: Alignment.bottomCenter, child: FileChips(d)),
      ),
    ),
  );
  return d;
}

void main() {
  tearDown(() => canCopyImageOverride = null);

  test('size line: bytes, plus pixels for a picture', () {
    final pic = DraftFile('a.png', png);
    expect(sizeLine(pic), endsWith(' · 1x1'));
    final txt = DraftFile('a.txt', Uint8List.fromList(utf8.encode('hello')));
    expect(sizeLine(txt), isNot(contains('·')));
  });

  testWidgets('a picture chip shows its pixel size and opens a menu', (
    tester,
  ) async {
    canCopyImageOverride = true;
    final d = await chips(tester);
    d.add('a.png', png);
    await tester.pump();
    await tester.pump();
    expect(find.textContaining('1x1'), findsOneWidget);
    expect(find.byType(Image), findsOneWidget);

    await tester.tap(find.text('a.png'));
    await tester.pumpAndSettle();
    expect(find.text('Preview'), findsOneWidget);
    expect(find.text('Copy image'), findsOneWidget);
    await tester.tap(find.text('Remove'));
    await tester.pumpAndSettle();
    expect(d.isEmpty, isTrue);
  });

  testWidgets('a text chip: no Preview, no Copy image', (tester) async {
    canCopyImageOverride = true;
    final d = await chips(tester);
    d.add('notes.txt', Uint8List.fromList(utf8.encode('hi')));
    await tester.pump();
    await tester.pump();
    await tester.tap(find.text('notes.txt'));
    await tester.pumpAndSettle();
    expect(find.text('Preview'), findsNothing);
    expect(find.text('Copy image'), findsNothing);
    expect(find.text('Remove'), findsOneWidget);
  });

  testWidgets('Preview opens the lightbox', (tester) async {
    canCopyImageOverride = false;
    final d = await chips(tester);
    d.add('a.png', png);
    await tester.pump();
    await tester.pump();
    await tester.tap(find.text('a.png'));
    await tester.pumpAndSettle();
    expect(find.text('Copy image'), findsNothing);
    await tester.tap(find.text('Preview'));
    await tester.pumpAndSettle();
    expect(find.bySemanticsLabel('Share'), findsOneWidget);
  });
}
