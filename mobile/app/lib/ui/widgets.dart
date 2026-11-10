import 'package:flutter/cupertino.dart';

import '../net/link.dart';
import '../net/models.dart';
import 'theme.dart';

Color statusColor(Session s) {
  if (s.needsYou) return FemmeColors.pause;
  return switch (s.status) {
    SessionStatus.working || SessionStatus.starting => FemmeColors.work,
    SessionStatus.failed || SessionStatus.crashed => FemmeColors.bad,
    SessionStatus.closed || SessionStatus.interrupted => FemmeColors.done,
    _ => FemmeColors.queue,
  };
}

/// The status dot: an 8 px ink-bordered square in the lane colour.
class StatusDot extends StatelessWidget {
  const StatusDot(this.color, {super.key});
  final Color color;

  @override
  Widget build(BuildContext context) => Container(
    width: 8,
    height: 8,
    decoration: BoxDecoration(
      color: color,
      border: Border.all(
        color: FemmeColors.inkC.resolveFrom(context),
        width: 1.2,
      ),
    ),
  );
}

class SessionTile extends StatelessWidget {
  const SessionTile(this.session, {super.key, this.subtitle, this.onTap});
  final Session session;
  final String? subtitle;
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) => CupertinoListTile(
    leading: StatusDot(statusColor(session)),
    leadingSize: 8,
    title: Text(session.title),
    subtitle: Text(
      subtitle ?? '${wireStatus(session)} · ${session.cwd}',
      maxLines: 1,
      overflow: TextOverflow.ellipsis,
    ),
    onTap: onTap,
  );
}

String wireStatus(Session s) => s.needsYou ? 'needs you' : s.status.name;

/// `online · femmed 0.9.1` / `connecting…` / `offline: <why>`.
String linkLine(LinkState s) => switch (s.phase) {
  LinkPhase.online =>
    'online${s.serverVersion == null ? '' : ' · femmed ${s.serverVersion}'}',
  LinkPhase.connecting => 'connecting…',
  LinkPhase.offline => 'offline${s.reason == null ? '' : ': ${s.reason}'}',
};

/// A tab's page: large title, the link line under it, then [slivers].
class TabPage extends StatelessWidget {
  const TabPage({
    super.key,
    required this.title,
    required this.status,
    required this.slivers,
  });
  final String title;
  final String status;
  final List<Widget> slivers;

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    child: CustomScrollView(
      slivers: [
        CupertinoSliverNavigationBar(largeTitle: Text(title)),
        SliverToBoxAdapter(
          child: Padding(
            padding: const EdgeInsets.fromLTRB(16, 4, 16, 8),
            child: Text(
              status,
              style: TextStyle(
                fontSize: 13,
                color: FemmeColors.mutedC.resolveFrom(context),
              ),
            ),
          ),
        ),
        ...slivers,
      ],
    ),
  );
}

/// A titled list section, or nothing when [children] is empty.
Widget section(String header, List<Widget> children) => SliverToBoxAdapter(
  child: children.isEmpty
      ? const SizedBox.shrink()
      : CupertinoListSection(header: Text(header), children: children),
);

/// A grey centred line for empty states.
Widget emptyLine(String text) => SliverFillRemaining(
  hasScrollBody: false,
  child: Center(
    child: Text(text, style: const TextStyle(color: FemmeColors.done)),
  ),
);
