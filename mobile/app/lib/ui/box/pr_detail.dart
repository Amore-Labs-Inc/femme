import 'dart:typed_data';

import 'package:flutter/cupertino.dart';
import 'package:url_launcher/url_launcher.dart';

import '../../net/rpc.dart';
import '../../state/box.dart';
import '../../state/box_watch.dart';
import '../chat/markdown.dart';
import '../femme.dart';
import '../media/lightbox.dart';
import '../theme.dart';
import '../widgets.dart';
import 'prs.dart';

/// One pull request: its description, the demo, and merge / close / fix. A merge is followed by
/// the app's [MergeWatch], so it goes on after the page closes.
class PrDetailScreen extends StatefulWidget {
  const PrDetailScreen({super.key, required this.pr});
  final Pr pr;

  @override
  State<PrDetailScreen> createState() => _PrDetailScreenState();
}

class _PrDetailScreenState extends State<PrDetailScreen> {
  bool _busy = false;
  late Pr _pr = widget.pr;

  /// The PR left the open list (merged or closed elsewhere).
  bool _gone = false;

  MergeWatch? _merges;

  /// A watch of this page's own when the app has none (tests).
  MergeWatch? _own;

  @override
  void didChangeDependencies() {
    if (_merges == null) {
      final f = Femme.of(context);
      final m = f.boxWatch?.merges ?? (_own = MergeWatch(f.link.req));
      m.prs.addListener(_onList);
      m.states.addListener(_onMerge);
      _merges = m;
    }
    super.didChangeDependencies();
  }

  @override
  void dispose() {
    _merges?.prs.removeListener(_onList);
    _merges?.states.removeListener(_onMerge);
    _own?.dispose();
    super.dispose();
  }

  Pr get pr => _pr;

  void _onList() {
    final list = _merges?.prs.value;
    if (list != null && mounted) _take(list);
  }

  MergeState? _last;

  void _onMerge() {
    final s = _merges?.of(pr.number);
    if (s is MergeDeployed && _last is! MergeDeployed) actionHaptic();
    _last = s;
    if (mounted) setState(() {});
  }

  void _take(List<Pr> list) {
    final p = list.where((p) => p.number == pr.number).firstOrNull;
    setState(() {
      _gone = p == null;
      if (p != null) _pr = p;
    });
  }

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
    final merges = _merges!;
    final number = pr.number;
    final first = await confirmAction(
      context,
      title: 'Merge #$number?',
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
      final from = await merges.deployed();
      await link.req(Req.prMerge(number), timeout: const Duration(minutes: 2));
      return (from,);
    });
    if (before == null) return;
    await merges.merged(number, before.$1);
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
    final merge = _merges?.of(pr.number);
    final line = merge == null ? null : mergeLine(merge, _merges!.poll);
    final following = merge is MergeQueued || merge is MergeDeploying;
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
            if (_gone && !following && merge is! MergeDeployed)
              Padding(
                padding: const EdgeInsets.fromLTRB(20, 12, 20, 0),
                child: Text('no longer open', style: TextStyle(color: muted)),
              ),
            if (line != null)
              Padding(
                padding: const EdgeInsets.fromLTRB(20, 12, 20, 0),
                child: Row(
                  children: [
                    if (following)
                      const Padding(
                        padding: EdgeInsets.only(right: 8),
                        child: CupertinoActivityIndicator(),
                      ),
                    Expanded(
                      child: Text(
                        line.$1,
                        style: TextStyle(
                          fontWeight: FontWeight.w600,
                          color: line.$2 ? null : FemmeColors.bad,
                        ),
                      ),
                    ),
                  ],
                ),
              ),
            CupertinoListSection.insetGrouped(
              children: [
                if (Uri.tryParse(pr.url)?.hasScheme ?? false)
                  CupertinoListTile(
                    leading: const Icon(CupertinoIcons.link),
                    title: const Text('Open on GitHub'),
                    subtitle: Text(pr.url, overflow: TextOverflow.ellipsis),
                    onTap: () {
                      tapHaptic();
                      launchUrl(
                        Uri.parse(pr.url),
                        mode: LaunchMode.externalApplication,
                      );
                    },
                  ),
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
                  onTap: _busy || pr.merging || following || _gone
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
