import 'package:flutter/cupertino.dart';
import 'package:package_info_plus/package_info_plus.dart';
import 'package:url_launcher/url_launcher.dart';

import '../net/link.dart';
import '../state/pairing.dart';
import '../state/prefs.dart';
import 'femme.dart';
import 'me/pairing.dart';
import 'theme.dart';
import 'widgets.dart';

/// Settings, once: the paired box, pairing again, appearance, notifications, about.
class MeTab extends StatefulWidget {
  const MeTab({super.key});

  @override
  State<MeTab> createState() => _MeTabState();
}

class _MeTabState extends State<MeTab> {
  PackageInfo? _info;

  @override
  void initState() {
    super.initState();
    PairInfo.load().then((_) => mounted ? setState(() {}) : null);
    PackageInfo.fromPlatform()
        .then((i) => mounted ? setState(() => _info = i) : null)
        .catchError((_) => null);
  }

  Future<void> _pairAgain() => Navigator.of(context).push(
    CupertinoPageRoute<void>(
      builder: (c) => CupertinoPageScaffold(
        navigationBar: const CupertinoNavigationBar(
          middle: Text('Pair with QR'),
        ),
        child: SafeArea(
          child: ListView(
            children: [PairForm(onPaired: () => Navigator.of(c).pop())],
          ),
        ),
      ),
    ),
  );

  Future<void> _forget() async {
    final prefs = Femme.of(context).prefs;
    final ok = await confirmAction(
      context,
      title: 'Forget this box?',
      message: 'The phone disconnects. Pair again with a new code from `femmed pair`.',
      action: 'Forget',
    );
    if (ok) {
      actionHaptic();
      await PairInfo.forget(prefs);
    }
  }

  Future<void> _open(String url) async {
    final u = Uri.tryParse(url);
    if (u != null) await launchUrl(u, mode: LaunchMode.externalApplication);
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final link = f.link.state.value;
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final host = f.prefs.host;
    final since = LinkClock.onlineSince;
    final board = host == null
        ? null
        : linkUrl(host).replace(
            scheme: linkUrl(host).scheme == 'ws' ? 'http' : 'https',
            path: '/',
          );
    final paired = PairInfo.pairedAt;
    return TabPage(
      title: 'Me',
      status: linkLine(link),
      dot: linkColor(link),
      slivers: [
        SliverToBoxAdapter(
          child: Column(
            children: [
              CupertinoListSection.insetGrouped(
                header: const Text('Pairing'),
                children: [
                  CupertinoListTile(
                    leading: const Icon(CupertinoIcons.cube_box),
                    title: Text(PairInfo.boxName ?? host ?? 'not paired'),
                    subtitle: host == null ? null : Text(host),
                  ),
                  CupertinoListTile(
                    title: const Text('This device'),
                    additionalInfo: Text(
                      paired == null
                          ? 'instance ${f.prefs.clientInstance}'
                          : 'paired ${ago(paired)} ago',
                    ),
                  ),
                  CupertinoListTile(
                    title: const Text('Connected'),
                    additionalInfo: Text(
                      link.online
                          ? (since == null ? 'yes' : 'for ${ago(since)}')
                          : link.phase.name,
                    ),
                  ),
                  CupertinoListTile(
                    leading: const Icon(CupertinoIcons.qrcode_viewfinder),
                    title: const Text('Pair with QR'),
                    trailing: const CupertinoListTileChevron(),
                    onTap: _pairAgain,
                  ),
                  CupertinoListTile(
                    leading: const Icon(
                      CupertinoIcons.trash,
                      color: CupertinoColors.destructiveRed,
                    ),
                    title: const Text(
                      'Forget this box',
                      style: TextStyle(color: CupertinoColors.destructiveRed),
                    ),
                    onTap: _forget,
                  ),
                ],
              ),
              CupertinoListSection.insetGrouped(
                header: const Text('Appearance'),
                children: [
                  Padding(
                    padding: const EdgeInsets.all(12),
                    child: CupertinoSlidingSegmentedControl<ThemePref>(
                      groupValue: f.prefs.theme,
                      onValueChanged: (t) {
                        if (t != null) {
                          tapHaptic();
                          f.prefs.setTheme(t);
                        }
                      },
                      children: const {
                        ThemePref.system: Text('System'),
                        ThemePref.light: Text('Light'),
                        ThemePref.dark: Text('Dark'),
                      },
                    ),
                  ),
                ],
              ),
              CupertinoListSection.insetGrouped(
                header: const Text('Notifications'),
                children: [
                  Padding(
                    padding: const EdgeInsets.all(16),
                    child: Text(
                      'For now the box pushes through ntfy: subscribe to its topic in the '
                      'ntfy app to hear when a session needs you or is done. Push straight '
                      'to this app (with answer buttons on the lock screen) comes with '
                      'PushRegister on the box.',
                      style: TextStyle(color: muted, fontSize: 14),
                    ),
                  ),
                ],
              ),
              CupertinoListSection.insetGrouped(
                header: const Text('About'),
                children: [
                  CupertinoListTile(
                    title: const Text('FEMME for iPhone and Android'),
                    additionalInfo: Text(
                      _info == null
                          ? '—'
                          : '${_info!.version} (${_info!.buildNumber})',
                    ),
                  ),
                  CupertinoListTile(
                    title: const Text('femmed'),
                    additionalInfo: Text(link.serverVersion ?? '—'),
                  ),
                  if (board != null)
                    CupertinoListTile(
                      title: const Text('Open the board'),
                      trailing: const Icon(CupertinoIcons.arrow_up_right),
                      onTap: () => _open(board.toString()),
                    ),
                ],
              ),
            ],
          ),
        ),
      ],
    );
  }
}
