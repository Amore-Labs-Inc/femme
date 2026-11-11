import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:package_info_plus/package_info_plus.dart';
import 'package:url_launcher/url_launcher.dart';

import '../net/link.dart';
import '../state/pairing.dart';
import '../state/prefs.dart';
import '../state/push.dart';
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
              _PushSection(push: f.push, muted: muted),
              CupertinoListSection.insetGrouped(
                header: const Text('About'),
                children: [
                  CupertinoListTile(
                    title: const Text('FEMME for iPhone and Android'),
                    additionalInfo: Text(appVersionText(_info)),
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

/// Push through ntfy: on/off, this phone's topic, what to push, and how the box took it.
class _PushSection extends StatelessWidget {
  const _PushSection({required this.push, required this.muted});
  final PushSync? push;
  final Color muted;

  @override
  Widget build(BuildContext context) {
    final p = push;
    if (p == null) return const SizedBox.shrink();
    final rules = p.rules;
    final topic = p.topic;
    final status = p.error != null
        ? (p.unsupported
              ? 'The box is too old for push: update femmed.'
              : 'Not registered yet: ${p.error}')
        : p.registered != null
        ? 'Registered with the box.'
        : (p.on ? 'Registering…' : null);
    return CupertinoListSection.insetGrouped(
      header: const Text('Notifications'),
      footer: Text(
        p.on
            ? 'Install the ntfy app, open the link above and allow notifications: the box '
                  'posts to your topic when a session needs you or finishes. The topic name '
                  'is the secret; make a new one if it leaks.'
            : 'The box pushes through ntfy, so the lock screen tells you when a session '
                  'needs you or is done. No account needed: the ntfy app and a topic.',
        style: TextStyle(color: muted),
      ),
      children: [
        CupertinoListTile(
          title: const Text('Push through ntfy'),
          trailing: CupertinoSwitch(
            value: p.on,
            onChanged: (v) {
              tapHaptic();
              p.setOn(v);
            },
          ),
        ),
        if (p.on && topic != null) ...[
          CupertinoListTile(
            title: const Text('Topic'),
            subtitle: Text(
              topic,
              style: const TextStyle(
                fontFamily: monoFamily,
                fontFamilyFallback: monoFallback,
                fontSize: 13,
              ),
            ),
            trailing: const Text(
              'Copy',
              style: TextStyle(color: FemmeColors.accent),
            ),
            onTap: () {
              tapHaptic();
              Clipboard.setData(ClipboardData(text: topic));
            },
          ),
          CupertinoListTile(
            title: const Text('Subscribe in ntfy'),
            trailing: const Icon(CupertinoIcons.arrow_up_right),
            onTap: () => launchUrl(
              Uri.parse('https://ntfy.sh/$topic'),
              mode: LaunchMode.externalApplication,
            ),
          ),
          CupertinoListTile(
            title: const Text('When a session needs you'),
            trailing: CupertinoSwitch(
              value: rules.blocked,
              onChanged: (v) => p.setRules(rules.copyWith(blocked: v)),
            ),
          ),
          CupertinoListTile(
            title: const Text('When a session finishes'),
            trailing: CupertinoSwitch(
              value: rules.done,
              onChanged: (v) => p.setRules(rules.copyWith(done: v)),
            ),
          ),
          CupertinoListTile(
            title: const Text('Quiet from 23:00 to 07:00'),
            trailing: CupertinoSwitch(
              value: rules.quiet != null,
              onChanged: (v) => p.setRules(
                v
                    ? rules.copyWith(quiet: (23, 7))
                    : rules.copyWith(clearQuiet: true),
              ),
            ),
          ),
          CupertinoListTile(
            title: const Text('New topic'),
            subtitle: const Text('the old one stops getting pushes'),
            onTap: () {
              actionHaptic();
              p.rotate();
            },
          ),
        ],
        if (status != null)
          CupertinoListTile(
            title: Text(
              status,
              style: TextStyle(
                color: p.error != null ? FemmeColors.bad : muted,
                fontSize: 14,
              ),
            ),
          ),
      ],
    );
  }
}

/// The commit this build was made from, passed by CI as `--dart-define=FEMME_SHA=<sha>`; empty
/// in a local build.
const femmeSha = String.fromEnvironment('FEMME_SHA');

/// `1.0.0 (12) · a3ca099`: the app version, its build number and, when the build knows it, the
/// commit (desktop `menu.rs` VERSION + COMMIT).
String appVersionText(PackageInfo? info, {String sha = femmeSha}) {
  if (info == null) return '—';
  final short = sha.length > 7 ? sha.substring(0, 7) : sha;
  final v = '${info.version} (${info.buildNumber})';
  return short.isEmpty ? v : '$v · $short';
}
