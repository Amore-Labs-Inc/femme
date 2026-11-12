import 'package:flutter/cupertino.dart';

import '../net/models.dart';
import '../net/rpc.dart';
import '../state/box.dart';
import 'chat.dart';
import 'femme.dart';
import 'theme.dart';
import 'widgets.dart';

/// The models the picker offers: label → id.
const chatModels = [
  ('opus 5.5', 'claude-opus-5-5'),
  ('sonnet 5.5', 'claude-sonnet-5-5'),
  ('haiku 5.5', 'claude-haiku-5-5'),
  ('fable 5.2', 'claude-fable-5-2'),
  ('deepseek', 'deepseek/deepseek-v4.1-flash-ultrafast'),
];

/// The folder to list for the typed [text] and the start of a name to filter its folders by:
/// `~/fe` is `~/` and `fe`, `~` is `~` and nothing (the desktop's `projects.rs` `split`).
(String, String) splitFolderPath(String text) {
  final i = text.lastIndexOf('/');
  if (i >= 0) return (text.substring(0, i + 1), text.substring(i + 1));
  if (text == '~') return (text, '');
  return ('', text);
}

/// The folders of a listing that start with [filter], case aside; hidden ones only once the
/// filter starts with a dot (the desktop's `projects.rs` `matching`).
List<String> matchingFolders(Iterable<String> names, String filter) {
  final f = filter.toLowerCase();
  return [
    for (final n in names)
      if ((f.startsWith('.') || !n.startsWith('.')) &&
          n.toLowerCase().startsWith(f))
        n,
  ];
}

/// Opens the New chat sheet; on Start pushes the new session's chat on this tab.
Future<void> openNewChat(BuildContext context, {String? cwd}) async {
  final nav = Navigator.of(context);
  final sid = await showCupertinoSheet<String>(
    context: context,
    scrollableBuilder: (_, _) => NewChatSheet(cwd: cwd),
  );
  if (sid != null) {
    await nav.push(
      CupertinoPageRoute<void>(builder: (_) => ChatScreen(sid: sid)),
    );
  }
}

class NewChatSheet extends StatefulWidget {
  const NewChatSheet({super.key, this.cwd});
  final String? cwd;

  @override
  State<NewChatSheet> createState() => _NewChatSheetState();
}

class _NewChatSheetState extends State<NewChatSheet> {
  final _path = TextEditingController();
  final _label = TextEditingController();
  final _prompt = TextEditingController();
  Agent _agent = Agent.claude;
  Mode _mode = Mode.headless;
  String _model = chatModels.first.$2;

  /// The folder shown (absolute, as the box answered), its subfolders, and home.
  String? _dir;
  String? _home;
  List<String> _names = const [];
  String? _error;
  bool _missing = false;
  bool _loading = false;
  bool _starting = false;
  bool _started = false;

  /// Counts listings asked for: only the newest one's answer is shown.
  int _asked = 0;

  /// What follows the last `/` of the field: the folders shown start with it.
  String _filter = '';

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (!_started) {
      _started = true;
      _open(widget.cwd ?? '~');
    }
  }

  @override
  void dispose() {
    _path.dispose();
    _label.dispose();
    _prompt.dispose();
    super.dispose();
  }

  /// Lists [path]. [keepText]: the user is typing, so the field keeps what they typed instead of
  /// becoming the folder's path.
  Future<void> _open(
    String path, {
    bool make = false,
    bool keepText = false,
  }) async {
    final link = Femme.of(context).link;
    final asked = ++_asked;
    setState(() {
      _loading = true;
      _error = null;
      _missing = false;
    });
    try {
      final r = (await link.req(Req.folders(path, make: make))).folders;
      if (!mounted || asked != _asked) return;
      setState(() {
        _home ??= path == '~' ? r.path : null;
        _dir = r.path;
        _names = [...r.names]
          ..sort((a, b) => a.toLowerCase().compareTo(b.toLowerCase()));
        if (!keepText) {
          _path.text = _shown(r.path);
          _filter = '';
        }
      });
    } catch (e) {
      if (!mounted || asked != _asked) return;
      setState(() {
        _error = describeError(e);
        _missing = e is FemmeError && e.code == 'NotFound';
      });
    } finally {
      if (mounted && asked == _asked) setState(() => _loading = false);
    }
  }

  /// The field for folder [abs]: its path ending in `/`, so what is typed next filters it.
  String _shown(String abs) {
    final d = _display(abs);
    return d.endsWith('/') ? d : '$d/';
  }

  /// The field changed: filter by what follows the last `/`, and list the folder before it when
  /// that is another one.
  void _typed(String text) {
    final (dir, filter) = splitFolderPath(text);
    setState(() => _filter = filter);
    final shownDir = _dir == null ? null : _shown(_dir!);
    if (dir.isNotEmpty && dir != shownDir && dir != '~') {
      _open(dir, keepText: true);
    } else if (dir == '~' && shownDir != '~/') {
      _open('~', keepText: true);
    }
  }

  String _display(String abs) {
    final h = _home;
    if (h != null && (abs == h || abs.startsWith('$h/'))) {
      return '~${abs.substring(h.length)}';
    }
    return homeShort(abs);
  }

  /// The breadcrumb: (name, absolute path) from home down to [_dir].
  List<(String, String)> _crumbs() {
    final dir = _dir;
    if (dir == null) return const [];
    final h = _home;
    final base = h != null && (dir == h || dir.startsWith('$h/')) ? h : '';
    final rest = dir
        .substring(base.length)
        .split('/')
        .where((p) => p.isNotEmpty);
    final out = <(String, String)>[
      (base.isEmpty ? '/' : '~', base.isEmpty ? '/' : base),
    ];
    var acc = base;
    for (final p in rest) {
      acc = '$acc/$p';
      out.add((p, acc));
    }
    return out;
  }

  Future<void> _start() async {
    final dir = _dir;
    if (dir == null) return;
    final link = Femme.of(context).link;
    setState(() => _starting = true);
    actionHaptic();
    String? opt(TextEditingController c) =>
        c.text.trim().isEmpty ? null : c.text.trim();
    try {
      final sid = (await link.req(
        Req.sessionCreate(
          cwd: dir,
          agent: _agent,
          mode: _mode,
          label: opt(_label),
          prompt: opt(_prompt),
          model: _agent == Agent.claude ? _model : null,
        ),
      )).session;
      if (mounted) Navigator.of(context).pop(sid);
    } catch (e) {
      if (mounted) await showProblem(context, 'Could not start', e);
    } finally {
      if (mounted) setState(() => _starting = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final crumbs = _crumbs();
    final shown = matchingFolders(_names, _filter);
    return SheetPage(
      title: 'New chat',
      doneLabel: 'Start',
      busy: _starting,
      onDone: _dir == null || _loading ? null : _start,
      child: ListView(
        children: [
          CupertinoListSection.insetGrouped(
            header: const Text('Folder'),
            children: [
              Padding(
                padding: const EdgeInsets.fromLTRB(12, 8, 12, 4),
                child: CupertinoTextField(
                  controller: _path,
                  placeholder: '~/path/to/project',
                  autocorrect: false,
                  prefix: const Padding(
                    padding: EdgeInsets.only(left: 8),
                    child: Icon(CupertinoIcons.folder, size: 18),
                  ),
                  textInputAction: TextInputAction.go,
                  onChanged: _typed,
                  onSubmitted: (v) => _open(v.trim().isEmpty ? '~' : v.trim()),
                ),
              ),
              SizedBox(
                height: 36,
                child: ListView(
                  scrollDirection: Axis.horizontal,
                  padding: const EdgeInsets.symmetric(horizontal: 8),
                  children: [
                    for (final (i, (name, abs)) in crumbs.indexed) ...[
                      if (i > 0)
                        Icon(
                          CupertinoIcons.chevron_right,
                          size: 12,
                          color: muted,
                        ),
                      CupertinoButton(
                        sizeStyle: CupertinoButtonSize.small,
                        padding: const EdgeInsets.symmetric(horizontal: 6),
                        onPressed: i == crumbs.length - 1
                            ? null
                            : () => _open(abs),
                        child: Text(name),
                      ),
                    ],
                  ],
                ),
              ),
              if (_loading)
                const Padding(
                  padding: EdgeInsets.all(12),
                  child: CupertinoActivityIndicator(),
                ),
              if (_error != null)
                CupertinoListTile(
                  title: Text(_error!, style: TextStyle(color: muted)),
                  trailing: _missing
                      ? CupertinoButton(
                          sizeStyle: CupertinoButtonSize.small,
                          onPressed: () => _open(_path.text.trim(), make: true),
                          child: const Text('Create folder'),
                        )
                      : null,
                ),
              if (!_loading && _error == null && shown.isEmpty)
                CupertinoListTile(
                  title: Text(
                    _filter.isEmpty
                        ? 'no subfolders'
                        : 'no folder starts with $_filter',
                    style: TextStyle(color: muted),
                  ),
                ),
              for (final n in shown)
                CupertinoListTile(
                  leading: const Icon(CupertinoIcons.folder_fill),
                  title: Text(n),
                  trailing: const CupertinoListTileChevron(),
                  onTap: () {
                    tapHaptic();
                    _open('$_dir/$n');
                  },
                ),
            ],
          ),
          CupertinoListSection.insetGrouped(
            header: const Text('Agent'),
            children: [
              _segment<Agent>(_agent, const {
                Agent.claude: 'Claude',
                Agent.codex: 'Codex',
                Agent.pi: 'Pi',
                Agent.shell: 'Shell',
              }, (a) => setState(() => _agent = a)),
              _segment<Mode>(_mode, const {
                Mode.headless: 'Headless',
                Mode.tui: 'Terminal',
              }, (m) => setState(() => _mode = m)),
              if (_agent == Agent.claude)
                CupertinoListTile(
                  title: const Text('Model'),
                  additionalInfo: Text(
                    chatModels.firstWhere((m) => m.$2 == _model).$1,
                  ),
                  trailing: const CupertinoListTileChevron(),
                  onTap: _pickModel,
                ),
            ],
          ),
          CupertinoListSection.insetGrouped(
            header: const Text('Start with'),
            children: [
              Padding(
                padding: const EdgeInsets.fromLTRB(12, 8, 12, 4),
                child: CupertinoTextField(
                  controller: _label,
                  placeholder: 'label (optional)',
                ),
              ),
              Padding(
                padding: const EdgeInsets.fromLTRB(12, 4, 12, 8),
                child: CupertinoTextField(
                  controller: _prompt,
                  placeholder: 'first prompt (optional)',
                  minLines: 3,
                  maxLines: 8,
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }

  Widget _segment<T extends Object>(
    T value,
    Map<T, String> labels,
    void Function(T) set,
  ) => Padding(
    padding: const EdgeInsets.all(10),
    child: CupertinoSlidingSegmentedControl<T>(
      groupValue: value,
      onValueChanged: (v) {
        if (v != null) {
          tapHaptic();
          set(v);
        }
      },
      children: {for (final e in labels.entries) e.key: Text(e.value)},
    ),
  );

  Future<void> _pickModel() async {
    final m = await showCupertinoModalPopup<String>(
      context: context,
      builder: (c) => CupertinoActionSheet(
        title: const Text('Model'),
        actions: [
          for (final (label, id) in chatModels)
            CupertinoActionSheetAction(
              isDefaultAction: id == _model,
              onPressed: () => Navigator.pop(c, id),
              child: Text(label),
            ),
        ],
        cancelButton: CupertinoActionSheetAction(
          onPressed: () => Navigator.pop(c),
          child: const Text('Cancel'),
        ),
      ),
    );
    if (m != null) setState(() => _model = m);
  }
}
