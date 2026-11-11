import 'package:femme_mobile/state/links.dart';
import 'package:flutter_test/flutter_test.dart';

import 'fixtures.dart';

DeepLink? p(String s) => parseDeepLink(Uri.parse(s));

void main() {
  test('femme://session/<sid> opens that session', () {
    expect(p('femme://session/$sid'), const OpenSession(sid));
    expect(p('femme://session/$sid/'), const OpenSession(sid));
  });

  test('a session link without a valid upper-case ULID is null', () {
    expect(p('femme://session/${sid.toLowerCase()}'), isNull);
    expect(p('femme://session/'), isNull);
    expect(p('femme://session/short'), isNull);
    expect(p('femme://session/${sid}X'), isNull);
    expect(p('femme://session/$sid/extra'), isNull);
    // I, L, O and U are not Crockford base32.
    expect(p('femme://session/01JD3Q8Z9X4K2M7N5P6R8S0TIL'), isNull);
  });

  test('femme://pair reuses the pairing parser', () {
    final l = p('femme://pair?host=box.example.com&tls=1&name=hq&token=abc');
    expect(l, isA<Pair>());
    final pair = l! as Pair;
    expect(pair.host, 'box.example.com');
    expect(pair.token, 'abc');
    expect(pair.tls, isTrue);
    expect(pair.name, 'hq');
    expect(pair.link.address, 'wss://box.example.com');

    final plain = p('femme://pair?host=10.0.0.2:8787&tls=0&token=t') as Pair;
    expect(plain.tls, isFalse);
    expect(plain.name, '10.0.0.2:8787');
  });

  test('junk is null', () {
    expect(p('femme://pair?host=box.example.com'), isNull); // no token
    expect(p('femme://nope/$sid'), isNull);
    expect(p('https://session/$sid'), isNull);
    expect(p('femme://'), isNull);
    expect(p('mailto:a@b.c'), isNull);
  });
}
