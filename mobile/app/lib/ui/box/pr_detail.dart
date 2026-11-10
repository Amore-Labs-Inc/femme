import 'dart:async';
import 'dart:typed_data';

import 'package:flutter/cupertino.dart';

import '../../net/rpc.dart';
import '../../state/box.dart';
import '../chat/markdown.dart';
import '../femme.dart';
import '../media/lightbox.dart';
import '../theme.dart';
import '../widgets.dart';
import 'prs.dart';

/// Where a merge stands after `PrMerge` answered.
enum _Deploy { none, queued, deploying, deployed, timedOut }

/// One pull request: its description, the demo, and merge / close / fix.
class PrDetailScreen extends StatefulWidget {
  const PrDetailScreen({super.key, required this.pr});
  final Pr pr;

  @override
  State<PrDetailScreen> createState() => _PrDetailScreenState();
}

class _PrDetailScreenState extends State<PrDetailScreen> {
  bool _busy = false;
  _Deploy _deploy = _Deploy.none;
  String? _deployed;
  Timer? _poll;

  @override
  void dispose() {
    _poll?.cancel();
    super.dispose();
  }

  Pr get pr => widget.pr;

  Future<T?> _run<T>(String what, Future<T> Function() f) async {
    setState(() => _busy = true);
    try {
      return await f();
    } catch (e) {
      if (mounted) await showProblem(context, what, e);
      return null;
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  Future<void> _watchDemo() async {
    final link = Femme.of(context).link;
    final name = pr.video!;
    final bytes = await _run(
      'Demo not loaded',
      () async => (await link.req(
        Req.prVideo(name),
        timeout: const Duration(minutes: 2),
      )).blob,
    );
    if (bytes == null || !mounted) return;
    await showLightbox(
      context,
      bytes: Uint8List.fromList(bytes),
      name: name,
      video: true,
    );
  }

  Future<void> _merge() async {
    final link = Femme.of(context).link;
    final first = await confirmAction(
      context,
      title: 'Merge #${pr.number}?',
      message: pr.mergeable == false
          ? 'It conflicts with main: Claude resolves the conflicts first, then it merges and deploys by itself.'
          : 'It merges into main and deploys.',
      action: 'Merge',
      destructive: false,
    );
    if (!first || !mounted) return;
    final second = await confirmAction(
      context,
      title: 'Ship to production?',
      message: 'The box deploys main right after the merge.',
      action: 'Merge & deploy',
    );
    if (!second || !mounted) return;
    actionHaptic();
    final before = await _run('Merge failed', () async {
      String? deployed;
      try {
        deployed = (await link.req(Req.serverInfo)).serverInfo.deployed;
      } catch (_) {
        deployed = null;
      }
      await link.req(
        Req.prMerge(pr.number),
        timeout: const Duration(minutes: 2),
      );
      return deployed ?? '';
    });
    if (before == null || !mounted) return;
    if (pr.mergeable == false) {
      setState(() => _deploy = _Deploy.queued);
      return;
    }
    setState(() => _deploy = _Deploy.deploying);
    final start = DateTime.now();
    _poll = Timer.periodic(const Duration(seconds: 5), (t) async {
      if (DateTime.now().difference(start) > const Duration(minutes: 10)) {
        t.cancel();
        if (mounted) setState(() => _deploy = _Deploy.timedOut);
        return;
      }
      try {
        final d = (await link.req(Req.serverInfo)).serverInfo.deployed;
        if (d != null && d != before && mounted) {
          t.cancel();
          actionHaptic();
          setState(() {
            _deploy = _Deploy.deployed;
            _deployed = d;
          });
        }
      } catch (_) {
        // the box restarts while it deploys; keep asking
      }
    });
  }

  Future<void> _close() async {
    final link = Femme.of(context).link;
    final ok = await confirmAction(
      context,
      title: 'Close #${pr.number} without merging?',
      message: 'Its branch is deleted.',
      action: 'Close PR',
    );
    if (!ok || !mounted) return;
    final done = await _run('Close failed', () async {
      await link.req(Req.prClose(pr.number));
      return true;
    });
    if (done == true && mounted) {
      actionHaptic();
      Navigator.of(context).pop();
    }
  }

  Future<void> _fix() async {
    final fixing = pr.fixing;
    if (fixing != null) {
      await pushChat(context, fixing);
      return;
    }
    final link = Femme.of(context).link;
    actionHaptic();
    final sid = await _run(
      'Fix not started',
      () async => (await link.req(Req.prFix(pr.number))).session,
    );
    if (sid != null && mounted) await pushChat(context, sid);
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final badge = prBadge(pr);
    final status = switch (_deploy) {
      _Deploy.none => null,
      _Deploy.queued => 'queued: Claude is resolving the conflicts; it merges and deploys by itself',
      _Deploy.deploying => 'merged · deploying…',
      _Deploy.deployed => 'deployed $_deployed',
      _Deploy.timedOut => 'merged; no new deploy seen in 10 minutes',
    };
    return CupertinoPageScaffold(
      navigationBar: CupertinoNavigationBar(
        middle: Text('#${pr.number}'),
        trailing: _busy ? const CupertinoActivityIndicator() : null,
      ),
      child: SafeArea(
        child: ListView(
          children: [
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 16, 20, 4),
              child: Text(
                pr.title,
                style: const TextStyle(
                  fontSize: 22,
                  fontWeight: FontWeight.w700,
                ),
              ),
            ),
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 20),
              child: Wrap(
                spacing: 8,
                runSpacing: 4,
                crossAxisAlignment: WrapCrossAlignment.center,
                children: [
                  Text(
                    '${pr.branch} · +${pr.additions} −${pr.deletions} · ${pr.author}',
                    style: TextStyle(color: muted, fontSize: 13),
                  ),
                  if (badge != null)
                    StatusChip(
                      prBadgeLabel(badge),
                      prBadgeColor(badge),
                      faint: badge == PrBadge.checking,
                    ),
                ],
              ),
            ),
            if (status != null)
              Padding(
                padding: const EdgeInsets.fromLTRB(20, 12, 20, 0),
                child: Row(
                  children: [
                    if (_deploy == _Deploy.deploying)
                      const Padding(
                        padding: EdgeInsets.only(right: 8),
                        child: CupertinoActivityIndicator(),
                      ),
                    Expanded(
                      child: Text(
                        status,
                        style: const TextStyle(fontWeight: FontWeight.w600),
                      ),
                    ),
                  ],
                ),
              ),
            CupertinoListSection.insetGrouped(
              children: [
                if (pr.video != null)
                  CupertinoListTile(
                    leading: const Icon(CupertinoIcons.play_rectangle_fill),
                    title: const Text('Watch demo'),
                    onTap: _busy ? null : _watchDemo,
                  ),
                CupertinoListTile(
                  leading: const Icon(
                    CupertinoIcons.arrow_merge,
                    color: FemmeColors.accent,
                  ),
                  title: const Text(
                    'Merge & deploy',
                    style: TextStyle(fontWeight: FontWeight.w600),
                  ),
                  onTap: _busy || pr.merging || _deploy != _Deploy.none
                      ? null
                      : _merge,
                ),
                if (pr.mergeable == false || pr.fixing != null)
                  CupertinoListTile(
                    leading: const Icon(CupertinoIcons.wand_stars),
                    title: Text(
                      pr.fixing != null ? 'Open fix chat' : 'Fix with Claude',
                    ),
                    onTap: _busy ? null : _fix,
                  ),
                CupertinoListTile(
                  leading: const Icon(
                    CupertinoIcons.xmark_circle,
                    color: CupertinoColors.destructiveRed,
                  ),
                  title: const Text(
                    'Close PR',
                    style: TextStyle(color: CupertinoColors.destructiveRed),
                  ),
                  onTap: _busy ? null : _close,
                ),
              ],
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 0, 20, 32),
              child: pr.body.trim().isEmpty
                  ? Text('No description.', style: TextStyle(color: muted))
                  : Md(pr.body),
            ),
          ],
        ),
      ),
    );
  }
}
