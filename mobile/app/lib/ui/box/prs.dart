import 'package:flutter/cupertino.dart';

import '../../net/rpc.dart';
import '../../state/box.dart';
import '../femme.dart';
import '../theme.dart';
import '../widgets.dart';
import 'poll.dart';
import 'pr_detail.dart';

Color prBadgeColor(PrBadge b) => switch (b) {
  PrBadge.merging => FemmeColors.accent,
  PrBadge.fixing => FemmeColors.pause,
  PrBadge.conflicts => FemmeColors.bad,
  PrBadge.checking => FemmeColors.done,
};

/// A small status tag: a capsule tinted with [color], or the grey system fill when [faint].
class StatusChip extends StatelessWidget {
  const StatusChip(this.label, this.color, {super.key, this.faint = false});
  final String label;
  final Color color;
  final bool faint;

  @override
  Widget build(BuildContext context) => Container(
    padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
    decoration: BoxDecoration(
      color: faint
          ? CupertinoColors.tertiarySystemFill.resolveFrom(context)
          : color.withValues(alpha: 0.16),
      borderRadius: const BorderRadius.all(Radius.circular(999)),
    ),
    child: Text(
      label,
      style: TextStyle(
        fontSize: 13,
        color: faint
            ? CupertinoColors.secondaryLabel.resolveFrom(context)
            : color,
        fontWeight: FontWeight.w600,
      ),
    ),
  );
}

/// The open pull requests, read every 10 s while shown.
class PrsSection extends StatefulWidget {
  const PrsSection({super.key});

  @override
  State<PrsSection> createState() => _PrsSectionState();
}

class _PrsSectionState extends State<PrsSection> with Polling {
  List<Pr>? _prs;
  Object? _error;

  /// The daemon's auto-fix setting; null when it does not know it (or not read yet).
  bool? _autoFix;
  bool _askedAutoFix = false;

  @override
  Duration get every => const Duration(seconds: 10);

  @override
  Future<void> poll() async {
    final link = Femme.of(context).link;
    try {
      final prs = (await link.req(Req.prs)).prs;
      if (mounted) {
        setState(() {
          _prs = prs;
          _error = null;
        });
      }
    } catch (e) {
      if (mounted) setState(() => _error = e);
    }
    if (!_askedAutoFix) {
      _askedAutoFix = true;
      try {
        final on = (await link.req(Req.prAutoFix())).prAutoFix;
        if (mounted) setState(() => _autoFix = on);
      } catch (_) {
        // an older daemon: the toggle stays hidden
      }
    }
  }

  Future<void> _setAutoFix(bool on) async {
    final link = Femme.of(context).link;
    tapHaptic();
    setState(() => _autoFix = on);
    try {
      final now = (await link.req(Req.prAutoFix(on: on))).prAutoFix;
      if (mounted) setState(() => _autoFix = now);
    } catch (e) {
      if (mounted) {
        setState(() => _autoFix = !on);
        await showProblem(context, 'Auto-fix not changed', e);
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final prs = _prs;
    return CupertinoListSection.insetGrouped(
      header: Text('Pull requests${prs == null ? '' : ' · ${prs.length}'}'),
      children: [
        if (prs == null && _error == null)
          const CupertinoListTile(title: CupertinoActivityIndicator()),
        if (_error != null)
          CupertinoListTile(
            title: Text(describeError(_error!), style: TextStyle(color: muted)),
          ),
        if (prs != null && prs.isEmpty)
          CupertinoListTile(
            title: Text(
              'no open pull requests',
              style: TextStyle(color: muted),
            ),
          ),
        for (final p in prs ?? const <Pr>[]) PrRow(pr: p, onChanged: pollNow),
        if (_autoFix != null)
          CupertinoListTile(
            title: const Text('Auto-fix conflicts'),
            subtitle: const Text(
              'after a merge, start a fix chat per conflict',
            ),
            trailing: CupertinoSwitch(value: _autoFix!, onChanged: _setAutoFix),
          ),
      ],
    );
  }
}

class PrRow extends StatelessWidget {
  const PrRow({super.key, required this.pr, required this.onChanged});
  final Pr pr;
  final Future<void> Function() onChanged;

  @override
  Widget build(BuildContext context) {
    final badge = prBadge(pr);
    final muted = FemmeColors.mutedC.resolveFrom(context);
    return CupertinoListTile(
      leadingSize: 8,
      leading: const StatusDot(FemmeColors.pr),
      title: Row(
        children: [
          Flexible(
            child: Text(
              '#${pr.number} ${pr.title}',
              overflow: TextOverflow.ellipsis,
            ),
          ),
          if (badge != null) ...[
            const SizedBox(width: 6),
            StatusChip(
              prBadgeLabel(badge),
              prBadgeColor(badge),
              faint: badge == PrBadge.checking,
            ),
          ],
        ],
      ),
      subtitle: Text.rich(
        TextSpan(
          children: [
            TextSpan(text: '${pr.branch} · '),
            TextSpan(
              text: '+${pr.additions} ',
              style: const TextStyle(color: FemmeColors.work),
            ),
            TextSpan(
              text: '−${pr.deletions}',
              style: const TextStyle(color: FemmeColors.bad),
            ),
            TextSpan(text: ' · ${pr.author}'),
          ],
        ),
        maxLines: 1,
        overflow: TextOverflow.ellipsis,
        style: TextStyle(color: muted),
      ),
      trailing: const CupertinoListTileChevron(),
      onTap: () async {
        tapHaptic();
        await Navigator.of(context).push(
          CupertinoPageRoute<void>(builder: (_) => PrDetailScreen(pr: pr)),
        );
        await onChanged();
      },
    );
  }
}
