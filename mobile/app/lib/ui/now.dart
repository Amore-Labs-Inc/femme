import 'package:flutter/cupertino.dart';

import '../net/models.dart';
import 'chat.dart';
import 'femme.dart';
import 'widgets.dart';

/// Placeholder Now tab: the store's needs-you / done / working lists.
class NowTab extends StatelessWidget {
  const NowTab({super.key});

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final s = f.store;
    final blocked = s.needsYou, done = s.done, working = s.working;
    List<Widget> tiles(List<Session> list) => [
      for (final x in list)
        SessionTile(x, onTap: () => openChat(context, x.id)),
    ];
    return TabPage(
      title: 'Now',
      status: linkLine(f.link.state.value),
      slivers: [
        if (blocked.isEmpty && done.isEmpty && working.isEmpty)
          emptyLine('Nothing needs you.')
        else ...[
          section('Needs you', tiles(blocked)),
          section('Done', tiles(done)),
          section('Working', tiles(working)),
        ],
      ],
    );
  }
}
