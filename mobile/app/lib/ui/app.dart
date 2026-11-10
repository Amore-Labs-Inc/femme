import 'package:flutter/cupertino.dart';

import '../net/lifecycle.dart';
import '../net/link.dart';
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
  (String, String)? _connectedTo;

  @override
  void initState() {
    super.initState();
    _store.attach(_link);
    _lifecycle;
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
      child: CupertinoApp(
        title: 'FEMME',
        theme: femmeTheme(brightness),
        home: widget.prefs.paired
            ? const Shell()
            : PairScreen(prefs: widget.prefs),
      ),
    );
  }
}

/// The four tabs.
class Shell extends StatelessWidget {
  const Shell({super.key});

  @override
  Widget build(BuildContext context) => CupertinoTabScaffold(
    tabBar: CupertinoTabBar(
      activeColor: FemmeColors.accent,
      items: const [
        BottomNavigationBarItem(
          icon: Icon(CupertinoIcons.bolt_fill),
          label: 'Now',
        ),
        BottomNavigationBarItem(
          icon: Icon(CupertinoIcons.chat_bubble_2_fill),
          label: 'Chats',
        ),
        BottomNavigationBarItem(
          icon: Icon(CupertinoIcons.cube_box_fill),
          label: 'Box',
        ),
        BottomNavigationBarItem(
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
