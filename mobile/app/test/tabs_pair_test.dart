import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/state/pairing.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('parses the link femmed pair prints', () {
    final l = PairLink.parse(
      'femme://pair?host=box.example.com&tls=1&name=cem-box&token=abc123',
    )!;
    expect(l.host, 'box.example.com');
    expect(l.tls, isTrue);
    expect(l.name, 'cem-box');
    expect(l.token, 'abc123');
    expect(l.address, 'wss://box.example.com');
    expect(linkUrl(l.address).toString(), 'wss://box.example.com/femme/ws');
  });

  test('tls=0 is plain ws, with a port; name defaults to the host', () {
    final l = PairLink.parse(
      '  femme://pair?host=127.0.0.1:8787&tls=0&token=t%2Bx  ',
    )!;
    expect(l.address, 'ws://127.0.0.1:8787');
    expect(l.name, '127.0.0.1:8787');
    expect(l.token, 't+x');
    expect(linkUrl(l.address).toString(), 'ws://127.0.0.1:8787/femme/ws');
  });

  test('missing tls means TLS', () {
    expect(PairLink.parse('femme://pair?host=h&token=t')!.tls, isTrue);
  });

  test('rejects what is not a pairing link', () {
    expect(PairLink.parse(''), isNull);
    expect(PairLink.parse('https://box.example.com'), isNull);
    expect(PairLink.parse('femme://open?host=h&token=t'), isNull);
    expect(PairLink.parse('femme://pair?host=h'), isNull);
    expect(PairLink.parse('femme://pair?token=t'), isNull);
    expect(PairLink.parse('femme://pair?host=&token=t'), isNull);
  });
}
