import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:femme_mobile/state/files.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  tearDown(() => netProbe = () => probeNet());

  test('thresholds: the desktop 8 MiB on Wi-Fi, 2 MiB on cellular', () {
    expect(askAboveWifi, 8 << 20);
    expect(askAboveCellular, 2 << 20);
    expect(askAboveFor(NetKind.wifi), askAboveWifi);
    expect(askAboveFor(NetKind.cellular), askAboveCellular);
  });

  test('the network from what the plugin reports', () {
    expect(netKindOf([ConnectivityResult.mobile]), NetKind.cellular);
    expect(
      netKindOf([ConnectivityResult.mobile, ConnectivityResult.vpn]),
      NetKind.cellular,
    );
    expect(
      netKindOf([ConnectivityResult.wifi, ConnectivityResult.mobile]),
      NetKind.wifi,
    );
    expect(netKindOf([ConnectivityResult.ethernet]), NetKind.wifi);
    expect(netKindOf([ConnectivityResult.none]), NetKind.wifi);
    expect(netKindOf(const []), NetKind.wifi);
  });

  test('a 3 MiB file reads on Wi-Fi and asks on cellular', () {
    const len = 3 << 20;
    expect(planOpen(len, askAbove: askAboveWifi), OpenPlan.read);
    expect(planOpen(len, askAbove: askAboveCellular), OpenPlan.ask);
    expect(planOpen(9 << 20, askAbove: askAboveWifi), OpenPlan.ask);
    expect(
      planOpen(maxRead + 1, askAbove: askAboveWifi),
      OpenPlan.refuse,
      reason: 'the ~16 MiB refuse does not move',
    );
  });

  test('tests and Linux count as Wi-Fi without the plugin', () async {
    expect(await probeNet(), NetKind.wifi);
    expect(await askAboveNow(), askAboveWifi);
  });

  test('a missing plugin or a failing one is Wi-Fi', () async {
    expect(
      await probeNet(check: () async => throw MissingPluginException('no')),
      NetKind.wifi,
    );
    expect(
      await probeNet(check: () async => throw StateError('x')),
      NetKind.wifi,
    );
    expect(
      await probeNet(check: () async => [ConnectivityResult.mobile]),
      NetKind.cellular,
    );
  });

  test(
    'the probe is replaceable; a throwing probe falls back to Wi-Fi',
    () async {
      netProbe = () async => NetKind.cellular;
      expect(await askAboveNow(), askAboveCellular);
      netProbe = () async => throw MissingPluginException('no');
      expect(await askAboveNow(), askAboveWifi);
    },
  );
}
