import 'package:flutter/widgets.dart';

import 'link.dart';

/// Drops the link while the app is in the background and reconnects on the way back: a phone
/// in a pocket should not hold a socket the OS will kill anyway.
class LinkLifecycle with WidgetsBindingObserver {
  LinkLifecycle(this.link) {
    WidgetsBinding.instance.addObserver(this);
  }
  final Link link;

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

  void dispose() => WidgetsBinding.instance.removeObserver(this);
}
