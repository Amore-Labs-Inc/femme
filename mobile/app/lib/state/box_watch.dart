/// The Box tab's red dot: whether any account's sign-in is red (ended, or ends within two days),
/// read from `/api/accounts` now and every 10 minutes while the link is up, like the desktop's
/// Accounts menu entry (`accounts.rs` `sign_in_bad`, `watch_sign_ins`).
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../net/link.dart';
import '../net/rpc.dart';
import 'box.dart';

/// Whether an `/api/accounts` body has an account whose sign-in is red at [now]; false when the
/// body holds no accounts (an error, cswap absent).
bool signInAttention(String body, DateTime now) =>
    parseAccounts(body).accounts?.any((a) => a.signIn(now)?.$2 == Level.bad) ??
    false;

class BoxWatch {
  BoxWatch(
    this._req,
    this._link, {
    this.every = const Duration(minutes: 10),
    DateTime Function()? now,
  }) : _now = now ?? DateTime.now {
    _link.addListener(_onLink);
    _onLink();
  }

  final Future<Resp> Function(Object req) _req;
  final ValueListenable<LinkState> _link;
  final Duration every;
  final DateTime Function() _now;

  /// Some account needs signing in again. Kept while offline; a failed read leaves it as it was.
  final attention = ValueNotifier(false);

  Timer? _timer;
  bool _busy = false;

  void _onLink() {
    final online = _link.value.online;
    if (online == (_timer != null)) return;
    _timer?.cancel();
    _timer = null;
    if (online) {
      _timer = Timer.periodic(every, (_) => check());
      check();
    }
  }

  /// Reads the accounts once (also after the Accounts card changed something).
  Future<void> check() async {
    if (_busy) return;
    _busy = true;
    try {
      final body = await boardCall(_req, 'GET', '/api/accounts');
      attention.value = signInAttention(body, _now());
    } catch (_) {
      // Offline, an old daemon, cswap down: keep the last answer.
    } finally {
      _busy = false;
    }
  }

  void dispose() {
    _link.removeListener(_onLink);
    _timer?.cancel();
    _timer = null;
    attention.dispose();
  }
}
