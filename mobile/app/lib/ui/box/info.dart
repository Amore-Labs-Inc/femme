import 'package:flutter/cupertino.dart';

import '../../net/rpc.dart';
import '../../state/box.dart';
import '../../state/pairing.dart';
import '../femme.dart';
import '../theme.dart';
import '../widgets.dart';
import 'poll.dart';

/// A deploy marker as the list shows it: a commit hash down to its first 7 characters, anything
/// else as is, `no marker` when the box has none.
String shortCommit(String? deployed) {
  if (deployed == null) return 'no marker';
  final hex = RegExp(r'^[0-9a-f]{12,64}$');
  return hex.hasMatch(deployed) ? deployed.substring(0, 7) : deployed;
}

/// The Box section: which box, what femmed runs, what is deployed.
class BoxInfoSection extends StatefulWidget {
  const BoxInfoSection({super.key});

  @override
  State<BoxInfoSection> createState() => _BoxInfoSectionState();
}

class _BoxInfoSectionState extends State<BoxInfoSection> with Polling {
  ({String version, String? deployed})? _info;
  Object? _error;
  bool _checking = false;
  DateTime? _checkedAt;

  @override
  Duration get every => const Duration(minutes: 1);

  @override
  Future<void> poll() async {
    final link = Femme.of(context).link;
    if (mounted) setState(() => _checking = true);
    try {
      final r = (await link.req(Req.serverInfo)).serverInfo;
      if (mounted) {
        setState(() {
          _info = r;
          _error = null;
          _checkedAt = DateTime.now();
        });
      }
    } catch (e) {
      if (mounted) setState(() => _error = e);
    } finally {
      if (mounted) setState(() => _checking = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final info = _info;
    final host = f.prefs.host ?? '—';
    return CupertinoListSection.insetGrouped(
      header: const Text('Box'),
      footer: _checkedAt == null
          ? null
          : sectionFooter(context, switch (ago(_checkedAt!)) {
              'now' => 'checked just now',
              final a => 'checked $a ago',
            }),
      children: [
        CupertinoListTile(
          leading: const Icon(CupertinoIcons.cube_box),
          title: Text(PairInfo.boxName ?? host),
          subtitle: Text(host, maxLines: 1, overflow: TextOverflow.ellipsis),
          additionalInfo: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              // the phase word beside it says it
              ExcludeSemantics(child: StatusDot(linkColor(f.link.state.value))),
              const SizedBox(width: 6),
              Text(f.link.state.value.phase.name),
            ],
          ),
        ),
        CupertinoListTile(
          title: const Text('femmed'),
          additionalInfo: Text(
            info?.version ?? f.link.state.value.serverVersion ?? '—',
          ),
        ),
        CupertinoListTile(
          title: const Text('Deployed'),
          additionalInfo: Text(
            info == null ? '—' : shortCommit(info.deployed),
            style: const TextStyle(
              fontFamily: monoFamily,
              fontFamilyFallback: monoFallback,
            ),
          ),
        ),
        CupertinoListTile(
          title: const Text('Link round-trip'),
          additionalInfo: Text(switch (f.link.state.value.rtt) {
            null => 'n/a',
            final rtt => rttText(rtt),
          }),
        ),
        if (_error != null)
          CupertinoListTile(
            title: Text(describeError(_error!), style: TextStyle(color: muted)),
          ),
        CupertinoListTile(
          title: const Text('Check'),
          trailing: _checking
              ? const CupertinoActivityIndicator()
              : const Icon(
                  CupertinoIcons.arrow_clockwise,
                  semanticLabel: 'check the box now',
                ),
          onTap: _checking
              ? null
              : () {
                  tapHaptic();
                  pollNow();
                },
        ),
      ],
    );
  }
}
