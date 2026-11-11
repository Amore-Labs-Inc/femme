import 'dart:async';

import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:femme_mobile/net/lifecycle.dart';
import 'package:femme_mobile/net/link.dart';
import 'package:flutter_test/flutter_test.dart';

class CountingLink extends Link {
  CountingLink() : super(clientInstance: 1);
  int reconnects = 0;
  @override
  void reconnect() => reconnects++;
}

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  test('a network change reconnects at once; the first report and losing all do not', () async {
    final net = StreamController<List<ConnectivityResult>>();
    final link = CountingLink();
    final life = LinkLifecycle(link, network: net.stream);
    Future<void> report(List<ConnectivityResult> r) async {
      net.add(r);
      await Future<void>.delayed(Duration.zero);
    }

    await report([ConnectivityResult.wifi]);
    expect(link.reconnects, 0, reason: 'where the phone is now');
    await report([ConnectivityResult.wifi]);
    expect(link.reconnects, 0, reason: 'no change');
    await report([ConnectivityResult.mobile]);
    expect(link.reconnects, 1, reason: 'Wi-Fi → cellular');
    await report([ConnectivityResult.none]);
    expect(link.reconnects, 1, reason: 'nothing to dial');
    await report([ConnectivityResult.wifi]);
    expect(link.reconnects, 2, reason: 'back from none');

    net.addError(Exception('plugin gone'));
    await Future<void>.delayed(Duration.zero);
    life.dispose();
    await net.close();
    await link.close();
  });

  test('no network stream on Linux and in tests', () {
    expect(LinkLifecycle.platformNetwork(), isNull);
  });
}
