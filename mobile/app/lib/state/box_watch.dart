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

  /// The merges the phone started, followed to their deploy.
  late final merges = MergeWatch(_req);

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
    merges.dispose();
  }
}

// ---- merges ------------------------------------------------------------------------------------

/// Where a merge stands on its way to the box (desktop `prs.rs` `Deploy` and the queued list).
sealed class MergeState {
  const MergeState(this.number);
  final int number;
}

/// Queued behind a Claude chat that resolves the conflicts; it merges by itself.
class MergeQueued extends MergeState {
  const MergeQueued(super.number);
}

/// Merged; the box still runs [from] (null: it has not said yet which commit it ran).
class MergeDeploying extends MergeState {
  const MergeDeploying(super.number, this.from, this.polls);
  final String? from;

  /// The `ServerInfo` answers so far, one every [MergeWatch.poll].
  final int polls;
}

/// The box runs the merge now.
class MergeDeployed extends MergeState {
  const MergeDeployed(super.number, this.sha);
  final String sha;
}

/// [MergeWatch.pollFor] went by with the box on the old commit.
class MergeGaveUp extends MergeState {
  const MergeGaveUp(super.number, this.minutes);
  final int minutes;
}

/// A queued merge left `merging` while the PR is still open: the fix stopped.
class MergeStopped extends MergeState {
  const MergeStopped(super.number);
}

/// The first seven characters of a commit.
String shortSha(String sha) => sha.length > 7 ? sha.substring(0, 7) : sha;

/// The line a merge's state reads as, and whether it is good news (desktop `Deploy::line` and
/// the notices of `settle_merges`, word for word).
(String, bool) mergeLine(MergeState s, Duration poll) => switch (s) {
  MergeQueued(:final number) => (
    '#$number conflicts with main: Claude resolves them, then it merges and deploys by itself',
    true,
  ),
  MergeDeploying(:final number, :final polls) => (
    '#$number merged · deploying… ${_elapsed(poll * polls)}',
    true,
  ),
  MergeDeployed(:final number, :final sha) => (
    '#$number deployed ${shortSha(sha)}',
    true,
  ),
  MergeGaveUp(:final number, :final minutes) => (
    '#$number merged, but the box still runs the old commit after $minutes min',
    false,
  ),
  MergeStopped(:final number) => (
    '#$number not merged: the conflict fix stopped',
    false,
  ),
};

String _elapsed(Duration d) {
  final s = d.inSeconds;
  if (s < 60) return '${s}s';
  return '${s ~/ 60}m ${(s % 60).toString().padLeft(2, '0')}s';
}

/// Follows merges past `PrMerge`: a merged PR's deploy (`ServerInfo` every [poll] until the
/// commit the box runs changes, at most [pollFor]), and a merge the daemon queued behind a
/// conflict fix (the list every [watch] until the PR leaves it, then its deploy). It lives as long
/// as the app, so a merge goes on being followed after its page closes, as on the desktop.
class MergeWatch {
  MergeWatch(
    this._req, {
    this.poll = const Duration(seconds: 5),
    this.pollFor = const Duration(minutes: 10),
    this.watch = const Duration(seconds: 10),
  });

  final Future<Resp> Function(Object req) _req;
  final Duration poll;
  final Duration pollFor;
  final Duration watch;

  /// Each merge followed, by PR number; the last state stays until the next merge of that PR.
  final states = ValueNotifier<Map<int, MergeState>>(const {});

  /// The newest list the watch read, for pages that show a PR.
  final prs = ValueNotifier<List<Pr>?>(null);

  final _queued = <int>{};
  Timer? _watchTimer;
  bool _reading = false;
  bool _disposed = false;

  MergeState? of(int number) => states.value[number];

  /// Whether a merge of [number] is still under way (queued or deploying).
  bool busy(int number) =>
      of(number) is MergeQueued || of(number) is MergeDeploying;

  void _set(MergeState s) {
    if (_disposed) return;
    states.value = {...states.value, s.number: s};
  }

  /// The commit the box runs, or null when it does not say (or the read failed).
  Future<String?> deployed() async {
    try {
      return (await _req(Req.serverInfo)).serverInfo.deployed;
    } catch (_) {
      return null;
    }
  }

  /// `PrMerge` of [number] answered Ok while the box ran [from]: the list read now says whether
  /// it merged (watch the deploy) or queued (watch the list). A failed read means it most likely
  /// merged: the daemon restarts as a deploy lands.
  Future<void> merged(int number, String? from) async {
    List<Pr> list;
    try {
      list = (await _req(Req.prs)).prs;
    } catch (_) {
      _deploying(number, from);
      return;
    }
    if (_disposed) return;
    prs.value = list;
    final p = list.where((p) => p.number == number).firstOrNull;
    if (p != null && p.merging) {
      _queued.add(number);
      _set(MergeQueued(number));
      _watchTimer ??= Timer.periodic(watch, (_) => _readQueued());
    } else {
      _deploying(number, from);
    }
  }

  Future<void> _readQueued() async {
    if (_reading || _disposed) return;
    _reading = true;
    try {
      final list = (await _req(Req.prs)).prs;
      if (_disposed) return;
      prs.value = list;
      for (final n in [..._queued]) {
        final p = list.where((p) => p.number == n).firstOrNull;
        if (p != null && p.merging) continue;
        _queued.remove(n);
        if (p == null) {
          // Gone from the list: merged. Note the commit the box runs now and watch for the next.
          _deploying(n, await deployed());
        } else {
          _set(MergeStopped(n));
        }
      }
    } catch (_) {
      // offline or the daemon restarting: ask again next round
    } finally {
      _reading = false;
      if (_queued.isEmpty) {
        _watchTimer?.cancel();
        _watchTimer = null;
      }
    }
  }

  Future<void> _deploying(int number, String? from) async {
    _set(MergeDeploying(number, from, 0));
    final rounds = pollFor.inMicroseconds ~/ poll.inMicroseconds;
    var base = from;
    for (var i = 1; i <= rounds; i++) {
      await Future<void>.delayed(poll);
      if (_disposed) return;
      final now = await deployed();
      if (_disposed) return;
      if (now != null && base == null) {
        // The commit before the merge was never read: this first answer is the baseline, not
        // the deploy (the box answers the old commit until the new one runs).
        base = now;
      } else if (now != null && now != base) {
        _set(MergeDeployed(number, now));
        return;
      }
      _set(MergeDeploying(number, base, i));
    }
    _set(MergeGaveUp(number, pollFor.inMinutes));
  }

  void dispose() {
    _disposed = true;
    _watchTimer?.cancel();
    _watchTimer = null;
    states.dispose();
    prs.dispose();
  }
}
