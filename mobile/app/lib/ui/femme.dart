import 'package:flutter/widgets.dart';

import '../net/link.dart';
import '../state/box_watch.dart';
import '../state/prefs.dart';
import '../state/push.dart';
import '../state/store.dart';

/// Hands the store, the link and the prefs down the tree; widgets that call [Femme.of] rebuild
/// when any of them changes.
class Femme extends InheritedNotifier<Listenable> {
  Femme({
    super.key,
    required this.store,
    required this.link,
    required this.prefs,
    this.boxWatch,
    this.push,
    required super.child,
  }) : super(notifier: Listenable.merge([store, link.state, prefs, ?push]));

  final Store store;
  final Link link;
  final Prefs prefs;

  /// The Box tab's sign-in watcher; absent in tests that do not need it.
  final BoxWatch? boxWatch;

  /// Push registration with the box; absent in tests that do not need it.
  final PushSync? push;

  static Femme of(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<Femme>()!;
}
