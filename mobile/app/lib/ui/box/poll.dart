import 'dart:async';

import 'package:flutter/widgets.dart';

import '../femme.dart';

/// Reads something now and every [every] while the widget is on screen (its tab shown, nothing
/// pushed over it) and the link is up; stops otherwise.
mixin Polling<T extends StatefulWidget> on State<T> {
  Duration get every;

  /// One read. Errors are the implementer's to show.
  Future<void> poll();

  Timer? _timer;
  bool _active = false;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final active =
        TickerMode.valuesOf(context).enabled && Femme.of(context).link.isOnline;
    if (active == _active) return;
    _active = active;
    _timer?.cancel();
    _timer = null;
    if (active) {
      poll();
      _timer = Timer.periodic(every, (_) => poll());
    }
  }

  /// Reads again at once (a Check button, after an action).
  Future<void> pollNow() => poll();

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }
}
