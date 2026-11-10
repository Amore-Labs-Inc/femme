import 'package:flutter/cupertino.dart';

import '../net/models.dart';
import 'chat.dart';
import 'femme.dart';
import 'widgets.dart';

/// Placeholder Chats tab: every open session, grouped by folder.
class ChatsTab extends StatelessWidget {
  const ChatsTab({super.key});

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final byFolder = <String, List<Session>>{};
    for (final s in f.store.sessions.values) {
      if (s.status == SessionStatus.closed) continue;
      byFolder.putIfAbsent(s.cwd, () => []).add(s);
    }
    final folders = byFolder.keys.toList()..sort();
    return TabPage(
      title: 'Chats',
      status:
          '${f.store.sessions.length} sessions · ${linkLine(f.link.state.value)}',
      slivers: [
        if (folders.isEmpty) emptyLine('No sessions yet.'),
        for (final dir in folders)
          section(dir, [
            for (final s
                in byFolder[dir]!
                  ..sort((a, b) => b.created.compareTo(a.created)))
              SessionTile(
                s,
                subtitle: wireStatus(s),
                onTap: () => openChat(context, s.id),
              ),
          ]),
      ],
    );
  }
}
