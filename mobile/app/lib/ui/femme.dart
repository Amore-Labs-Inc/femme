import 'package:flutter/widgets.dart';

import '../net/link.dart';
import '../state/box_watch.dart';
import '../state/prefs.dart';
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
    required super.child,
  }) : super(notifier: Listenable.merge([store, link.state, prefs]));

  final Store store;
  final Link link;
  final Prefs prefs;

  /// The Box tab's sign-in watcher; absent in tests that do not need it.
  final BoxWatch? boxWatch;

  static Femme of(BuildContext context) =>
      context.dependOnInheritedWidgetOfExactType<Femme>()!;
}
