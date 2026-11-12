import 'package:femme_mobile/ui/chat/attached.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('Attached lines leave the text, paths kept in order', () {
    final r = splitAttached(
      'fix the build\n\nAttached: /u/a/err.png\nAttached: /u/a/log.txt\n',
    );
    expect(r.text, 'fix the build');
    expect(r.paths, ['/u/a/err.png', '/u/a/log.txt']);
  });

  test('anywhere in the text; a bare Attached: stays', () {
    final r = splitAttached(
      'Attached: /x/a.mp4\nhello\nAttached: \nAttached:/no-space',
    );
    expect(r.paths, ['/x/a.mp4']);
    expect(r.text, 'hello\nAttached: \nAttached:/no-space');
  });

  test('only lines', () {
    final r = splitAttached('Attached: /a/b.JPG');
    expect(r.text, '');
    expect(r.paths, ['/a/b.JPG']);
  });

  test('kinds by extension', () {
    expect(isPicture('/a/b.JPG'), isTrue);
    expect(isPicture('/a/b.webp'), isTrue);
    expect(isPicture('/a/b.mp4'), isFalse);
    expect(isVideo('/a/b.mov'), isTrue);
    expect(isVideo('/a/b.txt'), isFalse);
    expect(fileName('/a/b/c.png'), 'c.png');
  });
}
