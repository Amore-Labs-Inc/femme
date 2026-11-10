import 'package:flutter/cupertino.dart';

import '../net/lifecycle.dart';
import '../net/link.dart';
import '../state/box_watch.dart';
import '../state/pairing.dart';
import '../state/prefs.dart';
import '../state/store.dart';
import 'box.dart';
import 'chats.dart';
import 'femme.dart';
import 'me.dart';
import 'now.dart';
import 'pair.dart';
import 'theme.dart';

class FemmeApp extends StatefulWidget {
  const FemmeApp({super.key, required this.prefs});
  final Prefs prefs;

  @override
  State<FemmeApp> createState() => _FemmeAppState();
}

class _FemmeAppState extends State<FemmeApp> {
  late final Link _link = Link(clientInstance: widget.prefs.clientInstance);
  final Store _store = Store();
  late final LinkLifecycle _lifecycle = LinkLifecycle(_link);
  late final LinkClock _clock = LinkClock(_link.state);
  late final BoxWatch _boxWatch = BoxWatch(_link.req, _link.state);
  (String, String)? _connectedTo;

  @override
  void initState() {
    super.initState();
    _store.attach(_link);
    _lifecycle;
    _clock;
    _boxWatch;
    PairInfo.load();
    widget.prefs.addListener(_onPrefs);
    _onPrefs();
  }

  /// Connects when pairing is saved or changes; drops the link when it is forgotten.
  void _onPrefs() {
    final p = widget.prefs;
    final target = p.paired ? (p.host!, p.token!) : null;
    if (target != _connectedTo) {
      _connectedTo = target;
      if (target == null) {
        _link.pause();
      } else {
        _link.connect(target.$1, target.$2);
      }
    }
    setState(() {});
  }

  @override
  void dispose() {
    widget.prefs.removeListener(_onPrefs);
    _lifecycle.dispose();
    _clock.dispose();
    _boxWatch.dispose();
    _link.close();
    _store.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final brightness = switch (widget.prefs.theme) {
      ThemePref.system => null,
      ThemePref.light => Brightness.light,
      ThemePref.dark => Brightness.dark,
    };
    return Femme(
      store: _store,
      link: _link,
      prefs: widget.prefs,
      boxWatch: _boxWatch,
      child: CupertinoApp(
        title: 'FEMME',
        debugShowCheckedModeBanner: false,
        theme: femmeTheme(brightness),
        home: widget.prefs.paired
            ? const Shell()
            : PairScreen(prefs: widget.prefs),
      ),
    );
  }
}

final _noAttention = ValueNotifier(false);

/// The four tabs; each its own navigator, so pushes keep the bar. Now carries the count of
/// sessions that need you; Box a red dot while an account's sign-in is red.
class Shell extends StatelessWidget {
  const Shell({super.key});

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final waiting = f.store.needsYou.length;
    final attention = f.boxWatch?.attention ?? _noAttention;
    return CupertinoTabScaffold(
      tabBar: CupertinoTabBar(
        activeColor: FemmeColors.accent,
        items: [
          BottomNavigationBarItem(
            icon: _Badged(
              count: waiting,
              child: const Icon(CupertinoIcons.bolt_fill),
            ),
            label: 'Now',
          ),
          const BottomNavigationBarItem(
            icon: Icon(CupertinoIcons.bubble_left_bubble_right_fill),
            label: 'Chats',
          ),
          BottomNavigationBarItem(
            icon: ValueListenableBuilder(
              valueListenable: attention,
              builder: (context, red, child) =>
                  _Dotted(dot: red, child: child!),
              child: const Icon(CupertinoIcons.cube_box_fill),
            ),
            label: 'Box',
          ),
          const BottomNavigationBarItem(
            icon: Icon(CupertinoIcons.person_crop_circle_fill),
            label: 'Me',
          ),
        ],
      ),
      tabBuilder: (context, i) => CupertinoTabView(
        builder: (context) => switch (i) {
          0 => const NowTab(),
          1 => const ChatsTab(),
          2 => const BoxTab(),
          _ => const MeTab(),
        },
      ),
    );
  }
}

/// An icon with a red count in its corner (nothing at zero).
class _Badged extends StatelessWidget {
  const _Badged({required this.count, required this.child});
  final int count;
  final Widget child;

  @override
  Widget build(BuildContext context) => Stack(
    clipBehavior: Clip.none,
    children: [
      child,
      if (count > 0)
        Positioned(
          right: -10,
          top: -4,
          child: Container(
            constraints: const BoxConstraints(minWidth: 18),
            padding: const EdgeInsets.symmetric(horizontal: 5, vertical: 1),
            decoration: BoxDecoration(
              color: CupertinoColors.systemRed,
              borderRadius: BorderRadius.circular(9),
            ),
            child: Text(
              count > 99 ? '99+' : '$count',
              textAlign: TextAlign.center,
              style: const TextStyle(
                color: CupertinoColors.white,
                fontSize: 12,
                fontWeight: FontWeight.w600,
              ),
            ),
          ),
        ),
    ],
  );
}

/// An icon with a plain red dot at its top right (the Box tab's sign-in warning).
class _Dotted extends StatelessWidget {
  const _Dotted({required this.dot, required this.child});
  final bool dot;
  final Widget child;

  @override
  Widget build(BuildContext context) => Stack(
    clipBehavior: Clip.none,
    children: [
      child,
      if (dot)
        Positioned(
          right: -2,
          top: -1,
          child: Container(
            key: const ValueKey('box-attention'),
            width: 8,
            height: 8,
            decoration: const BoxDecoration(
              color: FemmeColors.bad,
              shape: BoxShape.circle,
            ),
          ),
        ),
    ],
  );
}
