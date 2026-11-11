import 'dart:io';
import 'dart:typed_data';

import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:femme_mobile/ui/chat/attachments.dart';
import 'package:femme_mobile/ui/media/sniff.dart';
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

/// A 3x2 PNG header (enough for the sniffer).
final png = Uint8List.fromList([
  0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, //
  0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, //
  0, 0, 0, 3, 0, 0, 0, 2, 8, 6, 0, 0, 0,
]);

void main() {
  test('a pasted picture becomes paste-<n>.<ext>', () async {
    final d = DraftFiles(PutOk());
    final notes = await pasteInto(d, image: () async => png);
    expect(notes, isEmpty);
    await pasteInto(d, image: () async => png);
    expect(d.files.map((f) => f.name), ['paste-1.png', 'paste-2.png']);
  });

  test(
    'copied files are read from their paths; content URIs skipped',
    () async {
      final dir = await Directory.systemTemp.createTemp('femme-paste');
      final f = File('${dir.path}/notes.txt')..writeAsStringSync('hi');
      final d = DraftFiles(PutOk());
      final notes = await pasteInto(
        d,
        image: () async => null,
        files: () async => ['content://x/1', Uri.file(f.path).toString()],
      );
      expect(notes, isEmpty);
      expect(d.files.single.name, 'notes.txt');
      await dir.delete(recursive: true);
    },
  );

  test('an empty clipboard says so', () async {
    final d = DraftFiles(PutOk());
    final notes = await pasteInto(
      d,
      image: () async => null,
      files: () async => const [],
    );
    expect(notes, ['nothing to paste: copy a picture or a file first']);
    expect(d.isEmpty, isTrue);
  });

  test('sniff: formats and pixel sizes from headers', () {
    expect(pictureFormat(png), 'png');
    expect(pixelSize(png), (w: 3, h: 2));
    final gif = Uint8List.fromList([...'GIF89a'.codeUnits, 64, 0, 32, 0]);
    expect(pixelSize(gif), (w: 64, h: 32));
    // SOI, an APP0 of length 4, then SOF0 with height 32 and width 64
    final jpg = Uint8List.fromList([
      0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, //
      0xFF, 0xC0, 0, 11, 8, 0, 32, 0, 64, 3,
    ]);
    expect(pictureFormat(jpg), 'jpg');
    expect(pixelSize(jpg), (w: 64, h: 32));
    final svg = Uint8List.fromList(
      '<?xml version="1.0"?>\n<!-- x --><svg width="1"/>'.codeUnits,
    );
    expect(pictureFormat(svg), 'svg');
    expect(pixelSize(svg), isNull);
    expect(pictureFormat(Uint8List.fromList('<html><svg>'.codeUnits)), isNull);
    expect(pictureFormat(Uint8List.fromList('hello'.codeUnits)), isNull);
  });
}
