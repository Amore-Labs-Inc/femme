import 'dart:async';

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:url_launcher/url_launcher.dart';

import '../../state/box_flows.dart';
import '../theme.dart';
import '../widgets.dart';

/// "Connect GitHub" on an API key: GitHub's device code, shown until it is entered (desktop
/// `accounts/flow.rs` `github`, keys read every 3 s). Picks up a sign-in already in progress when
/// [key] shows its code. Swiping the sheet away leaves it running; Cancel gives it up.
Future<void> showConnectGithub(
  BuildContext context,
  BoardReq req,
  KeyGithub key,
) => showCupertinoSheet<void>(
  context: context,
  scrollableBuilder: (_, _) => GithubSheet(req: req, keyGithub: key),
);

class GithubSheet extends StatefulWidget {
  const GithubSheet({super.key, required this.req, required this.keyGithub});
  final BoardReq req;
  final KeyGithub keyGithub;

  @override
  State<GithubSheet> createState() => _GithubSheetState();
}

class _GithubSheetState extends State<GithubSheet> {
  late final GithubFlow _flow = GithubFlow(widget.req, widget.keyGithub.name)
    ..addListener(_changed);
  Timer? _poll;

  @override
  void initState() {
    super.initState();
    final k = widget.keyGithub;
    final d = k.device;
    if (d != null) {
      _flow.resume(d, k.account);
    } else {
      _flow.start(accountBefore: k.account);
    }
  }

  void _changed() {
    if (!mounted) return;
    if (_flow.polling && _poll == null) {
      _poll = Timer.periodic(GithubFlow.every, (_) => _flow.poll());
    } else if (!_flow.polling) {
      _poll?.cancel();
      _poll = null;
    }
    if (_flow.phase == GithubPhase.connected) actionHaptic();
    setState(() {});
  }

  @override
  void dispose() {
    _poll?.cancel();
    _flow.dispose();
    super.dispose();
  }

  Future<void> _cancel() async {
    final nav = Navigator.of(context);
    await _flow.cancel();
    nav.pop();
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final f = _flow;
    final out = f.polling || f.phase == GithubPhase.starting;
    final d = f.device;
    return CupertinoPageScaffold(
      backgroundColor: CupertinoColors.systemGroupedBackground,
      navigationBar: CupertinoNavigationBar(
        automaticallyImplyLeading: false,
        middle: Text('GitHub for ${f.name}'),
        leading: CupertinoButton(
          padding: EdgeInsets.zero,
          onPressed: out ? _cancel : () => Navigator.of(context).pop(),
          child: Text(out ? 'Cancel' : 'Done'),
        ),
      ),
      child: SafeArea(
        child: ListView(
          children: [
            CupertinoListSection.insetGrouped(
              footer: f.phase == GithubPhase.code
                  ? const Text(
                      'Its clones push as that account once the code is entered.',
                    )
                  : null,
              children: [
                switch (f.phase) {
                  GithubPhase.idle ||
                  GithubPhase.starting ||
                  GithubPhase.waiting => CupertinoListTile(
                    leading: const CupertinoActivityIndicator(),
                    title: Text(
                      'asking GitHub for a code',
                      style: TextStyle(color: muted),
                    ),
                  ),
                  GithubPhase.code when d != null => _Code(device: d),
                  GithubPhase.connected => CupertinoListTile(
                    leading: const Icon(
                      CupertinoIcons.check_mark_circled_solid,
                      color: FemmeColors.work,
                    ),
                    title: const Text('Connected'),
                    subtitle: Text('pushes as ${f.account ?? 'its account'}'),
                  ),
                  GithubPhase.failed => CupertinoListTile(
                    leading: const Icon(
                      CupertinoIcons.exclamationmark_circle,
                      color: FemmeColors.bad,
                    ),
                    title: Text(f.error ?? 'GitHub sign-in failed'),
                  ),
                  _ => CupertinoListTile(
                    title: Text('cancelled', style: TextStyle(color: muted)),
                  ),
                },
                if (f.phase == GithubPhase.failed)
                  CupertinoListTile(
                    leading: const Icon(CupertinoIcons.refresh),
                    title: const Text('Try again'),
                    onTap: () => f.start(accountBefore: f.account),
                  ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}

class _Code extends StatelessWidget {
  const _Code({required this.device});
  final GithubDevice device;

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final host = Uri.tryParse(device.url);
    return Padding(
      padding: const EdgeInsets.fromLTRB(16, 14, 16, 6),
      child: Column(
        children: [
          Text(
            'The account that should push enters',
            style: TextStyle(color: muted),
          ),
          const SizedBox(height: 8),
          Text(
            device.code,
            style: const TextStyle(
              fontFamily: monoFamily,
              fontFamilyFallback: monoFallback,
              fontSize: 34,
              fontWeight: FontWeight.w700,
              letterSpacing: 2,
            ),
          ),
          const SizedBox(height: 4),
          Text(
            'at ${host == null ? device.url : '${host.host}${host.path}'}',
            style: TextStyle(color: muted),
          ),
          Row(
            children: [
              Expanded(
                child: CupertinoButton(
                  onPressed: host == null
                      ? null
                      : () => launchUrl(
                          host,
                          mode: LaunchMode.externalApplication,
                        ),
                  child: const Text('Open github.com'),
                ),
              ),
              Expanded(
                child: CupertinoButton(
                  onPressed: () {
                    Clipboard.setData(ClipboardData(text: device.code));
                    tapHaptic();
                  },
                  child: const Text('Copy code'),
                ),
              ),
            ],
          ),
          Row(
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              const CupertinoActivityIndicator(radius: 7),
              const SizedBox(width: 6),
              Text(
                'waiting for the code to be entered',
                style: TextStyle(fontSize: 13, color: muted),
              ),
            ],
          ),
        ],
      ),
    );
  }
}
