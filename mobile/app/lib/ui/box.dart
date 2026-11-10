import 'package:flutter/cupertino.dart';

import '../net/rpc.dart';
import 'femme.dart';
import 'widgets.dart';

/// Placeholder Box tab: the daemon's version and deploy, the open PRs, the issue queue.
class BoxTab extends StatefulWidget {
  const BoxTab({super.key});

  @override
  State<BoxTab> createState() => _BoxTabState();
}

class _BoxTabState extends State<BoxTab> {
  Future<(Resp, Resp)>? _info;
  bool _wasOnline = false;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final link = Femme.of(context).link;
    // Ask again whenever the link comes (back) online.
    if (link.isOnline && !_wasOnline) {
      _info = Future.wait([link.req(Req.serverInfo), link.req(Req.prs)])
          .then((r) => (r[0], r[1]));
    }
    _wasOnline = link.isOnline;
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final issues = f.store.issues.values.toList()
      ..sort((a, b) => b.updated.compareTo(a.updated));
    return TabPage(
      title: 'Box',
      status: linkLine(f.link.state.value),
      slivers: [
        SliverToBoxAdapter(
          child: FutureBuilder<(Resp, Resp)>(
            future: _info,
            builder: (context, snap) {
              final err = snap.error;
              final data = snap.data;
              if (data == null) {
                return CupertinoListSection(
                  header: const Text('femmed'),
                  children: [
                    CupertinoListTile(
                      title: Text(
                        err == null ? 'waiting for the box…' : '$err',
                      ),
                    ),
                  ],
                );
              }
              final info = data.$1.serverInfo;
              final prs = data.$2.prs;
              return Column(
                children: [
                  CupertinoListSection(
                    header: const Text('femmed'),
                    children: [
                      CupertinoListTile(
                        title: Text('version ${info.version}'),
                        subtitle: Text(
                          'deployed ${info.deployed ?? 'unknown'}',
                        ),
                      ),
                    ],
                  ),
                  if (prs.isNotEmpty)
                    CupertinoListSection(
                      header: const Text('Pull requests'),
                      children: [
                        for (final p in prs)
                          CupertinoListTile(
                            title: Text('#${p.number} ${p.title}'),
                            subtitle: Text(
                              '${p.branch} · +${p.additions} −${p.deletions}',
                            ),
                          ),
                      ],
                    ),
                ],
              );
            },
          ),
        ),
        section('Issues', [
          for (final i in issues)
            CupertinoListTile(
              title: Text(i.title),
              subtitle: Text('${i.repo} · ${i.status.name}'),
            ),
        ]),
      ],
    );
  }
}
