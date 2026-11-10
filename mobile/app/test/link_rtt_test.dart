import 'package:femme_mobile/net/link.dart';
import 'package:femme_mobile/ui/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

import 'link_test.dart' show FakeServer, until;

void main() {
  late FakeServer server;
  late Link link;

  setUp(() async {
    server = FakeServer();
    await server.start();
    link = Link(
      clientInstance: 4242,
      minBackoff: const Duration(milliseconds: 20),
      maxBackoff: const Duration(milliseconds: 100),
      rttInterval: const Duration(milliseconds: 30),
    );
  });

  tearDown(() async {
    await link.close();
    await server.stop();
  });

  test('ServerInfo is timed while online and kept across reconnects', () async {
    server.answer = (_, req) => req == 'ServerInfo'
        ? {
            'Ok': {
              'ServerInfo': {'version': '0.9.1', 'deployed': null},
            },
          }
        : null;
    link.connect('ws://127.0.0.1:${server.port}', 'tok');
    await until(() => link.isOnline);
    expect(link.state.value.rtt, isNull);
    expect(linkLine(link.state.value), 'online · femmed 0.9.1');

    await until(() => link.state.value.rtt != null);
    expect(server.reqs.every((r) => r.$3 == 'ServerInfo'), isTrue);
    expect(
      linkLine(link.state.value),
      matches(RegExp(r'^ws \d+ ms · femmed 0\.9\.1$')),
    );

    // Offline: no pings are sent, the last value stays.
    await server.sockets.first.close();
    await until(() => !link.isOnline);
    final rtt = link.state.value.rtt;
    expect(rtt, isNotNull);
    await until(() => link.isOnline);
    expect(link.state.value.rtt, rtt);
  });

  test('an unanswered ping does not take the link down', () async {
    link.connect('ws://127.0.0.1:${server.port}', 'tok');
    await until(() => link.isOnline);
    await until(() => server.reqs.isNotEmpty);
    await Future<void>.delayed(const Duration(milliseconds: 120));
    expect(link.isOnline, isTrue);
    expect(link.state.value.rtt, isNull);
    // One ping in flight at a time.
    expect(server.reqs.length, 1);
  });

  test('rttText rounds to whole milliseconds', () {
    expect(rttText(const Duration(microseconds: 41400)), '41 ms');
    expect(rttText(const Duration(microseconds: 600)), '1 ms');
  });
}
