import 'package:flutter/cupertino.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../net/models.dart' hide Row;
import '../net/rpc.dart';
import 'femme.dart';
import 'new_chat.dart';
import 'theme.dart';
import 'widgets.dart';

/// One project folder of the Chats list.
class Project {
  const Project(this.dir, this.sessions, {this.label});

  /// `cwd` without a trailing `/`.
  final String dir;

  /// Newest first.
  final List<Session> sessions;

  /// Set when another project has the same last part: then the whole `~/...` path is shown.
  final String? label;

  /// The folder's last part, `~` for home; the whole path when two folders share a name.
  String get header => label ?? lastPart(dir);

  static String lastPart(String dir) {
    final short = homeShort(dir);
    if (short == '~') return '~';
    final parts = short.split('/').where((p) => p.isNotEmpty);
    return parts.isEmpty ? dir : parts.last;
  }
}

bool isLive(Session s) => s.closed == null && s.status != SessionStatus.closed;

/// Live sessions newest first by `(created, id)`, grouped by folder, the project of the newest
/// session first; [query] keeps the sessions whose title or folder holds it (any case).
List<Project> groupByProject(Iterable<Session> all, {String query = ''}) {
  final q = query.trim().toLowerCase();
  final live =
      all
          .where(isLive)
          .where(
            (s) =>
                q.isEmpty ||
                s.title.toLowerCase().contains(q) ||
                s.cwd.toLowerCase().contains(q),
          )
          .toList()
        ..sort((a, b) {
          final c = b.created.compareTo(a.created);
          return c != 0 ? c : b.id.compareTo(a.id);
        });
  final groups = <String, List<Session>>{};
  for (final s in live) {
    final dir = s.cwd.length > 1 && s.cwd.endsWith('/')
        ? s.cwd.substring(0, s.cwd.length - 1)
        : s.cwd;
    groups.putIfAbsent(dir, () => []).add(s);
  }
  final names = <String, int>{};
  for (final dir in groups.keys) {
    names.update(Project.lastPart(dir), (n) => n + 1, ifAbsent: () => 1);
  }
  return [
    for (final e in groups.entries)
      Project(
        e.key,
        e.value,
        label: names[Project.lastPart(e.key)]! > 1 ? homeShort(e.key) : null,
      ),
  ];
}

/// The session's lane in words, the same lanes as Now's sections and the status dot's colour:
/// `needs you`, `done`, `working`, else its status (`idle`, `paused`, `failed`, ...).
/// The row's second line: the status in words (the dot alone is colour), the agent (or a
/// gateway model's provider) and how long ago it started.
String sessionSubtitle(Session s, {DateTime? now}) {
  final model = s.model;
  final who = model != null && model.contains('/')
      ? model.split('/').first
      : s.agent.name;
  final when = ago(DateTime.fromMillisecondsSinceEpoch(s.created), now: now);
  return '${statusWord(s)} · $who · $when';
}

/// Every live session, grouped by project folder.
class ChatsTab extends StatefulWidget {
  const ChatsTab({super.key});

  @override
  State<ChatsTab> createState() => _ChatsTabState();
}

class _ChatsTabState extends State<ChatsTab> {
  static const _kFolded = 'chats.folded';
  final _search = TextEditingController();
  Set<String> _folded = {};
  final _starting = <String>{};
  SharedPreferences? _sp;

  @override
  void initState() {
    super.initState();
    SharedPreferences.getInstance().then((sp) {
      if (!mounted) return;
      setState(() {
        _sp = sp;
        _folded = (sp.getStringList(_kFolded) ?? const []).toSet();
      });
    });
  }

  @override
  void dispose() {
    _search.dispose();
    super.dispose();
  }

  void _toggle(String dir) {
    tapHaptic();
    setState(() {
      if (!_folded.remove(dir)) _folded.add(dir);
    });
    _sp?.setStringList(_kFolded, _folded.toList());
  }

  /// The header's `+`: a headless Claude in that folder, then its chat.
  Future<void> _startIn(String dir) async {
    if (_starting.contains(dir)) return;
    final link = Femme.of(context).link;
    setState(() => _starting.add(dir));
    actionHaptic();
    try {
      final sid = (await link.req(
        Req.sessionCreate(
          cwd: dir,
          agent: Agent.claude,
          mode: Mode.headless,
          model: 'claude-opus-5-5',
        ),
      )).session;
      if (mounted) await pushChat(context, sid);
    } catch (e) {
      if (mounted) await showProblem(context, 'Could not start', e);
    } finally {
      if (mounted) setState(() => _starting.remove(dir));
    }
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final link = f.link.state.value;
    final projects = groupByProject(f.store.live, query: _search.text);
    final count = projects.fold(0, (n, p) => n + p.sessions.length);
    final muted = FemmeColors.mutedC.resolveFrom(context);
    return TabPage(
      title: 'Chats',
      status: '$count live · ${linkLine(link)}',
      dot: linkColor(link),
      trailing: CupertinoButton(
        padding: EdgeInsets.zero,
        minimumSize: const Size(44, 44),
        onPressed: () => openNewChat(context),
        child: const Icon(CupertinoIcons.add, semanticLabel: 'New chat'),
      ),
      slivers: [
        SliverToBoxAdapter(
          child: Padding(
            padding: const EdgeInsets.fromLTRB(16, 4, 16, 0),
            child: CupertinoSearchTextField(
              controller: _search,
              placeholder: 'Search chats and folders',
              onChanged: (_) => setState(() {}),
            ),
          ),
        ),
        if (projects.isEmpty)
          emptyLine(_search.text.isEmpty ? 'No sessions yet.' : 'No matches.'),
        for (final p in projects)
          SliverToBoxAdapter(
            child: CupertinoListSection.insetGrouped(
              header: Row(
                children: [
                  Expanded(
                    child: GestureDetector(
                      behavior: HitTestBehavior.opaque,
                      onTap: () => _toggle(p.dir),
                      child: Row(
                        children: [
                          Icon(
                            _folded.contains(p.dir)
                                ? CupertinoIcons.chevron_right
                                : CupertinoIcons.chevron_down,
                            size: 14,
                            color: muted,
                          ),
                          const SizedBox(width: 6),
                          Flexible(
                            child: Text(
                              p.header,
                              overflow: TextOverflow.ellipsis,
                            ),
                          ),
                          if (_folded.contains(p.dir))
                            Text(
                              '  ${p.sessions.length}',
                              style: TextStyle(color: muted),
                            ),
                        ],
                      ),
                    ),
                  ),
                  CupertinoButton(
                    padding: EdgeInsets.zero,
                    sizeStyle: CupertinoButtonSize.small,
                    minimumSize: const Size(44, 44),
                    onPressed: _starting.contains(p.dir)
                        ? null
                        : () => _startIn(p.dir),
                    child: _starting.contains(p.dir)
                        ? const CupertinoActivityIndicator()
                        : Icon(
                            CupertinoIcons.add_circled,
                            semanticLabel: 'New chat in ${p.header}',
                          ),
                  ),
                ],
              ),
              children: [
                if (_folded.contains(p.dir))
                  CupertinoListTile(
                    title: Text(
                      homeShort(p.dir),
                      style: TextStyle(color: muted, fontSize: 13),
                    ),
                    onTap: () => _toggle(p.dir),
                  )
                else
                  for (final s in p.sessions)
                    SessionActions(
                      session: s,
                      child: CupertinoListTile(
                        leading: StatusDot(
                          statusColor(s),
                          label: statusWord(s),
                        ),
                        leadingSize: 8,
                        title: Text(s.title),
                        subtitle: Text(sessionSubtitle(s)),
                        trailing: const CupertinoListTileChevron(),
                        onTap: () {
                          tapHaptic();
                          pushChat(context, s.id);
                        },
                      ),
                    ),
              ],
            ),
          ),
      ],
    );
  }
}
