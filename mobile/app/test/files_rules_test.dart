import 'package:femme_mobile/state/files.dart';
import 'package:femme_mobile/ui/files/hex.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('joinPath', () {
    test('joins under the root and under a folder', () {
      expect(joinPath('', 'src'), 'src');
      expect(joinPath('src', 'main.rs'), 'src/main.rs');
      expect(joinPath('a/b', '.gitignore'), 'a/b/.gitignore');
      expect(joinPath('', '...'), '...', reason: 'three dots is a name');
    });
    test('refuses names that climb, stay or jump', () {
      for (final bad in ['..', '.', '', '/', '/etc', 'a/b', 'a/', '../x']) {
        expect(joinPath('src', bad), isNull, reason: bad);
        expect(joinPath('', bad), isNull, reason: bad);
      }
    });
    test('parent, base name, full path', () {
      expect(parentPath('a/b/c'), 'a/b');
      expect(parentPath('a'), '');
      expect(parentPath(''), '');
      expect(baseName('a/b/c.txt'), 'c.txt');
      expect(fullPath('/home/u/p', ''), '/home/u/p');
      expect(fullPath('/home/u/p', 'src/x'), '/home/u/p/src/x');
      expect(fullPath('/home/u/p/', 'x'), '/home/u/p/x');
    });
  });

  test('humanSize', () {
    expect(humanSize(0), '0 B');
    expect(humanSize(1023), '1023 B');
    expect(humanSize(1024), '1.0 KiB');
    expect(humanSize(3 << 20), '3.0 MiB');
  });

  test('viewer kind by name', () {
    final cases = {
      'a.png': ViewerKind.image,
      'A.PNG': ViewerKind.image,
      'photo.JPEG': ViewerKind.image,
      'x.webp': ViewerKind.image,
      'logo.svg': ViewerKind.svg,
      'main.rs': ViewerKind.text,
      'README.md': ViewerKind.text,
      'data.json': ViewerKind.text,
      'Makefile': ViewerKind.text,
      'Dockerfile': ViewerKind.text,
      'LICENSE': ViewerKind.text,
      '.gitignore': ViewerKind.text,
      '.env': ViewerKind.text,
      'doc.pdf': ViewerKind.pdf,
      'clip.MOV': ViewerKind.video,
      'a.webm': ViewerKind.video,
      'song.mp3': ViewerKind.audio,
      'v.flac': ViewerKind.audio,
      'app.bin': ViewerKind.binary,
      'noext': ViewerKind.binary,
      '.DS_Store': ViewerKind.binary,
    };
    cases.forEach((name, kind) => expect(viewerKind(name), kind, reason: name));
  });

  test('sniff: utf-8 without NUL is text', () {
    expect(sniffText('hello\nwörld'.codeUnits), isFalse, reason: 'latin-1 ö');
    expect(sniffText([0x68, 0x69, 0x0a]), isTrue);
    expect(sniffText([0x68, 0x00, 0x69]), isFalse);
    expect(sniffText([0xff, 0xfe]), isFalse);
    // a 4 KiB window cutting a two-byte character in half is still text
    final big = [...List.filled(headBytes - 1, 0x61), 0xc3, 0xb6, 0x61];
    expect(sniffText(big), isTrue);
    expect(shownKind('notes', [0x68, 0x69]), ViewerKind.text);
    expect(shownKind('blob', [0, 1, 2]), ViewerKind.binary);
    expect(
      shownKind('big.txt', List.filled(textMax + 1, 0x61)),
      ViewerKind.binary,
    );
    expect(shownKind('ok.txt', List.filled(10, 0x61)), ViewerKind.text);
  });

  group('planOpen', () {
    test('boundaries', () {
      expect(maxRead, (16 << 20) - 4096);
      expect(planOpen(0), OpenPlan.read);
      expect(
        planOpen(askAboveWifi),
        OpenPlan.read,
        reason: '8 MiB is not over',
      );
      expect(planOpen(askAboveWifi + 1), OpenPlan.ask);
      expect(planOpen(maxRead), OpenPlan.ask);
      expect(planOpen(maxRead + 1), OpenPlan.refuse);
      expect(planOpen(askAboveWifi + 1, confirmed: true), OpenPlan.read);
      expect(planOpen(maxRead + 1, confirmed: true), OpenPlan.refuse);
    });
    test('cache and reads in flight come first', () {
      expect(planOpen(maxRead + 1, cached: true), OpenPlan.show);
      expect(planOpen(askAboveWifi + 1, reading: true), OpenPlan.wait);
    });
  });

  test('hex rows', () {
    final bytes = 'Hello, world!\n\x00\x01AB'.codeUnits;
    final lines = hexLines(bytes);
    expect(lines, hasLength(2));
    expect(
      lines[0],
      '00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 0a 00 01  |Hello, world!...|',
    );
    expect(lines[1].startsWith('00000010  41 42 '), isTrue);
    expect(lines[1].endsWith('|AB|'), isTrue);
    expect(hexLines(List.filled(10000, 0)), hasLength(headBytes ~/ 16));
  });
}
