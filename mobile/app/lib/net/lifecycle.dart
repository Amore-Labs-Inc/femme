import 'dart:async';
import 'dart:io' show Platform;

import 'package:connectivity_plus/connectivity_plus.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/widgets.dart';

import 'link.dart';

/// Drops the link while the app is in the background and reconnects on the way back: a phone
/// in a pocket should not hold a socket the OS will kill anyway. Also reconnects at once when
/// the phone changes network (Wi-Fi ↔ cellular, or back from none): a socket on the network it
/// left would otherwise wait for its pings to time out, then for the backoff.
class LinkLifecycle with WidgetsBindingObserver {
  /// [network] reports the phone's networks; [platformNetwork] when null (nothing on Linux
  /// desktop and in tests).
  LinkLifecycle(this.link, {Stream<List<ConnectivityResult>>? network}) {
    WidgetsBinding.instance.addObserver(this);
    _net = (network ?? platformNetwork())?.listen(
      _onNetwork,
      // A missing plugin or a platform error: no network trigger, the rest still works.
      onError: (Object e) => debugPrint('femme link: no network changes: $e'),
    );
  }
  final Link link;
  StreamSubscription<List<ConnectivityResult>>? _net;
  Set<ConnectivityResult>? _last;

  /// `connectivity_plus` on iOS, Android and macOS; null elsewhere.
  static Stream<List<ConnectivityResult>>? platformNetwork() {
    if (kIsWeb) return null;
    if (!(Platform.isIOS || Platform.isAndroid || Platform.isMacOS)) {
      return null;
    }
    try {
      return Connectivity().onConnectivityChanged;
    } catch (e) {
      debugPrint('femme link: no network changes: $e');
      return null;
    }
  }

  void _onNetwork(List<ConnectivityResult> now) {
    final next = now.toSet()..remove(ConnectivityResult.none);
    final last = _last;
    _last = next;
    // The first report is where the phone is now, not a change.
    if (last == null || setEquals(last, next)) return;
    // Lost every network: nothing to dial; the drop shows by itself and the next change redials.
    if (next.isEmpty) return;
    link.reconnect();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    switch (state) {
      case AppLifecycleState.resumed:
        link.resume();
      case AppLifecycleState.paused:
      case AppLifecycleState.detached:
        link.pause();
      case AppLifecycleState.inactive:
      case AppLifecycleState.hidden:
        break;
    }
  }

  void dispose() {
    _net?.cancel();
    WidgetsBinding.instance.removeObserver(this);
  }
}
