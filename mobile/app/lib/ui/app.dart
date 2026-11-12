import 'dart:async';

import 'package:flutter/cupertino.dart';

import '../net/lifecycle.dart';
import '../net/link.dart';
import '../state/box_watch.dart';
import '../state/links.dart';
import '../state/push.dart';
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
import 'widgets.dart';

class FemmeApp extends StatefulWidget {
  const FemmeApp({super.key, required this.prefs, this.links, this.link});
  final Prefs prefs;

  /// The connection to use; a new [Link] when null (tests hand one that never dials).
  final Link? link;

  /// The `femme://` links to act on; [DeepLinks.platform] when null (tests hand their own).
  final Stream<DeepLink>? links;

  @override
  State<FemmeApp> createState() => _FemmeAppState();
}

class _FemmeAppState extends State<FemmeApp> {
  late final Link _link =
      widget.link ?? Link(clientInstance: widget.prefs.clientInstance);
  final Store _store = Store();
  late final LinkLifecycle _lifecycle = LinkLifecycle(_link);
  late final LinkClock _clock = LinkClock(_link.state);
  late final BoxWatch _boxWatch = BoxWatch(_link.req, _link.state);
  late final PushSync _push = PushSync(_link.req, _link.state, widget.prefs);
  (String, String)? _connectedTo;
  final _nav = GlobalKey<NavigatorState>();
  StreamSubscription<DeepLink>? _links;

  /// The session a link asked to open; [Shell] takes it (and clears it) once it is on screen.
  final _openSid = ValueNotifier<String?>(null);

  @override
  void initState() {
    super.initState();
    _store.attach(_link);
    _lifecycle;
    _clock;
    _boxWatch;
    _push;
    PairInfo.load();
    widget.prefs.addListener(_onPrefs);
    _onPrefs();
    _links = (widget.links ?? DeepLinks.platform()).listen(_onLink);
  }

  void _onLink(DeepLink l) {
    switch (l) {
      case OpenSession(:final sid):
        // Unpaired there is no box to open it on; the link is dropped.
        if (widget.prefs.paired) _openSid.value = sid;
      case Pair():
        _afterFirstFrame(() => _pair(l));
    }
  }

  /// Runs [f] once the app's navigator is built (a link can come before the first frame).
  void _afterFirstFrame(VoidCallback f) {
    if (_nav.currentContext != null) return f();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _afterFirstFrame(f);
    });
    WidgetsBinding.instance.ensureVisualUpdate();
  }

  /// Pairs with the link's box; asks first when it would replace a different pairing.
  Future<void> _pair(Pair p) async {
    final prefs = widget.prefs;
    if (prefs.paired) {
      if (prefs.host == p.link.address && prefs.token == p.token) return;
      final ok = await confirmAction(
        _nav.currentContext!,
        title: 'Pair with ${p.name}?',
        message: 'This replaces the current box.',
        action: 'Pair',
      );
      if (!ok || !mounted) return;
    }
    await PairInfo.save(prefs, p.link);
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
    _links?.cancel();
    _openSid.dispose();
    _lifecycle.dispose();
    _clock.dispose();
    _boxWatch.dispose();
    _push.dispose();
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
      push: _push,
      child: CupertinoApp(
        title: 'FEMME',
        debugShowCheckedModeBanner: false,
        navigatorKey: _nav,
        theme: femmeTheme(brightness),
        home: widget.prefs.paired
            ? Shell(open: _openSid)
            : PairScreen(prefs: widget.prefs),
      ),
    );
  }
}

final _noAttention = ValueNotifier(false);

/// The four tabs; each its own navigator, so pushes keep the bar. Now carries the count of
/// sessions that need you; Box a red dot while an account's sign-in is red.
class Shell extends StatefulWidget {
  const Shell({super.key, this.open});

  /// A session to open (from a `femme://session/<sid>` link): the shell switches to Now, pushes
  /// its chat there and sets this back to null. The chat subscribes on its own, so a session the
  /// store has not seen yet still opens.
  final ValueNotifier<String?>? open;

  @override
  State<Shell> createState() => _ShellState();
}

class _ShellState extends State<Shell> {
  final _tabs = CupertinoTabController();
  final _navs = List.generate(4, (_) => GlobalKey<NavigatorState>());

  /// Each tab's root list, for the scroll to the top on a second tap of its tab.
  final _scrolls = List.generate(4, (_) => ScrollController());

  /// The tab on screen before a tap (the controller already holds the new one when the bar's
  /// `onTap` runs).
  int _shownTab = 0;

  @override
  void initState() {
    super.initState();
    widget.open?.addListener(_onOpen);
    // A link that came before the shell was built is applied after its first frame.
    WidgetsBinding.instance.addPostFrameCallback((_) => _onOpen());
  }

  @override
  void didUpdateWidget(Shell old) {
    super.didUpdateWidget(old);
    if (old.open != widget.open) {
      old.open?.removeListener(_onOpen);
      widget.open?.addListener(_onOpen);
      _onOpen();
    }
  }

  @override
  void dispose() {
    widget.open?.removeListener(_onOpen);
    _tabs.dispose();
    for (final c in _scrolls) {
      c.dispose();
    }
    super.dispose();
  }

  void _onOpen() {
    final sid = widget.open?.value;
    if (sid == null || !mounted) return;
    widget.open!.value = null;
    _tabs.index = 0;
    _shownTab = 0;
    // Switching tabs builds Now's navigator if it was never shown; push once it exists.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      final nav = _navs[0].currentState;
      if (nav == null) return;
      nav.popUntil((r) => r.isFirst);
      final s = Femme.of(context).store.session(sid);
      nav.push(CupertinoPageRoute<void>(builder: (_) => sessionScreen(s, sid)));
    });
    WidgetsBinding.instance.ensureVisualUpdate();
  }

  /// A tab tapped again, as iOS does: back to the tab's first page, or, already there, its
  /// list to the top.
  void _onTabTap(int i) {
    final again = i == _shownTab;
    _shownTab = i;
    if (!again) return;
    final nav = _navs[i].currentState;
    if (nav != null && nav.canPop()) {
      nav.popUntil((r) => r.isFirst);
      return;
    }
    final c = _scrolls[i];
    if (c.hasClients && c.offset > c.position.minScrollExtent) {
      c.animateTo(
        c.position.minScrollExtent,
        duration: const Duration(milliseconds: 180),
        curve: const Cubic(0.32, 0.72, 0, 1),
      );
    }
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final waiting = f.store.needsYou.length;
    final attention = f.boxWatch?.attention ?? _noAttention;
    return CupertinoTabScaffold(
      controller: _tabs,
      tabBar: CupertinoTabBar(
        activeColor: FemmeColors.accent,
        onTap: _onTabTap,
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
        navigatorKey: _navs[i],
        // The root page's list takes this as its primary controller (pushed pages have their
        // own route's), so a second tap of the tab can scroll it.
        builder: (context) => PrimaryScrollController(
          controller: _scrolls[i],
          child: switch (i) {
            0 => const NowTab(),
            1 => const ChatsTab(),
            2 => const BoxTab(),
            _ => const MeTab(),
          },
        ),
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
