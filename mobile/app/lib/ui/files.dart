import 'package:flutter/cupertino.dart';

import '../state/files.dart';
import 'femme.dart';
import 'files/browser.dart';

export 'files/browser.dart' show FilesBrowser;

/// The session folder's files: browse, view, share (`mobile/PARITY.md` §10).
class FilesScreen extends StatefulWidget {
  const FilesScreen({super.key, required this.sid});

  final String sid;

  @override
  State<FilesScreen> createState() => _FilesScreenState();
}

class _FilesScreenState extends State<FilesScreen> {
  FilesTree? _tree;

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    _tree ??= FilesTree(requester: Femme.of(context).link, sid: widget.sid);
  }

  @override
  void dispose() {
    _tree?.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final cwd = Femme.of(context).store.sessions[widget.sid]?.cwd ?? '';
    final parts = cwd.split('/').where((p) => p.isNotEmpty);
    return FilesBrowser(
      tree: _tree!,
      cwd: cwd,
      rootName: parts.isEmpty ? (cwd.isEmpty ? 'session' : cwd) : parts.last,
    );
  }
}
